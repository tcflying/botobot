//! §5.6 C10 stage2：浏览器投屏 WS 端点。装配「启 Edge headless → 连页 target → screencast →
//! 二进制帧推 WS」，前端 canvas 镜像订阅。**feature `browser`**（默认构建不含）。
//!
//! 生命周期：每个 `/browser-ws` 连接独占一个 Edge headless 实例（`kill_on_drop`，断开即杀，
//! 不泄漏孤儿）。客户端文本消息 `{"type":"navigate","url":...}` 控制导航；服务端二进制帧 = JPEG。

use std::sync::Arc;

use agent_act::browser::connect::CdpConnection;
use agent_act::browser::launch::launch;
use agent_act::browser::screencast::ScreencastCore;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::json;

/// 一路浏览器镜像：Edge 子进程 + CDP 连接 + 页 session + screencast。
struct Mirror {
    _child: tokio::process::Child, // kill_on_drop：断开即杀 Edge
    conn: CdpConnection,
    screencast: Arc<ScreencastCore>,
    session_id: String,
}

impl Mirror {
    /// 启 Edge headless → 连浏览器端点 → createTarget+attach（flat session）→ 建 screencast。
    async fn start(url: &str) -> Result<Self, String> {
        // 随机高端口避免撞（多连接各自一实例）。
        let port = 9000 + (std::process::id() % 800) as u16;
        let (child, browser_ws) = launch(port).await?;
        let conn = CdpConnection::connect(&browser_ws).await?;
        let created = conn
            .send("Target.createTarget", json!({ "url": url }), None)
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or("createTarget: no targetId")?
            .to_string();
        let attached = conn
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("attachToTarget: no sessionId")?
            .to_string();
        let screencast = ScreencastCore::new(conn.sender(), Some(session_id.clone()));
        Ok(Self {
            _child: child,
            conn,
            screencast,
            session_id,
        })
    }

    fn screencast(&self) -> &Arc<ScreencastCore> {
        &self.screencast
    }

    /// 订阅 CDP 事件流（用于看 `Page.frameNavigated` 把当前 URL 回传前端地址栏）。
    fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<agent_act::browser::cdp::CdpEvent> {
        self.conn.sender().subscribe()
    }

    async fn navigate(&self, url: &str) -> Result<(), String> {
        self.conn
            .send("Page.navigate", json!({ "url": url }), Some(&self.session_id))
            .await?;
        Ok(())
    }

    /// 刷新当前页（Page.reload）。
    async fn reload(&self) {
        let _ = self
            .conn
            .send("Page.reload", json!({}), Some(&self.session_id))
            .await;
    }

    /// 后退/前进（delta=-1/+1）：取导航历史 → 跳到目标条目。无目标则 no-op。
    async fn history_nav(&self, delta: i64) {
        let sid = Some(self.session_id.as_str());
        let Ok(hist) = self
            .conn
            .send("Page.getNavigationHistory", json!({}), sid)
            .await
        else {
            return;
        };
        let cur = hist.get("currentIndex").and_then(|v| v.as_i64()).unwrap_or(0);
        let entries = hist.get("entries").and_then(|v| v.as_array());
        let Some(entries) = entries else { return };
        let target = cur + delta;
        if target < 0 || target as usize >= entries.len() {
            return; // 越界 = 没有可后退/前进的页
        }
        if let Some(id) = entries[target as usize].get("id").and_then(|v| v.as_i64()) {
            let _ = self
                .conn
                .send("Page.navigateToHistoryEntry", json!({ "entryId": id }), sid)
                .await;
        }
    }

    /// 优雅关闭：`Browser.close` 让 Edge 连同子进程（renderer/gpu/utility）一起退——`kill_on_drop`
    /// 只杀直接子进程，关不掉 Edge 多进程模型的子树，故断开时显式 close 防孤儿泄漏（对齐 §2.6 进程树兜底）。
    async fn close(&self) {
        let _ = self.conn.send("Browser.close", json!({}), None).await;
    }

    /// §5.6 stage3 双向控制：把前端转来的输入消息映射成 CDP `Input.dispatch*`（在页 session 上）。
    /// 坐标已是页面 CSS 像素（前端按帧 metadata 反算）。失败静默（单次输入丢了不致命）。
    async fn dispatch_input(&self, v: &serde_json::Value) {
        let sid = Some(self.session_id.as_str());
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        match v.get("type").and_then(|x| x.as_str()) {
            Some("mouse") => {
                let _ = self.conn.send("Input.dispatchMouseEvent", json!({
                    "type": s("kind"), "x": f("x"), "y": f("y"),
                    "button": s("button"), "buttons": i("buttons"),
                    "clickCount": i("clickCount"), "modifiers": i("modifiers"),
                }), sid).await;
            }
            Some("wheel") => {
                let _ = self.conn.send("Input.dispatchMouseEvent", json!({
                    "type": "mouseWheel", "x": f("x"), "y": f("y"),
                    "deltaX": f("dx"), "deltaY": f("dy"), "modifiers": i("modifiers"),
                }), sid).await;
            }
            Some("key") => {
                let _ = self.conn.send("Input.dispatchKeyEvent", json!({
                    "type": s("kind"), "text": s("text"), "code": s("code"),
                    "key": s("key"), "windowsVirtualKeyCode": i("vk"), "modifiers": i("modifiers"),
                }), sid).await;
            }
            _ => {}
        }
    }
}

/// `/browser-ws`：升级为 WS，启镜像并双向桥接（帧→client 二进制；client 文本→navigate）。
async fn browser_ws_upgrade(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_browser_ws)
}

async fn handle_browser_ws(mut socket: WebSocket) {
    // 起镜像（默认空白页）。失败 → 告知前端后关闭。
    let mirror = match Mirror::start("about:blank").await {
        Ok(m) => m,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("{{\"type\":\"error\",\"message\":{:?}}}", e)))
                .await;
            return;
        }
    };
    let _ = socket
        .send(Message::Text("{\"type\":\"ready\"}".to_string()))
        .await;
    let mut guard = mirror.screencast().subscribe();
    let mut events = mirror.subscribe_events(); // 看 frameNavigated 回传当前 URL

    loop {
        tokio::select! {
            // CDP 事件：主 frame 导航 → 把当前 URL 回传前端地址栏。
            ev = events.recv() => {
                if let Ok(ev) = ev {
                    if ev.method == "Page.frameNavigated" {
                        let frame = ev.params.get("frame");
                        // 主 frame 无 parentId；只回主页面 URL（忽略 iframe）。
                        let is_main = frame.and_then(|f| f.get("parentId")).is_none();
                        if is_main {
                            if let Some(url) = frame.and_then(|f| f.get("url")).and_then(|u| u.as_str()) {
                                let msg = serde_json::json!({ "type": "url", "url": url }).to_string();
                                if socket.send(Message::Text(msg)).await.is_err() { break; }
                            }
                        }
                    }
                }
            }
            frame = guard.recv() => {
                match frame {
                    Some(f) => {
                        // 帧格式 [u32_le metaLen][meta JSON][JPEG]——前端先读 metadata（设备宽高）
                        // 做坐标换算（§5.6 stage3），再画 JPEG。
                        let meta = serde_json::to_vec(&f.meta).unwrap_or_default();
                        let mut buf = Vec::with_capacity(4 + meta.len() + f.jpeg.len());
                        buf.extend_from_slice(&(meta.len() as u32).to_le_bytes());
                        buf.extend_from_slice(&meta);
                        buf.extend_from_slice(&f.jpeg);
                        if socket.send(Message::Binary(buf)).await.is_err() { break; }
                    }
                    None => break, // 推流结束
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                            match v.get("type").and_then(|x| x.as_str()) {
                                Some("navigate") => {
                                    if let Some(url) = v.get("url").and_then(|x| x.as_str()) {
                                        let _ = mirror.navigate(url).await;
                                    }
                                }
                                Some("mouse") | Some("wheel") | Some("key") => {
                                    mirror.dispatch_input(&v).await;
                                }
                                Some("reload") => mirror.reload().await,
                                Some("back") => mirror.history_nav(-1).await,
                                Some("forward") => mirror.history_nav(1).await,
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    // 优雅关 Edge（连同子进程）防孤儿；drop 兜底 kill_on_drop 杀主进程。
    mirror.close().await;
}

/// 浏览器投屏路由（feature `browser` 时合并进主 router）。
pub fn browser_mirror_router() -> Router {
    Router::new().route("/browser-ws", get(browser_ws_upgrade))
}
