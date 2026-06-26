//! bot-api：bot 层对外接口——把 Agent 事件流经 Hub 桥接到 WebSocket。
//!
//! bot = 壳 / 总机（决策④）：[`Hub`] 并发托管多个 session，只管生命周期/路由、零 cognition。
//!
//! 协议：
//!   浏览器 → 服务器: {"type":"user_message","text":"...","images":["data:...",...]}
//!                    {"type":"steer","text":"..."}
//!                    {"id":"...","session_id":"...","op":{"type":"user_message",...}}
//!                    {"type":"subscribe","session_id":"..."}
//!                    {"type":"ping"}
//!                    {"type":"subscribe_logs"}    ← 打开日志面板订阅
//!   服务器 → 浏览器: AgentEvent 序列（start/token/reasoning/tool_start/tool_end/done/error），
//!                    其中事件带 `session_id`，嵌套子 agent 的事件带非空 parent_id。
//!                    {"type":"log","time":"...","level":"info","target":"...","message":"..."}
//!
//! 静态 webui 资源**不在本 crate**——归 `webui-bin` 管。装配时调用方把 `webui_bin::webui_handler`
//! 当 axum fallback 挂上即可，本 crate 只负责协议与 session 生命周期。

use std::net::SocketAddr;
use std::sync::Arc;

pub mod cron;
pub mod cron_tools;
pub mod heartbeat;
pub mod logs;
pub mod protocol;
pub mod session_store;
pub mod transport;

mod hub;
mod session_driver;
pub mod team_runner;
mod team_tools;

pub use cron_tools::{CancelTaskTool, CronToolCtx, ListTasksTool, ScheduleTaskTool};
pub use hub::{BotEntry, DEFAULT_BOT_ID, Hub, HubConfig};
pub use session_store::{SessionStore, SessionStoreSubsessions};
pub use team_tools::{TeamDelegateTool, TeamMembersTool, TeamPostTool, TeamReadTool, TeamToolCtx};

use agent_loop::Agent;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::routing::get;

/// 装配 axum Router：仅协议层（`/ws`、`/api/health`），不挂静态资源 fallback。
/// 静态 webui 由调用方（[`webui_bin::webui_handler`] 之类）自行挂上。
/// 端口绑定与 serve 启动见 [`serve_with`]。
pub fn router(agent: Arc<Agent>) -> Router {
    router_with_config(
        agent,
        hub::HubConfig {
            store_root: Some(".bot".into()),
            ..hub::HubConfig::default()
        },
    )
}

/// 装配 Router，并共享外部传入的 `Switchboard`（§4.5）：让 agent 侧 team 工具与 Hub 用同一棵协作树。
pub fn router_with_switchboard(
    agent: Arc<Agent>,
    switchboard: Arc<std::sync::Mutex<team_core::Switchboard>>,
) -> Router {
    router_with_config(
        agent,
        hub::HubConfig {
            store_root: Some(".bot".into()),
            switchboard: Some(switchboard),
            ..hub::HubConfig::default()
        },
    )
}

/// 装配 Router，共享外部 `Switchboard` + 外部 `CronJobs`（§2.10）：让 agent 侧 cron 工具
/// 与 Hub 的 CronHandler 用同一份定时任务表（工具排的任务到点能被心跳触发）。
pub fn router_with_switchboard_and_cron(
    agent: Arc<Agent>,
    switchboard: Arc<std::sync::Mutex<team_core::Switchboard>>,
    cron_jobs: cron::CronJobs,
) -> Router {
    router_with_switchboard_cron_profiles(agent, switchboard, cron_jobs, Vec::new())
}

/// §5.7：同上，额外注入多 profile base agents（`profile_id → agent`），Hub 按 bot.profile 路由。
/// 空 `profile_agents` 等价于 [`router_with_switchboard_and_cron`]（单 agent，向后兼容）。
pub fn router_with_switchboard_cron_profiles(
    agent: Arc<Agent>,
    switchboard: Arc<std::sync::Mutex<team_core::Switchboard>>,
    cron_jobs: cron::CronJobs,
    profile_agents: Vec<(String, Arc<Agent>)>,
) -> Router {
    router_with_config(
        agent,
        hub::HubConfig {
            store_root: Some(".bot".into()),
            switchboard: Some(switchboard),
            cron_jobs: Some(cron_jobs),
            profile_agents,
            ..hub::HubConfig::default()
        },
    )
}

fn router_with_config(agent: Arc<Agent>, config: hub::HubConfig) -> Router {
    let hub = Hub::with_config(agent, config);
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/health", get(|| async { "ok" }))
        .merge(transport::http::router())
        .with_state(hub)
}

/// 在已绑定的 `listener` 上启动装配好的 `app`，阻塞到 server 退出。
/// 端口与后台任务句柄另见 [`serve`]。
pub async fn run(listener: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
    let addr: SocketAddr = listener.local_addr()?;
    tracing::info!(target: "botobot::ws", "listening on http://{addr}");
    axum::serve(listener, app).await
}

/// 旧便捷入口：装配 + 绑定 + 后台跑。固定/回退端口策略同 `run_inner`。
/// `port`：0 = 随机端口；非 0 = 固定端口（占用时回退到随机，避免直接失败）。
/// **不挂静态资源**——webui 调用方应在 `webui_bin::serve` 中处理。
pub async fn serve(
    agent: Arc<Agent>,
    port: u16,
) -> std::io::Result<(u16, tokio::task::JoinHandle<()>)> {
    let listener = bind_listener(port).await?;
    let port = listener.local_addr()?.port();
    let app = router(agent);
    let handle = tokio::spawn(async move {
        if let Err(e) = run(listener, app).await {
            tracing::error!(target: "botobot::ws", "server error: {e}");
        }
    });
    Ok((port, handle))
}

pub async fn bind_listener(want_port: u16) -> std::io::Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind(("127.0.0.1", want_port)).await {
        Ok(l) => Ok(l),
        Err(e) if want_port != 0 => {
            tracing::warn!(target: "botobot::ws", "端口 {want_port} 不可用（{e}），回退到随机端口");
            tokio::net::TcpListener::bind(("127.0.0.1", 0)).await
        }
        Err(e) => Err(e),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(hub): State<Hub>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| transport::ws::handle_socket(socket, hub))
}
