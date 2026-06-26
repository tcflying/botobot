//! §4.6 步④：把浏览器能力包成 coder profile 可用的 [`Tool`]（browser_navigate / browser_snapshot
//! / browser_click）。
//!
//! **生命周期拍板（2026-06-25，按倾向自定）**：**每 agent 一个 [`BrowserHandle`]，首次 browser 工具
//! 调用时懒启动 Chrome**；snapshot 的 RefMap 缓存在 handle 里供随后 click 用（ref 失效=页面已变，
//! 提示重新 snapshot）。简单够用；多标签/多会话隔离属后续。
//!
//! **feature-gated**（`browser`）。**全链运行验证待真 Chrome**——工具定义/schema/参数解析编译验证，
//! 实际驱动浏览器需机器装 Chrome（`BOTOBOT_CHROME` 可指定）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use base_types::{Tool, ToolResult, ToolTier};

use super::page::Browser;
use super::snapshot::AxSnapshot;

struct BrowserState {
    browser: Option<Browser>,
    last_snapshot: Option<AxSnapshot>,
    port: u16,
}

/// 共享浏览器句柄（懒启动 + 缓存最近 snapshot）。三个工具共持一份。
#[derive(Clone)]
pub struct BrowserHandle(Arc<Mutex<BrowserState>>);

impl BrowserHandle {
    pub fn new(port: u16) -> Self {
        Self(Arc::new(Mutex::new(BrowserState {
            browser: None,
            last_snapshot: None,
            port,
        })))
    }
}

/// 取/懒启动 Browser，对其执行 `f`（持锁串行，简单安全）。
async fn with_browser<R>(
    h: &BrowserHandle,
    f: impl AsyncFnOnce(&Browser, &mut Option<AxSnapshot>) -> Result<R, String>,
) -> Result<R, String> {
    let mut st = h.0.lock().await;
    if st.browser.is_none() {
        let b = Browser::launch_headless(st.port).await?;
        st.browser = Some(b);
    }
    // 分别可变借用：先取出 browser 引用 + snapshot 槽。
    let BrowserState {
        browser,
        last_snapshot,
        ..
    } = &mut *st;
    let b = browser.as_ref().unwrap();
    f(b, last_snapshot).await
}

/// `browser_navigate(url)` — 打开/导航到 URL（Exec：会发起外部请求）。
pub struct BrowserNavigateTool(pub BrowserHandle);
#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Navigate the headless browser to a URL. Then call browser_snapshot to read the page."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if url.is_empty() {
            anyhow::bail!("browser_navigate: missing 'url'");
        }
        with_browser(&self.0, async move |b, _snap| b.navigate(&url, None).await)
            .await
            .map(|frame| json!({ "ok": true, "frameId": frame }))
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// `browser_snapshot()` — 取当前页 AX 文本快照（Read：只读观察）。缓存 RefMap 供 click。
pub struct BrowserSnapshotTool(pub BrowserHandle);
#[async_trait]
impl Tool for BrowserSnapshotTool {
    fn name(&self) -> &str {
        "browser_snapshot"
    }
    fn description(&self) -> &str {
        "Read the current page as an accessibility-tree text outline. Interactive elements are \
         tagged [ref=eN]; pass a ref to browser_click."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> ToolResult {
        with_browser(&self.0, async move |b, snap| {
            let s = b.snapshot(None).await?;
            let text = s.text.clone();
            *snap = Some(s);
            Ok(text)
        })
        .await
        .map(|text| json!({ "snapshot": text }))
        .map_err(|e| anyhow::anyhow!(e))
    }
}

/// `browser_click(ref)` — 点击最近 snapshot 里的某 ref（Exec：改变页面状态）。
pub struct BrowserClickTool(pub BrowserHandle);
#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        "browser_click"
    }
    fn description(&self) -> &str {
        "Click an interactive element by its [ref=eN] from the latest browser_snapshot."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "ref": { "type": "string" } }, "required": ["ref"] })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let r = args
            .get("ref")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if r.is_empty() {
            anyhow::bail!("browser_click: missing 'ref'");
        }
        with_browser(&self.0, async move |b, snap| {
            let s = snap
                .as_ref()
                .ok_or_else(|| "先 browser_snapshot 再 click".to_string())?;
            b.click_ref(s, &r, None).await
        })
        .await
        .map(|_| json!({ "ok": true }))
        .map_err(|e| anyhow::anyhow!(e))
    }
}

/// 造三个共享同一 [`BrowserHandle`] 的浏览器工具（供装配层注册进 coder profile）。
pub fn browser_tools(port: u16) -> Vec<Arc<dyn Tool>> {
    let h = BrowserHandle::new(port);
    vec![
        Arc::new(BrowserNavigateTool(h.clone())),
        Arc::new(BrowserSnapshotTool(h.clone())),
        Arc::new(BrowserClickTool(h)),
    ]
}
