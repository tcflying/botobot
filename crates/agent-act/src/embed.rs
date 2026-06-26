//! §1.8.3b 召回热路径优化：`Embedder` 端口的**单条 query 记忆装饰器**。
//!
//! `force_recall` 默认开后，每个 turn 的 `recall_block` 会让同一句 query 被嵌入多次
//! （memory recall_ranked + recall_graph + skill 提示 + book 提示）。把同一个 `CachingEmbedder`
//! 注入这几路（memory/skill/book 共享一份缓存），**同一 query 字符串连续嵌入只真算一次**——
//! 零召回逻辑改动即去重。多条文本（建索引/回填）直通不缓存（不同语料、缓存无意义）。
//!
//! 正确性：只在 query 字符串**完全相同**时返回缓存向量；模型升级=新 embedder 实例=新装饰器，
//! 不跨向量空间（§4.9 A3）。并发下用 Mutex 保护（最坏只是命中率降，不会返回错向量）。

use std::sync::{Arc, Mutex};

use base_types::Embedder;

/// 包一个底层 [`Embedder`]，记住**最近一条单文本**的嵌入结果。
pub struct CachingEmbedder {
    inner: Arc<dyn Embedder>,
    last: Mutex<Option<(String, Vec<f32>)>>,
}

impl CachingEmbedder {
    pub fn new(inner: Arc<dyn Embedder>) -> Self {
        Self {
            inner,
            last: Mutex::new(None),
        }
    }
}

impl Embedder for CachingEmbedder {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        // 只 memo 单条（query 路径）；多条（建索引/回填旧向量）直通——语料各异，缓存无价值。
        if texts.len() != 1 {
            return self.inner.embed(texts);
        }
        let q = texts[0];
        if let Some((k, v)) = self.last.lock().unwrap().as_ref() {
            if k == q {
                return Ok(vec![v.clone()]);
            }
        }
        let out = self.inner.embed(texts)?;
        if let Some(v) = out.first() {
            *self.last.lock().unwrap() = Some((q.to_string(), v.clone()));
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 计数底层：记录被真正调用的次数（按调用次数，不按文本条数）。
    struct Counting {
        calls: AtomicUsize,
    }
    impl Embedder for Counting {
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(texts.iter().map(|t| vec![t.len() as f32]).collect())
        }
        fn dim(&self) -> usize {
            1
        }
        fn model_id(&self) -> &str {
            "stub-model"
        }
    }

    fn counting() -> (Arc<Counting>, CachingEmbedder) {
        let inner = Arc::new(Counting {
            calls: AtomicUsize::new(0),
        });
        let cache = CachingEmbedder::new(inner.clone());
        (inner, cache)
    }

    // 同一 query 连续嵌入 → 底层只调一次；返回值一致。
    #[test]
    fn repeats_same_query_hit_cache() {
        let (inner, cache) = counting();
        let a = cache.embed(&["fix login bug"]).unwrap();
        let b = cache.embed(&["fix login bug"]).unwrap();
        let c = cache.embed(&["fix login bug"]).unwrap();
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            1,
            "同 query 只真算一次"
        );
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    // 不同 query → 重新嵌入（缓存按字符串精确匹配）。
    #[test]
    fn different_query_misses_cache() {
        let (inner, cache) = counting();
        cache.embed(&["alpha"]).unwrap();
        cache.embed(&["beta"]).unwrap();
        cache.embed(&["alpha"]).unwrap(); // alpha 已被 beta 挤掉 → 再算
        assert_eq!(inner.calls.load(Ordering::Relaxed), 3);
    }

    // 多条文本（建索引/回填）直通不缓存，也不污染单条缓存。
    #[test]
    fn multi_text_bypasses_and_preserves_single_cache() {
        let (inner, cache) = counting();
        cache.embed(&["q"]).unwrap(); // 缓存 q（call 1）
        let multi = cache.embed(&["a", "b", "c"]).unwrap(); // 直通（call 2）
        assert_eq!(multi.len(), 3);
        cache.embed(&["q"]).unwrap(); // 仍命中（不再调用）
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "多条直通、单条缓存不被污染"
        );
    }

    // dim / model_id 透传。
    #[test]
    fn forwards_dim_and_model_id() {
        let (_inner, cache) = counting();
        assert_eq!(cache.dim(), 1);
        assert_eq!(cache.model_id(), "stub-model");
    }
}
