//! botobot `server` bin（§1.6 S4b）：远端轻量**只读市场服务端**。
//!
//! 与默认 `bots`（本地全权工作台）分立：server 只挂只读市场面（catalog/package/capabilities），
//! 不开 chat/write/exec/memory——可公开部署，给市场客户端（bots.exe）拉包。
//!
//! 用法：
//!   server                 # 启动市场服务端（端口默认 8788，BOTOBOT_SERVER_PORT 覆盖）
//!
//! ⚠️ 当前仍与 bots 同 crate、未做 `--no-default-features` 依赖裁剪（S4c 待办）；
//!    remote SPA（weui3 改造）也未接入（fallback 占位）。

use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let filter = std::env::var("BOTOBOT_LOG")
        .ok()
        .map(|l| format!("botobot={l}"))
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "botobot=info".into());
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(filter))
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();

    let port: u16 = std::env::var("BOTOBOT_SERVER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8788);
    webui_bin::server_market::run_market_server("skills", port).await
}
