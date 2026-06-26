//! §4.6 浏览器能力（operator 方向）：把外部 `agent-browser`（OSS Rust CDP 引擎）的**自包含器官**
//! 抄进 botobot 自有 trait 接缝——不 fork 其 daemon/CLI/IPC/云 providers（§0 抄器官不 fork 身体）。
//!
//! **进度（分步施工）**：
//! - 步① `cdp`：CDP 协议**调度核心**——命令 id 分配 + `id↔oneshot` 请求/响应配对 + 无 id 消息作
//!   **事件 broadcast**。**transport-agnostic**（喂入站文本即可），不绑具体 WebSocket，故现可单测。
//!   真 WS 连接（tokio-tungstenite）+ Chrome 启动 = 步①下半（后续）。
//! - 步② manager（Target attach / navigate）+ snapshot（AX 树）= 后续。
//! - 步③ element/RefMap + interaction（click/fill）= 后续。
//! - 步④ tools.rs 包 TypedTool 注册进 coder profile = 后续。
//!
//! 落点纪律：本模块 `crates/agent-act/src/browser/`（严禁 `world-*`/`-tech`/`-layer`，不发独立包）。

pub mod cdp;

// 步③上半：AX 树渲染（纯解析，零网络依赖，不 feature-gate；取树才需 browser+Chrome）。
pub mod snapshot;

// 步③下半：交互原语（quad_center 纯函数恒在；click/type 的 CDP 发送 feature-gated）。
pub mod interaction;

// 步①下半：真 WS 连接 + Chrome CDP 端点（需 `browser` feature + 运行时真 Chrome）。
#[cfg(feature = "browser")]
pub mod connect;

// 步②上半：Chrome 启动 + CDP 端点发现（需 `browser` feature；spawn 待运行时真 Chrome）。
#[cfg(feature = "browser")]
pub mod launch;

// 步②下半：Browser（Chrome 子进程 + CDP 连接）+ navigate（需 `browser` feature + 真 Chrome）。
#[cfg(feature = "browser")]
pub mod page;

// 步④：浏览器工具（navigate/snapshot/click）+ 懒启动生命周期（需 `browser` feature + 真 Chrome）。
#[cfg(feature = "browser")]
pub mod tools;

// §5.6 C10 投屏：Page.startScreencast 帧流核心（需 `browser` feature + 真 Chrome/Edge）。
#[cfg(feature = "browser")]
pub mod screencast;
