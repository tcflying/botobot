//! §4.7 OfficeCLI 薄壳 Tool（shell-out 到 [iOfficeAI/OfficeCLI] 二进制操作 .docx/.xlsx/.pptx）。
//! 配套 skill 已在 `skills/officecli*`；本模块补「Rust 薄壳工具」——把 officecli 当 vendored 黑盒
//! 后端接入（类比 ripgrep/git），**tier 分级**：view/get/query=Read（不打断），edit/raw=Exec（过 exec policy）。
//!
//! **默认路径**：优先 `BOTOBOT_OFFICECLI` 环境变量，其次同级目录 `officecli-win-x64.exe`（Windows）/ `officecli`（Linux/Mac）
//!
//! **二进制定位可单测**；**实际执行待 officecli 二进制**（`BOTOBOT_OFFICECLI` 指定，或 PATH）。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use base_types::{Tool, ToolResult, ToolTier};
/// 本平台 officecli 二进制候选名（标准名在前 + 常见 release 产物名——让用户把下载的 exe 原样
/// 丢进 `.bot/bin/` 即可，免改名）。回退 PATH 时用候选[0]（标准名）。
fn officecli_candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        &["officecli.exe", "officecli-win-x64.exe"]
    } else if cfg!(target_os = "macos") {
        &["officecli", "officecli-macos", "officecli-darwin-x64"]
    } else {
        &["officecli", "officecli-linux-x64"]
    }
}

/// 定位 officecli 二进制。查找顺序：
/// ① `BOTOBOT_OFFICECLI` env（显式覆盖，最高优先）；
/// ② **`.bot/bin/<候选名>`**（bot 专用工具目录，workdir 下、分发可移植；候选含 release 产物名
///    `officecli-win-x64.exe` 等，下载的 exe 原样丢进去即可、免改名）；
/// ③ PATH 裸名（系统已装 officecli 则照用）。
/// 全取 **CWD 相对的 `.bot`**——与 SessionStore/artifacts/shell PATH 前置同根（一致、可移植）。
pub fn officecli_path() -> PathBuf {
    let env_override = std::env::var("BOTOBOT_OFFICECLI")
        .ok()
        .filter(|p| !p.trim().is_empty());
    resolve_officecli(env_override, Path::new(".bot"))
}

/// 纯函数版（可单测）：给定 env 覆盖与 `.bot` 根，在 `<bot_root>/bin/` 试所有候选名。
fn resolve_officecli(env_override: Option<String>, bot_root: &Path) -> PathBuf {
    if let Some(p) = env_override {
        return PathBuf::from(p);
    }
    let bin = bot_root.join("bin");
    for name in officecli_candidates() {
        let p = bin.join(name);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from(officecli_candidates()[0])
}

/// 执行 officecli 子命令，返回 stdout（失败带 stderr）。需二进制存在（运行待环境）。
async fn run_officecli(args: &[String]) -> Result<String, String> {
    let out = tokio::process::Command::new(officecli_path())
        .args(args)
        .output()
        .await
        .map_err(|e| format!("officecli 启动失败（设 BOTOBOT_OFFICECLI 指定路径）: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "officecli 退出码 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// `officecli_view(path)` — 概览一个 Office 文档结构（Read·不打断）。
pub struct OfficeCliViewTool;
#[async_trait]
impl Tool for OfficeCliViewTool {
    fn name(&self) -> &str {
        "officecli_view"
    }
    fn description(&self) -> &str {
        "Outline the structure of an Office file (.docx/.xlsx/.pptx) via OfficeCLI. Read-only."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if path.is_empty() {
            anyhow::bail!("officecli_view: missing 'path'");
        }
        run_officecli(&["view".into(), path.into(), "--json".into()])
            .await
            .map(|o| json!({ "output": o }))
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// `officecli_raw(args)` — 透传任意 officecli 子命令（Exec·过 exec policy，含 edit/写）。
pub struct OfficeCliRawTool;
#[async_trait]
impl Tool for OfficeCliRawTool {
    fn name(&self) -> &str {
        "officecli_raw"
    }
    fn description(&self) -> &str {
        "Run an arbitrary OfficeCLI subcommand (args array). Use for get/query/edit. Exec tier."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "args": { "type": "array", "items": { "type": "string" } } },
            "required": ["args"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let list: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if list.is_empty() {
            anyhow::bail!("officecli_raw: empty 'args'");
        }
        run_officecli(&list)
            .await
            .map(|o| json!({ "output": o }))
            .map_err(|e| anyhow::anyhow!(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // env 覆盖 + 回退合并为一个测试（避免两个测试并行互踩同一 env var）。
    #[test]
    fn path_env_override_then_fallback() {
        // SAFETY: 单测内串行设置/清除自有 env key。
        unsafe { std::env::set_var("BOTOBOT_OFFICECLI", "/opt/officecli") };
        assert_eq!(
            officecli_path(),
            PathBuf::from("/opt/officecli"),
            "env 覆盖应优先"
        );
        unsafe { std::env::remove_var("BOTOBOT_OFFICECLI") };
        assert!(
            officecli_path().to_string_lossy().contains("officecli"),
            "回退裸名"
        );
    }

    // ② .bot/bin/ 项目本地 vendored：标准名与 release 产物名都能命中；env 覆盖仍最高优先；
    //    都不在则回退裸名。纯函数 resolve_officecli 注入临时根，避免污染真实 .bot。
    #[test]
    fn resolve_prefers_env_then_bot_bin_then_bare() {
        let bot_root = std::env::temp_dir().join(format!("botobot-occ-{}", uuid::Uuid::new_v4()));
        let bin = bot_root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // 都不在 → 回退裸名（候选[0]）。
        assert_eq!(
            resolve_officecli(None, &bot_root),
            PathBuf::from(officecli_candidates()[0])
        );

        // 放一个 release 产物名进 .bot/bin/ → 命中它（免改名）。
        let artifact = officecli_candidates()
            .iter()
            .rev()
            .find(|n| **n != officecli_candidates()[0])
            .copied()
            .unwrap_or(officecli_candidates()[0]);
        std::fs::write(bin.join(artifact), b"x").unwrap();
        assert_eq!(
            resolve_officecli(None, &bot_root),
            bin.join(artifact),
            ".bot/bin/ 应命中 vendored"
        );

        // env 覆盖仍最高优先。
        assert_eq!(
            resolve_officecli(Some("/opt/oc".into()), &bot_root),
            PathBuf::from("/opt/oc")
        );

        let _ = std::fs::remove_dir_all(&bot_root);
    }

    #[test]
    fn tiers_are_read_and_exec() {
        assert_eq!(OfficeCliViewTool.tier(), ToolTier::Read);
        assert_eq!(OfficeCliRawTool.tier(), ToolTier::Exec);
    }
}
