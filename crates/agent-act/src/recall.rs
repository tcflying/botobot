//! §1.8.3b 统一语义召回入口：把 memory 召回 + skill/book **能力提示**组合成一个 [`QueryRecall`]。
//!
//! 吸收「外脑」论 2.1：用户请求上来都访问记忆，且记忆里**含 skill 与 book 的向量**——一次召回
//! 应同时浮出「相关记忆 + 你有一条能干这事的 skill + 书里有一节讲这个」。本地小模型常忘了自己
//! 有可用工具/资料，这条提示把它们的概要拉到眼前（低可信、可 `read` 下钻）。
//!
//! 架构：三套 store **不合并**（各自生命周期/可信度不同），改用组合——
//! - [`CapabilityHint`]：细端口，skill/book 各自实现「按 query 语义粗筛、回 ≤max 条提示」；
//! - [`UnifiedRecall`]：包一个记忆 [`QueryRecall`] + 若干 [`CapabilityHint`]，拼成单块。

use async_trait::async_trait;

use crate::memory::QueryRecall;

/// 一条能力提示：`kind`=skill|book，`label`=展示名/一句话，`citation`=供 `read` 下钻的资源串。
#[derive(Clone, Debug)]
pub struct CapHint {
    pub kind: &'static str,
    pub label: String,
    pub citation: String,
}

/// §1.8.3b 能力提示端口：给定 query，语义粗筛出本源（skill / book）≤`max` 条相关项。
/// 无 embedder / 无命中 → 返回空（调用方不渲染该段）。
#[async_trait]
pub trait CapabilityHint: Send + Sync {
    async fn hint(&self, query: &str, max: usize) -> Vec<CapHint>;
}

/// §1.8.3b 组合召回：记忆图块（来自 `memory`）+「能力提示」块（来自各 `hints`）。
/// 决策（已拍板）：能力提示**总是出现**（只要语义命中），各源 ≤2 条；记忆为空时仍可只出能力提示。
pub struct UnifiedRecall {
    memory: std::sync::Arc<dyn QueryRecall>,
    hints: Vec<std::sync::Arc<dyn CapabilityHint>>,
    /// 每个能力源最多附几条（默认 2）。
    max_per_source: usize,
}

impl UnifiedRecall {
    pub fn new(
        memory: std::sync::Arc<dyn QueryRecall>,
        hints: Vec<std::sync::Arc<dyn CapabilityHint>>,
    ) -> Self {
        Self {
            memory,
            hints,
            max_per_source: 2,
        }
    }

    /// 收集所有能力源的提示，渲染成一段（无任何命中则 None）。
    async fn capability_block(&self, query: &str) -> Option<String> {
        let mut all: Vec<CapHint> = Vec::new();
        for h in &self.hints {
            all.extend(h.hint(query, self.max_per_source).await);
        }
        if all.is_empty() {
            return None;
        }
        let mut s = String::from("[能力提示 / you may have a tool for this · 仅供参考]\n");
        for c in &all {
            s.push_str(&format!("- {}: {} ({})\n", c.kind, c.label, c.citation));
        }
        Some(s)
    }
}

#[async_trait]
impl QueryRecall for UnifiedRecall {
    async fn recall_block(&self, query: &str) -> Option<String> {
        let mem = self.memory.recall_block(query).await;
        let cap = self.capability_block(query).await;
        match (mem, cap) {
            (Some(m), Some(c)) => Some(format!("{m}{c}")),
            (Some(m), None) => Some(m),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct StubMem(Option<&'static str>);
    #[async_trait]
    impl QueryRecall for StubMem {
        async fn recall_block(&self, _q: &str) -> Option<String> {
            self.0.map(|s| s.to_string())
        }
    }

    struct StubHint(Vec<CapHint>);
    #[async_trait]
    impl CapabilityHint for StubHint {
        async fn hint(&self, _q: &str, max: usize) -> Vec<CapHint> {
            self.0.iter().take(max).cloned().collect()
        }
    }

    fn cap(kind: &'static str, label: &str) -> CapHint {
        CapHint {
            kind,
            label: label.into(),
            citation: format!("read({label})"),
        }
    }

    // 记忆 + 能力提示都在 → 一块拼接，含两段标题。
    #[tokio::test]
    async fn merges_memory_and_capability_blocks() {
        let u = UnifiedRecall::new(
            Arc::new(StubMem(Some("[记忆图]\n事实:\n- x\n"))),
            vec![Arc::new(StubHint(vec![cap("skill", "officecli")]))],
        );
        let block = u.recall_block("q").await.unwrap();
        assert!(block.contains("[记忆图]"));
        assert!(block.contains("能力提示"));
        assert!(block.contains("skill: officecli"));
    }

    // 记忆为空但 skill 命中 → 仍出能力提示（决策：总是出现）。
    #[tokio::test]
    async fn capability_only_when_memory_empty() {
        let u = UnifiedRecall::new(
            Arc::new(StubMem(None)),
            vec![Arc::new(StubHint(vec![cap("book", "api#错误码")]))],
        );
        let block = u.recall_block("q").await.unwrap();
        assert!(block.contains("能力提示"));
        assert!(block.contains("book: api#错误码"));
    }

    // 各源封顶 max_per_source（默认 2）。
    #[tokio::test]
    async fn caps_each_source_to_max() {
        let many = (0..5).map(|i| cap("skill", &format!("s{i}"))).collect();
        let u = UnifiedRecall::new(Arc::new(StubMem(None)), vec![Arc::new(StubHint(many))]);
        let block = u.recall_block("q").await.unwrap();
        let n = block.matches("skill:").count();
        assert_eq!(n, 2, "每源应封顶 2 条");
    }

    // 全空 → None（不增广）。
    #[tokio::test]
    async fn nothing_yields_none() {
        let u = UnifiedRecall::new(Arc::new(StubMem(None)), vec![Arc::new(StubHint(vec![]))]);
        assert!(u.recall_block("q").await.is_none());
    }
}
