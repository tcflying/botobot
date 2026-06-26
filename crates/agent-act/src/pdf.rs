//! §4.8 PDF 解读：纯 Rust `pdf-inspector`（Firecrawl）引擎——分类 + 直出 Markdown（守「无 C 依赖」）。
//!
//! **feature-gated**（`pdf`）。引擎一趟把文档 load 一次，既**分类**（TextBased / Scanned / ImageBased /
//! Mixed + 置信度 + 逐页 `pages_needing_ocr`），又把文字层转成**干净 Markdown**（标题/表格/多栏阅读序/列表/
//! 代码块），对 agent 的 token 与语义都友好。`pdf_type` 非 `text_based`、Markdown 为空、或检出断字编码
//! （`has_encoding_issues`）即「需 OCR」——回退多模态 OCR（页渲染成图喂本地 Qwen 多模态）属后续
//! （需页→图栅格化后端 + 页数闸，见 todo §4.8 待定②③）。
//!
//! 工具形状：`pdf_read(path)`，tier=Read。

use async_trait::async_trait;
use serde_json::{Value, json};

use base_types::{Tool, ToolResult, ToolTier};

/// 解读结果（薄封装引擎返回，便于上层复用/测试装配）。`Err` 当文件不可读/非 PDF。
pub struct PdfRead {
    /// Markdown 正文（无文字层时为空串）。
    pub markdown: String,
    /// 文档类型字符串：`text_based` / `scanned` / `image_based` / `mixed`。
    pub pdf_type: &'static str,
    /// 检测置信度 0.0–1.0。
    pub confidence: f32,
    /// 总页数。
    pub page_count: u32,
    /// 需 OCR 的 1-indexed 页号。
    pub pages_needing_ocr: Vec<u32>,
    /// 检出断字编码（mojibake / 替换符）——应回退 OCR。
    pub has_encoding_issues: bool,
}

impl PdfRead {
    /// 是否「需 OCR」：无可用文字层（扫描/图片型、Markdown 空、或编码断字）。等价旧 `is_sparse`，但引擎级判定。
    pub fn needs_ocr(&self) -> bool {
        self.has_encoding_issues
            || self.markdown.trim().is_empty()
            || matches!(self.pdf_type, "scanned" | "image_based")
            || !self.pages_needing_ocr.is_empty()
    }
}

/// 解读 PDF（纯函数封装）：分类 + 抽取 Markdown。`Err` 当文件不可读/非 PDF。
pub fn read(path: &str) -> Result<PdfRead, String> {
    let r = pdf_inspector::process_pdf(path).map_err(|e| format!("PDF 解读失败: {e}"))?;
    Ok(PdfRead {
        markdown: r.markdown.unwrap_or_default(),
        pdf_type: pdf_type_str(r.pdf_type),
        confidence: r.confidence,
        page_count: r.page_count,
        pages_needing_ocr: r.pages_needing_ocr,
        has_encoding_issues: r.has_encoding_issues,
    })
}

/// 引擎 `PdfType` → 稳定小写字符串（对外协议）。
fn pdf_type_str(t: pdf_inspector::PdfType) -> &'static str {
    use pdf_inspector::PdfType::*;
    match t {
        TextBased => "text_based",
        Scanned => "scanned",
        ImageBased => "image_based",
        Mixed => "mixed",
    }
}

/// `pdf_read(path)` — 读 PDF（Read·不打断）：分类 + 直出 Markdown。需 OCR 时 `needs_ocr=true` 提示回退。
pub struct PdfReadTool;

#[async_trait]
impl Tool for PdfReadTool {
    fn name(&self) -> &str {
        "pdf_read"
    }
    fn description(&self) -> &str {
        "Read a PDF: classify it (text_based / scanned / image_based / mixed) and extract its text \
         layer as clean Markdown (headings, tables, multi-column reading order). When `needs_ocr` is \
         true the PDF lacks a usable text layer (scanned/image-only or broken encoding); OCR fallback \
         is not yet available."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if path.is_empty() {
            anyhow::bail!("pdf_read: missing 'path'");
        }
        // 解读放阻塞线程池（pdf-inspector 同步、可能重）。
        let r = tokio::task::spawn_blocking(move || read(&path))
            .await
            .map_err(|e| anyhow::anyhow!("pdf_read join error: {e}"))?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(json!({
            "markdown": r.markdown,
            "pdf_type": r.pdf_type,
            "confidence": r.confidence,
            "page_count": r.page_count,
            "pages_needing_ocr": r.pages_needing_ocr,
            "has_encoding_issues": r.has_encoding_issues,
            "needs_ocr": r.needs_ocr(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_ocr_logic() {
        let base = PdfRead {
            markdown: "# Title\n\nbody text".into(),
            pdf_type: "text_based",
            confidence: 0.99,
            page_count: 1,
            pages_needing_ocr: vec![],
            has_encoding_issues: false,
        };
        assert!(!base.needs_ocr());
        assert!(PdfRead { markdown: "   \n ".into(), ..base_clone(&base) }.needs_ocr()); // 空文
        assert!(PdfRead { pdf_type: "scanned", ..base_clone(&base) }.needs_ocr()); // 扫描件
        assert!(PdfRead { pdf_type: "image_based", ..base_clone(&base) }.needs_ocr());
        assert!(PdfRead { has_encoding_issues: true, ..base_clone(&base) }.needs_ocr()); // 断字编码
        assert!(PdfRead { pages_needing_ocr: vec![2], ..base_clone(&base) }.needs_ocr()); // 逐页路由
    }

    // PdfRead 无 Clone（markdown 可大）；测试用浅拷贝构造器。
    fn base_clone(b: &PdfRead) -> PdfRead {
        PdfRead {
            markdown: b.markdown.clone(),
            pdf_type: b.pdf_type,
            confidence: b.confidence,
            page_count: b.page_count,
            pages_needing_ocr: b.pages_needing_ocr.clone(),
            has_encoding_issues: b.has_encoding_issues,
        }
    }

    #[test]
    fn tool_metadata() {
        assert_eq!(PdfReadTool.name(), "pdf_read");
        assert_eq!(PdfReadTool.tier(), ToolTier::Read);
    }
}
