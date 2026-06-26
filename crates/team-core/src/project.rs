//! 被处理的项目（独立实体，记录整个项目；≠ bot 的工作目录）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一个项目。`default_bots` = open_team 未显式给 members 时的 fallback 名单（Q11=C）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamProject {
    pub id: String,
    pub name: String,
    pub root_dir: PathBuf,
    #[serde(default)]
    pub default_bots: Vec<String>,
}
