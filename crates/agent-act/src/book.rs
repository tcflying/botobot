//! Read-only book resources backed by markdown files.
//!
//! Books are institutional or policy documents. The prompt gets only a compact
//! mechanical table of contents; full body text is loaded through `read`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::resource::{Resource, ResourceDoc};

#[derive(Debug, Clone)]
pub struct Book {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
struct BookNode {
    title: String,
    level: usize,
    path: String,
    summary: String,
    start_line: usize,
    end_line: usize,
}

pub fn load_books(dir: impl AsRef<Path>) -> Vec<Book> {
    let dir = dir.as_ref();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push(Book {
            name: stem.to_string(),
            path,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn books_prompt(books: &[Book]) -> Option<String> {
    if books.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Books (MUST authority)\nBooks are read-only policy/context documents. Do not contradict them. Read full sections with `read(book://<name>#<node_path>)`; reason over the cross-book outline with `read(book://?)`, or semantic-search with `read(book://?<query>)`.\n",
    );
    for book in books {
        let Ok(raw) = std::fs::read_to_string(&book.path) else {
            continue;
        };
        out.push_str(&format!("\n### book://{}\n", book.name));
        out.push_str(&render_tree(&raw));
    }
    Some(out)
}

/// §1.8.8 S5：book 语义索引一条 = 一个 section 的标题+摘要嵌入 + citation 指针。
struct BookChunk {
    book: String,
    path: String,
    title: String,
    vec: Vec<f32>,
}

pub struct BookResource {
    paths: HashMap<String, PathBuf>,
    /// §1.8.8 S5：语义索引（注入 embedder 后构建）。与标题树**并存**——语义只做粗筛，
    /// 命中回 `read(book://<name>#<path>)` 取正文 + citation（§1.5 Book 检索纪律）。
    embedder: Mutex<Option<Arc<dyn base_types::Embedder>>>,
    index: Mutex<Vec<BookChunk>>,
}

impl BookResource {
    pub fn new(books: &[Book]) -> Self {
        Self {
            paths: books
                .iter()
                .map(|b| (b.name.clone(), b.path.clone()))
                .collect(),
            embedder: Mutex::new(None),
            index: Mutex::new(Vec::new()),
        }
    }

    /// §1.8.8 S5：注入嵌入器并构建语义索引（解析各书 section、嵌入「标题 + 摘要」）。
    /// 装配时后台 bge 加载完调用（与 MemoryStore::set_embedder 同模式）。
    pub fn set_embedder(&self, embedder: Arc<dyn base_types::Embedder>) {
        let mut chunks: Vec<(String, String, String, String)> = Vec::new(); // book, path, title, text
        let mut names: Vec<&String> = self.paths.keys().collect();
        names.sort();
        for name in names {
            let Some(p) = self.paths.get(name) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(p) else {
                continue;
            };
            for node in parse_nodes(&raw) {
                let text = if node.summary.is_empty() {
                    node.title.clone()
                } else {
                    format!("{} {}", node.title, node.summary)
                };
                chunks.push((name.clone(), node.path, node.title, text));
            }
        }
        if !chunks.is_empty() {
            let texts: Vec<&str> = chunks.iter().map(|c| c.3.as_str()).collect();
            if let Ok(vecs) = embedder.embed(&texts) {
                let index: Vec<BookChunk> = chunks
                    .into_iter()
                    .zip(vecs)
                    .map(|((book, path, title, _), vec)| BookChunk {
                        book,
                        path,
                        title,
                        vec,
                    })
                    .collect();
                *self.index.lock().unwrap() = index;
            }
        }
        *self.embedder.lock().unwrap() = Some(embedder);
    }

    /// §4.6 knowledge-tech / PageIndex 式 reasoning 检索的底座：跨书**节点 outline**
    /// `(citation=book#path, title, level)`，按书名排序、书内按出现序。零 embedding 依赖——
    /// 供 agent（主 LLM）对 outline 推理选择相关节点，再 `read(book://<cite>)` 取正文 + citation
    /// （映射 §1.5「Book = 结构树 + 推理式检索」，对齐 `.oni/PageIndex` 的 tree index 思路）。
    pub fn outline(&self) -> Vec<(String, String, usize)> {
        let mut names: Vec<&String> = self.paths.keys().collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let Some(p) = self.paths.get(name) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(p) else {
                continue;
            };
            for node in parse_nodes(&raw) {
                out.push((format!("{name}#{}", node.path), node.title, node.level));
            }
        }
        out
    }

    /// §1.8.8 S5：跨书语义检索，返回 top-K `(score, book, path, title)`（降序）。
    /// 无 embedder/空索引则返回空（调用方回退标题树）。
    pub fn semantic_search(&self, query: &str, top_k: usize) -> Vec<(f32, String, String, String)> {
        let qvec = {
            let emb = self.embedder.lock().unwrap();
            let Some(e) = emb.as_ref() else {
                return Vec::new();
            };
            match e.embed(&[query]) {
                Ok(mut v) => match v.drain(..).next() {
                    Some(q) => q,
                    None => return Vec::new(),
                },
                Err(_) => return Vec::new(),
            }
        };
        let index = self.index.lock().unwrap();
        let mut scored: Vec<(f32, &BookChunk)> = index
            .iter()
            .map(|c| (cosine(&qvec, &c.vec), c))
            .filter(|(s, _)| *s >= 0.3)
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(top_k)
            .map(|(s, c)| (s, c.book.clone(), c.path.clone(), c.title.clone()))
            .collect()
    }
}

/// §1.8.3b 能力提示：按 query 语义粗筛相关书节，回 `book://<book>#<path>` citation 供下钻。
#[async_trait]
impl crate::recall::CapabilityHint for BookResource {
    async fn hint(&self, query: &str, max: usize) -> Vec<crate::recall::CapHint> {
        self.semantic_search(query, max)
            .into_iter()
            .map(|(_, book, path, title)| crate::recall::CapHint {
                kind: "book",
                label: title,
                citation: format!("book://{book}#{path}"),
            })
            .collect()
    }
}

/// 余弦相似度（L2 归一化输入即点积）。
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[async_trait]
impl Resource for BookResource {
    fn scheme(&self) -> &str {
        "book"
    }

    fn immutable(&self) -> bool {
        true
    }

    async fn resolve(&self, rest: &str) -> anyhow::Result<ResourceDoc> {
        // §1.8.8 S5：`book://?<query>` = 跨书语义检索（粗筛 → citation 列表，回 read 取正文）。
        if let Some(query) = rest.strip_prefix('?') {
            let query = query.trim();
            // §4.6 PageIndex：`book://?`（空 query）= 跨书完整 outline，供 agent 推理选择节点。
            if query.is_empty() {
                let outline = self.outline();
                let body = if outline.is_empty() {
                    "(no books)".to_string()
                } else {
                    outline
                        .iter()
                        .map(|(cite, title, level)| {
                            format!(
                                "{}{} — citation=book://{cite}",
                                "  ".repeat(level.saturating_sub(1)),
                                title
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                return Ok(ResourceDoc {
                    url: "book://?".to_string(),
                    content: format!(
                        "[BOOK outline — reason over this index, then open a citation for authoritative text]\n{body}"
                    ),
                    content_type: "text/markdown",
                    immutable: true,
                });
            }
            let hits = self.semantic_search(query, 5);
            let body = if hits.is_empty() {
                "(no semantic match — try the title tree: read(book://<name>))".to_string()
            } else {
                hits.iter()
                    .map(|(s, book, path, title)| {
                        format!("- (score={s:.2}) {title} — citation=book://{book}#{path}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Ok(ResourceDoc {
                url: format!("book://?{query}"),
                content: format!(
                    "[BOOK semantic — coarse filter; open the citation for authoritative text]\n{body}"
                ),
                content_type: "text/markdown",
                immutable: true,
            });
        }
        let (name, node_path) = rest.split_once('#').unwrap_or((rest, ""));
        let Some(path) = self.paths.get(name) else {
            return Err(anyhow::anyhow!("unknown book: {name}"));
        };
        let raw = tokio::fs::read_to_string(path).await?;
        let body = if node_path.trim().is_empty() {
            render_tree(&raw)
        } else {
            section_body(&raw, node_path)
                .ok_or_else(|| anyhow::anyhow!("unknown book node: {name}#{node_path}"))?
        };
        let suffix = if node_path.trim().is_empty() {
            String::new()
        } else {
            format!("#{node_path}")
        };
        // §1.5 read 输出纪律：Book 最高可信、可溯源——带 source/section/citation。
        let section = if node_path.trim().is_empty() {
            "root".to_string()
        } else {
            node_path.to_string()
        };
        let header = format!(
            "[BOOK high-confidence] source={} section={section} citation=book://{name}{suffix}",
            path.display()
        );
        Ok(ResourceDoc {
            url: format!("book://{name}{suffix}"),
            content: format!("{header}\n{body}"),
            content_type: "text/markdown",
            immutable: true,
        })
    }
}

#[async_trait]
impl crate::context::ContextSource for BookResource {
    fn scheme(&self) -> &str {
        "book"
    }

    fn facets(&self) -> crate::context::ContextFacets {
        use crate::context::*;
        // Book 最高可信、不可变依据、结构树检索、只读。
        ContextFacets {
            trust: Trust::Authority,
            volatility: Volatility::Immutable,
            retrieval: Retrieval::StructuredTree,
            writeback: Writeback::ReadOnly,
        }
    }

    async fn handle(&self, budget: usize) -> Option<crate::context::Handle> {
        use crate::context::*;
        // §1.8.7「能去翻的」钩子小节：每本书只给「name + 首标题」指针；标题树 + 内容
        // 降为按需 `read(book://<name>)` / `read(book://<name>#<section>)`（渐进披露）。
        if self.paths.is_empty() {
            return None;
        }
        let mut digest = String::from(
            "## Reference books (authoritative; open with read book://<name> then #<section>)\n",
        );
        // 名次稳定：按 name 排序，避免 HashMap 迭代序抖动。
        let mut names: Vec<&String> = self.paths.keys().collect();
        names.sort();
        for name in names {
            let Some(path) = self.paths.get(name) else {
                continue;
            };
            // 只读首个 markdown 标题作为指针标签（读不到则只给名字）。
            let title = tokio::fs::read_to_string(path)
                .await
                .ok()
                .and_then(|raw| raw.lines().find_map(|l| parse_heading(l).map(|(_, t)| t)));
            match title {
                Some(t) => digest.push_str(&format!("- `book://{name}` — {t}\n")),
                None => digest.push_str(&format!("- `book://{name}`\n")),
            }
        }
        let digest = fit_budget(&digest, budget);
        Some(Handle {
            est_tokens: est_tokens(&digest),
            digest,
            trust: Trust::Authority,
        })
    }

    async fn expand(&self, query: &str) -> anyhow::Result<ResourceDoc> {
        Resource::resolve(self, query).await
    }
}

/// §4.6 knowledge-tech / §1.5「Book = 结构树 + 推理式检索」：`book_search`（Read）——
/// 把跨书 outline 喂 LLM 让它**推理挑 1-3 个 section**（PageIndex 思路，非向量；与 `book://?<query>`
/// 的语义粗筛互补），再取这些节的权威正文 + citation 一次返回。无书/无命中则提示走标题树。
pub struct BookSearchTool {
    books: Arc<BookResource>,
    llm: Arc<dyn base_types::Llm>,
}

impl BookSearchTool {
    pub fn new(books: Arc<BookResource>, llm: Arc<dyn base_types::Llm>) -> Self {
        Self { books, llm }
    }
}

#[async_trait]
impl base_types::Tool for BookSearchTool {
    fn name(&self) -> &str {
        "book_search"
    }
    fn description(&self) -> &str {
        "Search institutional/policy books by reasoning over their table of contents: an LLM picks the \
         most relevant sections for your query and returns their authoritative text with citations. \
         Prefer this over guessing section paths. `query` = what you need; optional `max_sections` (default 3)."
    }
    fn tier(&self) -> base_types::ToolTier {
        base_types::ToolTier::Read
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What you need from the books." },
                "max_sections": { "type": "integer", "description": "Max sections to fetch (default 3)." }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> base_types::ToolResult {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("book_search: missing 'query'"))?;
        let max_sections = args
            .get("max_sections")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 5))
            .unwrap_or(3);

        // 全部可选 citation（合法集合）= 跨书 outline。
        let outline = self.books.outline();
        if outline.is_empty() {
            return Ok(serde_json::json!({ "result": "(no books available)" }));
        }
        let valid: std::collections::HashSet<String> =
            outline.iter().map(|(cite, _, _)| cite.clone()).collect();
        let index = outline
            .iter()
            .map(|(cite, title, level)| {
                format!(
                    "{}{title} — book://{cite}",
                    "  ".repeat(level.saturating_sub(1))
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        // LLM 推理选节点。
        let sys = format!(
            "You pick the most relevant book sections for a query by reasoning over a table of contents.\n\
             Below is the cross-book outline; each line ends with its citation `book://<name>#<path>`.\n\
             Choose AT MOST {max_sections} citations most relevant to the user's query. Reply with ONLY the \
             chosen citations, one per line (the `book://<name>#<path>` token), nothing else. If none are \
             relevant, reply with an empty line."
        );
        let msgs = vec![
            base_types::Message::system(sys),
            base_types::Message::user(format!("Outline:\n{index}\n\nQuery: {query}")),
        ];
        let picks = match collect_decision(self.llm.as_ref(), &msgs).await {
            Some(d) => parse_citations(&d.text, &valid, max_sections),
            None => Vec::new(),
        };
        if picks.is_empty() {
            return Ok(serde_json::json!({
                "result": format!("(LLM picked no section; browse the outline via read(book://?) for query: {query})")
            }));
        }

        // 取每个选中节的权威正文。
        let mut sections = Vec::new();
        for cite in &picks {
            // cite 形如 "name#path"；resolve 接受不带 scheme 的 rest。
            if let Ok(doc) = Resource::resolve(self.books.as_ref(), cite).await {
                sections.push(doc.content);
            }
        }
        Ok(serde_json::json!({
            "query": query,
            "citations": picks,
            "result": sections.join("\n\n---\n\n"),
        }))
    }
}

/// 收一次 LLM 决策（取最后的 Done）。
async fn collect_decision(
    llm: &dyn base_types::Llm,
    msgs: &[base_types::Message],
) -> Option<base_types::Decision> {
    use futures::StreamExt;
    let mut stream = llm
        .infer(msgs, &[], &base_types::LlmOpts::default())
        .await
        .ok()?;
    let mut last = None;
    while let Some(ev) = stream.next().await {
        if let Ok(base_types::LlmEvent::Done(d)) = ev {
            last = Some(d);
        }
    }
    last
}

/// 从 LLM 文本里抽合法 citation（`book://name#path` → `name#path`），按 outline 集合校验、去重、封顶。
fn parse_citations(
    text: &str,
    valid: &std::collections::HashSet<String>,
    max: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        let cite = raw.trim().trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '#' && c != '/' && c != '_' && c != '-' && c != '.'
        });
        let cite = cite.strip_prefix("book://").unwrap_or(cite);
        if valid.contains(cite) && !out.contains(&cite.to_string()) {
            out.push(cite.to_string());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

fn render_tree(raw: &str) -> String {
    let nodes = parse_nodes(raw);
    if nodes.is_empty() {
        return "(no markdown headings found)\n".into();
    }
    let mut out = String::new();
    for node in nodes {
        let indent = "  ".repeat(node.level.saturating_sub(1));
        let summary = if node.summary.is_empty() {
            String::new()
        } else {
            format!(" - {}", node.summary)
        };
        out.push_str(&format!(
            "{indent}- `{}` {}{}\n",
            node.path, node.title, summary
        ));
    }
    out
}

fn section_body(raw: &str, wanted_path: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let node = parse_nodes(raw)
        .into_iter()
        .find(|node| node.path == wanted_path)?;
    let body = lines[node.start_line.saturating_sub(1)..node.end_line].join("\n");
    Some(body.trim().to_string())
}

fn parse_nodes(raw: &str) -> Vec<BookNode> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut headings = Vec::<(usize, String, usize)>::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some((level, title)) = parse_heading(line) {
            headings.push((level, title, idx + 1));
        }
    }

    let mut nodes = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut sibling_counts: HashMap<(usize, String), usize> = HashMap::new();
    for (i, (level, title, start_line)) in headings.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|(parent_level, _)| parent_level >= level)
        {
            stack.pop();
        }
        let base = slug(title);
        let parent = stack
            .iter()
            .map(|(_, slug)| slug.as_str())
            .collect::<Vec<_>>()
            .join("/");
        let key = (*level, format!("{parent}/{base}"));
        let count = sibling_counts.entry(key).or_insert(0);
        *count += 1;
        let segment = if *count == 1 {
            base
        } else {
            format!("{base}-{count}")
        };
        stack.push((*level, segment.clone()));
        let path = stack
            .iter()
            .map(|(_, slug)| slug.as_str())
            .collect::<Vec<_>>()
            .join("/");
        let end_line = headings
            .iter()
            .skip(i + 1)
            .find(|(next_level, _, _)| next_level <= level)
            .map(|(_, _, line)| line.saturating_sub(1))
            .unwrap_or(lines.len());
        nodes.push(BookNode {
            title: title.clone(),
            level: *level,
            path,
            summary: first_paragraph(&lines[*start_line..end_line]),
            start_line: *start_line,
            end_line,
        });
    }
    nodes
}

fn parse_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=3).contains(&hashes) {
        return None;
    }
    let rest = trimmed.get(hashes..)?.trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((hashes, rest.trim_end_matches('#').trim().to_string()))
}

fn first_paragraph(lines: &[&str]) -> String {
    let mut out = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !out.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        out.push(trimmed);
    }
    out.join(" ")
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "section".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_uses_heading_paths_and_summaries() {
        let raw = "# Handbook\nTop summary.\n\n## Install\nDo this first.\n\n### Windows\nUse PowerShell.\n";
        let tree = render_tree(raw);
        assert!(tree.contains("`handbook` Handbook - Top summary."));
        assert!(tree.contains("`handbook/install` Install - Do this first."));
        assert!(tree.contains("`handbook/install/windows` Windows - Use PowerShell."));
    }

    #[test]
    fn section_body_returns_until_next_sibling() {
        let raw = "# Handbook\nIntro.\n\n## Install\nA\n\n### Windows\nB\n\n## Run\nC\n";
        let body = section_body(raw, "handbook/install").unwrap();
        assert!(body.contains("## Install"));
        assert!(body.contains("### Windows"));
        assert!(!body.contains("## Run"));
    }

    #[test]
    fn unicode_headings_keep_readable_paths() {
        let raw = "# 制度\n总则。\n\n## 安装 步骤\n正文。\n";
        let tree = render_tree(raw);
        assert!(tree.contains("`制度`"));
        assert!(tree.contains("`制度/安装-步骤`"));
    }

    #[tokio::test]
    async fn book_resource_reads_current_file_contents() {
        let root = std::env::temp_dir().join(format!(
            "botobot-book-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("policy.md");
        std::fs::write(&path, "# Policy\nFirst.\n\n## Rule\nOriginal.\n").unwrap();

        let books = load_books(&root);
        let res = BookResource::new(&books);
        let doc = res.resolve("policy#policy/rule").await.unwrap();
        assert!(doc.content.contains("Original."));

        std::fs::write(&path, "# Policy\nFirst.\n\n## Rule\nUpdated.\n").unwrap();
        let doc = res.resolve("policy#policy/rule").await.unwrap();
        assert!(doc.content.contains("Updated."));

        let _ = std::fs::remove_dir_all(root);
    }

    // §4.6 PageIndex：outline() 跨书节点索引；book://? 空 query 返回 outline 供推理选择。
    #[tokio::test]
    async fn outline_lists_nodes_and_empty_query_returns_index() {
        let root = std::env::temp_dir().join(format!(
            "botobot-bookoutline-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("guide.md"), "# Guide\nTop.\n\n## Setup\nDo it.\n").unwrap();
        let books = load_books(&root);
        let res = BookResource::new(&books);

        let outline = res.outline();
        assert!(
            outline
                .iter()
                .any(|(cite, title, _)| cite == "guide#guide" && title == "Guide")
        );
        assert!(
            outline
                .iter()
                .any(|(cite, title, lvl)| cite == "guide#guide/setup"
                    && title == "Setup"
                    && *lvl == 2)
        );

        // book://?（空 query）→ outline 文档（含 citation，供 agent 推理后 read）。
        let doc = res.resolve("?").await.unwrap();
        assert!(doc.content.contains("BOOK outline"));
        assert!(doc.content.contains("citation=book://guide#guide/setup"));
        assert_eq!(doc.url, "book://?");

        let _ = std::fs::remove_dir_all(root);
    }

    // §4.6 knowledge-tech：book_search 让 LLM 从 outline 推理选节点，取回该节正文 + citation。
    #[tokio::test]
    async fn book_search_llm_selects_section_and_returns_body() {
        use base_types::{Decision, Llm, LlmEvent, LlmOpts, LlmResult, Message, ToolSpec};

        // 脚本化 LLM：忽略输入，回吐固定 citation。
        struct ScriptedLlm(&'static str);
        #[async_trait]
        impl Llm for ScriptedLlm {
            async fn infer(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _o: &LlmOpts,
            ) -> LlmResult<base_types::LlmStream> {
                let d = Decision {
                    text: self.0.to_string(),
                    finish_reason: Some("stop".into()),
                    ..Default::default()
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(LlmEvent::Done(d))])))
            }
        }

        let root = std::env::temp_dir().join(format!(
            "botobot-booksearch-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("guide.md"),
            "# Guide\nTop.\n\n## Setup\nRun the installer.\n\n## Other\nUnrelated stuff.\n",
        )
        .unwrap();
        let books = load_books(&root);
        let res = Arc::new(BookResource::new(&books));
        let llm: Arc<dyn Llm> = Arc::new(ScriptedLlm("book://guide#guide/setup"));
        let tool = BookSearchTool::new(res, llm);

        let out = base_types::Tool::call(&tool, serde_json::json!({"query": "how to install"}))
            .await
            .unwrap();
        let result = out["result"].as_str().unwrap();
        assert!(
            result.contains("Run the installer."),
            "应取回 Setup 正文: {result}"
        );
        assert!(
            !result.contains("Unrelated stuff."),
            "不应含未选中节: {result}"
        );
        assert!(
            out["citations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str() == Some("guide#guide/setup")),
            "citations 应含选中节: {out}"
        );

        // 无书 → 优雅降级。
        let empty = Arc::new(BookResource::new(&[]));
        let tool2 = BookSearchTool::new(empty, Arc::new(ScriptedLlm("x")));
        let out2 = base_types::Tool::call(&tool2, serde_json::json!({"query": "q"}))
            .await
            .unwrap();
        assert!(out2["result"].as_str().unwrap().contains("no books"));

        let _ = std::fs::remove_dir_all(root);
    }

    // §1.8.8 S5：注入 embedder 后 book://?<query> 语义检索返回带 citation 的命中。
    #[tokio::test]
    async fn semantic_search_returns_citations() {
        struct StubEmb;
        impl base_types::Embedder for StubEmb {
            fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
                Ok(texts
                    .iter()
                    .map(|t| {
                        let t = t.to_lowercase();
                        let mut v = [
                            (t.contains("install") || t.contains("安装")) as i32 as f32,
                            (t.contains("run") || t.contains("运行")) as i32 as f32,
                        ];
                        let n = (v[0] * v[0] + v[1] * v[1]).sqrt();
                        if n > 0.0 {
                            for x in &mut v {
                                *x /= n;
                            }
                        } else {
                            v = [0.7, 0.7];
                        }
                        v.to_vec()
                    })
                    .collect())
            }
            fn dim(&self) -> usize {
                2
            }
        }
        let root = std::env::temp_dir().join(format!(
            "botobot-booksem-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("hb.md"),
            "# HB\nintro\n\n## Install\nhow to install it\n\n## Run\nhow to run it\n",
        )
        .unwrap();
        let books = load_books(&root);
        let res = BookResource::new(&books);
        // 无 embedder → 空。
        assert!(res.semantic_search("安装", 5).is_empty());
        res.set_embedder(Arc::new(StubEmb));
        // 语义命中 Install section（而非 Run）。
        let doc = res.resolve("?怎么 install").await.unwrap();
        assert!(
            doc.content.contains("citation=book://hb#hb/install"),
            "got: {}",
            doc.content
        );
        assert!(doc.content.starts_with("[BOOK semantic"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn book_read_has_high_confidence_header_with_citation() {
        let root = std::env::temp_dir().join(format!(
            "botobot-bookprov-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("manual.md"), "# Intro\nhello\n").unwrap();
        let books = load_books(&root);
        let res = BookResource::new(&books);
        let doc = res.resolve("manual#intro").await.unwrap();
        assert!(
            doc.content.starts_with("[BOOK high-confidence]"),
            "got: {}",
            doc.content
        );
        assert!(doc.content.contains("citation=book://manual#intro"));
        assert!(doc.content.contains("section=intro"));
        // 读整树 section=root。
        let tree = res.resolve("manual").await.unwrap();
        assert!(tree.content.contains("section=root"));
        let _ = std::fs::remove_dir_all(root);
    }
}
