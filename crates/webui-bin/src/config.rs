//! 端点配置（装配层的事，不进可复用的 llm-tech）。
//!
//! 复用 datoobot llm-tech 的 TOML 子集：`[providers.X]` + `[[llms]]`，但**单端点、无 failover**。
//! 解析顺序：`config.toml` 的第一个 `[[llms]]` → 内置 dev 默认；之后 env 变量逐字段覆盖。
//!
//! 内置默认指向本地 unsloth 的多模态 Qwen3.6（dev 便利，非生产）。

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    /// None=服务端默认；Some(false)=关闭 Qwen thinking（默认关，提速）。
    pub thinking: Option<bool>,
    /// 真实模型窗口 tokens。三级解析 env(BOTOBOT_CONTEXT) > config > 默认 32768。
    pub context_window: usize,
}

impl Default for Endpoint {
    fn default() -> Self {
        // dev 便利默认：本地 unsloth Qwen3.6（与你给的 [providers.unsloth] 一致）。
        Self {
            base_url: "http://localhost:8888/v1".into(),
            api_key: "sk-unsloth-8541d4454c0ae0bb178b94b82cfcaee3".into(),
            model: "Qwen3.6".into(),
            temperature: 0.7,
            thinking: Some(false),
            context_window: 32768,
        }
    }
}

impl Endpoint {
    /// config.toml（若有）→ 内置默认，然后用 env 覆盖任意字段。
    pub fn resolve() -> Self {
        let base = Self::from_config_file().unwrap_or_default();
        base.with_env_overrides()
    }

    fn from_config_file() -> Option<Self> {
        let text = std::fs::read_to_string("config.toml").ok()?;
        let cfg: ConfigFile = toml::from_str(&text).ok()?;
        let llm = cfg.llms.first()?;
        let provider = cfg.providers.get(&llm.provider)?;
        Some(Self {
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            model: llm.model_id.clone(),
            temperature: llm.temperature.unwrap_or(0.7),
            thinking: llm.enable_thinking,
            context_window: llm.context_window.unwrap_or(32768),
        })
    }

    fn with_env_overrides(self) -> Self {
        // 从进程全局 env 读取，再交给纯函数 apply_overrides 应用。
        // 读取与应用分离，使测试可直接喂参数而不触碰 env（避免并行测试竞态）。
        let env = EnvOverrides {
            base_url: std::env::var("OPENAI_BASE_URL").ok(),
            api_key: std::env::var("OPENAI_API_KEY").ok(),
            model: std::env::var("BOTOBOT_MODEL").ok(),
            thinking: std::env::var("BOTOBOT_THINKING").ok(),
            context: std::env::var("BOTOBOT_CONTEXT").ok(),
        };
        self.apply_overrides(&env)
    }

    /// 纯函数：把（来自 env 或测试构造的）覆盖值应用到 self，不读任何进程全局。
    fn apply_overrides(mut self, env: &EnvOverrides) -> Self {
        if let Some(v) = &env.base_url {
            self.base_url = v.clone();
        }
        if let Some(v) = &env.api_key {
            self.api_key = v.clone();
        }
        if let Some(v) = &env.model {
            self.model = v.clone();
        }
        if let Some(v) = &env.thinking {
            self.thinking = Some(matches!(v.as_str(), "1" | "true" | "on"));
        }
        if let Some(raw) = &env.context {
            match raw.trim().parse::<usize>() {
                Ok(w) if w > 0 => self.context_window = w,
                _ => eprintln!(
                    "(warn: BOTOBOT_CONTEXT={raw:?} 非法（需正整数 token 数），忽略，沿用 {})",
                    self.context_window
                ),
            }
        }
        self
    }
}

/// 各字段的覆盖值（None=不覆盖）。由 env 读取或测试直接构造。
#[derive(Default)]
struct EnvOverrides {
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    context: Option<String>,
}

impl Endpoint {
    /// §5.7 真异构多模型：通用 bot 的独立端点（None=与编程 bot 共用同一模型，默认）。
    /// 仅当用户设了 `BOTOBOT_GENERAL_MODEL` 才激活——以编程 bot 端点为底，逐字段用
    /// `BOTOBOT_GENERAL_{MODEL,BASE_URL,API_KEY,THINKING}` 覆盖（base_url/api_key 不给则
    /// 沿用同一服务，常见用法=同一本地服务上换个模型名）。装配层据此给 general agent `with_llm`。
    pub fn resolve_general(&self) -> Option<Self> {
        let ov = GeneralOverrides {
            base_url: std::env::var("BOTOBOT_GENERAL_BASE_URL").ok(),
            api_key: std::env::var("BOTOBOT_GENERAL_API_KEY").ok(),
            model: std::env::var("BOTOBOT_GENERAL_MODEL").ok(),
            thinking: std::env::var("BOTOBOT_GENERAL_THINKING").ok(),
        };
        self.apply_general_overrides(&ov)
    }

    /// 纯函数：通用端点派生。`model` 为 None（即未设 `BOTOBOT_GENERAL_MODEL`）→ 返回 None
    /// （不派生、共用编程端点）。否则克隆 self 并逐字段覆盖。
    fn apply_general_overrides(&self, ov: &GeneralOverrides) -> Option<Self> {
        let model = ov.model.as_ref()?.trim().to_string();
        if model.is_empty() {
            return None;
        }
        let mut g = self.clone();
        g.model = model;
        if let Some(v) = &ov.base_url {
            g.base_url = v.clone();
        }
        if let Some(v) = &ov.api_key {
            g.api_key = v.clone();
        }
        if let Some(v) = &ov.thinking {
            g.thinking = Some(matches!(v.as_str(), "1" | "true" | "on"));
        }
        Some(g)
    }
}

/// 通用 bot 端点的覆盖值（None=不覆盖；model=None 表示不派生独立端点）。
#[derive(Default)]
struct GeneralOverrides {
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
}

pub fn resolve_workdir() -> Option<PathBuf> {
    let mut workdir = ConfigFile::from_config_file().and_then(|cfg| cfg.workdir.map(PathBuf::from));
    if let Ok(v) = std::env::var("BOTOBOT_WORKDIR") {
        workdir = Some(PathBuf::from(v));
    }
    workdir
}

/// §2.9③：把资产（`skills`/`books`）的运行期家收敛到 `.bot/<name>`，与 sessions/memory/artifacts
/// 同处。返回该运行期目录。**首次播种**：若 `.bot/<name>` 不存在但仓库基线 `./<name>`（git 跟踪）
/// 存在，则递归拷入（基线即分发/出厂源；删 `.bot/<name>` 可重播种）。两者都不存在时返回 `.bot/<name>`
/// （load_* 对空目录优雅降级）。播种失败仅告警并回退仓库基线，不致命。
pub fn seed_bot_assets(name: &str) -> PathBuf {
    let dest = PathBuf::from(".bot").join(name);
    let src = PathBuf::from(name);
    if !dest.exists() && src.is_dir() {
        if let Err(err) = copy_dir_recursive(&src, &dest) {
            tracing::warn!("seed {name} into .bot failed: {err}（回退仓库基线 ./{name}）");
            return src; // 播种失败 → 直接用仓库基线，功能不受损
        }
        tracing::info!("seeded ./{name} → .bot/{name}（运行期家，仓库为基线）");
    }
    dest
}

/// 递归拷贝目录（仅文件与子目录；跳过符号链接环不在 skills/books 场景内）。
fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

impl ConfigFile {
    fn from_config_file() -> Option<Self> {
        let text = std::fs::read_to_string("config.toml").ok()?;
        toml::from_str(&text).ok()
    }
}

#[derive(Deserialize)]
struct Provider {
    base_url: String,
    #[serde(default)]
    api_key: String,
}

#[derive(Deserialize)]
struct Llm {
    provider: String,
    model_id: String,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    enable_thinking: Option<bool>,
    #[serde(default)]
    context_window: Option<usize>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    providers: HashMap<String, Provider>,
    #[serde(default)]
    llms: Vec<Llm>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_default_context_window_is_32768() {
        assert_eq!(Endpoint::default().context_window, 32768);
    }

    #[test]
    fn env_context_window_overrides_config_value() {
        // 模拟 config 已给 8192，env 给 16384 → 取 env。
        // 不触碰进程全局 env：直接喂 apply_overrides，故可与其他测试并行。
        let base = Endpoint {
            context_window: 8192,
            ..Endpoint::default()
        };
        let env = EnvOverrides {
            context: Some("16384".into()),
            ..Default::default()
        };
        assert_eq!(base.apply_overrides(&env).context_window, 16384);
    }

    #[test]
    fn config_window_used_when_no_env() {
        let base = Endpoint {
            context_window: 8192,
            ..Endpoint::default()
        };
        // 无任何覆盖 → 沿用 config 值。
        assert_eq!(
            base.apply_overrides(&EnvOverrides::default())
                .context_window,
            8192
        );
    }

    #[test]
    fn illegal_context_window_is_ignored() {
        let base = Endpoint {
            context_window: 8192,
            ..Endpoint::default()
        };
        let env = EnvOverrides {
            context: Some("not-a-number".into()),
            ..Default::default()
        };
        assert_eq!(base.apply_overrides(&env).context_window, 8192);
    }

    // §5.7 真异构多模型：未设 general model → 不派生（None，共用编程端点）。
    #[test]
    fn general_endpoint_none_without_model() {
        let base = Endpoint::default();
        assert!(base.apply_general_overrides(&GeneralOverrides::default()).is_none());
        // 空白 model 也视为未设。
        let ov = GeneralOverrides {
            model: Some("  ".into()),
            ..Default::default()
        };
        assert!(base.apply_general_overrides(&ov).is_none());
    }

    // 设了 general model → 派生独立端点：换模型名，base_url/api_key 默认沿用编程端点。
    #[test]
    fn general_endpoint_inherits_endpoint_overriding_model() {
        let base = Endpoint {
            base_url: "http://local:8888/v1".into(),
            api_key: "KEY".into(),
            model: "Coder".into(),
            ..Endpoint::default()
        };
        let ov = GeneralOverrides {
            model: Some("Chat".into()),
            ..Default::default()
        };
        let g = base.apply_general_overrides(&ov).expect("应派生");
        assert_eq!(g.model, "Chat", "模型应换");
        assert_eq!(g.base_url, "http://local:8888/v1", "base_url 默认沿用");
        assert_eq!(g.api_key, "KEY", "api_key 默认沿用");
        // 显式 base_url 覆盖。
        let ov2 = GeneralOverrides {
            model: Some("Chat".into()),
            base_url: Some("http://other:9/v1".into()),
            ..Default::default()
        };
        assert_eq!(
            base.apply_general_overrides(&ov2).unwrap().base_url,
            "http://other:9/v1"
        );
    }
}
