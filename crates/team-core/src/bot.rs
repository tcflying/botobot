//! 成员（人）。心智 / 记忆 / 工具都在 bot 层，协作层只持身份标识。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 协作层登记的一个 bot 成员。bot.exe 路径 / 启动方式（adapter）v1 不建模。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub home: Option<PathBuf>,
}
