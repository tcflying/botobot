//! HTTP transport：把 Hub 暴露成可 curl 的请求/响应 API。

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use base_types::{AgentEvent, Message};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant, timeout};

use crate::protocol::{Event, EventMsg, Op, OpId, SessionId, Submission};
use crate::{BotEntry, Hub};

pub fn router() -> Router<Hub> {
    Router::new()
        .route("/api/bots", get(list_bots).post(create_bot))
        // §5.7 bot 市场：内置模板清单。
        .route("/api/bot-templates", get(list_bot_templates))
        .route("/api/bots/:bot_id/sessions", post(create_bot_session))
        // §5.5 B8：能力协商——前端据此显隐 UI（一前端多后端形态）。
        .route("/api/capabilities", get(get_capabilities))
        // §5.5 C11：bot 属性面板数据。
        .route("/api/bots/:bot_id/info", get(get_bot_info))
        // §5.7：换 bot profile（人格）。
        .route("/api/bots/:bot_id", patch(update_bot))
        .route("/api/teams", get(list_teams).post(open_team))
        .route("/api/teams/:team_id/conduct", post(conduct_team))
        // §5.6 群聊：用户在 team 对话栏发言 → 进 transcript + 触发 leader 编排。
        .route("/api/teams/:team_id/message", post(team_message))
        .route("/api/projects", post(create_project))
        // §2.10 定时任务（心跳 cron handler 到点 submit）。
        .route("/api/cron", get(list_cron).post(schedule_cron))
        .route("/api/cron/:job_id", delete(cancel_cron))
        // §2.9：`/api/threads/*` 已退场（threads 是 SessionStore 迁移残留 URL；前端只用 /api/sessions）。
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/:session_id/messages", post(post_message))
        .route(
            "/api/sessions/:session_id/history",
            get(get_session_history),
        )
        .route("/api/sessions/:session_id/fork", post(fork_session))
        .route("/api/sessions/:session_id/resume", post(resume_session))
        .route("/api/sessions/:session_id/messages/steer", post(post_steer))
        .route("/api/sessions/:session_id/events", get(get_events))
        .route(
            "/api/sessions/:session_id/approvals/:approval_id",
            post(post_approval),
        )
        .route("/api/sessions/:session_id/cancel", post(cancel_session))
        .route("/api/sessions/:session_id", delete(delete_session))
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/:session_id/messages", post(post_message))
        .route("/sessions/:session_id/events", get(get_events))
        .route(
            "/sessions/:session_id/approvals/:approval_id",
            post(post_approval),
        )
        .route("/sessions/:session_id/cancel", post(cancel_session))
        .route("/sessions/:session_id", delete(delete_session))
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    #[serde(default)]
    session_id: Option<SessionId>,
    #[serde(default)]
    bot_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateBotRequest {
    name: String,
    workdir: String,
    /// §5.7 bot 市场：选用的模板 id（`coder`/`general`）。缺省/未知 → 回退 `coder`（向后兼容）。
    #[serde(default)]
    template_id: Option<String>,
}

/// §5.7 内置 bot 模板（市场清单）。v1 编译期内置两条：通用 / 编程。
/// **诚实范围**：Hub 当前单 agent，`template_id` 落 `BotEntry.profile` 作组织/元数据，
/// 行为差异待 Hub 多 agent（§4.5）。模板「分发」语义（拉远端包）后置到 §1.6 server 线。
#[derive(Debug, Serialize)]
struct BotTemplate {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    /// 单字符身份标（前端卡片用）。
    emoji: &'static str,
}

const BOT_TEMPLATES: &[BotTemplate] = &[
    BotTemplate {
        id: "general",
        name: "通用助手",
        description: "全工具通用 agent——读写/检索/记忆/书/技能/shell 都在，但不注入编程专属 SOP 纪律。日常问答、调研、文档、杂活。",
        emoji: "✨",
    },
    BotTemplate {
        id: "coder",
        name: "编程 bot",
        description: "软件工程专精：理解文件、精准搜索、补丁式编辑、按需跑工具，并带 brainstorm/计划/TDD/调试等 SOP 纪律。",
        emoji: "⌨",
    },
];

fn is_known_template(id: &str) -> bool {
    BOT_TEMPLATES.iter().any(|t| t.id == id)
}

#[derive(Debug, Deserialize)]
struct CreateProjectRequest {
    id: String,
    name: String,
    root_dir: String,
    #[serde(default)]
    default_bots: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ScheduleCronRequest {
    session_id: String,
    prompt: String,
    /// 首次触发延迟（秒），缺省即刻（0）。
    #[serde(default)]
    delay_secs: u64,
    /// 周期（秒），缺省则为一次性。
    #[serde(default)]
    interval_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CronIdResponse {
    id: String,
}

#[derive(Debug, Serialize)]
struct CronJobView {
    id: String,
    session_id: String,
    prompt: String,
    recurring: bool,
}

#[derive(Debug, Deserialize)]
struct OpenTeamRequest {
    project_id: String,
    #[serde(default)]
    members: Vec<String>,
    leader: String,
    #[serde(default)]
    task: String,
}

#[derive(Debug, Serialize)]
struct OpenTeamResponse {
    team_id: String,
}

/// §4.5：触发**已开的群**真正编排（leader 规划分工 → 各 member 并行跑 → 汇总贴回 transcript）。
#[derive(Debug, Deserialize)]
struct ConductTeamRequest {
    #[serde(default)]
    members: Vec<String>,
    leader: String,
    #[serde(default)]
    task: String,
}

#[derive(Debug, Serialize)]
struct ConductTeamResponse {
    /// 已受理（编排在后台跑，结果异步贴回 team transcript，可经 GET /api/teams 看）。
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct BotResponse {
    bot: BotEntry,
}

#[derive(Debug, Serialize)]
struct BotsResponse {
    bots: Vec<BotEntry>,
}

/// §5.5 C11：bot 属性面板数据——基础身份（笼子=workdir/profile）+ 会话计数 + 工具/subagent 自省。
#[derive(Debug, Serialize)]
struct BotInfo {
    id: String,
    name: String,
    profile: String,
    workdir: String,
    session_count: usize,
    chat_count: usize,
    /// 已注册工具（`{name, tier}`，按名排序）；subagent 工具（explore/editor）一并在内，前端按名拆分。
    tools: Vec<ToolBrief>,
    /// subagent 工具名（从 tools 里析出，前端「subagent」tab 用）。
    subagents: Vec<String>,
    /// bot 角色 system prompt（bot.md 本体；前端 bot.md tab 渲染）。`None`=未设。
    system_prompt: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolBrief {
    name: String,
    tier: String,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    session_id: SessionId,
}

/// 会话列表视图（§2.8 前端回读）：带 kind/bot_id/parent 让前端按 bot 归类、建树、过滤 subagent。
#[derive(Debug, Serialize)]
struct SessionView {
    id: String,
    bot_id: String,
    kind: crate::session_store::SessionKind,
    parent_session: Option<String>,
    message_count: usize,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct SessionsResponse {
    sessions: Vec<SessionView>,
}

#[derive(Debug, Deserialize)]
struct MessageRequest {
    #[serde(default)]
    text: String,
    #[serde(default)]
    images: Vec<String>,
    #[serde(default)]
    thinking: Option<bool>,
    #[serde(default)]
    web_search: Option<bool>,
    #[serde(default)]
    code_execution: Option<bool>,
    /// §1.8.3b 召回**默认开**（仍可关）：人面向请求缺省即强制召回；省略字段 → true。
    #[serde(default = "default_force_recall")]
    force_recall: bool,
}

/// §1.8.3b 召回默认开关：请求未带 `force_recall` 时的默认值（开）。
fn default_force_recall() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ApprovalRequest {
    approved: bool,
    /// §2.11 四档（once/session/always/deny）；缺省回退 `approved`。
    #[serde(default)]
    decision: Option<base_types::ApprovalDecision>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForkSessionRequest {
    // `thread_id` 别名保留：兼容旧客户端（§2.8 缺陷B：函数/字段去 thread，外部别名留兼容）。
    #[serde(default, alias = "thread_id")]
    new_session_id: Option<SessionId>,
}

#[derive(Debug, Serialize)]
struct SubmissionResponse {
    id: OpId,
    session_id: SessionId,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Op id to collect. Kept as `since` to match the Phase 2 sketch.
    #[serde(default)]
    since: Option<OpId>,
    /// Seconds to wait for matching events. Capped at 30s.
    #[serde(default)]
    wait: Option<u64>,
}

#[derive(Debug, Serialize)]
struct EventsResponse {
    session_id: SessionId,
    events: Vec<Event>,
}

#[derive(Debug, Serialize)]
struct HistoryResponse {
    session_id: SessionId,
    messages: Vec<Message>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

async fn create_session(
    State(hub): State<Hub>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<SessionResponse>, HttpError> {
    let session_id = req
        .session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id = if let Some(bot_id) = req.bot_id {
        hub.open_session_for_bot(session_id, bot_id)
    } else {
        hub.open_session(session_id)
    }
    .map_err(hub_error)?;
    Ok(Json(SessionResponse { session_id }))
}

async fn list_bots(State(hub): State<Hub>) -> Json<BotsResponse> {
    Json(BotsResponse {
        bots: hub.list_bots(),
    })
}

/// `GET /api/capabilities`（§5.5 B8）：后端能力清单，前端启动探测后据此显隐 UI
/// （`[data-cap="<key>"]` 元素在对应能力为 false 时隐藏）。当前 bots 全功能后端：
/// 写/执行/记忆/技能/书/团队/cron 皆开，browser 未落地为 false。远端只读 server
/// （§1.6，parked）将来覆盖为只读子集（无 memory / 无 write/exec）。
async fn get_capabilities() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "memory": true,
        "skills": true,
        "books": true,
        "tools_write": true,
        "tools_exec": true,
        "teams": true,
        "cron": true,
        // §5.6 C10：webui-bin 以 browser feature 构建时设 `BOTOBOT_CAP_BROWSER`（投屏 /browser-ws 在场）。
        "browser": std::env::var("BOTOBOT_CAP_BROWSER").is_ok(),
    }))
}

/// `GET /api/bots/:bot_id/info`（§5.5 C11）：bot 属性面板数据——身份 + 会话计数。
/// 404 当 bot 不存在。
async fn get_bot_info(
    State(hub): State<Hub>,
    Path(bot_id): Path<String>,
) -> Result<Json<BotInfo>, HttpError> {
    let bot = hub
        .list_bots()
        .into_iter()
        .find(|b| b.id == bot_id)
        .ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, format!("unknown bot: {bot_id}")))?;
    let metas = hub.list_session_metas();
    let session_count = metas.iter().filter(|m| m.bot_id == bot_id).count();
    let chat_count = metas
        .iter()
        .filter(|m| m.bot_id == bot_id && m.kind == crate::session_store::SessionKind::Chat)
        .count();
    // §5.5 C11 工具/subagent 自省：按该 bot 的 profile 取工具简介；subagent = 已知子 agent 工具名。
    const SUBAGENT_NAMES: &[&str] = &["explore", "editor"];
    let brief = hub.tool_brief_for_bot(&bot_id);
    let subagents: Vec<String> = brief
        .iter()
        .filter(|(n, _)| SUBAGENT_NAMES.contains(&n.as_str()))
        .map(|(n, _)| n.clone())
        .collect();
    let tools: Vec<ToolBrief> = brief
        .into_iter()
        .map(|(name, tier)| ToolBrief {
            name,
            tier: tier.to_string(),
        })
        .collect();
    Ok(Json(BotInfo {
        id: bot.id,
        name: bot.name,
        profile: bot.profile,
        workdir: bot.workdir.display().to_string(),
        session_count,
        chat_count,
        tools,
        subagents,
        system_prompt: hub.system_prompt_for_bot(&bot_id),
    }))
}

/// `/api/teams`：Switchboard 快照（teams + projects）。bots 已在 `/api/bots`。
async fn list_teams(State(hub): State<Hub>) -> Json<team_core::SwitchboardSnapshot> {
    Json(hub.teams_snapshot())
}

/// `POST /api/projects`：登记项目（open_team 的 default_bots fallback 据此）。
async fn create_project(
    State(hub): State<Hub>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<Json<team_core::SwitchboardSnapshot>, HttpError> {
    hub.create_team_project(req.id, req.name, req.root_dir, req.default_bots)
        .map_err(hub_error)?;
    Ok(Json(hub.teams_snapshot()))
}

/// `POST /api/cron`：登记一条定时任务。到点由心跳 CronHandler 异步 submit 发起一个 turn。
async fn schedule_cron(
    State(hub): State<Hub>,
    Json(req): Json<ScheduleCronRequest>,
) -> Json<CronIdResponse> {
    let id = hub.schedule_job(
        req.session_id,
        req.prompt,
        std::time::Duration::from_secs(req.delay_secs),
        req.interval_secs.map(std::time::Duration::from_secs),
    );
    Json(CronIdResponse { id })
}

/// `GET /api/cron`：列出当前所有定时任务。
async fn list_cron(State(hub): State<Hub>) -> Json<Vec<CronJobView>> {
    let jobs = hub
        .list_cron()
        .into_iter()
        .map(|(id, session_id, prompt, recurring)| CronJobView {
            id,
            session_id,
            prompt,
            recurring,
        })
        .collect();
    Json(jobs)
}

/// `DELETE /api/cron/:job_id`：取消一条定时任务。命中→200，未知→404。
async fn cancel_cron(
    State(hub): State<Hub>,
    Path(job_id): Path<String>,
) -> Result<Json<CronIdResponse>, HttpError> {
    if hub.cancel_cron(&job_id) {
        Ok(Json(CronIdResponse { id: job_id }))
    } else {
        Err(HttpError::new(StatusCode::NOT_FOUND, format!("cron job not found: {job_id}")))
    }
}

/// `POST /api/teams`：开一个群（leader-mediated）。返回 team_id。
async fn open_team(
    State(hub): State<Hub>,
    Json(req): Json<OpenTeamRequest>,
) -> Result<Json<OpenTeamResponse>, HttpError> {
    let team_id = hub
        .open_team(&req.project_id, req.members, req.leader, req.task)
        .map_err(hub_error)?;
    Ok(Json(OpenTeamResponse { team_id }))
}

/// `POST /api/teams/:team_id/conduct`：真正**编排**已开的群（§4.5 leader 主动编排）。
/// 后台 spawn `conduct_team_planned`（leader LLM 拆分工 → member 并行跑 → 汇总贴回 transcript），
/// 立即返回 `accepted`（编排耗时，异步完成；结果经 `GET /api/teams` 看 transcript）。
async fn conduct_team(
    State(hub): State<Hub>,
    Path(team_id): Path<String>,
    Json(req): Json<ConductTeamRequest>,
) -> Json<ConductTeamResponse> {
    let llm = hub.llm();
    tokio::spawn(async move {
        crate::team_runner::conduct_team_planned(
            &hub,
            &team_id,
            &req.members,
            &req.leader,
            &req.task,
            llm,
        )
        .await;
    });
    Json(ConductTeamResponse { accepted: true })
}

#[derive(Debug, Deserialize)]
struct TeamMessageRequest {
    text: String,
}

/// `POST /api/teams/:team_id/message`（§5.6 群聊）：用户在 team 对话栏发言。
/// 把消息以 `Author::User` 贴进 team transcript（立即可见），并触发 leader 编排（后台，
/// 以该消息为本轮总任务拆分给成员、汇总贴回）。未知 team → 404。返回 `accepted`。
async fn team_message(
    State(hub): State<Hub>,
    Path(team_id): Path<String>,
    Json(req): Json<TeamMessageRequest>,
) -> Result<Json<ConductTeamResponse>, HttpError> {
    let text = req.text.trim().to_string();
    if text.is_empty() {
        return Err(HttpError::new(StatusCode::BAD_REQUEST, "empty message".to_string()));
    }
    // 取该 team 的成员/leader（编排需要）。
    let snap = hub.teams_snapshot();
    let team = snap
        .teams
        .iter()
        .find(|t| t.id == team_id)
        .ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, format!("team not found: {team_id}")))?;
    let members = team.members.clone();
    let leader = team.leader.clone();
    // 1) 用户发言进 transcript（立即可见）。
    hub.team_post(&team_id, team_core::Author::User, text.clone())
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // 2) 触发 leader 编排（后台，以该消息为本轮总任务）。
    let llm = hub.llm();
    tokio::spawn(async move {
        crate::team_runner::conduct_team_planned(&hub, &team_id, &members, &leader, &text, llm).await;
    });
    Ok(Json(ConductTeamResponse { accepted: true }))
}

/// `GET /api/bot-templates`（§5.7）：内置 bot 模板清单，供 nail `+` 市场模态渲染卡片。
async fn list_bot_templates() -> Json<&'static [BotTemplate]> {
    Json(BOT_TEMPLATES)
}

#[derive(Debug, Deserialize)]
struct UpdateBotRequest {
    /// 新 profile（模板 id）。present 且未知 → 400。
    #[serde(default)]
    profile: Option<String>,
    /// 自定义 bot.md（角色 prompt 覆盖）。present 时设置；空白=清除回 profile 默认。`absent`=不动。
    #[serde(default)]
    system: Option<String>,
}

/// `PATCH /api/bots/:bot_id`（§5.7）：换 profile（人格）和/或自定义 bot.md，不重建。下一轮 live 生效。
async fn update_bot(
    State(hub): State<Hub>,
    Path(bot_id): Path<String>,
    Json(req): Json<UpdateBotRequest>,
) -> Result<Json<BotResponse>, HttpError> {
    let mut bot = None;
    if let Some(profile) = req.profile {
        if !is_known_template(&profile) {
            return Err(HttpError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown profile: {profile}"),
            ));
        }
        bot = Some(hub.set_bot_profile(&bot_id, profile).map_err(hub_error)?);
    }
    if let Some(system) = req.system {
        bot = Some(hub.set_bot_system(&bot_id, Some(system)).map_err(hub_error)?);
    }
    // 两者皆 absent → 仍按当前状态回（确认 bot 存在）。
    let bot = match bot {
        Some(b) => b,
        None => hub
            .list_bots()
            .into_iter()
            .find(|b| b.id == bot_id)
            .ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, format!("bot not found: {bot_id}")))?,
    };
    Ok(Json(BotResponse { bot }))
}

async fn create_bot(
    State(hub): State<Hub>,
    Json(req): Json<CreateBotRequest>,
) -> Result<Json<BotResponse>, HttpError> {
    // §5.7：未知/缺省模板 → 回退 coder（向后兼容旧前端不带 template_id）。
    let profile = req
        .template_id
        .filter(|id| is_known_template(id))
        .unwrap_or_else(|| "coder".to_string());
    let bot = hub
        .create_bot_with_profile(req.name, req.workdir, profile)
        .map_err(hub_error)?;
    Ok(Json(BotResponse { bot }))
}

async fn create_bot_session(
    State(hub): State<Hub>,
    Path(bot_id): Path<String>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<SessionResponse>, HttpError> {
    let session_id = hub
        .open_session_for_bot(
            req.session_id
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            bot_id,
        )
        .map_err(hub_error)?;
    Ok(Json(SessionResponse { session_id }))
}

async fn list_sessions(State(hub): State<Hub>) -> Json<SessionsResponse> {
    // 基于持久化 meta 返回富视图（前端据 bot_id/kind 归类与建树）；按 updated_at 倒序（新在前）。
    let mut sessions: Vec<SessionView> = hub
        .list_session_metas()
        .into_iter()
        .map(|m| SessionView {
            id: m.session_id,
            bot_id: m.bot_id,
            kind: m.kind,
            parent_session: m.parent_session,
            message_count: m.message_count,
            updated_at: m.updated_at,
        })
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(SessionsResponse { sessions })
}

async fn get_session_history(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<HistoryResponse>, HttpError> {
    let messages = hub.session_history(&session_id).map_err(hub_error)?;
    Ok(Json(HistoryResponse {
        session_id,
        messages,
    }))
}

async fn fork_session(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<ForkSessionRequest>,
) -> Result<Json<SessionResponse>, HttpError> {
    let new_session_id = req
        .new_session_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id = hub
        .fork_session(&session_id, new_session_id)
        .map_err(hub_error)?;
    Ok(Json(SessionResponse { session_id }))
}

async fn resume_session(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<SessionResponse>, HttpError> {
    let session_id = hub.open_session(session_id).map_err(hub_error)?;
    Ok(Json(SessionResponse { session_id }))
}

async fn post_message(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<MessageRequest>,
) -> Result<Json<SubmissionResponse>, HttpError> {
    if req.text.is_empty() && req.images.is_empty() {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            "message requires text or images",
        ));
    }

    let session_id = hub.open_session(session_id).map_err(hub_error)?;
    let sub = Submission::new(
        session_id.clone(),
        Op::UserMessage {
            text: req.text,
            images: req.images,
            thinking: req.thinking,
            web_search: req.web_search,
            code_execution: req.code_execution,
            force_recall: req.force_recall,
        },
    );
    let id = sub.id.clone();
    hub.submit(sub)
        .await
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SubmissionResponse { id, session_id }))
}

async fn post_steer(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<MessageRequest>,
) -> Result<Json<SubmissionResponse>, HttpError> {
    if req.text.is_empty() && req.images.is_empty() {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            "steer requires text or images",
        ));
    }

    let sub = Submission::new(
        session_id.clone(),
        Op::Steer {
            text: req.text,
            images: req.images,
            thinking: req.thinking,
            web_search: req.web_search,
            code_execution: req.code_execution,
            force_recall: req.force_recall,
        },
    );
    let id = sub.id.clone();
    hub.submit(sub)
        .await
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SubmissionResponse { id, session_id }))
}

async fn post_approval(
    State(hub): State<Hub>,
    Path((session_id, approval_id)): Path<(SessionId, String)>,
    Json(req): Json<ApprovalRequest>,
) -> Result<Json<SubmissionResponse>, HttpError> {
    let sub = Submission::new(
        session_id.clone(),
        Op::Approval {
            approval_id,
            approved: req.approved,
            decision: req.decision,
            reason: req.reason,
        },
    );
    let id = sub.id.clone();
    hub.submit(sub)
        .await
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(SubmissionResponse { id, session_id }))
}

async fn cancel_session(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<SubmissionResponse>, HttpError> {
    let Some(id) = hub.cancel_session(&session_id).await.map_err(hub_error)? else {
        return Err(HttpError::new(StatusCode::NOT_FOUND, "session not found"));
    };
    Ok(Json(SubmissionResponse { id, session_id }))
}

async fn get_events(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, HttpError> {
    let Some(mut rx) = hub.subscribe(&session_id) else {
        return Err(HttpError::new(StatusCode::NOT_FOUND, "session not found"));
    };

    let mut events = hub
        .events_for(&session_id, query.since.as_deref())
        .ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "session not found"))?;
    if events.iter().any(is_terminal) {
        return Ok(Json(EventsResponse { session_id, events }));
    }

    let wait = Duration::from_secs(query.wait.unwrap_or(0).min(30));
    if wait.is_zero() {
        return Ok(Json(EventsResponse { session_id, events }));
    }

    let deadline = Instant::now() + wait;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                if query.since.as_ref().is_none_or(|id| ev.id == *id) {
                    let terminal = is_terminal(&ev);
                    events.push(ev);
                    if terminal {
                        break;
                    }
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => break,
        }
    }

    Ok(Json(EventsResponse { session_id, events }))
}

async fn delete_session(
    State(hub): State<Hub>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<SubmissionResponse>, HttpError> {
    let Some(id) = hub
        .delete_session(&session_id)
        .await
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?
    else {
        return Err(HttpError::new(StatusCode::NOT_FOUND, "session not found"));
    };
    Ok(Json(SubmissionResponse { id, session_id }))
}

fn is_terminal(ev: &Event) -> bool {
    matches!(
        &ev.msg,
        EventMsg::TurnComplete
            | EventMsg::CancelComplete
            | EventMsg::ShutdownComplete
            | EventMsg::Error { .. }
    ) || matches!(&ev.msg, EventMsg::Agent(AgentEvent::Error { .. }))
}

fn hub_error(message: String) -> HttpError {
    let status = if message.starts_with("max sessions reached") {
        StatusCode::TOO_MANY_REQUESTS
    } else if message.starts_with("session not found") || message.starts_with("bot not found") {
        StatusCode::NOT_FOUND
    } else if message.starts_with("invalid bot workdir")
        || message.starts_with("bot workdir is not a directory")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    HttpError::new(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{Decision, Llm, LlmError, LlmEvent, LlmOpts, Message, ToolSpec};
    use std::sync::Arc;

    // §1.8.3b 钉死「召回默认开」决策：请求省略 force_recall → true；显式 false 仍可关。
    // 防有人误删 #[serde(default = "default_force_recall")] 静默关掉所有用户的召回。
    #[test]
    fn message_request_force_recall_defaults_on() {
        let omitted: MessageRequest = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert!(omitted.force_recall, "省略 force_recall 应默认开");
        let off: MessageRequest =
            serde_json::from_str(r#"{"text":"hi","force_recall":false}"#).unwrap();
        assert!(!off.force_recall, "显式 false 仍可关");
    }

    struct OneShotLlm;

    #[async_trait]
    impl Llm for OneShotLlm {
        async fn infer(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            let decision = Decision {
                text: "hello over http".into(),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            let events = vec![
                Ok(LlmEvent::TextDelta(decision.text.clone())),
                Ok(LlmEvent::Done(decision)),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn test_hub() -> Hub {
        Hub::new(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
        )
    }

    #[tokio::test]
    async fn get_bot_info_returns_default_bot_and_404_for_unknown() {
        let hub = test_hub();
        // 默认 bot 存在。
        let Json(info) = get_bot_info(State(hub.clone()), Path(crate::DEFAULT_BOT_ID.to_string()))
            .await
            .expect("默认 bot 应有 info");
        assert_eq!(info.id, crate::DEFAULT_BOT_ID);
        assert!(!info.profile.is_empty());
        // 未知 bot → 404。
        let err = get_bot_info(State(hub), Path("no-such-bot".to_string()))
            .await
            .expect_err("未知 bot 应 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_sessions_returns_enriched_metas() {
        let dir = std::env::temp_dir().join(format!("botobot-http-list-{}", uuid::Uuid::new_v4()));
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            crate::HubConfig {
                store_root: Some(dir.clone()),
                ..Default::default()
            },
        );
        // 懒持久化后空会话不落 meta；直接写一条带内容的 meta 模拟「有消息的持久会话」。
        let store = crate::session_store::SessionStore::new(dir.clone());
        let mut meta = crate::session_store::SessionMeta::new_chat("sess-x", crate::DEFAULT_BOT_ID);
        meta.message_count = 1;
        store.write_meta("sess-x", &meta).unwrap();

        let Json(resp) = list_sessions(State(hub)).await;
        let v = resp
            .sessions
            .iter()
            .find(|s| s.id == "sess-x")
            .expect("应回读到持久化会话");
        assert_eq!(v.bot_id, crate::DEFAULT_BOT_ID);
        assert_eq!(v.kind, crate::session_store::SessionKind::Chat);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn http_message_then_events_returns_logged_turn() {
        let hub = test_hub();
        let Json(created) = create_session(
            State(hub.clone()),
            Json(CreateSessionRequest {
                session_id: Some("http-s-1".into()),
                bot_id: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(created.session_id, "http-s-1");

        let Json(submitted) = post_message(
            State(hub.clone()),
            Path("http-s-1".into()),
            Json(MessageRequest {
                text: "hello".into(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            }),
        )
        .await
        .unwrap();

        let Json(batch) = get_events(
            State(hub),
            Path("http-s-1".into()),
            Query(EventsQuery {
                since: Some(submitted.id),
                wait: Some(2),
            }),
        )
        .await
        .unwrap();

        assert_eq!(batch.session_id, "http-s-1");
        assert!(
            batch
                .events
                .iter()
                .any(|ev| matches!(ev.msg, EventMsg::Agent(_)))
        );
        assert!(
            batch
                .events
                .iter()
                .any(|ev| matches!(ev.msg, EventMsg::TurnComplete))
        );
    }

    #[tokio::test]
    async fn http_delete_unknown_session_returns_not_found() {
        let err = delete_session(State(test_hub()), Path("missing".into()))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    // §2.10：定时任务 list/cancel——schedule→list 见到→DELETE 取消→list 不再有；未知 id→404。
    #[tokio::test]
    async fn cron_list_and_cancel() {
        let hub = test_hub();
        let id = hub.schedule_job("s-cron", "ping", std::time::Duration::from_secs(60), None);
        let Json(jobs) = list_cron(State(hub.clone())).await;
        assert!(jobs.iter().any(|j| j.id == id && j.prompt == "ping"), "应列出该任务");
        // 取消命中。
        let Json(r) = cancel_cron(State(hub.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(r.id, id);
        let Json(after) = list_cron(State(hub.clone())).await;
        assert!(!after.iter().any(|j| j.id == id), "取消后不应再列出");
        // 未知 id → 404。
        let err = cancel_cron(State(hub), Path("nope".into())).await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    // §5.7：模板清单非空且含通用/编程；带 template_id 创建落 profile；未知/缺省回退 coder。
    #[tokio::test]
    async fn bot_templates_listed_and_applied_on_create() {
        let Json(tmpls) = list_bot_templates().await;
        let ids: Vec<&str> = tmpls.iter().map(|t| t.id).collect();
        assert!(ids.contains(&"general") && ids.contains(&"coder"));

        let hub = test_hub();
        let wd = std::env::temp_dir().display().to_string();
        // 选 general 模板 → profile=general。
        let Json(r1) = create_bot(
            State(hub.clone()),
            Json(CreateBotRequest {
                name: "g".into(),
                workdir: wd.clone(),
                template_id: Some("general".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(r1.bot.profile, "general");
        // 未知模板 → 回退 coder。
        let Json(r2) = create_bot(
            State(hub.clone()),
            Json(CreateBotRequest {
                name: "x".into(),
                workdir: wd.clone(),
                template_id: Some("bogus".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(r2.bot.profile, "coder");
        // 缺省（旧前端）→ coder。
        let Json(r3) = create_bot(
            State(hub),
            Json(CreateBotRequest {
                name: "n".into(),
                workdir: wd,
                template_id: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(r3.bot.profile, "coder");
    }

    // §5.7：PATCH 换 profile——已知 profile 成功改 BotEntry.profile；未知 → 400。
    #[tokio::test]
    async fn update_bot_switches_profile() {
        let hub = test_hub();
        let wd = std::env::temp_dir().display().to_string();
        let Json(created) = create_bot(State(hub.clone()), Json(CreateBotRequest {
            name: "b".into(), workdir: wd, template_id: Some("coder".into()),
        })).await.unwrap();
        assert_eq!(created.bot.profile, "coder");
        // 切到 general。
        let Json(updated) = update_bot(State(hub.clone()), Path(created.bot.id.clone()),
            Json(UpdateBotRequest { profile: Some("general".into()), system: None })).await.unwrap();
        assert_eq!(updated.bot.profile, "general");
        // 未知 profile → 400。
        let err = update_bot(State(hub.clone()), Path(created.bot.id.clone()),
            Json(UpdateBotRequest { profile: Some("bogus".into()), system: None })).await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        // 未知 bot → 404（经 hub_error）。
        let err2 = update_bot(State(hub), Path("nope".into()),
            Json(UpdateBotRequest { profile: Some("general".into()), system: None })).await.unwrap_err();
        assert_eq!(err2.status, StatusCode::NOT_FOUND);
    }

    // §5.7：PATCH 自定义 bot.md——设置后 system_prompt_for_bot 返回它；空白清除回 profile 默认。
    #[tokio::test]
    async fn update_bot_sets_and_clears_custom_system() {
        let hub = Hub::new(
            agent_loop::Agent::builder().llm(Arc::new(OneShotLlm)).system("CODER DEFAULT").build(),
        );
        // 设自定义 bot.md。
        let Json(r) = update_bot(State(hub.clone()), Path(crate::DEFAULT_BOT_ID.to_string()),
            Json(UpdateBotRequest { profile: None, system: Some("MY CUSTOM ROLE".into()) })).await.unwrap();
        assert_eq!(r.bot.system.as_deref(), Some("MY CUSTOM ROLE"));
        assert_eq!(hub.system_prompt_for_bot(crate::DEFAULT_BOT_ID).as_deref(), Some("MY CUSTOM ROLE"));
        // 空白清除 → 回 profile 默认。
        let _ = update_bot(State(hub.clone()), Path(crate::DEFAULT_BOT_ID.to_string()),
            Json(UpdateBotRequest { profile: None, system: Some("   ".into()) })).await.unwrap();
        assert_eq!(hub.system_prompt_for_bot(crate::DEFAULT_BOT_ID).as_deref(), Some("CODER DEFAULT"));
    }

    #[tokio::test]
    async fn http_create_bot_and_session_binds_workdir() {
        let hub = test_hub();
        let Json(bot_response) = create_bot(
            State(hub.clone()),
            Json(CreateBotRequest {
                name: "tmp".into(),
                workdir: std::env::temp_dir().display().to_string(),
                template_id: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(bot_response.bot.profile, "coder");
        assert!(
            hub.list_bots()
                .iter()
                .any(|bot| bot.id == bot_response.bot.id)
        );

        let Json(session) = create_bot_session(
            State(hub.clone()),
            Path(bot_response.bot.id.clone()),
            Json(CreateSessionRequest {
                session_id: Some("bot-http-s-1".into()),
                bot_id: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(session.session_id, "bot-http-s-1");
    }

    #[tokio::test]
    async fn http_create_project_and_open_team() {
        let hub = test_hub();
        let tmp = std::env::temp_dir().display().to_string();
        let Json(bot) = create_bot(
            State(hub.clone()),
            Json(CreateBotRequest {
                name: "leader".into(),
                workdir: tmp.clone(),
                template_id: None,
            }),
        )
        .await
        .unwrap();

        let _ = create_project(
            State(hub.clone()),
            Json(CreateProjectRequest {
                id: "proj-1".into(),
                name: "Proj".into(),
                root_dir: tmp,
                default_bots: vec![],
            }),
        )
        .await
        .unwrap();

        let Json(team) = open_team(
            State(hub.clone()),
            Json(OpenTeamRequest {
                project_id: "proj-1".into(),
                members: vec![bot.bot.id.clone()],
                leader: bot.bot.id.clone(),
                task: "build it".into(),
            }),
        )
        .await
        .unwrap();
        assert!(team.team_id.starts_with("team-"));

        // 不存在的 leader → 400-ish hub error
        let bad = open_team(
            State(hub.clone()),
            Json(OpenTeamRequest {
                project_id: "proj-1".into(),
                members: vec![bot.bot.id.clone()],
                leader: "ghost".into(),
                task: "x".into(),
            }),
        )
        .await;
        assert!(bad.is_err());

        let Json(snap) = list_teams(State(hub)).await;
        assert_eq!(snap.teams.len(), 1);
        assert_eq!(snap.projects.len(), 1);
        assert_eq!(snap.teams[0].id, team.team_id);
    }

    // §5.6 群聊：team_message 把用户发言贴进 transcript（立即可见）+ 受理；空→400，未知 team→404。
    #[tokio::test]
    async fn team_message_posts_user_and_accepts() {
        let hub = test_hub();
        let tmp = std::env::temp_dir().display().to_string();
        let Json(bot) = create_bot(State(hub.clone()), Json(CreateBotRequest {
            name: "m".into(), workdir: tmp.clone(), template_id: None,
        })).await.unwrap();
        let Json(_) = create_project(State(hub.clone()), Json(CreateProjectRequest {
            id: "p".into(), name: "P".into(), root_dir: tmp, default_bots: vec![],
        })).await.unwrap();
        let Json(team) = open_team(State(hub.clone()), Json(OpenTeamRequest {
            project_id: "p".into(), members: vec![bot.bot.id.clone()], leader: bot.bot.id.clone(), task: "t".into(),
        })).await.unwrap();
        // 发言。
        let Json(r) = team_message(State(hub.clone()), Path(team.team_id.clone()),
            Json(TeamMessageRequest { text: "大家好".into() })).await.unwrap();
        assert!(r.accepted);
        // 用户发言已进 transcript。
        let Json(snap) = list_teams(State(hub.clone())).await;
        let t = snap.teams.iter().find(|t| t.id == team.team_id).unwrap();
        assert!(t.messages.iter().any(|m| m.content == "大家好"), "用户发言应在 transcript");
        // 空消息 → 400。
        let e1 = team_message(State(hub.clone()), Path(team.team_id.clone()),
            Json(TeamMessageRequest { text: "  ".into() })).await.unwrap_err();
        assert_eq!(e1.status, StatusCode::BAD_REQUEST);
        // 未知 team → 404。
        let e2 = team_message(State(hub), Path("team-nope".into()),
            Json(TeamMessageRequest { text: "x".into() })).await.unwrap_err();
        assert_eq!(e2.status, StatusCode::NOT_FOUND);
    }

    // ───────── 经真实 router 的端到端集成测试（覆盖路由接线/序列化/状态码）─────────
    use http_body_util::BodyExt;
    use tower::ServiceExt; // oneshot

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn router_wires_team_and_session_endpoints() {
        // 经真实 router（非直接调 handler）跑通 §2.8/§2.9 改过的端点，验证路由接线/方法/序列化。
        let hub = test_hub();
        let app = router().with_state(hub);
        let tmp = std::env::temp_dir().display().to_string();

        // 创建 bot（POST /api/bots）
        let (st, bot) = send(
            &app,
            "POST",
            "/api/bots",
            &format!(
                r#"{{"name":"leader","workdir":"{}"}}"#,
                tmp.replace('\\', "/")
            ),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::OK);
        let bot_id = bot["bot"]["id"].as_str().unwrap().to_string();

        // 创建项目 + 开队（POST /api/projects, POST /api/teams）
        let (st, _) = send(
            &app,
            "POST",
            "/api/projects",
            &format!(
                r#"{{"id":"p1","name":"P","root_dir":"{}"}}"#,
                tmp.replace('\\', "/")
            ),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::OK);
        let (st, team) = send(
            &app,
            "POST",
            "/api/teams",
            &format!(
                r#"{{"project_id":"p1","members":["{bot_id}"],"leader":"{bot_id}","task":"x"}}"#
            ),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::OK);
        assert!(team["team_id"].as_str().unwrap().starts_with("team-"));

        // GET /api/teams 快照含该队
        let (st, snap) = send(&app, "GET", "/api/teams", "").await;
        assert_eq!(st, axum::http::StatusCode::OK);
        assert_eq!(snap["teams"].as_array().unwrap().len(), 1);

        // §4.5：POST /api/teams/:id/conduct 触发编排（验证路由接线 + 受理；编排逻辑本身在
        // team_runner 单测覆盖，后台 spawn 异步完成、贴回 transcript）。
        let tid = team["team_id"].as_str().unwrap();
        let (st, conducted) = send(
            &app,
            "POST",
            &format!("/api/teams/{tid}/conduct"),
            &format!(r#"{{"members":["{bot_id}"],"leader":"{bot_id}","task":"build it"}}"#),
        )
        .await;
        assert_eq!(st, axum::http::StatusCode::OK);
        assert_eq!(conducted["accepted"], true, "conduct 端点应受理编排");

        // GET /api/sessions（§2.8 富视图路由）+ unknown history（§2.8 新增路由不 404 路由本身）
        let (st, _) = send(&app, "GET", "/api/sessions", "").await;
        assert_eq!(st, axum::http::StatusCode::OK);
        let (st, hist) = send(&app, "GET", "/api/sessions/nope/history", "").await;
        assert_eq!(st, axum::http::StatusCode::OK, "history 路由应存在");
        assert!(hist["messages"].as_array().unwrap().is_empty());

        // 旧 /api/threads 已退场 → 404
        let (st, _) = send(&app, "GET", "/api/threads", "").await;
        assert_eq!(
            st,
            axum::http::StatusCode::NOT_FOUND,
            "/api/threads 应已退场"
        );
    }
}
