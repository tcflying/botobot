//! Agent-facing Team 工具（§4.5）：让 leader bot 在对话里观察/发言/委派。
//!
//! 方案 A（共享 Switchboard）：工具就近定义在 bot-api（依赖 team-core），持 `Arc<Mutex<Switchboard>>`
//! 与 Hub 共用同一棵协作树；在 webui-bin 装配时构造并注册进 coder agent。
//! 作者身份由 `Switchboard.session_links` 按 `ToolCtx.session_id` 解析（leader/member→Bot，否则 User）。
//!
//! 边界：`team_delegate` 本切片只**记意图**（在 transcript 落一条委派消息）；同步开 member
//! session 并执行的编排属 session_driver 集成切片，不在此。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use team_core::{Author, Switchboard, TeamStore};

use base_types::{Tool, ToolCtx, ToolResult, ToolTier};

/// 四个工具共享的协作句柄。
#[derive(Clone)]
pub struct TeamToolCtx {
    switchboard: Arc<Mutex<Switchboard>>,
    store: Option<TeamStore>,
}

impl TeamToolCtx {
    pub fn new(switchboard: Arc<Mutex<Switchboard>>, store: Option<TeamStore>) -> Self {
        Self { switchboard, store }
    }

    fn author_for(&self, team_id: &str, session_id: &str) -> Author {
        if let Ok(o) = self.switchboard.lock() {
            if let Some(team) = o.team(team_id) {
                if let Some(link) = team
                    .session_links
                    .iter()
                    .find(|l| l.session_id == session_id)
                {
                    return Author::Bot(link.bot_id.clone());
                }
            }
        }
        Author::User
    }
}

fn arg_str(args: &Value, key: &str) -> Result<String, anyhow::Error> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing required arg: {key}"))
}

// ───────────────────────── team_members ─────────────────────────

pub struct TeamMembersTool {
    cx: TeamToolCtx,
}
impl TeamMembersTool {
    pub fn new(cx: TeamToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for TeamMembersTool {
    fn name(&self) -> &str {
        "team_members"
    }
    fn description(&self) -> &str {
        "List the bot members of a team (and who is leader). Arg: team_id."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "team_id": { "type": "string" } },
            "required": ["team_id"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let team_id = arg_str(&args, "team_id")?;
        let o = self
            .cx
            .switchboard
            .lock()
            .map_err(|_| anyhow::anyhow!("switchboard poisoned"))?;
        let team = o
            .team(&team_id)
            .ok_or_else(|| anyhow::anyhow!("team not found: {team_id}"))?;
        Ok(json!({
            "team_id": team_id,
            "leader": team.leader,
            "members": team.members,
            "status": team.status,
        }))
    }
}

// ───────────────────────── team_read ─────────────────────────

pub struct TeamReadTool {
    cx: TeamToolCtx,
}
impl TeamReadTool {
    pub fn new(cx: TeamToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for TeamReadTool {
    fn name(&self) -> &str {
        "team_read"
    }
    fn description(&self) -> &str {
        "Read a team's message transcript (IM-level). Arg: team_id."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "team_id": { "type": "string" } },
            "required": ["team_id"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let team_id = arg_str(&args, "team_id")?;
        let o = self
            .cx
            .switchboard
            .lock()
            .map_err(|_| anyhow::anyhow!("switchboard poisoned"))?;
        let team = o
            .team(&team_id)
            .ok_or_else(|| anyhow::anyhow!("team not found: {team_id}"))?;
        let lines: Vec<String> = team
            .messages
            .iter()
            .map(|m| {
                let who = match &m.author {
                    Author::User => "user".to_string(),
                    Author::Bot(id) => format!("bot:{id}"),
                };
                format!("[{}] {}: {}", m.seq, who, m.content)
            })
            .collect();
        Ok(Value::String(lines.join("\n")))
    }
}

// ───────────────────────── team_post ─────────────────────────

pub struct TeamPostTool {
    cx: TeamToolCtx,
}
impl TeamPostTool {
    pub fn new(cx: TeamToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for TeamPostTool {
    fn name(&self) -> &str {
        "team_post"
    }
    fn description(&self) -> &str {
        "Post a message to a team's transcript (authored by the calling bot). Args: team_id, content."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "team_id": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["team_id", "content"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        // 无上下文时作者降级为 User（极少走到；正常经 call_with_context）。
        self.post(args, "").await
    }
    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        self.post(args, &ctx.session_id).await
    }
}

impl TeamPostTool {
    async fn post(&self, args: Value, session_id: &str) -> ToolResult {
        let team_id = arg_str(&args, "team_id")?;
        let content = arg_str(&args, "content")?;
        let author = self.cx.author_for(&team_id, session_id);
        let seq = {
            let mut o = self
                .cx
                .switchboard
                .lock()
                .map_err(|_| anyhow::anyhow!("switchboard poisoned"))?;
            o.post_message(&team_id, author.clone(), content.clone())
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
        };
        if let Some(store) = &self.cx.store {
            if let Err(e) = store.append_team_message(
                &team_id,
                &team_core::Message {
                    seq,
                    author,
                    content,
                },
            ) {
                tracing::warn!("append team message failed: {e}");
            }
        }
        Ok(json!({ "team_id": team_id, "seq": seq }))
    }
}

// ───────────────────────── team_delegate ─────────────────────────

pub struct TeamDelegateTool {
    cx: TeamToolCtx,
}
impl TeamDelegateTool {
    pub fn new(cx: TeamToolCtx) -> Self {
        Self { cx }
    }
}

#[async_trait]
impl Tool for TeamDelegateTool {
    fn name(&self) -> &str {
        "team_delegate"
    }
    fn description(&self) -> &str {
        "Delegate a sub-task to a team member (records the delegation in the transcript). Args: team_id, bot_id, task."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "team_id": { "type": "string" },
                "bot_id": { "type": "string" },
                "task": { "type": "string" }
            },
            "required": ["team_id", "bot_id", "task"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        self.delegate(args, "").await
    }
    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        self.delegate(args, &ctx.session_id).await
    }
}

impl TeamDelegateTool {
    async fn delegate(&self, args: Value, session_id: &str) -> ToolResult {
        let team_id = arg_str(&args, "team_id")?;
        let bot_id = arg_str(&args, "bot_id")?;
        let task = arg_str(&args, "task")?;
        let author = self.cx.author_for(&team_id, session_id);
        let content = format!("@{bot_id} (delegate) {task}");
        let seq = {
            let mut o = self
                .cx
                .switchboard
                .lock()
                .map_err(|_| anyhow::anyhow!("switchboard poisoned"))?;
            // 成员校验（DelegateNotMember）
            o.delegate(&team_id, &bot_id)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            o.post_message(&team_id, author.clone(), content.clone())
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
        };
        if let Some(store) = &self.cx.store {
            let _ = store.append_team_message(
                &team_id,
                &team_core::Message {
                    seq,
                    author,
                    content,
                },
            );
        }
        Ok(json!({
            "team_id": team_id,
            "delegated_to": bot_id,
            "recorded_seq": seq,
            "note": "intent recorded; member session orchestration is handled separately"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use team_core::{Bot, TeamProject};

    fn ctx_with_team() -> (TeamToolCtx, String, String) {
        let mut switchboard = Switchboard::new();
        switchboard
            .add_bot(Bot {
                id: "leader".into(),
                name: "L".into(),
                role: "coder".into(),
                home: None,
            })
            .unwrap();
        switchboard
            .add_bot(Bot {
                id: "memb".into(),
                name: "M".into(),
                role: "coder".into(),
                home: None,
            })
            .unwrap();
        switchboard
            .add_project(TeamProject {
                id: "p1".into(),
                name: "p1".into(),
                root_dir: "/tmp".into(),
                default_bots: vec![],
            })
            .unwrap();
        let tid = switchboard
            .open_team(
                "p1",
                vec!["leader".into(), "memb".into()],
                "leader".into(),
                "x",
            )
            .unwrap();
        // leader 会话边
        switchboard
            .link_session(
                &tid,
                "leader",
                "sess-leader",
                team_core::RoleInTeam::Leader,
                None,
            )
            .unwrap();
        let cx = TeamToolCtx::new(Arc::new(Mutex::new(switchboard)), None);
        (cx, tid, "sess-leader".into())
    }

    #[tokio::test]
    async fn post_resolves_author_from_session_link() {
        let (cx, tid, leader_session) = ctx_with_team();
        let tool = TeamPostTool::new(cx.clone());
        let ctx = ToolCtx {
            session_id: leader_session,
            run_id: "r".into(),
            parent_id: None,
            workdir: "/tmp".into(),
            cancel: Default::default(),
            depth: 0,
            max_depth: 4,
            token_budget: None,
            llm_opts: Default::default(),
        };
        tool.call_with_context(json!({"team_id": tid, "content": "hello team"}), &ctx)
            .await
            .unwrap();
        let o = cx.switchboard.lock().unwrap();
        let msg = &o.team(&tid).unwrap().messages[0];
        assert_eq!(msg.author, Author::Bot("leader".into()));
        assert_eq!(msg.content, "hello team");
    }

    #[tokio::test]
    async fn delegate_validates_membership() {
        let (cx, tid, _) = ctx_with_team();
        let tool = TeamDelegateTool::new(cx.clone());
        // 成员 ok
        assert!(
            tool.call(json!({"team_id": tid, "bot_id": "memb", "task": "do y"}))
                .await
                .is_ok()
        );
        // 非成员被拒
        assert!(
            tool.call(json!({"team_id": tid, "bot_id": "ghost", "task": "z"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn read_and_members() {
        let (cx, tid, _) = ctx_with_team();
        TeamPostTool::new(cx.clone())
            .call(json!({"team_id": tid, "content": "x"}))
            .await
            .unwrap();
        let read = TeamReadTool::new(cx.clone())
            .call(json!({"team_id": tid}))
            .await
            .unwrap();
        assert!(read.as_str().unwrap().contains("x"));
        let members = TeamMembersTool::new(cx.clone())
            .call(json!({"team_id": tid}))
            .await
            .unwrap();
        assert_eq!(members["leader"], "leader");
    }
}
