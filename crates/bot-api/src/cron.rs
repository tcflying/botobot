//! 定时任务（§2.10 心跳第一个真 handler）：cron 是「时间中断」——心跳每 tick 扫 job 表，
//! 到点就 `hub.submit(时间刺激)` 发起一个 turn。登记面（agent `schedule` 工具）后续落 agent-act；
//! 当前可经 `Hub::schedule_job` / `POST /api/cron` 登记。
//!
//! 铁律①：`on_tick` 只**瞬时派发**——到点的 job 用 `tokio::spawn` 发 submit（异步），绝不在
//! tick 里 `.await`。绝对时刻用 `Instant`（一次性 = 到点即删；周期 = 重排 `next_run += interval`）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::heartbeat::TickHandler;
use crate::hub::Hub;
use crate::protocol::{Op, Submission};

/// 一条定时任务。到点向 `session_id` 发 `prompt`（作为 user_message 刺激发起 turn）。
#[derive(Debug)]
pub struct CronJob {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    /// `Some` = 周期重复；`None` = 一次性（触发后删除）。
    pub interval: Option<Duration>,
    /// 下次触发的绝对时刻。
    next_run: Instant,
}

/// 共享 job 表（Hub 与 CronHandler 同持一份）。
pub type CronJobs = Arc<Mutex<Vec<CronJob>>>;

/// 心跳订阅者：每 tick 扫 job 表，到点派发 submit。
///
/// ⚠️ 持 `Hub` 以便 submit——这与 `Hub.tick_handlers → CronHandler → Hub` 形成 Arc 环。
/// **有意接受**：心跳/cron 都是进程级单例常驻（与进程同生命周期），环不导致运行期泄漏问题；
/// 真正多 Hub 频繁创建销毁的场景（仅测试）由进程退出回收。
pub struct CronHandler {
    hub: Hub,
    jobs: CronJobs,
}

impl CronHandler {
    pub fn new(hub: Hub, jobs: CronJobs) -> Self {
        Self { hub, jobs }
    }
}

impl TickHandler for CronHandler {
    fn on_tick(&self, _counter: u64) {
        let now = Instant::now();
        let mut due: Vec<(String, String)> = Vec::new();
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.retain_mut(|j| {
                if now < j.next_run {
                    return true;
                }
                due.push((j.session_id.clone(), j.prompt.clone()));
                match j.interval {
                    Some(iv) => {
                        j.next_run = now + iv; // 周期：重排，保留
                        true
                    }
                    None => false, // 一次性：触发后删除
                }
            });
        }
        // 铁律①：到点 job 异步派发（spawn），on_tick 本身瞬时返回。
        for (session_id, prompt) in due {
            let hub = self.hub.clone();
            tokio::spawn(async move {
                let sub = Submission::new(
                    session_id,
                    Op::UserMessage {
                        text: prompt,
                        images: Vec::new(),
                        thinking: None,
                        web_search: None,
                        code_execution: None,
                        force_recall: false,
                    },
                );
                if let Err(e) = hub.submit(sub).await {
                    tracing::warn!("cron submit failed: {e}");
                }
            });
        }
    }
}

/// 往共享 job 表登记一条任务，返回 job id。Hub 方法与 agent 工具共用此入口。
pub fn schedule(
    jobs: &CronJobs,
    session_id: impl Into<String>,
    prompt: impl Into<String>,
    first_delay: Duration,
    interval: Option<Duration>,
) -> String {
    let job = make_job(session_id, prompt, first_delay, interval);
    let id = job.id.clone();
    if let Ok(mut g) = jobs.lock() {
        g.push(job);
    }
    id
}

/// 列出 job 表（id, session_id, prompt, 是否周期）。
pub fn list(jobs: &CronJobs) -> Vec<(String, String, String, bool)> {
    jobs.lock()
        .map(|g| {
            g.iter()
                .map(|j| {
                    (
                        j.id.clone(),
                        j.session_id.clone(),
                        j.prompt.clone(),
                        j.interval.is_some(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 按 id 取消一条 job；返回是否命中。
pub fn cancel(jobs: &CronJobs, id: &str) -> bool {
    if let Ok(mut g) = jobs.lock() {
        let before = g.len();
        g.retain(|j| j.id != id);
        return g.len() != before;
    }
    false
}

/// 构造一条 job（`first_delay` 后首次触发；`interval` 为周期，`None`=一次性）。
pub fn make_job(
    session_id: impl Into<String>,
    prompt: impl Into<String>,
    first_delay: Duration,
    interval: Option<Duration>,
) -> CronJob {
    CronJob {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.into(),
        prompt: prompt.into(),
        interval,
        next_run: Instant::now() + first_delay,
    }
}
