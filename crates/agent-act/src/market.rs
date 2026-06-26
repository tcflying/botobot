//! §1.6 S3 客户端侧：skill 市场源配置 + 拉取远端 catalog / 下载包。
//!
//! 角色（对齐 §1.6 拍板）：本地 `bots.exe` 是**市场客户端**——配置受信的远端源（任一跑着
//! `bots`/`server` 的实例都暴露 `GET /api/skills` + `GET /api/skills/:id/package`），从源拉
//! catalog 展示、选包下载，落进本地磁盘 overlay（`SkillStore::install_overlay`）。
//!
//! 信任模型：用户配置过的源即视为可信（§1.6 拍板）——配置源是一次有安全含义的信任决定，
//! 装来的 skill 其 scripts 按正常 exec policy 管辖，本层不额外沙箱。
//!
//! 本模块只管「源清单持久化 + HTTP 拉取」；`update_available` 比对与 install 编排由
//! 调用方（webui-bin 路由）结合 `SkillStore` 完成。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一个市场源 = 一个可信远端基址（如 `http://10.0.0.2:8787`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSource {
    pub name: String,
    pub url: String,
}

/// 文件落盘的源清单（JSON 数组）。轻量、原子写。
#[derive(Debug, Clone)]
pub struct MarketSources {
    path: PathBuf,
}

impl MarketSources {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// 读全部源（文件不存在 → 空）。
    pub fn list(&self) -> Vec<MarketSource> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    /// 添加/更新一个源（按 `name` 去重覆盖），原子写回。返回去重后的全量清单。
    pub fn add(&self, source: MarketSource) -> anyhow::Result<Vec<MarketSource>> {
        let mut all = self.list();
        all.retain(|s| s.name != source.name);
        all.push(source);
        self.save(&all)?;
        Ok(all)
    }

    /// 按 name 删除一个源，返回是否命中。
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut all = self.list();
        let before = all.len();
        all.retain(|s| s.name != name);
        let hit = all.len() != before;
        if hit {
            self.save(&all)?;
        }
        Ok(hit)
    }

    fn save(&self, all: &[MarketSource]) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(all)?;
        // 原子写：临时文件 + rename。
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// 远端 catalog 的一项（= server 侧 `SkillDescriptor` 的客户端反序列化视图）。
/// `kind` 用 `String`（server 侧是 `&'static str`，反序列化需 owned）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSkill {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

/// 拉取远端市场的 HTTP 客户端。
#[derive(Debug, Clone)]
pub struct MarketClient {
    http: reqwest::Client,
}

impl Default for MarketClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketClient {
    pub fn new() -> Self {
        // 总超时兜底：市场服务慢/挂死时不卡住安装请求。
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http }
    }

    /// `GET <base>/api/skills` → 远端可装 skill 列表。
    pub async fn fetch_catalog(&self, base: &str) -> anyhow::Result<Vec<RemoteSkill>> {
        let url = format!("{}/api/skills", base.trim_end_matches('/'));
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }

    /// `GET <base>/api/skills/:id/package` → 原始 SKILL.md 正文（供 install_overlay）。
    pub async fn fetch_package(&self, base: &str, id: &str) -> anyhow::Result<String> {
        let url = format!("{}/api/skills/{id}/package", base.trim_end_matches('/'));
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        Ok(resp.text().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "botobot-market-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn sources_add_dedup_remove_roundtrip() {
        let path = temp_path();
        let src = MarketSources::new(&path);
        assert!(src.list().is_empty(), "无文件时空清单");

        src.add(MarketSource {
            name: "home".into(),
            url: "http://a".into(),
        })
        .unwrap();
        // 同名覆盖（不重复）。
        let all = src
            .add(MarketSource {
                name: "home".into(),
                url: "http://b".into(),
            })
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].url, "http://b");

        src.add(MarketSource {
            name: "work".into(),
            url: "http://c".into(),
        })
        .unwrap();
        assert_eq!(src.list().len(), 2);

        assert!(src.remove("home").unwrap());
        assert!(!src.remove("home").unwrap());
        assert_eq!(src.list().len(), 1);
        assert_eq!(src.list()[0].name, "work");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_skill_deserializes_with_omitted_fields() {
        // server 侧 version 缺省时省略字段——客户端应容忍。
        let json = r#"{"id":"greet","source":"overlay","kind":"skill","hidden":false}"#;
        let rs: RemoteSkill = serde_json::from_str(json).unwrap();
        assert_eq!(rs.id, "greet");
        assert_eq!(rs.version, None);
        assert_eq!(rs.description, None);
        assert_eq!(rs.source.as_deref(), Some("overlay"));
    }
}
