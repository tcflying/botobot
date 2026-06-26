//! model-embed —— 本地中文文本嵌入（纯 Rust，无 C 依赖，自包含单二进制）。
//!
//! 层级：`model-*`（神经/模型计算基座，位于 `base-*` 与 `agent-*` 之间）的首住户——
//! 用重 ML 依赖（candle）实现 `base-types` 的端口，被 `agent-*` 认知层经端口消费。
//! 角色：记忆语义召回的模型计算原语，把文本编码成稠密向量。candle 推理 +
//! bge-small-zh-v1.5 权重（`include_bytes!` 嵌入，见 build.rs）+ tokenizers（fancy-regex）。
//!
//! 核心 [`EmbedCore`]（candle BERT + 分词器）impl `base_types::Embedder`，经端口注入
//! `agent-act` 的 `MemoryStore`，使记忆模块**不直接依赖 candle**。
//! 借鉴前身 datoobot `embed-tech`。

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

// 权重/分词器/配置在构建时下载并缓存（build.rs），此处嵌进二进制（自包含）。
const TOKENIZER_JSON: &str = include_str!(concat!(env!("BGE_DIR"), "/tokenizer.json"));
const CONFIG_JSON: &str = include_str!(concat!(env!("BGE_DIR"), "/config.json"));
const WEIGHTS_F16: &[u8] = include_bytes!(concat!(env!("BGE_DIR"), "/model.f16.safetensors"));

fn ce(e: impl std::fmt::Display) -> String {
    format!("model-embed: {e}")
}

/// 本地中文嵌入核心：candle BERT + 分词器，`embed` 把文本批量编码成 L2 归一化向量。
/// bge 推荐 **CLS pooling** + L2 归一化，余弦相似度即语义相近度。
pub struct EmbedCore {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    dim: usize,
}

impl EmbedCore {
    /// 加载内嵌的 bge-small-zh-v1.5（CPU）。权重/分词器/配置均来自二进制内嵌资源。
    pub fn load() -> Result<Self, String> {
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(CONFIG_JSON).map_err(ce)?;
        let dim = config.hidden_size;
        let vb = VarBuilder::from_buffered_safetensors(WEIGHTS_F16.to_vec(), DType::F32, &device)
            .map_err(ce)?;
        let model = BertModel::load(vb, &config).map_err(ce)?;

        let mut tokenizer = Tokenizer::from_bytes(TOKENIZER_JSON.as_bytes()).map_err(ce)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: config.max_position_embeddings,
                ..Default::default()
            }))
            .map_err(ce)?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        Ok(Self {
            model,
            tokenizer,
            device,
            dim,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 批量把文本编码成 **L2 归一化**句向量（CLS pooling）。空输入返回空。
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encs = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(ce)?;
        let batch = encs.len();
        let seq = encs[0].get_ids().len();

        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        let mut types = Vec::with_capacity(batch * seq);
        for e in &encs {
            ids.extend(e.get_ids().iter().copied());
            mask.extend(e.get_attention_mask().iter().copied());
            types.extend(e.get_type_ids().iter().copied());
        }
        let input_ids = Tensor::from_vec(ids, (batch, seq), &self.device).map_err(ce)?;
        let type_ids = Tensor::from_vec(types, (batch, seq), &self.device).map_err(ce)?;
        let attn_f32: Vec<f32> = mask.into_iter().map(|m| m as f32).collect();
        let attn = Tensor::from_vec(attn_f32, (batch, seq), &self.device).map_err(ce)?;

        let out = self
            .model
            .forward(&input_ids, &type_ids, Some(&attn))
            .map_err(ce)?;
        let cls = out.i((.., 0)).map_err(ce)?;
        let norm = cls
            .sqr()
            .map_err(ce)?
            .sum_keepdim(1)
            .map_err(ce)?
            .sqrt()
            .map_err(ce)?;
        let normed = cls.broadcast_div(&norm).map_err(ce)?;
        normed.to_vec2::<f32>().map_err(ce)
    }
}

/// 经 `base_types::Embedder` 把 [`EmbedCore`] 注入上层（agent-act 记忆），上层无需依赖 candle。
impl base_types::Embedder for EmbedCore {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        EmbedCore::embed(self, texts)
    }
    fn dim(&self) -> usize {
        EmbedCore::dim(self)
    }
    fn model_id(&self) -> &str {
        "bge-small-zh-v1.5"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum() // 已 L2 归一化 → 点积即余弦
    }

    #[test]
    fn loads_and_embeds_with_semantic_cosine() {
        let core = EmbedCore::load().expect("加载内嵌 bge 模型");
        assert!(core.dim() > 0);
        let v = core
            .embed(&["小猫在睡觉", "一只猫咪", "我开着一辆快车"])
            .unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].len(), core.dim());
        // 「小猫在睡觉」与「一只猫咪」（同主题猫）应比与「快车」更相近
        let sim_cat = cos(&v[0], &v[1]);
        let sim_car = cos(&v[0], &v[2]);
        assert!(
            sim_cat > sim_car,
            "同主题应更相近: cat={sim_cat:.3} car={sim_car:.3}"
        );
        // 归一化检查：自身余弦 ≈ 1
        assert!((cos(&v[0], &v[0]) - 1.0).abs() < 0.01);
    }
}
