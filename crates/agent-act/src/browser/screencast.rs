//! §5.6 C10 浏览器投屏核心：把一路 tab 的画面变成「视频式」帧流。
//!
//! 经 `Page.startScreencast` 让 Chrome/Edge **主动推** `Page.screencastFrame`（每帧 base64 JPEG），
//! 收到即 `Page.screencastFrameAck` 回执做背压（不回执浏览器就不发下一帧），解码后广播给订阅者。
//! **订阅计数自动启停**：首个订阅者到来才开 screencast，最后一个走掉就停——没人看不耗资源。
//!
//! 装配层（WS 端点）订阅帧流 → 二进制 WS 推给 webui canvas。坐标换算所需的帧 metadata 随帧附带
//! （供未来双向控制阶段把 canvas 像素反算回页面 CSS 坐标）。
//!
//! **抄器官（§0 移植纪律）**：移植自前身 datoobot `browser-tech/src/screencast.rs`（Rust·MIT），
//! 重写接 botobot 的 [`CdpSender`]（dispatcher+sink 句柄）而非 datoobot 的 `CdpClient`。
//! feature-gated（`browser`）：默认构建不拉。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::connect::CdpSender;

/// 一帧的 metadata（`Page.screencastFrame` 附带），前端据此把 canvas 像素反算回页面 CSS 坐标。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMeta {
    pub device_width: f64,
    pub device_height: f64,
    pub page_scale_factor: f64,
    pub offset_top: f64,
    pub scroll_offset_x: f64,
    pub scroll_offset_y: f64,
}

impl FrameMeta {
    /// 从 `screencastFrame` 的 `metadata` 对象抽取（缺字段取 0，容错）。
    pub fn from_cdp(v: &Value) -> Self {
        let g = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        Self {
            device_width: g("deviceWidth"),
            device_height: g("deviceHeight"),
            page_scale_factor: g("pageScaleFactor"),
            offset_top: g("offsetTop"),
            scroll_offset_x: g("scrollOffsetX"),
            scroll_offset_y: g("scrollOffsetY"),
        }
    }
}

/// 一帧画面：JPEG 字节 + 坐标换算所需 metadata。
#[derive(Debug, Clone)]
pub struct Frame {
    pub jpeg: Vec<u8>,
    pub meta: FrameMeta,
}

/// 帧格式与质量预算（headless 下省带宽）。
const FRAME_FORMAT: &str = "jpeg";
const FRAME_QUALITY: i32 = 60;
const MAX_WIDTH: i32 = 2560;
const MAX_HEIGHT: i32 = 1440;

/// 一路 tab 的 screencast：帧广播 + 订阅计数 + 后台抓帧任务。
pub struct ScreencastCore {
    sender: CdpSender,
    session_id: Option<String>,
    frame_tx: broadcast::Sender<Arc<Frame>>,
    count: AtomicUsize,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ScreencastCore {
    /// 建一个 screencast 核心（尚未开始推流）。`session_id`=flat-session 目标（单页直连时 None）。
    pub fn new(sender: CdpSender, session_id: Option<String>) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(8);
        Arc::new(Self {
            sender,
            session_id,
            frame_tx,
            count: AtomicUsize::new(0),
            task: Mutex::new(None),
        })
    }

    /// 订阅帧流。首个订阅者触发 `startScreencast` 并起后台抓帧任务。
    pub fn subscribe(self: &Arc<Self>) -> ScreencastGuard {
        let rx = self.frame_tx.subscribe();
        if self.count.fetch_add(1, Ordering::SeqCst) == 0 {
            self.start();
        }
        ScreencastGuard {
            core: self.clone(),
            rx,
        }
    }

    /// 起后台任务：enable Page → startScreencast → 循环收帧（先 ack 再广播）。
    fn start(self: &Arc<Self>) {
        let me = self.clone();
        let handle = tokio::spawn(async move {
            let sid = me.session_id.as_deref();
            let _ = me.sender.send("Page.enable", json!({}), sid).await;
            if let Err(e) = me
                .sender
                .send(
                    "Page.startScreencast",
                    json!({
                        "format": FRAME_FORMAT,
                        "quality": FRAME_QUALITY,
                        "maxWidth": MAX_WIDTH,
                        "maxHeight": MAX_HEIGHT,
                        "everyNthFrame": 1,
                    }),
                    sid,
                )
                .await
            {
                tracing::warn!(target: "botobot::browser", "screencast start failed: {e}");
                return;
            }
            let mut events = me.sender.subscribe();
            loop {
                match events.recv().await {
                    Ok(ev)
                        if ev.method == "Page.screencastFrame"
                            && ev.session_id.as_deref() == me.session_id.as_deref() =>
                    {
                        // 先 ack：不回执浏览器不发下一帧——天然背压。
                        if let Some(ack) = ev.params.get("sessionId").and_then(|v| v.as_i64()) {
                            let _ = me
                                .sender
                                .send(
                                    "Page.screencastFrameAck",
                                    json!({ "sessionId": ack }),
                                    me.session_id.as_deref(),
                                )
                                .await;
                        }
                        if let Some(b64) = ev.params.get("data").and_then(|v| v.as_str()) {
                            if let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(b64) {
                                let meta = ev
                                    .params
                                    .get("metadata")
                                    .map(FrameMeta::from_cdp)
                                    .unwrap_or_default();
                                let _ = me.frame_tx.send(Arc::new(Frame { jpeg, meta }));
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    _ => {} // Lagged/其它事件忽略
                }
            }
        });
        *self.task.lock().unwrap() = Some(handle);
    }

    /// 订阅者退订：计数归零则停后台任务 + `stopScreencast`。
    fn release(self: &Arc<Self>) {
        if self.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Some(h) = self.task.lock().unwrap().take() {
                h.abort();
            }
            let me = self.clone();
            tokio::spawn(async move {
                let _ = me
                    .sender
                    .send("Page.stopScreencast", json!({}), me.session_id.as_deref())
                    .await;
            });
        }
    }
}

/// 帧流订阅守卫：持接收端，Drop 时递减订阅计数（可能触发停推流）。
pub struct ScreencastGuard {
    core: Arc<ScreencastCore>,
    rx: broadcast::Receiver<Arc<Frame>>,
}

impl ScreencastGuard {
    /// 收下一帧。滞后（订阅者太慢）时跳过丢失帧取最新；推流结束返回 `None`。
    pub async fn recv(&mut self) -> Option<Arc<Frame>> {
        loop {
            match self.rx.recv().await {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for ScreencastGuard {
    fn drop(&mut self) {
        self.core.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_meta_parses_cdp_metadata_with_defaults() {
        let v = json!({
            "deviceWidth": 1280.0, "deviceHeight": 720.0,
            "pageScaleFactor": 1.0, "scrollOffsetY": 40.0,
        });
        let m = FrameMeta::from_cdp(&v);
        assert_eq!(m.device_width, 1280.0);
        assert_eq!(m.device_height, 720.0);
        assert_eq!(m.scroll_offset_y, 40.0);
        // 缺失字段容错为 0。
        assert_eq!(m.scroll_offset_x, 0.0);
        assert_eq!(m.offset_top, 0.0);
    }
}
