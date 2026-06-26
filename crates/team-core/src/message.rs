//! 群消息（IM 粒度 transcript）。正文 = **物理记录**（恢复/审计/回放），非语义记忆。

use serde::{Deserialize, Serialize};

/// 消息作者：用户或某个 bot。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    User,
    Bot(String),
}

/// 一条群消息。`seq` 在所属 Team 内单调递增。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub seq: u64,
    pub author: Author,
    pub content: String,
}
