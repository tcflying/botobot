//! open-tech：World 层——打开系统默认浏览器。
//!
//! Core 极薄（一个 [`open`] 函数）；保留独立 crate 是因为它是 World 关注点的可复用技术，
//! 与 `api-tech` 同列在 `bot-layer/` 下。

/// 在默认浏览器打开 URL。
pub fn open(url: &str) -> std::io::Result<()> {
    tracing::info!("opening browser: {url}");
    webbrowser::open(url).map_err(|e| std::io::Error::other(e.to_string()))
}
