//! §1.6 S4：远端 `server.exe` 只读市场服务端的**轻量核心**——只依赖 `agent-act`（skill）
//! + axum + tokio，**不碰** agent-loop/candle/bot-api 等重依赖（S4c 依赖裁剪的落点）。
//!
//! 本模块**不受 `full` feature 门控**，故 `server` bin 可经 `--no-default-features` 构建出
//! 不含 ML/agent 栈的精简二进制。`bots`（本地工作台）的 serve 路径复用这里的 `Capabilities`
//! 与 `package_response`。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower_http::services::{ServeDir, ServeFile};

use agent_act::skill::SkillStore;

/// §1.6 `/api/capabilities`：能力自描述——前端（含 weui3 远端 SPA）按此显隐 UI，不硬编码判环境。
#[derive(Debug, Serialize)]
pub struct Capabilities {
    /// 是否远端只读站点。`bots.exe`=false（本地全权工作台）。
    pub remote: bool,
    pub chat: bool,
    pub skills: bool,
    /// 能否安装/更新/删除本地 overlay skill（市场客户端能力）。
    pub skill_install: bool,
    pub market: bool,
    pub books: bool,
    pub memory: bool,
}

impl Capabilities {
    /// 本地 `bots.exe` 工作台：全能力。仅 `full`（bots）路径使用。
    #[allow(dead_code)]
    pub fn local_workstation() -> Self {
        Self {
            remote: false,
            chat: true,
            skills: true,
            skill_install: true,
            market: true,
            books: true,
            memory: true,
        }
    }

    /// 远端 `server.exe` 只读市场服务端：收窄集——可浏览 skill/book，但不装/不市场客户端/无记忆。
    pub fn remote_readonly() -> Self {
        Self {
            remote: true,
            chat: false,
            skills: true,
            skill_install: false,
            market: false,
            books: true,
            memory: false,
        }
    }
}

/// §1.6 S3/S4：包下载响应（原始 SKILL.md 或 404/400）。bots 的 skills_api_router 与
/// server_router 共用。
pub fn package_response(store: &SkillStore, id: &str) -> Response {
    match store.package_md(id) {
        Ok(Some(md)) => ([("content-type", "text/markdown; charset=utf-8")], md).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no such skill: {id}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// §1.6 S4b：远端 `server.exe` 只读市场路由——仅浏览/下载（无装删、无 chat、无记忆）。
/// capabilities 上报 `remote_readonly()` 收窄集；catalog = 已装清单别名；package = 原始 SKILL.md。
/// remote SPA（weui3 改造）尚未接入，fallback 暂返回占位说明。
/// `spa_dir`：Some + 存在 → 把该目录（weui3 改造后 `vite build` 的 dist）当 SPA 静态托管，
/// 未命中路由回退 `index.html`（hash 路由 SPA）；None/不存在 → 回退占位说明文本。
pub fn server_router(skill_store: Arc<SkillStore>, spa_dir: Option<PathBuf>) -> Router {
    let list_store = skill_store.clone();
    let cat_store = skill_store.clone();
    let pkg_store = skill_store;
    let base = Router::new()
        .route(
            "/api/capabilities",
            get(|| async {
                // chat 能力随 `chat` feature 开关（server 是否带对话端点）。
                let mut c = Capabilities::remote_readonly();
                c.chat = cfg!(feature = "chat");
                Json(c)
            }),
        )
        .route(
            "/api/skills",
            get(move || {
                let s = list_store.clone();
                async move { Json(s.list_installed()) }
            }),
        )
        .route(
            "/api/catalog",
            get(move || {
                let s = cat_store.clone();
                async move { Json(s.list_installed()) }
            }),
        )
        .route(
            "/api/skills/:id/package",
            get(move |Path(id): Path<String>| {
                let s = pkg_store.clone();
                async move { package_response(&s, &id) }
            }),
        );

    // SPA 静态托管：dir 存在则 ServeDir + index.html 回退（hash 路由）；否则占位说明。
    match spa_dir.filter(|d| d.is_dir()) {
        Some(dir) => {
            let index = dir.join("index.html");
            base.fallback_service(ServeDir::new(dir).not_found_service(ServeFile::new(index)))
        }
        None => base.fallback(|| async {
            (
                [("content-type", "text/plain; charset=utf-8")],
                "botobot server.exe (§1.6 远端只读市场服务端)\n\
                 remote SPA 未接入（设 BOTOBOT_SPA_DIR 指向 weui3 改造后的 dist 即可托管）；\n\
                 API：GET /api/capabilities · /api/skills · /api/catalog · /api/skills/:id/package\n",
            )
        }),
    }
}

/// §1.6 S4b：启动远端市场服务端（绑定端口 + serve，阻塞到退出）。
/// `port`：0=随机；非 0 占用时回退随机。直接用 tokio+axum，不引 bot-api（S4c 裁依赖）。
pub async fn run_market_server(skills_root: &str, port: u16) -> anyhow::Result<()> {
    let skill_store = Arc::new(SkillStore::new(skills_root));
    // BOTOBOT_SPA_DIR 指向 weui3 改造后 `vite build` 的 dist（远端 SPA）；缺省 `webui-dist`。
    let spa_dir = std::env::var("BOTOBOT_SPA_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("webui-dist")));
    #[allow(unused_mut)]
    let mut app = server_router(skill_store, spa_dir);
    // §1.6 助手半边：带 chat feature 时挂上无状态对话端点（/api/chat SSE）。
    #[cfg(feature = "chat")]
    {
        let agent = crate::server_chat::build_chat_agent();
        app = app.merge(crate::server_chat::chat_router(agent));
        eprintln!("botobot server: chat 端点已启用 (/api/chat)");
    }
    let listener = bind_listener(port).await?;
    let addr = listener.local_addr()?;
    eprintln!(
        "botobot server (market{}) ready at http://{addr}",
        if cfg!(feature = "chat") { "+chat" } else { "" }
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn bind_listener(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => Ok(l),
        Err(e) if port != 0 => {
            tracing::warn!(target: "botobot::server", "端口 {port} 不可用（{e}），回退随机端口");
            tokio::net::TcpListener::bind(("127.0.0.1", 0)).await
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_act::market::MarketClient;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn local_capabilities_are_full_workstation() {
        let c = Capabilities::local_workstation();
        assert!(!c.remote, "bots.exe 不是远端只读站");
        assert!(c.chat && c.skills && c.skill_install && c.market && c.books && c.memory);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["remote"], false);
        assert_eq!(v["skill_install"], true);
    }

    /// §1.6 S4b：server_router 只读市场面——capabilities remote=true、catalog/package 可读、
    /// 装删路由不挂（不暴露写能力）。
    #[tokio::test]
    async fn server_router_is_readonly_market_surface() {
        let root = std::env::temp_dir().join(format!(
            "botobot-server-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Arc::new(SkillStore::new(&root));
        store
            .install_overlay("deploy", "---\ndescription: ship\n---\nSteps.\n")
            .unwrap();

        let app = server_router(store.clone(), None);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let http = reqwest::Client::new();
        let caps: serde_json::Value = http
            .get(format!("{base}/api/capabilities"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(caps["remote"], true);
        assert_eq!(caps["skill_install"], false, "市场服务端不暴露装删（只读）");
        // chat 随 `chat` feature：带对话端点构建时为 true，纯市场构建为 false。
        assert_eq!(caps["chat"], cfg!(feature = "chat"));

        let client = MarketClient::new();
        assert!(
            client
                .fetch_catalog(&base)
                .await
                .unwrap()
                .iter()
                .any(|r| r.id == "deploy")
        );
        assert!(
            client
                .fetch_package(&base, "deploy")
                .await
                .unwrap()
                .contains("ship")
        );

        // 写路由不挂：install 端点未注册 → 落 SPA fallback（占位文本），而非真的安装。
        let resp = http
            .post(format!("{base}/api/skills/install"))
            .json(&serde_json::json!({ "id": "x", "skill_md": "y" }))
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("server.exe") && body.contains("remote SPA"),
            "install 应落 fallback 占位（server 不暴露写端点），got: {body}"
        );
        assert!(store.list_installed().iter().all(|d| d.id != "x"));

        server.abort();
        let _ = std::fs::remove_dir_all(root);
    }
}
