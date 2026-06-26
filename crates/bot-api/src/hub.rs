//! Hub: session routing, fan-out, and lifecycle management for bot-api.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_loop::Agent;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::protocol::{Event, Op, OpId, SessionId, Submission};
use crate::session_driver::{EventLog, SessionHandle, spawn_session_driver};
use crate::session_store::{SessionKind, SessionMeta, SessionStore};
use team_core::{Author, RoleInTeam, Switchboard, SwitchboardSnapshot, TeamStore};

pub const DEFAULT_BOT_ID: &str = "bot-default";
/// §5.6：第二个出厂默认 bot——通用助手（与 coder 默认并存，nail 栏 IM 化的两个起始联系人）。
pub const DEFAULT_GENERAL_BOT_ID: &str = "bot-general";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BotEntry {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub workdir: PathBuf,
    /// §5.7 自定义 bot.md：覆盖 profile 默认角色 prompt。`None`=用 profile 默认（旧行兼容）。
    /// 经 `agent_for_session` 的 `with_system` live 应用（下一轮生效、无需重启）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

// 注：不 derive Debug——`profile_agents` 含 `Arc<Agent>`（Agent 无 Debug，§5.7）。
#[derive(Clone)]
pub struct HubConfig {
    pub broadcast_capacity: usize,
    pub event_log_capacity: usize,
    pub max_sessions: usize,
    pub idle_ttl: Duration,
    /// SessionStore 根目录（= `.bot`）。`None` 表示不持久化。
    pub store_root: Option<PathBuf>,
    /// 共享 Switchboard（§4.5）：若提供，Hub 与 agent 侧 team 工具共用同一棵协作树。
    /// `None` 时 Hub 自建私有 Switchboard。
    pub switchboard: Option<Arc<Mutex<Switchboard>>>,
    /// §2.10 心跳晶振间隔（最细粒度，cron 秒级 → 默认 1s）。各 handler 用 `counter % N` 分频。
    pub tick_interval: Duration,
    /// §2.10 共享 cron job 表：若提供，Hub 的 CronHandler 与 agent 侧 cron 工具共用同一份表。
    /// `None` 时 Hub 自建私有表。
    pub cron_jobs: Option<crate::cron::CronJobs>,
    /// §5.7 多 profile base agents（`profile_id → agent`）：装配时注入，按 bot.profile 路由。
    /// 空=纯单 agent（向后兼容）。
    pub profile_agents: Vec<(String, Arc<Agent>)>,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            broadcast_capacity: 1024,
            event_log_capacity: 1024,
            max_sessions: 100,
            idle_ttl: Duration::from_secs(60 * 60),
            store_root: None,
            switchboard: None,
            tick_interval: Duration::from_secs(1),
            cron_jobs: None,
            profile_agents: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Hub {
    agent: Arc<Agent>,
    /// §5.7 多 profile：`profile_id → 该 profile 的 base agent`（共享 llm/tools，仅 system prompt 不同）。
    /// webui-bin 装配时注入（如 general/coder）。**空=旧行为**：`agent_for_session` 全回退 `agent`（向后兼容）。
    profile_agents: Arc<DashMap<String, Arc<Agent>>>,
    sessions: Arc<DashMap<SessionId, SessionHandle>>,
    rooms: Arc<DashMap<SessionId, Room>>,
    bots: Arc<DashMap<String, BotEntry>>,
    session_bots: Arc<DashMap<SessionId, String>>,
    config: HubConfig,
    store: Option<SessionStore>,
    /// IM 协作层（§4.5）。Switchboard 机械传话；TeamStore 可选 JSONL 持久化（只存协作层）。
    switchboard: Arc<Mutex<Switchboard>>,
    team_store: Option<TeamStore>,
    /// §2.10 心跳订阅者注册表（cron / ping-sweep / hook 等平等订阅；运行期可追加）。
    tick_handlers: crate::heartbeat::TickHandlers,
    /// §2.10 定时任务表（CronHandler 与 Hub 同持；`schedule_job` 登记，心跳到点 submit）。
    cron_jobs: crate::cron::CronJobs,
}

#[derive(Clone)]
struct Room {
    tx: broadcast::Sender<Event>,
    log: EventLog,
    last_active: Arc<Mutex<Instant>>,
}

impl Hub {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self::with_config(agent, HubConfig::default())
    }

    /// §4.5：共享 agent 的 LLM 句柄（Hub 单 agent 架构——bots = profile/workdir 共用一个 agent/模型）。
    /// 供 `conduct_team_planned` 的 leader 规划器复用。
    pub fn llm(&self) -> Arc<dyn base_types::Llm> {
        self.agent.llm_handle()
    }

    /// §5.5 C11：共享 agent 的工具简介（Hub 单 agent——各 bot 工具集相同）。供 bot 属性面板自省。
    pub fn tool_brief(&self) -> Vec<(String, &'static str)> {
        self.agent.tool_brief()
    }

    /// §5.5 C11：bot 的角色 system prompt（bot.md 本体）。按 bot 的 profile 选对应 agent（多 profile 下
    /// 通用/编程 prompt 不同）；未注册 profile 回退默认 agent。供属性面板 bot.md tab 只读展示。
    pub fn system_prompt(&self) -> Option<String> {
        self.agent.system_prompt().map(|s| s.to_string())
    }

    /// 同上但指定 bot：自定义 bot.md 优先，否则按 profile 取 base agent 的 system prompt。
    pub fn system_prompt_for_bot(&self, bot_id: &str) -> Option<String> {
        let entry = self.bot_entry(bot_id).ok();
        // 自定义 bot.md 覆盖优先。
        if let Some(s) = entry.as_ref().and_then(|b| b.system.clone()) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
        let base = entry
            .as_ref()
            .and_then(|b| self.profile_agents.get(&b.profile).map(|a| a.value().clone()))
            .unwrap_or_else(|| self.agent.clone());
        base.system_prompt().map(|s| s.to_string())
    }

    /// §5.7：设/清某 bot 的自定义 bot.md（角色 prompt 覆盖）。空白=清除（回 profile 默认）。
    /// `agent_for_session` 每轮现取 → 下一轮 live 生效。落盘。未知 bot → Err。
    pub fn set_bot_system(&self, bot_id: &str, system: Option<String>) -> Result<BotEntry, String> {
        let updated = {
            let mut entry = self
                .bots
                .get_mut(bot_id)
                .ok_or_else(|| format!("bot not found: {bot_id}"))?;
            entry.system = system.filter(|s| !s.trim().is_empty());
            entry.clone()
        };
        if let Some(store) = &self.store {
            if let Err(err) = store.save_bots(&self.list_bots()) {
                tracing::warn!("save bots failed: {err}");
            }
        }
        Ok(updated)
    }

    /// §5.7：按其 profile 取 base agent 的工具简介（多 profile 下工具集可不同）。属性面板按 bot 显示。
    pub fn tool_brief_for_bot(&self, bot_id: &str) -> Vec<(String, &'static str)> {
        let profile = self.bot_entry(bot_id).ok().map(|b| b.profile);
        let base = profile
            .as_deref()
            .and_then(|p| self.profile_agents.get(p).map(|a| a.value().clone()))
            .unwrap_or_else(|| self.agent.clone());
        base.tool_brief()
    }

    /// §5.7：注册一个 profile 的 base agent（webui-bin 装配时调用，如 `register_profile_agent("general", g)`）。
    /// 之后该 profile 的 bot 会用这个 agent（叠加各自 workdir 视图）。覆盖式（同 id 后注册覆盖前者）。
    pub fn register_profile_agent(&self, profile_id: impl Into<String>, agent: Arc<Agent>) {
        self.profile_agents.insert(profile_id.into(), agent);
    }

    /// §5.7：改某 bot 的 profile（在不重建 bot 的前提下换人格）。`agent_for_session` 每次按 `bot.profile`
    /// 现取，故下一轮/下一会话即生效。落盘 + 同步协作层 role。未知 bot → Err。
    pub fn set_bot_profile(&self, bot_id: &str, profile: impl Into<String>) -> Result<BotEntry, String> {
        let profile = profile.into();
        let profile = if profile.trim().is_empty() { "coder".to_string() } else { profile };
        let updated = {
            let mut entry = self
                .bots
                .get_mut(bot_id)
                .ok_or_else(|| format!("bot not found: {bot_id}"))?;
            entry.profile = profile.clone();
            entry.clone()
        };
        if let Some(store) = &self.store {
            if let Err(err) = store.save_bots(&self.list_bots()) {
                tracing::warn!("save bots failed: {err}");
            }
        }
        // 同步协作层 role（Switchboard 镜像）。
        if let Ok(mut o) = self.switchboard.lock() {
            let _ = o.add_bot(team_core::Bot {
                id: updated.id.clone(),
                name: updated.name.clone(),
                role: updated.profile.clone(),
                home: Some(updated.workdir.clone()),
            });
        }
        Ok(updated)
    }

    pub fn with_config(agent: Arc<Agent>, mut config: HubConfig) -> Self {
        // §5.7：取出注入的 profile agents（在 config 被各字段读取前），构造后登记。
        let injected_profiles = std::mem::take(&mut config.profile_agents);
        let store = config.store_root.clone().map(SessionStore::new);
        let bots = Arc::new(DashMap::new());
        // 两个出厂默认 bot 兜底（§5.6 IM 化两个起始联系人）：coder + 通用。重启时若 bots.json
        // 含同 id 会覆盖为落盘值。
        bots.insert(
            DEFAULT_BOT_ID.to_string(),
            BotEntry {
                id: DEFAULT_BOT_ID.to_string(),
                name: "botobot".to_string(),
                profile: "coder".to_string(),
                workdir: agent.workdir().to_path_buf(),
                system: None,
            },
        );
        bots.insert(
            DEFAULT_GENERAL_BOT_ID.to_string(),
            BotEntry {
                id: DEFAULT_GENERAL_BOT_ID.to_string(),
                name: "通用助手".to_string(),
                profile: "general".to_string(),
                workdir: agent.workdir().to_path_buf(),
                system: None,
            },
        );
        let session_bots = Arc::new(DashMap::new());
        if let Some(store) = &store {
            // 旧 threads/*.jsonl 自动导入（幂等）。
            if let Err(err) = store.migrate_legacy_threads(DEFAULT_BOT_ID) {
                tracing::warn!("legacy thread migration failed: {err}");
            }
            // 重建 bot 注册表。
            if let Ok(loaded) = store.load_bots() {
                for bot in loaded {
                    bots.insert(bot.id.clone(), bot);
                }
            }
            // 重建 session→bot 归属。
            if let Ok(metas) = store.list_metas() {
                for meta in metas {
                    session_bots.insert(meta.session_id, meta.bot_id);
                }
            }
        }
        // IM 协作层：TeamStore 与 SessionStore 同 root（.bot），子路径不冲突
        // （sessions/ + bots.json vs teams/ + projects.json）。
        let team_store = config.store_root.clone().map(TeamStore::new);
        // 共享 Switchboard（webui-bin 注入）或 Hub 自建；从 TeamStore 载入 + 镜像 hub bots 供成员校验。
        let switchboard = config
            .switchboard
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(Switchboard::default())));
        if let Ok(mut o) = switchboard.lock() {
            if let Some(ts) = &team_store {
                if let Ok(loaded) = ts.load() {
                    *o = loaded;
                }
            }
            for entry in bots.iter() {
                let b = entry.value();
                let _ = o.add_bot(team_core::Bot {
                    id: b.id.clone(),
                    name: b.name.clone(),
                    role: b.profile.clone(),
                    home: Some(b.workdir.clone()),
                });
            }
        }
        // §2.10 心跳 + cron：进程级常驻晶振遍历订阅者派发；cron 作为第一个 handler。
        // 仅在 tokio 运行时上下文内 spawn/注册（无运行时则跳过，反正此时也无订阅者）。
        let has_runtime = tokio::runtime::Handle::try_current().is_ok();
        let tick_handlers: crate::heartbeat::TickHandlers = Arc::new(Mutex::new(Vec::new()));
        // 优先用外部注入的共享表（与 agent 侧 cron 工具同一份），否则自建私有表。
        let cron_jobs: crate::cron::CronJobs = config
            .cron_jobs
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));
        if has_runtime {
            crate::heartbeat::spawn_heartbeat(tick_handlers.clone(), config.tick_interval);
        }
        let hub = Self {
            agent,
            profile_agents: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            rooms: Arc::new(DashMap::new()),
            bots,
            session_bots,
            config,
            store,
            switchboard,
            team_store,
            tick_handlers,
            cron_jobs,
        };
        // §5.7：登记注入的 profile base agents（按 bot.profile 路由）。
        for (id, profile_agent) in injected_profiles {
            hub.register_profile_agent(id, profile_agent);
        }
        // cron = 心跳第一个 handler（铁律③：注册即加，不改本体）。
        if has_runtime {
            let handler = crate::cron::CronHandler::new(hub.clone(), hub.cron_jobs.clone());
            hub.register_tick_handler(Arc::new(handler));
        }
        hub
    }

    /// §2.10 登记一条定时任务：`first_delay` 后向 `session_id` 发 `prompt`；`interval` 为周期
    /// （`None`=一次性）。返回 job id。到点由心跳的 CronHandler 异步 `submit` 发起 turn。
    pub fn schedule_job(
        &self,
        session_id: impl Into<String>,
        prompt: impl Into<String>,
        first_delay: Duration,
        interval: Option<Duration>,
    ) -> String {
        crate::cron::schedule(&self.cron_jobs, session_id, prompt, first_delay, interval)
    }

    /// 列出定时任务（id, session_id, prompt, 是否周期）。
    pub fn list_cron(&self) -> Vec<(String, String, String, bool)> {
        crate::cron::list(&self.cron_jobs)
    }

    /// 取消一条定时任务，返回是否删除。
    pub fn cancel_cron(&self, id: &str) -> bool {
        crate::cron::cancel(&self.cron_jobs, id)
    }

    /// §2.10 暴露共享 job 表（给 agent-facing cron 工具构造用）。
    pub fn cron_jobs(&self) -> crate::cron::CronJobs {
        self.cron_jobs.clone()
    }

    /// §2.10：注册一个心跳订阅者（运行期可追加；如 cron / ping-sweep handler）。
    /// ⚠️ handler 的 `on_tick` 必须瞬时（铁律①：只派发不阻塞）。
    pub fn register_tick_handler(&self, handler: Arc<dyn crate::heartbeat::TickHandler>) {
        if let Ok(mut hs) = self.tick_handlers.lock() {
            hs.push(handler);
        }
    }

    pub async fn submit(&self, sub: Submission) -> Result<(), String> {
        self.reap_idle();
        let session_id = sub.session_id.clone();
        let handle = match &sub.op {
            Op::UserMessage { .. } | Op::Steer { .. } => self.ensure_session(&sub.session_id)?,
            Op::Approval { .. } | Op::Cancel => self
                .existing_session(&sub.session_id)
                .ok_or_else(|| format!("session not found: {}", sub.session_id))?,
            Op::Shutdown => {
                let Some((_, handle)) = self.sessions.remove(&sub.session_id) else {
                    self.rooms.remove(&sub.session_id);
                    return Err(format!("session not found: {}", sub.session_id));
                };
                self.rooms.remove(&sub.session_id);
                handle
            }
        };
        self.touch(&session_id);
        handle.submit(sub).await.map_err(|e| e.to_string())
    }

    pub fn open_session(&self, session_id: impl Into<SessionId>) -> Result<SessionId, String> {
        self.open_session_for_bot(session_id, DEFAULT_BOT_ID)
    }

    pub fn open_session_for_bot(
        &self,
        session_id: impl Into<SessionId>,
        bot_id: impl AsRef<str>,
    ) -> Result<SessionId, String> {
        self.reap_idle();
        let session_id = session_id.into();
        let bot_id = bot_id.as_ref();
        self.bot_entry(bot_id)?;
        if self.existing_session(&session_id).is_none() {
            self.session_bots
                .insert(session_id.clone(), bot_id.to_string());
        }
        // §2.8 懒持久化：**不**在 open 时给空会话写 meta（WS 连接即开但从不发言会攒空壳）。
        // meta 在首条消息提交时由 session_driver `upsert_meta_after_turn` 创建。fork/团队委派
        // 等有内容的会话仍在各自路径即时写 meta。
        self.ensure_session(&session_id)?;
        Ok(session_id)
    }

    pub fn list_bots(&self) -> Vec<BotEntry> {
        let mut bots: Vec<_> = self
            .bots
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        bots.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        bots
    }

    pub fn create_bot(
        &self,
        name: impl Into<String>,
        workdir: impl Into<PathBuf>,
    ) -> Result<BotEntry, String> {
        self.create_bot_with_profile(name, workdir, "coder")
    }

    /// §5.7：带模板/profile 创建 bot（市场选模板）。`profile` 落 `BotEntry.profile`——Hub 单 agent
    /// 下作组织/元数据，行为差异待 Hub 多 agent。其余（workdir 校验/落盘/注册）与 [`Self::create_bot`] 同。
    pub fn create_bot_with_profile(
        &self,
        name: impl Into<String>,
        workdir: impl Into<PathBuf>,
        profile: impl Into<String>,
    ) -> Result<BotEntry, String> {
        let name = name.into().trim().to_string();
        let name = if name.is_empty() {
            "bot".to_string()
        } else {
            name
        };
        let profile = profile.into();
        let profile = if profile.trim().is_empty() {
            "coder".to_string()
        } else {
            profile
        };
        let raw_workdir = workdir.into();
        let workdir = std::fs::canonicalize(&raw_workdir)
            .map_err(|err| format!("invalid bot workdir {}: {err}", raw_workdir.display()))?;
        if !workdir.is_dir() {
            return Err(format!(
                "bot workdir is not a directory: {}",
                workdir.display()
            ));
        }
        let bot = BotEntry {
            id: format!("bot-{}", uuid::Uuid::new_v4()),
            name,
            profile,
            workdir,
            system: None,
        };
        self.bots.insert(bot.id.clone(), bot.clone());
        if let Some(store) = &self.store {
            if let Err(err) = store.save_bots(&self.list_bots()) {
                tracing::warn!("save bots failed: {err}");
            }
        }
        // 镜像进 switchboard，供 open_team 成员校验。
        if let Ok(mut switchboard) = self.switchboard.lock() {
            let _ = switchboard.add_bot(team_core::Bot {
                id: bot.id.clone(),
                name: bot.name.clone(),
                role: bot.profile.clone(),
                home: Some(bot.workdir.clone()),
            });
        }
        Ok(bot)
    }

    // ───────────────────────── IM 协作层（§4.5）─────────────────────────

    /// 登记一个项目（open_team 的 default_bots fallback 据此）。
    pub fn create_team_project(
        &self,
        id: impl Into<String>,
        name: impl Into<String>,
        root_dir: impl Into<PathBuf>,
        default_bots: Vec<String>,
    ) -> Result<(), String> {
        {
            let mut switchboard = self
                .switchboard
                .lock()
                .map_err(|_| "switchboard poisoned")?;
            switchboard
                .add_project(team_core::TeamProject {
                    id: id.into(),
                    name: name.into(),
                    root_dir: root_dir.into(),
                    default_bots,
                })
                .map_err(|e| e.to_string())?;
        }
        self.persist_switchboard();
        Ok(())
    }

    /// 开一个群（A 薄层）：switchboard 校验/建群 → 为 leader ensure_session 并记委派边 → 持久化。
    pub fn open_team(
        &self,
        project_id: &str,
        members: Vec<String>,
        leader: String,
        task: impl Into<String>,
    ) -> Result<String, String> {
        let team_id = {
            let mut switchboard = self
                .switchboard
                .lock()
                .map_err(|_| "switchboard poisoned")?;
            switchboard
                .open_team(project_id, members, leader.clone(), task)
                .map_err(|e| e.to_string())?
        };
        // leader 开 session（复用现有单会话路径），并记 Leader 委派边。
        let leader_session = uuid::Uuid::new_v4().to_string();
        self.open_session_for_bot(&leader_session, &leader)?;
        {
            let mut switchboard = self
                .switchboard
                .lock()
                .map_err(|_| "switchboard poisoned")?;
            switchboard
                .link_session(&team_id, &leader, &leader_session, RoleInTeam::Leader, None)
                .map_err(|e| e.to_string())?;
        }
        self.persist_switchboard();
        Ok(team_id)
    }

    /// 群内发一条消息（用户或 bot），返回 seq；增量落盘 transcript。
    pub fn team_post(
        &self,
        team_id: &str,
        author: Author,
        content: impl Into<String>,
    ) -> Result<u64, String> {
        let content = content.into();
        let seq = {
            let mut switchboard = self
                .switchboard
                .lock()
                .map_err(|_| "switchboard poisoned")?;
            switchboard
                .post_message(team_id, author.clone(), content.clone())
                .map_err(|e| e.to_string())?
        };
        if let Some(ts) = &self.team_store {
            if let Err(e) = ts.append_team_message(
                team_id,
                &team_core::Message {
                    seq,
                    author,
                    content,
                },
            ) {
                tracing::warn!("append team message failed: {e}");
            }
        }
        Ok(seq)
    }

    /// leader 委派 member（v1）：校验成员 → 为 member 开 session 并写 team_member meta → 记委派边。
    /// 返回 member session id。
    pub fn team_delegate(&self, team_id: &str, member_bot: &str) -> Result<SessionId, String> {
        // 校验 + 取 leader session（requested_by）
        let requested_by = {
            let switchboard = self
                .switchboard
                .lock()
                .map_err(|_| "switchboard poisoned")?;
            switchboard
                .delegate(team_id, member_bot)
                .map_err(|e| e.to_string())?;
            switchboard.team(team_id).and_then(|t| {
                t.session_links
                    .iter()
                    .find(|l| l.role == RoleInTeam::Leader)
                    .map(|l| l.session_id.clone())
            })
        };
        let member_session = uuid::Uuid::new_v4().to_string();
        self.open_session_for_bot(&member_session, member_bot)?;
        // 覆写为 team_member meta（边类型分离：用 team_id/requested_by/role，不用 parent_session）。
        if let Some(store) = &self.store {
            let mut meta = SessionMeta::new_chat(&member_session, member_bot);
            meta.kind = SessionKind::TeamMember;
            meta.team_id = Some(team_id.to_string());
            meta.requested_by_session = requested_by.clone();
            meta.role_in_team = Some("member".to_string());
            if let Err(e) = store.write_meta(&member_session, &meta) {
                tracing::warn!("write team_member meta failed: {e}");
            }
        }
        {
            let mut switchboard = self
                .switchboard
                .lock()
                .map_err(|_| "switchboard poisoned")?;
            switchboard
                .link_session(
                    team_id,
                    member_bot,
                    &member_session,
                    RoleInTeam::Member,
                    requested_by,
                )
                .map_err(|e| e.to_string())?;
        }
        self.persist_switchboard();
        Ok(member_session)
    }

    /// `/api/teams` 快照（teams + projects）。
    pub fn teams_snapshot(&self) -> SwitchboardSnapshot {
        self.switchboard
            .lock()
            .map(|o| o.snapshot())
            .unwrap_or(SwitchboardSnapshot {
                teams: Vec::new(),
                projects: Vec::new(),
            })
    }

    fn persist_switchboard(&self) {
        if let Some(ts) = &self.team_store {
            if let Ok(switchboard) = self.switchboard.lock() {
                if let Err(e) = ts.persist(&switchboard) {
                    tracing::warn!("persist switchboard failed: {e}");
                }
            }
        }
    }

    pub fn subscribe(&self, session_id: &str) -> Option<broadcast::Receiver<Event>> {
        self.reap_idle();
        self.touch(session_id);
        self.rooms.get(session_id).map(|room| room.tx.subscribe())
    }

    pub fn events_for(&self, session_id: &str, op_id: Option<&str>) -> Option<Vec<Event>> {
        self.reap_idle();
        self.touch(session_id);
        self.rooms.get(session_id).map(|room| {
            let Ok(log) = room.log.lock() else {
                return Vec::new();
            };
            log.iter()
                .filter(|ev| op_id.is_none_or(|id| ev.id == id))
                .cloned()
                .collect()
        })
    }

    pub async fn cancel_session(&self, session_id: &str) -> Result<Option<OpId>, String> {
        self.reap_idle();
        let Some(handle) = self.existing_session(session_id) else {
            return Ok(None);
        };
        self.touch(session_id);
        let sub = Submission::new(session_id.to_string(), Op::Cancel);
        let id = sub.id.clone();
        handle.submit(sub).await.map_err(|e| e.to_string())?;
        Ok(Some(id))
    }

    pub async fn shutdown_session(&self, session_id: &str) -> Result<Option<OpId>, String> {
        self.reap_idle();
        let Some((_, handle)) = self.sessions.remove(session_id) else {
            self.rooms.remove(session_id);
            self.session_bots.remove(session_id);
            return Ok(None);
        };
        let sub = Submission::new(session_id.to_string(), Op::Shutdown);
        let id = sub.id.clone();
        handle.submit(sub).await.map_err(|e| e.to_string())?;
        self.rooms.remove(session_id);
        self.session_bots.remove(session_id);
        Ok(Some(id))
    }

    /// 删除会话：停掉内存 actor **并删除持久化目录** `sessions/<id>/`（§2.8）。
    /// 区别于 `shutdown_session`（只停 actor、保留磁盘，供 reap/重启复用）：DELETE 语义须真删盘，
    /// 否则重启后回读会把"已删"会话又显示出来（空壳累积）。
    pub async fn delete_session(&self, session_id: &str) -> Result<Option<OpId>, String> {
        let id = self.shutdown_session(session_id).await?;
        if let Some(store) = &self.store {
            if let Err(err) = store.delete_session(session_id) {
                tracing::warn!("delete persisted session failed: {err}");
            }
        }
        Ok(id)
    }

    pub fn list_sessions(&self) -> Vec<SessionId> {
        self.reap_idle();
        let mut ids: Vec<_> = self
            .sessions
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        if let Some(store) = &self.store {
            if let Ok(metas) = store.list_metas() {
                ids.extend(metas.into_iter().map(|meta| meta.session_id));
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }

    /// 会话元信息列表（带 kind/parent_session/bot_id，供建树）。
    pub fn list_session_metas(&self) -> Vec<SessionMeta> {
        self.store
            .as_ref()
            .and_then(|store| store.list_metas().ok())
            .unwrap_or_default()
    }

    pub fn session_history(&self, session_id: &str) -> Result<Vec<base_types::Message>, String> {
        self.store
            .as_ref()
            .map(|store| {
                // §2.6 缺陷3 阶0：加载前先恢复上次半途崩溃的 turn（scratch 非空则并回 messages.jsonl）。
                match store.recover_scratch(session_id) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(target: "botobot::recover", session = session_id, recovered = n, "恢复半途崩溃 turn 的 {n} 条消息"),
                    Err(err) => tracing::warn!("recover turn-scratch failed: {err}"),
                }
                store.load_messages(session_id)
            })
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    pub fn fork_session(
        &self,
        source_session_id: &str,
        new_session_id: impl Into<SessionId>,
    ) -> Result<SessionId, String> {
        self.reap_idle();
        let new_session_id = new_session_id.into();
        if self.existing_session(&new_session_id).is_some() {
            return Err(format!("session already exists: {new_session_id}"));
        }
        let history = self.session_history(source_session_id)?;
        if history.is_empty()
            && !self
                .list_sessions()
                .iter()
                .any(|id| id == source_session_id)
        {
            return Err(format!("session not found: {source_session_id}"));
        }
        let bot_id = self
            .session_bot_id(source_session_id)
            .unwrap_or_else(|| DEFAULT_BOT_ID.to_string());
        if let Some(store) = &self.store {
            store.append_messages(&new_session_id, &history)?;
            let mut meta = SessionMeta::new_chat(&new_session_id, &bot_id);
            meta.kind = SessionKind::Fork;
            meta.parent_session = Some(source_session_id.to_string());
            meta.fork_point = Some(history.len());
            meta.message_count = history.len();
            store.write_meta(&new_session_id, &meta)?;
        }
        self.session_bots.insert(new_session_id.clone(), bot_id);
        self.ensure_session_with_history(&new_session_id, history)?;
        Ok(new_session_id)
    }

    pub fn count(&self) -> usize {
        self.reap_idle();
        self.sessions.len()
    }

    fn existing_session(&self, session_id: &str) -> Option<SessionHandle> {
        self.sessions.get(session_id).map(|handle| handle.clone())
    }

    fn bot_entry(&self, bot_id: &str) -> Result<BotEntry, String> {
        self.bots
            .get(bot_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| format!("bot not found: {bot_id}"))
    }

    fn session_bot_id(&self, session_id: &str) -> Option<String> {
        self.session_bots
            .get(session_id)
            .map(|entry| entry.value().clone())
    }

    fn agent_for_session(&self, session_id: &str) -> Arc<Agent> {
        let Some(bot_id) = self.session_bot_id(session_id) else {
            return self.agent.clone();
        };
        let Ok(bot) = self.bot_entry(&bot_id) else {
            return self.agent.clone();
        };
        // §5.7：先按 profile 选 base agent（通用/编程人格不同），profile 未注册则回退默认 agent。
        let base = self
            .profile_agents
            .get(&bot.profile)
            .map(|a| a.value().clone())
            .unwrap_or_else(|| self.agent.clone());
        // §5.7：自定义 bot.md 覆盖角色 prompt（live，下一轮生效）。
        let base = match &bot.system {
            Some(s) if !s.trim().is_empty() => Arc::new(base.with_system(s.clone())),
            _ => base,
        };
        // 再叠 workdir 视图（与 profile 选择/system 覆盖正交）。
        if bot.workdir == base.workdir() {
            base
        } else {
            Arc::new(base.with_workdir(bot.workdir))
        }
    }

    fn ensure_session(&self, session_id: &str) -> Result<SessionHandle, String> {
        let history = self.session_history(session_id)?;
        self.ensure_session_with_history(session_id, history)
    }

    fn ensure_session_with_history(
        &self,
        session_id: &str,
        history: Vec<base_types::Message>,
    ) -> Result<SessionHandle, String> {
        if let Some(handle) = self.existing_session(session_id) {
            self.touch(session_id);
            return Ok(handle);
        }

        if self.sessions.len() >= self.config.max_sessions {
            return Err(format!(
                "max sessions reached ({})",
                self.config.max_sessions
            ));
        }

        let (event_tx, _) = broadcast::channel(self.config.broadcast_capacity.max(1));
        let event_log = Arc::new(Mutex::new(Vec::new()));
        let bot_id = self
            .session_bot_id(session_id)
            .unwrap_or_else(|| DEFAULT_BOT_ID.to_string());
        let handle = spawn_session_driver(
            session_id.to_string(),
            self.agent_for_session(session_id),
            event_tx.clone(),
            event_log.clone(),
            self.config.event_log_capacity,
            history,
            self.store.clone(),
            bot_id,
        );
        self.rooms.insert(
            session_id.to_string(),
            Room {
                tx: event_tx,
                log: event_log,
                last_active: Arc::new(Mutex::new(Instant::now())),
            },
        );
        self.sessions.insert(session_id.to_string(), handle.clone());
        Ok(handle)
    }

    fn touch(&self, session_id: &str) {
        if let Some(room) = self.rooms.get(session_id) {
            if let Ok(mut last_active) = room.last_active.lock() {
                *last_active = Instant::now();
            }
        }
    }

    fn reap_idle(&self) {
        if self.config.idle_ttl.is_zero() {
            return;
        }
        let now = Instant::now();
        let stale: Vec<_> = self
            .rooms
            .iter()
            .filter_map(|entry| {
                let Ok(last_active) = entry.last_active.lock() else {
                    return None;
                };
                (now.duration_since(*last_active) > self.config.idle_ttl)
                    .then(|| entry.key().clone())
            })
            .collect();

        for session_id in stale {
            if let Some((_, handle)) = self.sessions.remove(&session_id) {
                let _ = handle.try_submit(Submission::new(session_id.clone(), Op::Shutdown));
            }
            self.rooms.remove(&session_id);
            self.session_bots.remove(&session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{Decision, Llm, LlmError, LlmEvent, LlmOpts, Message, ToolSpec};

    struct OneShotLlm;
    struct SlowLlm;

    #[async_trait]
    impl Llm for OneShotLlm {
        async fn infer(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            let decision = Decision {
                text: "hello from hub".into(),
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

    #[async_trait]
    impl Llm for SlowLlm {
        async fn infer(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    fn test_hub() -> Hub {
        Hub::new(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
        )
    }

    fn test_hub_with_store(dir: PathBuf) -> Hub {
        Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            HubConfig {
                store_root: Some(dir),
                ..HubConfig::default()
            },
        )
    }

    fn msg_text(message: &Message) -> String {
        message
            .content
            .iter()
            .map(|part| match part {
                base_types::ContentPart::Text(text) => text.as_str(),
                base_types::ContentPart::ImageUrl(_) => "",
            })
            .collect()
    }

    #[tokio::test]
    async fn submit_creates_session_and_broadcasts_events() {
        let hub = test_hub();
        let sub = Submission::new(
            "s-1",
            crate::protocol::Op::UserMessage {
                text: "hello".into(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            },
        );
        hub.submit(sub).await.unwrap();

        let mut rx = hub.subscribe("s-1").expect("session room should exist");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.session_id, "s-1");
        assert_eq!(hub.count(), 1);
        assert_eq!(hub.list_sessions(), vec!["s-1".to_string()]);
    }

    #[test]
    fn subscribe_unknown_session_returns_none() {
        let hub = test_hub();
        assert!(hub.subscribe("missing").is_none());
    }

    // §5.7：注册 general profile agent 后，profile=general 的 bot 路由到它的 system prompt；
    // 未注册 profile（coder）回退默认 agent；空注册表=纯默认（向后兼容）。
    #[test]
    fn profile_agent_routes_system_prompt_by_bot_profile() {
        let hub = Hub::new(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .system("CODER DEFAULT")
                .build(),
        );
        // 空注册表：默认 bot（coder）→ 默认 prompt。
        assert_eq!(
            hub.system_prompt_for_bot(DEFAULT_BOT_ID).as_deref(),
            Some("CODER DEFAULT")
        );
        // 注册 general profile（不同 prompt）。
        let general = agent_loop::Agent::builder()
            .llm(Arc::new(OneShotLlm))
            .system("GENERAL ROLE")
            .build();
        hub.register_profile_agent("general", general);
        // 建一个 general bot。
        let tmp = std::env::temp_dir().display().to_string();
        let bot = hub.create_bot_with_profile("g", tmp, "general").unwrap();
        assert_eq!(hub.system_prompt_for_bot(&bot.id).as_deref(), Some("GENERAL ROLE"));
        // coder profile 未注册 → 回退默认。
        assert_eq!(hub.system_prompt_for_bot(DEFAULT_BOT_ID).as_deref(), Some("CODER DEFAULT"));
    }

    #[tokio::test]
    async fn max_sessions_rejects_new_session() {
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            HubConfig {
                max_sessions: 1,
                ..HubConfig::default()
            },
        );

        assert!(hub.open_session("s-1").is_ok());
        assert!(hub.open_session("s-2").is_err());
        assert_eq!(hub.count(), 1);
    }

    #[tokio::test]
    async fn idle_sessions_are_reaped_lazily() {
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            HubConfig {
                idle_ttl: Duration::from_millis(1),
                ..HubConfig::default()
            },
        );

        hub.open_session("s-1").unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(hub.list_sessions().is_empty());
    }

    #[tokio::test]
    async fn cancel_session_publishes_cancel_complete() {
        let hub = Hub::new(agent_loop::Agent::builder().llm(Arc::new(SlowLlm)).build());
        hub.open_session("s-1").unwrap();
        let mut rx = hub.subscribe("s-1").unwrap();
        hub.submit(Submission::new(
            "s-1",
            Op::UserMessage {
                text: "hang".into(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            },
        ))
        .await
        .unwrap();

        let cancel_id = hub.cancel_session("s-1").await.unwrap().unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if ev.id == cancel_id {
                    break ev;
                }
            }
        })
        .await
        .unwrap();

        assert!(matches!(ev.msg, crate::protocol::EventMsg::CancelComplete));
    }

    #[tokio::test]
    async fn direct_shutdown_submission_removes_session() {
        let hub = test_hub();
        hub.open_session("s-1").unwrap();

        hub.submit(Submission::new("s-1", Op::Shutdown))
            .await
            .unwrap();

        assert_eq!(hub.count(), 0);
        assert!(hub.subscribe("s-1").is_none());
    }

    #[tokio::test]
    async fn session_store_persists_history_and_forks_session() {
        let dir = std::env::temp_dir().join(format!("botobot-hub-store-{}", uuid::Uuid::new_v4()));
        let hub = test_hub_with_store(dir.clone());
        let mut rx = {
            hub.open_session("s-1").unwrap();
            hub.subscribe("s-1").unwrap()
        };
        hub.submit(Submission::new(
            "s-1",
            Op::UserMessage {
                text: "hello store".into(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            },
        ))
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if matches!(ev.msg, crate::protocol::EventMsg::TurnComplete) {
                    break;
                }
            }
        })
        .await
        .unwrap();

        let history = hub.session_history("s-1").unwrap();
        assert!(history.iter().any(|m| msg_text(m).contains("hello store")));
        assert!(
            history
                .iter()
                .any(|m| msg_text(m).contains("hello from hub"))
        );

        let forked = hub.fork_session("s-1", "s-2").unwrap();
        assert_eq!(forked, "s-2");
        let fork_history = hub.session_history("s-2").unwrap();
        assert_eq!(fork_history.len(), history.len());
        assert!(hub.list_sessions().contains(&"s-2".to_string()));

        // fork meta 记录父子关系（验收 ③）
        let store = crate::session_store::SessionStore::new(dir.clone());
        let fork_meta = store.read_meta("s-2").unwrap().unwrap();
        assert_eq!(fork_meta.kind, crate::session_store::SessionKind::Fork);
        assert_eq!(fork_meta.parent_session.as_deref(), Some("s-1"));
        assert_eq!(fork_meta.fork_point, Some(history.len()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn team_lifecycle_open_delegate_post_persist() {
        let dir = std::env::temp_dir().join(format!("botobot-team-{}", uuid::Uuid::new_v4()));
        let wd_a = std::env::temp_dir().join(format!("wd-a-{}", uuid::Uuid::new_v4()));
        let wd_b = std::env::temp_dir().join(format!("wd-b-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&wd_a).unwrap();
        std::fs::create_dir_all(&wd_b).unwrap();

        let team_id = {
            let hub = test_hub_with_store(dir.clone());
            let a = hub.create_bot("leader", &wd_a).unwrap();
            let b = hub.create_bot("member", &wd_b).unwrap();
            hub.create_team_project("p1", "Project", &wd_a, vec![])
                .unwrap();
            let tid = hub
                .open_team(
                    "p1",
                    vec![a.id.clone(), b.id.clone()],
                    a.id.clone(),
                    "build x",
                )
                .unwrap();
            hub.team_post(&tid, Author::User, "kick off").unwrap();
            let member_session = hub.team_delegate(&tid, &b.id).unwrap();

            // 委派 → member session 记 team_member meta（边类型分离）
            let store = crate::session_store::SessionStore::new(dir.clone());
            let meta = store.read_meta(&member_session).unwrap().unwrap();
            assert_eq!(meta.kind, crate::session_store::SessionKind::TeamMember);
            assert_eq!(meta.team_id.as_deref(), Some(tid.as_str()));
            assert!(meta.requested_by_session.is_some());

            // 非成员委派被拒
            assert!(hub.team_delegate(&tid, "ghost").is_err());
            tid
        };

        // 重启：同 root 新建 Hub，switchboard 从 TeamStore 恢复
        let hub2 = test_hub_with_store(dir.clone());
        let snap = hub2.teams_snapshot();
        assert_eq!(snap.teams.len(), 1);
        let t = &snap.teams[0];
        assert_eq!(t.id, team_id);
        assert_eq!(t.messages.len(), 1);
        assert_eq!(t.messages[0].content, "kick off");
        // leader + member 两条委派边
        assert_eq!(t.session_links.len(), 2);
        assert_eq!(snap.projects.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(wd_a);
        let _ = std::fs::remove_dir_all(wd_b);
    }

    // 发一条消息并等 TurnComplete（懒持久化后 meta 在首条消息提交时才落盘）。
    async fn message_and_wait(hub: &Hub, sid: &str) {
        let mut rx = {
            hub.open_session(sid).unwrap();
            hub.subscribe(sid).unwrap()
        };
        hub.submit(Submission::new(
            sid,
            Op::UserMessage {
                text: "hi".into(),
                images: Vec::new(),
                thinking: None,
                web_search: None,
                code_execution: None,
                force_recall: false,
            },
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if matches!(ev.msg, crate::protocol::EventMsg::TurnComplete) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn clean_turn_clears_turn_scratch() {
        // §2.6 缺陷3 阶0：干净收尾的 turn 应把消息落 messages.jsonl 并清空 turn-scratch。
        let dir = std::env::temp_dir().join(format!("botobot-scratch-{}", uuid::Uuid::new_v4()));
        let hub = test_hub_with_store(dir.clone());
        message_and_wait(&hub, "s-scratch").await;

        let store = hub.store.as_ref().unwrap();
        assert!(
            !store.load_messages("s-scratch").unwrap().is_empty(),
            "干净 turn 应已落 messages.jsonl"
        );
        assert!(
            store.read_scratch("s-scratch").unwrap().is_empty(),
            "干净收尾后 turn-scratch 应已清空"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn delete_session_removes_persisted_dir() {
        let dir = std::env::temp_dir().join(format!("botobot-del-{}", uuid::Uuid::new_v4()));
        let hub = test_hub_with_store(dir.clone());
        message_and_wait(&hub, "s-del").await; // 懒持久化：发消息后才落盘
        let sess_dir = dir.join("sessions").join("s-del");
        assert!(sess_dir.exists(), "发消息后应有持久化目录");

        hub.delete_session("s-del").await.unwrap();
        assert!(!sess_dir.exists(), "delete 后持久化目录应被删除");
        // 重启回读不再出现
        let hub2 = test_hub_with_store(dir.clone());
        assert!(
            !hub2
                .list_session_metas()
                .iter()
                .any(|m| m.session_id == "s-del")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn restart_recovers_bots_and_session_binding() {
        let dir =
            std::env::temp_dir().join(format!("botobot-hub-restart-{}", uuid::Uuid::new_v4()));
        let workdir = std::env::temp_dir().join(format!("botobot-bot-wd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workdir).unwrap();

        // 第一次启动：建自定义 bot + 绑定会话 + 发消息（懒持久化：发消息后 meta 才落盘）
        let bot_id = {
            let hub = test_hub_with_store(dir.clone());
            let bot = hub.create_bot("research", &workdir).unwrap();
            hub.open_session_for_bot("sess-a", &bot.id).unwrap();
            let mut rx = hub.subscribe("sess-a").unwrap();
            hub.submit(Submission::new(
                "sess-a",
                Op::UserMessage {
                    text: "hi".into(),
                    images: Vec::new(),
                    thinking: None,
                    web_search: None,
                    code_execution: None,
                    force_recall: false,
                },
            ))
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let ev = rx.recv().await.unwrap();
                    if matches!(ev.msg, crate::protocol::EventMsg::TurnComplete) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            bot.id
        };

        // 模拟重启：同 root 新建 Hub
        let hub2 = test_hub_with_store(dir.clone());
        // ① 自定义 bot 仍在
        assert!(
            hub2.list_bots()
                .iter()
                .any(|b| b.id == bot_id && b.name == "research")
        );
        // ② 会话仍绑定原 bot（非回退默认）
        assert_eq!(
            hub2.session_bot_id("sess-a").as_deref(),
            Some(bot_id.as_str())
        );

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(workdir);
    }

    // §2.10：Hub 心跳晶振常驻，注册的 TickHandler 被周期派发。
    #[tokio::test]
    async fn hub_heartbeat_dispatches_registered_handler() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct Counting(Arc<AtomicU64>);
        impl crate::heartbeat::TickHandler for Counting {
            fn on_tick(&self, _c: u64) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            HubConfig {
                tick_interval: Duration::from_millis(10),
                ..HubConfig::default()
            },
        );
        let hits = Arc::new(AtomicU64::new(0));
        hub.register_tick_handler(Arc::new(Counting(hits.clone())));
        tokio::time::sleep(Duration::from_millis(55)).await;
        assert!(
            hits.load(Ordering::Relaxed) >= 3,
            "心跳应周期派发已注册 handler"
        );
    }

    #[tokio::test]
    async fn cron_job_fires_and_submits_turn() {
        let hub = Hub::with_config(
            agent_loop::Agent::builder()
                .llm(Arc::new(OneShotLlm))
                .build(),
            HubConfig {
                tick_interval: Duration::from_millis(10),
                ..HubConfig::default()
            },
        );
        // 一次性 job：~20ms 后向 "cron-s" 发 prompt。
        hub.schedule_job("cron-s", "tick!", Duration::from_millis(20), None);
        assert_eq!(hub.list_cron().len(), 1, "登记后应有一条 job");

        // 等到点 + submit + spawn 落地：会话应被建出并广播事件。
        let mut got = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if hub.subscribe("cron-s").is_some() {
                got = true;
                break;
            }
        }
        assert!(got, "cron 到点应 submit 发起 turn，建出目标会话");
        // 一次性 job 触发后应从表中移除。
        assert!(hub.list_cron().is_empty(), "一次性 job 触发后应清除");
    }
}
