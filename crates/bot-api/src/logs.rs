//! Web 端日志面板支持：把进程内 `tracing` 事件通过 WS 推给订阅者。
//!
//! ## 数据流
//!
//! ```text
//!   tracing::Event
//!        │
//!        ▼
//!   BotobotLogLayer (impl Layer)        ──── 全局一份,在 main.rs 装到 subscriber 上
//!        │  to_log_event()
//!        ▼
//!   broadcast::Sender<LogEvent>         ──── 全局,容量 LOG_CHANNEL_CAPACITY
//!        │
//!        ├─► Subscriber A (browser WS #1 的 handle_socket)
//!        ├─► Subscriber B (browser WS #2 的 handle_socket)
//!        └─► …
//! ```
//!
//! - **背压**:broadcast channel 容量有限,满了就**丢老的**(`Sender::send` 失败被吞掉)。
//!   日志是辅助视图,丢比卡好。
//! - **零侵入**:Layer 不感知 WS,只在全局里广播;`bot-api::handle_socket` 通过
//!   `logs::subscribe()` 拿到一个 `Receiver`,再转发给前端。
//! - **协议**:`ClientMsg::SubscribeLogs` 开启,服务端开始推 `{"type":"log",...}`。
//!   关闭连接 = 取消订阅(`Receiver` drop,broadcast 自动清理)。
//!
//! ## 注意
//!
//! `install()` 只能在进程里调一次(broadcast 频道是单例)。`bots serve` / `bots "..."`
//! 都过同一份 main,先 init tracing 再 `install()`。两次 install 是无害的——
//! 会替换 sender,但 channel 已存在的 receiver 会失活,新 receiver 拿到新 sender,
//! 旧 WS 自然收不到日志 → 实际场景里 WS 还没建就 install 完了,不会出现这种竞争。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// 单条日志事件,序列化后通过 WS 推给前端。
///
/// 字段保持紧凑:`target` 用 `botobot::*` 已很说明问题,`message` 是
/// `format!` 后的字符串(由 tracing-subscriber 标准 fmt 渲染),`time` 是 RFC3339。
#[derive(Debug, Clone, Serialize)]
pub struct LogEvent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub seq: u64,
    /// RFC3339 字符串,前端直接显示;不要再 parse。
    pub time: String,
    /// `tracing::Level` 小写:trace/debug/info/warn/error。
    pub level: String,
    /// `botobot::session`、`botobot::ws` 这类 target。
    pub target: String,
    /// `format!("{}", record)` 渲染出的最终消息(含结构化字段)。
    pub message: String,
}

const LOG_CHANNEL_CAPACITY: usize = 1024;
const LOG_RING_MAX_ITEMS: usize = 500;
const LOG_RING_MAX_BYTES: usize = 256 * 1024;

static BROADCAST: OnceLock<broadcast::Sender<LogEvent>> = OnceLock::new();
static RING: OnceLock<Mutex<LogRing>> = OnceLock::new();
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct LogRing {
    events: VecDeque<LogEvent>,
    bytes: usize,
}

/// 拿到(惰性创建)全局 broadcast sender。
fn sender() -> &'static broadcast::Sender<LogEvent> {
    BROADCAST.get_or_init(|| broadcast::channel(LOG_CHANNEL_CAPACITY).0)
}

fn ring() -> &'static Mutex<LogRing> {
    RING.get_or_init(|| Mutex::new(LogRing::default()))
}

/// 订阅一份日志流。返回的 `Receiver` 在 bot-api 的 `handle_socket` 里被
/// 轮询,把收到的 `LogEvent` 序列化成 JSON 推给前端。
pub fn subscribe() -> broadcast::Receiver<LogEvent> {
    sender().subscribe()
}

pub fn snapshot() -> Vec<LogEvent> {
    ring()
        .lock()
        .map(|r| r.events.iter().cloned().collect())
        .unwrap_or_default()
}

fn approx_bytes(ev: &LogEvent) -> usize {
    ev.time.len() + ev.level.len() + ev.target.len() + ev.message.len() + 32
}

fn publish(mut ev: LogEvent) {
    ev.seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let ev_bytes = approx_bytes(&ev);
    if let Ok(mut r) = ring().lock() {
        r.bytes += ev_bytes;
        r.events.push_back(ev.clone());
        while r.events.len() > LOG_RING_MAX_ITEMS || r.bytes > LOG_RING_MAX_BYTES {
            let Some(old) = r.events.pop_front() else {
                break;
            };
            r.bytes = r.bytes.saturating_sub(approx_bytes(&old));
        }
    }
    let _ = sender().send(ev);
}

/// 给 `tracing_subscriber::fmt()` 拼上的自定义 Layer,把每个 event 转成
/// `LogEvent` 推到 broadcast。**只接受 `botobot::*` target**——其它 crate
/// 的 trace 噪音不进面板(那些通常只对开发者有用)。
pub struct BotobotLogLayer;

impl<S> Layer<S> for BotobotLogLayer
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // 只转发 botobot 自家 target;第三方 crate 的 debug 噪音不进面板。
        let target = event.metadata().target();
        if !target.starts_with("botobot::") && target != "botobot" {
            return;
        }
        let level = level_str(event.metadata().level());
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let message = visitor.render();
        let time = chrono_like_now();
        let _ = ctx; // 当前未用;保留 ctx 以备未来按 span 着色
        publish(LogEvent {
            kind: "log",
            seq: 0,
            time,
            level,
            target: target.to_string(),
            message,
        });
    }
}

/// `tracing::Event` 的字段访问器:把结构化字段 name=value 收下来,
/// 最后拼成 `format!("{name}={value}")` 形式——和 `tracing_subscriber::fmt()`
/// 默认输出对齐,前端一份和终端一份看起来一致。
#[derive(Default)]
struct FieldVisitor {
    fields: Vec<(String, String)>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
}

impl FieldVisitor {
    /// 渲染策略:若只有一个 `message` 字段(或 `{}` 隐式 message),就只输出它的值;
    /// 否则输出 `k=v k=v` 列表,逗号分隔。
    fn render(&self) -> String {
        if self.fields.is_empty() {
            return String::new();
        }
        // `tracing::info!("hi")` 这种无字段事件:fields 为空(上面的 early return 已处理)。
        // `tracing::info!(target, kind, "msg")` 会带 message 字段 + 其它字段。
        let mut msg = None;
        let mut kv = Vec::new();
        for (k, v) in &self.fields {
            if k == "message" {
                msg = Some(v.as_str());
            } else {
                kv.push(format!("{k}={v}"));
            }
        }
        match msg {
            Some(m) if kv.is_empty() => m.to_string(),
            Some(m) => format!("{m} {{{}}}", kv.join(", ")),
            None => kv.join(" "),
        }
    }
}

fn level_str(l: &Level) -> String {
    match *l {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
    .to_string()
}

/// 简易 RFC3339-ish 时间戳(避免引 `chrono`)。精度到秒就够了——
/// 日志面板不需要亚秒级,前端也只显示 `HH:MM:SS`。
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_visitor_empty() {
        let v = FieldVisitor::default();
        assert_eq!(v.render(), "");
    }

    #[test]
    fn field_visitor_message_only() {
        let v = FieldVisitor {
            fields: vec![("message".into(), "hello".into())],
        };
        assert_eq!(v.render(), "hello");
    }

    #[test]
    fn field_visitor_kv_with_message() {
        let v = FieldVisitor {
            fields: vec![
                ("message".into(), "open".into()),
                ("id".into(), "abc".into()),
                ("live".into(), "1".into()),
            ],
        };
        assert_eq!(v.render(), "open {id=abc, live=1}");
    }
}
