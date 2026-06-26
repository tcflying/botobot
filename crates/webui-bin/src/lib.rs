//! webui-bin lib：装配与 HTTP 路由构造器，供 `bots`（本地工作台）与 `server`
//! （远端只读市场服务端，§1.6 S4）两个 bin 复用。
//!
//! 纯 bin crate 无法让第二个 bin 复用第一个 bin 的私有项——故 §1.6 S4a 先把模块
//! 上提到 lib，`main.rs`/`server.rs` 变薄、各自 `use webui_bin::*`。

// §1.6 S4c：server_market 轻量核心不门控（server bin 经 --no-default-features 精简构建）。
pub mod server_market;

// §1.6 助手半边：chat 端点 + 端点配置在 `chat` feature 之后（full 含 chat）。
#[cfg(feature = "chat")]
pub mod config;
#[cfg(feature = "chat")]
pub mod server_chat;

// 重依赖（candle/team/mcp/browser…）的本地工作台路径门控在 `full`（默认开）之后。
#[cfg(feature = "full")]
pub mod bot;
#[cfg(feature = "full")]
pub mod gc;
#[cfg(feature = "full")]
pub mod open;
#[cfg(feature = "full")]
pub mod profile;
#[cfg(feature = "full")]
pub mod webui;
// §5.6 C10 stage2：浏览器投屏 WS 端点（需 `browser` feature + 真 Edge/Chrome）。
#[cfg(feature = "browser")]
pub mod browser_mirror;
