//! 环境上下文块（§4.6 env-tech，借鉴前身 datoobot `env-tech` + Claude Code environment）。
//!
//! 生成一段注入 system prompt 的 `<environment>` 块，让 agent 自带 cwd/os/facts。

use std::path::{Path, PathBuf};

/// 环境上下文核心：cwd + os + 可选 facts，渲染成 `<environment>` 块。
pub struct EnvCore {
    cwd: PathBuf,
    os: String,
    facts: Vec<(String, String)>,
}

impl EnvCore {
    /// 以给定 workdir 构造，os 取 `std::env::consts::OS`。
    pub fn here(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: cwd.as_ref().to_path_buf(),
            os: std::env::consts::OS.to_string(),
            facts: Vec::new(),
        }
    }

    /// 追加一条环境事实（如 `shell=bash`）。
    pub fn with_fact(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push((key.into(), value.into()));
        self
    }

    /// 渲染注入 system 的环境块。
    pub fn environment_block(&self) -> String {
        let mut out = String::from("<environment>\n");
        out.push_str(&format!("cwd: {}\n", self.cwd.display()));
        out.push_str(&format!("os: {}\n", self.os));
        for (k, v) in &self.facts {
            out.push_str(&format!("{k}: {v}\n"));
        }
        out.push_str("</environment>");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_block_has_cwd_os_and_facts() {
        let block = EnvCore::here("/work")
            .with_fact("shell", "bash")
            .environment_block();
        assert!(block.contains("<environment>"));
        assert!(block.contains("cwd: /work"));
        assert!(block.contains("os: "));
        assert!(block.contains("shell: bash"));
        assert!(block.trim_end().ends_with("</environment>"));
    }
}
