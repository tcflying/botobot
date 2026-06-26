//! 瞬时错误重试装饰器（P-1/§8，借鉴 oh-my-pi `non-compaction-retry-policy`）。
//!
//! 把任意 [`Llm`] 包一层：`infer` 初次请求若返回**瞬时**错误（网络/429/5xx/断流），
//! 指数退避后重试，最多 `max_retries` 次。非瞬时错误（鉴权/请求错误）立即返回。
//!
//! 边界（v1）：只重试 `infer().await` 的**初次建流**失败——这覆盖了 429/5xx/连接拒绝等主流情形。
//! **流中途**（已开始 emit token 后）的失败不重试（会重复输出），原样上抛走两级错误。
//! 上下文溢出归 compaction，不在此处（与 oh-my-pi 一致：retry 与 compaction 互斥）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use base_types::{Llm, LlmOpts, LlmResult, LlmStream, Message, ToolSpec};

pub struct RetryLlm {
    inner: Arc<dyn Llm>,
    max_retries: usize,
    base_delay: Duration,
}

/// 退避计算（§2.6 缺陷2）：full jitter（实际等待 = `[0, base×2^attempt]` 内随机）
/// 并尊重 `Retry-After`（取 `max(jitter, retry_after)`）。
/// 随机源用廉价 nanos（非密码学，对退避抖动足够），不引 `rand`/`fastrand` 依赖。
fn backoff(attempt: usize, base: Duration, retry_after: Option<Duration>) -> Duration {
    let cap_ms = (base.as_millis() as u64).saturating_mul(1u64 << attempt.min(16));
    let jitter_ms = if cap_ms == 0 {
        0
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        nanos % (cap_ms + 1)
    };
    Duration::from_millis(jitter_ms).max(retry_after.unwrap_or(Duration::ZERO))
}

/// 从错误里取 `Retry-After`（仅 `LlmError::Api { retry_after }`）。
fn retry_after_of(e: &base_types::LlmError) -> Option<Duration> {
    match e {
        base_types::LlmError::Api { retry_after, .. } => *retry_after,
        _ => None,
    }
}

impl RetryLlm {
    pub fn new(inner: Arc<dyn Llm>, max_retries: usize) -> Self {
        Self {
            inner,
            max_retries,
            base_delay: Duration::from_millis(500),
        }
    }
    /// 自定义基础退避（默认 500ms，第 n 次退避 = base × 2^n）。
    pub fn with_base_delay(mut self, d: Duration) -> Self {
        self.base_delay = d;
        self
    }
}

#[async_trait]
impl Llm for RetryLlm {
    async fn infer(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &LlmOpts,
    ) -> LlmResult<LlmStream> {
        let mut attempt = 0usize;
        loop {
            match self.inner.infer(messages, tools, opts).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    if attempt >= self.max_retries || !e.is_transient() {
                        return Err(e);
                    }
                    // full jitter + 尊重 Retry-After（§2.6 缺陷2）。
                    let delay = backoff(attempt, self.base_delay, retry_after_of(&e));
                    tracing::warn!(
                        target: "botobot::llm",
                        error = %e,
                        attempt = attempt + 1,
                        max = self.max_retries,
                        "瞬时错误，{delay:?} 后重试"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_types::{Decision, LlmError, LlmEvent};
    use std::sync::Mutex;

    /// 前 `fail` 次返回指定错误，之后成功返回一个空 Decision 流。
    struct FlakyLlm {
        remaining_fails: Mutex<usize>,
        err: fn() -> LlmError,
    }
    #[async_trait]
    impl Llm for FlakyLlm {
        async fn infer(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> LlmResult<LlmStream> {
            let mut n = self.remaining_fails.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err((self.err)());
            }
            let evs: Vec<LlmResult<LlmEvent>> = vec![Ok(LlmEvent::Done(Decision::default()))];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    fn transient() -> LlmError {
        LlmError::Api {
            status: 503,
            body: "overloaded".into(),
            retry_after: None,
        }
    }
    fn permanent() -> LlmError {
        LlmError::Api {
            status: 401,
            body: "unauthorized".into(),
            retry_after: None,
        }
    }

    #[test]
    fn backoff_bounded_jittered_and_respects_retry_after() {
        let base = Duration::from_millis(100);
        // retry_after 大于 jitter cap → 退避 ≥ retry_after。
        let b = backoff(0, base, Some(Duration::from_secs(5)));
        assert!(b >= Duration::from_secs(5), "应尊重 Retry-After");
        // 无 retry_after → full jitter 落 [0, cap]，cap = base × 2^attempt。
        for attempt in 0..4 {
            let cap = base * 2u32.pow(attempt as u32);
            let d = backoff(attempt, base, None);
            assert!(d <= cap, "jitter 应 ≤ cap (attempt={attempt})");
        }
        // 多次采样应有抖动（不全等）。
        let samples: Vec<_> = (0..30).map(|_| backoff(3, base, None)).collect();
        assert!(
            samples.iter().any(|d| *d != samples[0]),
            "full jitter 应产生不同退避值"
        );
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let inner = Arc::new(FlakyLlm {
            remaining_fails: Mutex::new(2),
            err: transient,
        });
        let llm = RetryLlm::new(inner, 5).with_base_delay(Duration::from_millis(1));
        let r = llm.infer(&[], &[], &LlmOpts::default()).await;
        assert!(r.is_ok(), "瞬时失败 2 次后应重试成功");
    }

    #[tokio::test]
    async fn does_not_retry_permanent() {
        let inner = Arc::new(FlakyLlm {
            remaining_fails: Mutex::new(1),
            err: permanent,
        });
        let llm = RetryLlm::new(inner, 5).with_base_delay(Duration::from_millis(1));
        let r = llm.infer(&[], &[], &LlmOpts::default()).await;
        assert!(r.is_err(), "401 非瞬时，不应重试");
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let inner = Arc::new(FlakyLlm {
            remaining_fails: Mutex::new(10),
            err: transient,
        });
        let llm = RetryLlm::new(inner, 2).with_base_delay(Duration::from_millis(1));
        let r = llm.infer(&[], &[], &LlmOpts::default()).await;
        assert!(r.is_err(), "超过 max_retries 应放弃");
    }
}
