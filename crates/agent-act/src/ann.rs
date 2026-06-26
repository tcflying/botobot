//! §1.8.3④ Tier2 向量 ANN（HNSW，纯 Rust `instant-distance`，守「无 C 依赖」）。
//!
//! **feature-gated**（`hnsw`）：默认关——小规模记忆 cosine over Vec 已足够快，规模触发再开
//！（§0 不预付：默认构建不拉此依赖）。本模块是「按 id 建索引 + 查近邻」的封装，供 `MemoryStore`
//! 在规模大时替代线性扫描；删点弱（HNSW 通病）→ forget 走重建（rebuild），与 store 的全量重写一致。
//!
//! 向量约定：L2 归一化（与记忆向量一致）→ 欧氏距离单调对应余弦相似度，排序等价。

use instant_distance::{Builder, HnswMap, Point, Search};

/// 一个归一化向量点（欧氏距离 ∝ 余弦，排序等价）。
#[derive(Clone, Debug)]
pub struct VecPoint(pub Vec<f32>);

impl Point for VecPoint {
    fn distance(&self, other: &Self) -> f32 {
        // 欧氏距离平方根；维度不等给极大值（防误配）。
        if self.0.len() != other.0.len() {
            return f32::MAX;
        }
        self.0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt()
    }
}

/// 记忆向量 ANN 索引：`build` 一次（id↔vec），`query` 取近邻 id（按距离升序）。
pub struct MemoryAnn {
    map: HnswMap<VecPoint, usize>,
}

impl MemoryAnn {
    /// 从 `(行索引, 向量)` 建 HNSW（行索引作 value，回指 store 的 entries）。空输入返回 `None`。
    pub fn build(items: Vec<(usize, Vec<f32>)>) -> Option<Self> {
        if items.is_empty() {
            return None;
        }
        let (values, points): (Vec<usize>, Vec<VecPoint>) =
            items.into_iter().map(|(id, v)| (id, VecPoint(v))).unzip();
        let map = Builder::default().build(points, values);
        Some(Self { map })
    }

    /// 查 `query` 的近邻，返回至多 `top_k` 个 `(行索引, 距离)`（距离升序=最近在前）。
    pub fn query(&self, query: &[f32], top_k: usize) -> Vec<(usize, f32)> {
        let mut search = Search::default();
        let q = VecPoint(query.to_vec());
        self.map
            .search(&q, &mut search)
            .take(top_k)
            .map(|item| (*item.value, item.distance))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: [f32; 3]) -> Vec<f32> {
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        v.iter().map(|x| x / n).collect()
    }

    #[test]
    fn build_empty_is_none() {
        assert!(MemoryAnn::build(vec![]).is_none());
    }

    #[test]
    fn query_returns_nearest_first() {
        // 三个正交主题向量。
        let cat = norm([1.0, 0.0, 0.0]);
        let car = norm([0.0, 1.0, 0.0]);
        let food = norm([0.0, 0.0, 1.0]);
        let ann = MemoryAnn::build(vec![(10, cat.clone()), (20, car), (30, food)]).unwrap();
        // 查接近 cat 的向量 → 行索引 10 最近。
        let q = norm([0.9, 0.1, 0.0]);
        let hits = ann.query(&q, 2);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, 10, "最近应是 cat(行 10): {hits:?}");
        // 距离升序。
        if hits.len() == 2 {
            assert!(hits[0].1 <= hits[1].1);
        }
    }
}
