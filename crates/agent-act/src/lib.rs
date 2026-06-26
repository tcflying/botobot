//! act-tech：动作切入点 [`base_types::Tool`] 的实现层（"手"的装配）。
//!
//! - Core = [`registry::ToolRegistry`]（按名查找，impl [`base_types::ToolLookup`]）。
//! - 辅助 = [`typed::TypedTool`]（作者面强类型）+ [`typed::Erased`]（擦成 `dyn Tool`）+ schemars 自动 schema。
//! - 装配入口 = `ToolRegistry::new` + `register*`。
//!
//! `Tool` / `ToolResult` / `ToolLookup` 契约在 [`base_types`]，本 crate re-export。

pub use base_types::{Tool, ToolLookup, ToolResult};

// §1.8.3④ 向量 ANN（feature `hnsw` 才拉 instant-distance）。
#[cfg(feature = "hnsw")]
pub mod ann;
pub mod artifact;
pub mod background;
pub mod book;
pub mod browser;
pub mod compact;
pub mod context;
pub mod dap;
pub mod dap_session;
pub mod dap_wire;
pub mod edit;
pub mod embed;
pub mod env;
pub mod episode;
pub mod lsp;
pub mod market;
pub mod memory;
pub mod officecli;
pub mod output;
// §4.8 PDF 解读（feature `pdf` 才拉 pdf-inspector）。
pub mod patch;
#[cfg(feature = "pdf")]
pub mod pdf;
pub mod recall;
mod registry;
pub mod rename;
pub mod resource;
pub mod search;
pub mod shell;
pub mod skill;
pub mod todo;
pub mod tools;
mod typed;

pub use registry::ToolRegistry;
pub use typed::{Erased, TypedTool};
