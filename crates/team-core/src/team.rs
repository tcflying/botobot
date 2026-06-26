//! 群（一次性：一队 bot 干一个 task）。
//!
//! transcript 是 **IM 粒度**（`Vec<Message>`），与各 bot session 的 LLM/tool 粒度 history
//! 是两个抽象、不双真相源——故 team-core **不依赖 `SessionId` 类型**，session id 以字符串绑定。

use serde::{Deserialize, Serialize};

use crate::message::Message;
use crate::task::TeamTask;

/// 发言权策略。默认 `LeaderMediated`；`Broadcast` v1 留结构不实装。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicy {
    #[default]
    LeaderMediated,
    Broadcast,
}

/// 群生命周期状态。一次性：干完即 `Done`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamStatus {
    Active,
    Done,
    Cancelled,
}

/// 成员在群里的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleInTeam {
    Leader,
    Member,
}

/// 协作层绑定：team ↔ bot session 的委派边。session 详情仍归 SessionStore。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSessionLink {
    pub bot_id: String,
    pub session_id: String,
    pub role: RoleInTeam,
    #[serde(default)]
    pub requested_by_session: Option<String>,
}

/// 一个群。`task` 一次性，`leader` 必填（创建时定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub task: TeamTask,
    pub members: Vec<String>,
    pub leader: String,
    pub routing: RoutingPolicy,
    pub status: TeamStatus,
    pub messages: Vec<Message>,
    pub session_links: Vec<TeamSessionLink>,
}

impl Team {
    /// 下一条消息的 seq（= 当前消息数，0 起）。
    pub(crate) fn next_seq(&self) -> u64 {
        self.messages.len() as u64
    }

    pub fn is_member(&self, bot_id: &str) -> bool {
        self.members.iter().any(|m| m == bot_id)
    }
}
