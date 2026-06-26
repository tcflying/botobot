//! §4.6 步①下半：真 WebSocket 连接到 Chrome CDP 端点，把入站文本喂给 [`CdpDispatcher`]、
//! 把出站命令通过 WS 发出（移植 `.oni/agent-browser` cdp/client 的连接+reader 循环，重写为
//! 接 botobot 的 transport-agnostic dispatcher）。
//!
//! **feature-gated**（`browser`）：默认构建不拉 `tokio-tungstenite` 网络栈（守 §0 不预付）。
//! **运行验证待真 Chrome**：本模块编译验证通过；`connect()` 的实连/收发需 `chrome --remote-debugging-port`
//! 起的 CDP 端点（`ws://127.0.0.1:<port>/devtools/browser/<id>`）才能 live 验证——属步①收尾的
//! 运行期验证，需用户提供 Chrome 环境。

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::cdp::{CdpDispatcher, CdpResponse};

type WsSink = futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// 一条到 Chrome CDP 端点的活连接。持调度器 + WS 发送端；reader 任务在后台把入站喂调度器。
pub struct CdpConnection {
    dispatcher: Arc<CdpDispatcher>,
    sink: Arc<Mutex<WsSink>>,
    _reader: tokio::task::JoinHandle<()>,
}

impl CdpConnection {
    /// 连接到 `ws_url`（Chrome 的 `webSocketDebuggerUrl`）。后台起 reader 循环喂 dispatcher。
    pub async fn connect(ws_url: &str) -> Result<Self, String> {
        let (stream, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("CDP connect failed: {e}"))?;
        let (sink, mut reader) = stream.split();
        let dispatcher = Arc::new(CdpDispatcher::new());
        let d = dispatcher.clone();
        let _reader = tokio::spawn(async move {
            while let Some(msg) = reader.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        d.on_incoming(&text);
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {} // ping/pong/binary 忽略
                }
            }
        });
        Ok(Self {
            dispatcher,
            sink: Arc::new(Mutex::new(sink)),
            _reader,
        })
    }

    /// 暴露调度器（订阅事件 / 诊断）。
    pub fn dispatcher(&self) -> &Arc<CdpDispatcher> {
        &self.dispatcher
    }

    /// §5.6 投屏：克隆一个**可独立持有**的发送/订阅句柄（dispatcher + sink 都已是 Arc），
    /// 供 screencast 后台任务持有（send 命令 + 订阅 screencastFrame 事件），与连接生命周期解耦。
    pub fn sender(&self) -> CdpSender {
        CdpSender {
            dispatcher: self.dispatcher.clone(),
            sink: self.sink.clone(),
        }
    }

    /// 发一条 CDP 命令并等响应。`session_id` 为 flat-session 模式目标会话。
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let (id, rx) = self.dispatcher.begin();
        let text = self.dispatcher.encode(id, method, params, session_id);
        self.sink
            .lock()
            .await
            .send(Message::Text(text))
            .await
            .map_err(|e| format!("CDP send failed: {e}"))?;
        match rx.await {
            Ok(CdpResponse::Ok(v)) => Ok(v),
            Ok(CdpResponse::Err(e)) => Err(e),
            Err(_) => Err("CDP response channel closed".into()),
        }
    }
}

/// §5.6 投屏：可克隆的 CDP 发送/订阅句柄（持 dispatcher + sink 的 Arc）。供 screencast 后台任务
/// 独立持有——既能 `send` 命令（startScreencast/ack/stop），又能 `subscribe` 入站事件（帧）。
#[derive(Clone)]
pub struct CdpSender {
    dispatcher: Arc<CdpDispatcher>,
    sink: Arc<Mutex<WsSink>>,
}

impl CdpSender {
    /// 同 [`CdpConnection::send`]。
    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let (id, rx) = self.dispatcher.begin();
        let text = self.dispatcher.encode(id, method, params, session_id);
        self.sink
            .lock()
            .await
            .send(Message::Text(text))
            .await
            .map_err(|e| format!("CDP send failed: {e}"))?;
        match rx.await {
            Ok(CdpResponse::Ok(v)) => Ok(v),
            Ok(CdpResponse::Err(e)) => Err(e),
            Err(_) => Err("CDP response channel closed".into()),
        }
    }

    /// 订阅 CDP 事件流（含 `Page.screencastFrame`）。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<super::cdp::CdpEvent> {
        self.dispatcher.subscribe()
    }
}
