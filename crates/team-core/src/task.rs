//! 这次要干的活（一次性）。一队 bot 干一个 task。

use serde::{Deserialize, Serialize};

/// 一个一次性任务。Team 自带它唯一的那个任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub project_id: String,
    pub description: String,
}
