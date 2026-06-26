//! tools：可复用叶子工具集（`agent-act` 的子 module）。
//!
//! 每个工具只 `impl crate::TypedTool`，永不关心流式（不变式 #1）。
//!
//! 装配入口：[`register_all`] 把所有默认叶子工具注册到传入的 `ToolRegistry`，
//! 由上层（agent-loop / webui-bin）控制调用时机。新增工具时只在此函数内追加一行。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::patch::ApplyPatchTool;
use crate::resource::{FileResource, HttpResource, ResourceRouter};
use crate::search::{FindTool, SearchTool};
use crate::shell::{ShellCommandArgs, ShellCommandTool, run_shell_command};
use crate::{ToolRegistry, TypedTool};
use base_types::{Tool, ToolLoadMode, ToolResult, ToolTier};

#[derive(Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// 要读取的文件路径（相对当前工作目录或绝对路径）。
    pub path: String,
}

#[derive(Serialize)]
pub struct ReadFileOut {
    pub path: String,
    pub bytes: usize,
    pub content: String,
}

/// Search discoverable tools and activate matching tools for the next reasoning step.
pub struct ToolSearchTool {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolSearchTool {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut tools: Vec<_> = tools
            .into_iter()
            .filter(|t| t.load_mode() == ToolLoadMode::Discoverable)
            .collect();
        tools.sort_by(|a, b| a.name().cmp(b.name()));
        Self { tools }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Search discoverable tools by name, summary, and description. Returned tools are activated for the next reasoning step."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keywords describing the tool capability to find." },
                "limit": { "type": "integer", "description": "Maximum number of tools to return. Defaults to 5, capped at 10." }
            },
            "required": ["query"]
        })
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    async fn call(&self, args: Value) -> ToolResult {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(5)
            .clamp(1, 10);

        let mut matches: Vec<(usize, Arc<dyn Tool>)> = self
            .tools
            .iter()
            .filter_map(|tool| {
                let score = tool_search_score(tool.as_ref(), query);
                (score > 0).then(|| (score, tool.clone()))
            })
            .collect();
        matches.sort_by(|(a_score, a), (b_score, b)| {
            b_score.cmp(a_score).then_with(|| a.name().cmp(b.name()))
        });
        matches.truncate(limit);

        let activated: Vec<String> = matches
            .iter()
            .map(|(_, tool)| tool.name().to_string())
            .collect();
        let tools: Vec<Value> = matches
            .into_iter()
            .map(|(_, tool)| {
                json!({
                    "name": tool.name(),
                    "summary": tool.summary(),
                    "description": tool.description(),
                    "tier": format!("{:?}", tool.tier()).to_lowercase(),
                })
            })
            .collect();

        Ok(json!({
            "query": query,
            "activated": activated,
            "tools": tools,
        }))
    }
}

fn tool_search_score(tool: &dyn Tool, query: &str) -> usize {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return 1;
    }
    let name = tool.name().to_lowercase();
    let summary = tool.summary().to_lowercase();
    let description = tool.description().to_lowercase();
    let mut score = 0usize;
    for term in query.split_whitespace() {
        if name == term {
            score += 10;
        } else if name.contains(term) {
            score += 6;
        }
        if summary.contains(term) {
            score += 3;
        }
        if description.contains(term) {
            score += 2;
        }
    }
    score
}

/// 读取一个文本文件的内容。
pub struct ReadFile;

#[async_trait]
impl TypedTool for ReadFile {
    type Args = ReadFileArgs;
    type Out = ReadFileOut;

    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a UTF-8 text file at the given path."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    async fn run(&self, args: Self::Args) -> anyhow::Result<Self::Out> {
        let content = tokio::fs::read_to_string(&args.path).await?;
        Ok(ReadFileOut {
            path: args.path,
            bytes: content.len(),
            content,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// Search query.
    pub query: String,
    /// Maximum number of results to return. Defaults to 5, capped at 8.
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Serialize)]
pub struct WebSearchOut {
    pub query: String,
    pub results: Vec<WebSearchResult>,
}

/// Lightweight web search via DuckDuckGo HTML results. It is intentionally
/// dependency-light: enough to ground browsing, not a full search backend.
pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Self {
        // 总超时兜底（无 per-call 参数）：防 duckduckgo 慢/挂死阻塞整个 turn（自主运行无人解救）。
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TypedTool for WebSearchTool {
    type Args = WebSearchArgs;
    type Out = WebSearchOut;

    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web for current public information. Input: query string and optional limit."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    async fn run(&self, args: Self::Args) -> anyhow::Result<Self::Out> {
        let limit = args.limit.unwrap_or(5).clamp(1, 8);
        let url = format!(
            "https://duckduckgo.com/html/?q={}",
            encode_query(&args.query)
        );
        let html = self
            .client
            .get(url)
            .header("user-agent", "botobot/0.1")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let results = parse_duckduckgo_results(&html, limit);
        Ok(WebSearchOut {
            query: args.query,
            results,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CodeExecutionArgs {
    /// Command to execute in the local shell.
    pub command: String,
    /// Optional timeout in milliseconds. Defaults to 5000 and is capped at 600000 (10 min);
    /// set high for builds/tests like `cargo build`/`cargo test`.
    pub timeout_ms: Option<u64>,
    /// Optional output byte cap. Defaults to 12000 and is capped at 64000.
    pub max_output_bytes: Option<usize>,
}

#[derive(Serialize)]
pub struct CodeExecutionOut {
    pub command: String,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_total_bytes: usize,
    pub stdout_total_lines: usize,
    pub stdout_truncated: bool,
    pub stderr_total_bytes: usize,
    pub stderr_total_lines: usize,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// Execute a short local shell command. This is an Exec-tier tool; callers can
/// still block it with policy.
pub struct CodeExecutionTool;

#[async_trait]
impl TypedTool for CodeExecutionTool {
    type Args = CodeExecutionArgs;
    type Out = CodeExecutionOut;

    fn name(&self) -> &str {
        "code_execution"
    }
    fn description(&self) -> &str {
        "Execute a short local shell command for code or environment inspection. Use only when Code is enabled."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    async fn run(&self, args: Self::Args) -> anyhow::Result<Self::Out> {
        let ctx = base_types::ToolCtx {
            session_id: String::new(),
            run_id: String::new(),
            parent_id: None,
            workdir: std::env::current_dir()?,
            cancel: tokio_util::sync::CancellationToken::new(),
            depth: 0,
            max_depth: 0,
            token_budget: None,
            llm_opts: Default::default(),
        };
        let out = run_shell_command(
            ShellCommandArgs {
                command: args.command,
                cwd: None,
                env: None,
                timeout_ms: args.timeout_ms,
                max_output_bytes: args.max_output_bytes,
            },
            &ctx,
        )
        .await?;
        Ok(CodeExecutionOut {
            command: out.command,
            status: out.status,
            stdout: out.stdout,
            stderr: out.stderr,
            stdout_total_bytes: out.stdout_total_bytes,
            stdout_total_lines: out.stdout_total_lines,
            stdout_truncated: out.stdout_truncated,
            stderr_total_bytes: out.stderr_total_bytes,
            stderr_total_lines: out.stderr_total_lines,
            stderr_truncated: out.stderr_truncated,
            timed_out: out.timed_out,
        })
    }
}

/// Arguments for `http_request`.
#[derive(Deserialize, JsonSchema)]
pub struct HttpRequestArgs {
    /// HTTP method. Defaults to GET. Use POST/PUT/PATCH/DELETE for write-like requests.
    pub method: Option<String>,
    /// Target HTTP(S) URL.
    pub url: String,
    /// Optional request headers, for example Authorization or Content-Type.
    pub headers: Option<BTreeMap<String, String>>,
    /// Optional raw request body.
    pub body: Option<String>,
    /// Optional timeout in milliseconds. Defaults to 10000 and is capped at 30000.
    pub timeout_ms: Option<u64>,
    /// Optional response body byte cap. Defaults to 64000 and is capped at 262144.
    pub max_output_bytes: Option<usize>,
}

#[derive(Serialize)]
pub struct HttpRequestOut {
    pub method: String,
    pub url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    pub body_total_bytes: usize,
    pub body_total_lines: usize,
    pub truncated: bool,
}

/// Send an HTTP(S) request, including POST/auth-style requests. This is Exec
/// tier because it may mutate external systems.
pub struct HttpRequestTool {
    client: reqwest::Client,
}

impl HttpRequestTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TypedTool for HttpRequestTool {
    type Args = HttpRequestArgs;
    type Out = HttpRequestOut;

    fn name(&self) -> &str {
        "http_request"
    }
    fn description(&self) -> &str {
        "Send an HTTP(S) request with optional method, headers, body, timeout, and response cap. Use only when Code is enabled."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }
    async fn run(&self, args: Self::Args) -> anyhow::Result<Self::Out> {
        let timeout_ms = args.timeout_ms.unwrap_or(10_000).clamp(1, 30_000);
        let max_output = args.max_output_bytes.unwrap_or(64_000).clamp(1, 262_144);
        let url = reqwest::Url::parse(&args.url)?;
        match url.scheme() {
            "http" | "https" => {}
            scheme => anyhow::bail!("http_request only supports http/https URLs, got {scheme:?}"),
        }

        let method = args.method.unwrap_or_else(|| "GET".into()).to_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        // reqwest 请求级超时**覆盖整个请求（含响应体读取）**——仅 tokio::timeout(send) 不挡慢响应体挂死。
        let mut req = self
            .client
            .request(method.clone(), url.clone())
            .timeout(Duration::from_millis(timeout_ms));
        if let Some(headers) = args.headers {
            for (name, value) in headers {
                req = req.header(
                    reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
                    reqwest::header::HeaderValue::from_str(&value)?,
                );
            }
        }
        if let Some(body) = args.body {
            req = req.body(body);
        }

        let response = timeout(Duration::from_millis(timeout_ms), req.send()).await??;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or("<non-utf8>").to_string(),
                )
            })
            .collect();
        let (body, body_total_bytes, body_total_lines, truncated) =
            read_response_body_capped(response, max_output, Duration::from_millis(timeout_ms))
                .await?;

        Ok(HttpRequestOut {
            method: method.as_str().to_string(),
            url: url.to_string(),
            status,
            headers,
            body,
            body_total_bytes,
            body_total_lines,
            truncated,
        })
    }
}

/// 统一读取工具（P-2/§8）：按 url 路由到资源 handler（裸路径 / `file://` / `http(s)://`，后续 `skill://`、`memory://`…）。
/// 取代裸 `read_file`——一个 `read` + 路由器即"核心+辅助递归组合"。
pub struct ReadTool {
    router: Arc<ResourceRouter>,
}

impl ReadTool {
    pub fn new(router: Arc<ResourceRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a resource by URL or file path. For LOCAL FILES, write a plain path relative to the \
         working directory (e.g. \"Cargo.toml\" or \"crates/foo/src/lib.rs\") — do NOT use file:// or a \
         leading slash. Reserve scheme URLs for non-file resources: http://, https://, skill://, book://, \
         artifact://, blob:sha256:, memory:// (when those handlers are registered). \
         File reads return hashline anchors by default and support :N, :A-B, and :raw selectors."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "For local files: a path relative to the working directory (e.g. \"Cargo.toml\"), optionally with :N, :A-B, or :raw — no file:// and no leading slash. For other resources: a scheme URL like https://example.com/page, skill://name, book://name#node, artifact://id, blob:sha256:<hex>, or memory://query." }
            },
            "required": ["url"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read: missing 'url' argument"))?;
        let doc = self.router.resolve(url).await?;
        Ok(json!({ "url": doc.url, "content": doc.content }))
    }
}

/// 构造默认资源路由（`file://` + `http(s)://`）。后续 scheme（skill/memory/artifact）在此追加。
pub fn default_router() -> ResourceRouter {
    let mut router = ResourceRouter::new();
    router.register(Arc::new(FileResource));
    router.register(Arc::new(HttpResource::http()));
    router.register(Arc::new(HttpResource::https()));
    router
}

/// 用给定路由注册 `read` 工具（让装配方可塞入 skill://、memory:// 等 handler）。
pub fn register_read(reg: &mut ToolRegistry, router: Arc<ResourceRouter>) -> &mut ToolRegistry {
    reg.register(Arc::new(ReadTool::new(router)));
    reg
}

/// 把所有默认叶子工具注册到传入的注册器（默认 `file://` + `http(s)://`）。
pub fn register_all(reg: &mut ToolRegistry) -> &mut ToolRegistry {
    register_read(reg, Arc::new(default_router()));
    reg.register(Arc::new(SearchTool));
    reg.register(Arc::new(FindTool));
    reg.register(Arc::new(ApplyPatchTool));
    reg.register(Arc::new(crate::edit::EditByHashlineTool));
    reg.register(Arc::new(crate::rename::RenameFileTool));
    reg.register(Arc::new(crate::lsp::LspTool));
    reg.register(Arc::new(crate::dap::DapTool));
    reg.register(Arc::new(crate::dap_session::DebugTool));
    reg.register(Arc::new(crate::todo::TodoWriteTool));
    reg.register_typed(WebSearchTool::new());
    reg.register(Arc::new(ShellCommandTool));
    reg.register_typed(CodeExecutionTool);
    reg.register_typed(HttpRequestTool::new());
    reg
}

async fn read_response_body_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
    deadline: Duration,
) -> anyhow::Result<(String, usize, usize, bool)> {
    let mut out = Vec::new();
    let mut total_bytes = 0usize;
    let mut newline_count = 0usize;
    let mut last_byte = None;
    let mut truncated = false;
    loop {
        let chunk = timeout(deadline, response.chunk()).await??;
        let Some(chunk) = chunk else {
            break;
        };
        total_bytes += chunk.len();
        newline_count += chunk.iter().filter(|b| **b == b'\n').count();
        last_byte = chunk.last().copied().or(last_byte);
        let remaining = max_bytes.saturating_sub(out.len());
        if chunk.len() > remaining {
            out.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            continue;
        }
        if !truncated {
            out.extend_from_slice(&chunk);
        }
    }
    let body = String::from_utf8_lossy(&out).into_owned();
    let total_lines = if total_bytes == 0 {
        0
    } else if last_byte == Some(b'\n') {
        newline_count
    } else {
        newline_count + 1
    };
    Ok((body, total_bytes, total_lines, truncated))
}

fn encode_query(query: &str) -> String {
    let mut out = String::new();
    for b in query.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_duckduckgo_results(html: &str, limit: usize) -> Vec<WebSearchResult> {
    let mut results = Vec::new();
    let mut cursor = 0;
    while results.len() < limit {
        let Some(rel) = html[cursor..].find("result__a") else {
            break;
        };
        let start = cursor + rel;
        let anchor_start = html[..start].rfind("<a").unwrap_or(start);
        let Some(anchor_end_rel) = html[start..].find("</a>") else {
            break;
        };
        let anchor_end = start + anchor_end_rel + 4;
        let anchor = &html[anchor_start..anchor_end];
        let title = html_text(anchor);
        let url = extract_attr(anchor, "href")
            .map(clean_duckduckgo_url)
            .unwrap_or_default();
        cursor = anchor_end;
        let snippet = html[cursor..]
            .find("result__snippet")
            .and_then(|s| {
                let s = cursor + s;
                let snippet_start = html[..s]
                    .rfind("<a")
                    .or_else(|| html[..s].rfind("<div"))
                    .unwrap_or(s);
                let end = html[snippet_start..]
                    .find("</a>")
                    .or_else(|| html[snippet_start..].find("</div>"))?;
                Some(html_text(&html[snippet_start..snippet_start + end]))
            })
            .unwrap_or_default();
        if !title.is_empty() || !url.is_empty() {
            results.push(WebSearchResult {
                title,
                url,
                snippet,
            });
        }
    }
    results
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    for needle in [format!("{name}=\""), format!("{name}='")] {
        if let Some(hit) = tag.find(&needle) {
            let start = hit + needle.len();
            let quote = needle.chars().last()?;
            let end = tag[start..].find(quote)?;
            return Some(html_decode(&tag[start..start + end]));
        }
    }
    None
}

fn clean_duckduckgo_url(url: String) -> String {
    if let Some(i) = url.find("uddg=") {
        let encoded = &url[i + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        return percent_decode(&encoded[..end]);
    }
    url
}

fn html_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    html_decode(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_decode(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_result_links() {
        let html = r#"
          <div class="result">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc">
              Example &amp; Result
            </a>
            <a class="result__snippet">A <b>short</b> snippet &amp; detail.</a>
          </div>
        "#;
        let results = parse_duckduckgo_results(html, 3);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example & Result");
        assert_eq!(results[0].url, "https://example.com/a?x=1");
        assert_eq!(results[0].snippet, "A short snippet & detail.");
    }

    #[test]
    fn query_encoding_keeps_safe_bytes() {
        assert_eq!(encode_query("rust async?"), "rust+async%3F");
    }

    #[tokio::test]
    async fn http_request_sends_post_headers_body_and_caps_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0_u8; 1024];
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                assert_ne!(n, 0, "client closed before sending full request");
                buf.extend_from_slice(&tmp[..n]);
                let request = String::from_utf8_lossy(&buf);
                let Some(header_end) = request.find("\r\n\r\n") else {
                    continue;
                };
                let content_len = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buf.len() >= header_end + 4 + content_len {
                    assert!(request.starts_with("POST /api HTTP/1.1"));
                    assert!(request.lines().any(|line| {
                        let Some((name, value)) = line.split_once(':') else {
                            return false;
                        };
                        name.eq_ignore_ascii_case("authorization") && value.trim() == "Bearer test"
                    }));
                    assert!(request.ends_with("ping"));
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 201 Created\r\ncontent-type: text/plain\r\ncontent-length: 6\r\n\r\nabcdef",
                )
                .await
                .unwrap();
        });

        let mut headers = BTreeMap::new();
        headers.insert("authorization".into(), "Bearer test".into());
        headers.insert("content-type".into(), "text/plain".into());
        let out = HttpRequestTool::new()
            .run(HttpRequestArgs {
                method: Some("POST".into()),
                url: format!("http://{addr}/api"),
                headers: Some(headers),
                body: Some("ping".into()),
                timeout_ms: Some(5_000),
                max_output_bytes: Some(4),
            })
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(out.status, 201);
        assert_eq!(out.body, "abcd");
        assert_eq!(out.body_total_bytes, 6);
        assert_eq!(out.body_total_lines, 1);
        assert!(out.truncated);
    }

    #[tokio::test]
    async fn code_execution_reports_tail_and_total_counts() {
        #[cfg(windows)]
        let command = "1..5 | ForEach-Object { \"line$_\" }";
        #[cfg(not(windows))]
        let command = "printf 'line1\\nline2\\nline3\\nline4\\nline5\\n'";

        let out = CodeExecutionTool
            .run(CodeExecutionArgs {
                command: command.into(),
                timeout_ms: Some(5_000),
                max_output_bytes: Some(12),
            })
            .await
            .unwrap();

        assert_eq!(out.status, Some(0));
        assert!(out.stdout_truncated);
        assert!(out.stdout.contains("tail"));
        assert!(out.stdout_total_bytes > 12);
        assert!(out.stdout_total_lines >= 5);
        assert_eq!(out.stderr_total_bytes, 0);
        assert!(!out.stderr_truncated);
    }
}
