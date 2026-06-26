//! §4.6 步① CDP 协议调度核心（移植自 `.oni/agent-browser` `cdp/client.rs` 的请求/响应配对思路，
//! 重写为 **transport-agnostic** 形态：本模块只管「命令 id 分配 + `id↔oneshot` 配对 + 事件 broadcast」，
//! 不绑 WebSocket，故可不连 Chrome 单测）。真 WS 连接（tokio-tungstenite）+ Chrome 启动属步①下半。
//!
//! 用法（未来步①下半接 WS 时）：
//! - 发命令：`let (id, rx) = d.begin(); ws.send(d.encode(id, method, params)); let resp = rx.await?;`
//! - 收消息：WS reader 每条文本 `d.on_incoming(text)` —— 有 `id` 的解析为响应 resolve 对应 oneshot，
//!   无 `id` 有 `method` 的作为 CDP 事件 broadcast 给订阅者。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

/// 一条 CDP 事件（无 id 的入站消息）：`method` + `params`（+ 可选 `sessionId`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// 一条 CDP 响应（对应某命令 id）：`result` 或 `error`。
#[derive(Debug, Clone)]
pub enum CdpResponse {
    Ok(Value),
    Err(String),
}

/// CDP 协议调度器：命令 id 分配 + `id↔oneshot` 请求/响应配对 + 事件 broadcast。
/// transport-agnostic——WS（或任何全双工通道）只需把入站文本喂 [`Self::on_incoming`]、
/// 把 [`Self::encode`] 的出站文本发出去。
pub struct CdpDispatcher {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<CdpResponse>>>,
    events: broadcast::Sender<CdpEvent>,
}

impl Default for CdpDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl CdpDispatcher {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events,
        }
    }

    /// 订阅 CDP 事件流（无 id 的入站消息）。多订阅者各收一份。
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    /// 开始一条命令：分配 id 并登记 oneshot，返回 `(id, 响应接收端)`。
    /// 调用方随后 `encode(id, method, params)` 发出，`await` 接收端拿响应。
    pub fn begin(&self) -> (u64, oneshot::Receiver<CdpResponse>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// 编码一条出站命令为 CDP JSON 文本。`session_id` 为 flat-session 模式的目标会话。
    pub fn encode(&self, id: u64, method: &str, params: Value, session_id: Option<&str>) -> String {
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            msg["sessionId"] = json!(sid);
        }
        msg.to_string()
    }

    /// 处理一条入站文本：有 `id` → 解析为响应 resolve 对应 oneshot（返回 `true`）；
    /// 无 `id` 有 `method` → 作为事件 broadcast（返回 `true`）；无法识别 → `false`。
    /// 解析失败 / 无对应 pending 的响应（迟到/重复）安全忽略。
    pub fn on_incoming(&self, text: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return false;
        };
        // 响应：含 id（CDP id 为整数）。
        if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
            let resp = if let Some(err) = v.get("error") {
                CdpResponse::Err(
                    err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("cdp error")
                        .to_string(),
                )
            } else {
                CdpResponse::Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                let _ = tx.send(resp); // 接收端已 drop（超时/取消）则安全忽略
            }
            return true;
        }
        // 事件：无 id，有 method。
        if let Some(method) = v.get("method").and_then(|x| x.as_str()) {
            let ev = CdpEvent {
                method: method.to_string(),
                params: v.get("params").cloned().unwrap_or(Value::Null),
                session_id: v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(str::to_string),
            };
            let _ = self.events.send(ev); // 无订阅者则安全忽略
            return true;
        }
        false
    }

    /// 当前未决命令数（测试/诊断用）。
    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_allocates_monotonic_ids() {
        let d = CdpDispatcher::new();
        let (a, _ra) = d.begin();
        let (b, _rb) = d.begin();
        assert!(b > a, "id 应单调递增");
        assert_eq!(d.pending_len(), 2);
    }

    #[test]
    fn encode_includes_id_method_and_optional_session() {
        let d = CdpDispatcher::new();
        let s = d.encode(
            7,
            "Page.navigate",
            json!({ "url": "https://x" }),
            Some("S1"),
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "Page.navigate");
        assert_eq!(v["params"]["url"], "https://x");
        assert_eq!(v["sessionId"], "S1");
        // 无 session_id 时不带该字段。
        let s2 = d.encode(8, "Page.enable", json!({}), None);
        let v2: Value = serde_json::from_str(&s2).unwrap();
        assert!(v2.get("sessionId").is_none());
    }

    #[tokio::test]
    async fn response_resolves_pending_oneshot_by_id() {
        let d = CdpDispatcher::new();
        let (id, rx) = d.begin();
        // 模拟入站响应。
        let handled = d.on_incoming(&format!(
            "{{\"id\":{id},\"result\":{{\"frameId\":\"f1\"}}}}"
        ));
        assert!(handled);
        match rx.await.unwrap() {
            CdpResponse::Ok(v) => assert_eq!(v["frameId"], "f1"),
            CdpResponse::Err(e) => panic!("应为 Ok, got Err({e})"),
        }
        assert_eq!(d.pending_len(), 0, "resolve 后 pending 清空");
    }

    #[tokio::test]
    async fn error_response_resolves_as_err() {
        let d = CdpDispatcher::new();
        let (id, rx) = d.begin();
        d.on_incoming(&format!(
            "{{\"id\":{id},\"error\":{{\"code\":-32000,\"message\":\"boom\"}}}}"
        ));
        match rx.await.unwrap() {
            CdpResponse::Err(e) => assert_eq!(e, "boom"),
            CdpResponse::Ok(_) => panic!("应为 Err"),
        }
    }

    #[tokio::test]
    async fn idless_message_broadcasts_as_event() {
        let d = CdpDispatcher::new();
        let mut sub = d.subscribe();
        let handled = d.on_incoming(
            "{\"method\":\"Target.attachedToTarget\",\"params\":{\"targetId\":\"t1\"},\"sessionId\":\"S9\"}",
        );
        assert!(handled);
        let ev = sub.recv().await.unwrap();
        assert_eq!(ev.method, "Target.attachedToTarget");
        assert_eq!(ev.params["targetId"], "t1");
        assert_eq!(ev.session_id.as_deref(), Some("S9"));
    }

    #[test]
    fn unknown_and_stale_messages_are_safe() {
        let d = CdpDispatcher::new();
        assert!(!d.on_incoming("not json"));
        assert!(!d.on_incoming("{\"foo\":1}")); // 无 id 无 method
        // 迟到响应（无对应 pending）：识别为响应但无 panic。
        assert!(d.on_incoming("{\"id\":999,\"result\":{}}"));
    }
}
