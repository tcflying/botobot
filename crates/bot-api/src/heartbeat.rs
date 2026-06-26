//! 心跳内核（§2.10）：把 Hub 当操作系统内核，心跳 = **唯一晶振**——进程级常驻、与连接无关，
//! 每 tick 遍历订阅者派发。这是「主动发起 turn」的驱动源（cron / world 外部刺激 / ping-sweep
//! 都退化为它的 `TickHandler`）。
//!
//! 四条铁律（§2.10）：① on_tick 只派发不阻塞（绝不 `.await` 慢活，类比时钟中断 ISR 极短）；
//! ② 单一最细粒度 + 计数器分频（心跳跑最细需求，各 handler 用 `counter % N` 自分频）；
//! ③ handler 注册表（加定时行为=注册一个 handler，不改心跳本体）；④ per-connection 存活态各连接自管。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::MissedTickBehavior;

/// 心跳订阅者（§2.10 铁律③）。
///
/// ⚠️ **铁律①**：`on_tick` 必须**瞬时**——只做登记/派发（`hub.submit` 刺激、`spawn` 慢活），
/// 绝不在其中 `.await` 慢 hook/慢 LLM，否则会拖垮整个心跳（ping 迟到、连接误判死亡）。
pub trait TickHandler: Send + Sync {
    /// `counter` = 自启动累计 tick 数。用 `counter % N`（铁律②）自行分频。
    fn on_tick(&self, counter: u64);
}

/// 共享的 tick 订阅者注册表（Hub 持有；handler 可在运行期追加）。
pub type TickHandlers = Arc<Mutex<Vec<Arc<dyn TickHandler>>>>;

/// 起进程级常驻心跳（§2.10 骨架）：`interval` 间隔 tick，每 tick `counter++` 后遍历订阅者。
/// 用 `MissedTickBehavior::Skip` 防漂移堆积。**先克隆订阅者列表再放锁调用**，避免 handler
/// 内部回注册表/Hub 时死锁，也让运行期注册安全。返回的 `JoinHandle` 随 Hub 生命周期，丢弃即停。
pub fn spawn_heartbeat(handlers: TickHandlers, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut counter: u64 = 0;
        loop {
            tick.tick().await;
            counter += 1;
            // 克隆 Arc 列表后放锁再调用（铁律①派发不阻塞 + 防重入死锁）。
            let snapshot: Vec<Arc<dyn TickHandler>> = {
                match handlers.lock() {
                    Ok(g) => g.clone(),
                    Err(p) => p.into_inner().clone(),
                }
            };
            for h in snapshot {
                h.on_tick(counter);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Counting(Arc<AtomicU64>);
    impl TickHandler for Counting {
        fn on_tick(&self, _counter: u64) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn heartbeat_ticks_and_dispatches_to_handlers() {
        let handlers: TickHandlers = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicU64::new(0));
        handlers
            .lock()
            .unwrap()
            .push(Arc::new(Counting(hits.clone())));
        let h = spawn_heartbeat(handlers.clone(), Duration::from_millis(10));

        // 等几个 tick。
        tokio::time::sleep(Duration::from_millis(55)).await;
        h.abort();
        let n = hits.load(Ordering::Relaxed);
        assert!(n >= 3, "约 50ms/10ms 应 tick 多次，实际 {n}");
    }

    #[tokio::test]
    async fn handler_registered_after_spawn_still_fires() {
        let handlers: TickHandlers = Arc::new(Mutex::new(Vec::new()));
        let h = spawn_heartbeat(handlers.clone(), Duration::from_millis(10));
        // spawn 后再注册（运行期追加）。
        let hits = Arc::new(AtomicU64::new(0));
        handlers
            .lock()
            .unwrap()
            .push(Arc::new(Counting(hits.clone())));
        tokio::time::sleep(Duration::from_millis(40)).await;
        h.abort();
        assert!(
            hits.load(Ordering::Relaxed) >= 1,
            "运行期注册的 handler 也应被派发"
        );
    }
}
