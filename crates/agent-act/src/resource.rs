//! 统一资源 URL 路由（P-2/§8，借鉴 oh-my-pi `internal-urls`）。
//!
//! 一个 `read(url)` 工具（见 [`crate::tools::ReadTool`]）背后挂一张 scheme→handler 路由：
//! 每个 [`Resource`] 处理一个 scheme（`file://`/`http://`/`https://` 现有；
//! `skill://`/`memory://`/`artifact://` 等由装配方追加）。
//! 裸路径（无 `scheme://`）按 `file://` 处理。`immutable` 标志预留（控制能否被 agent 改写）。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::CONTENT_TYPE;
use sha2::{Digest, Sha256};

const MAX_HTTP_BYTES: usize = 1024 * 1024;

/// 解析出来的资源（借鉴 oh-my-pi `InternalResource`）。
#[derive(Debug)]
pub struct ResourceDoc {
    pub url: String,
    pub content: String,
    /// "text/plain" | "text/markdown" | "application/json"。
    pub content_type: &'static str,
    /// true=不可被 agent 编辑（如技能、机器生成的摘要）。v1 预留不强用。
    pub immutable: bool,
}

/// 资源处理器（借鉴 oh-my-pi `ProtocolHandler`）：每个 scheme 一个。
#[async_trait]
pub trait Resource: Send + Sync {
    /// 处理的 scheme（不带 `://`）。
    fn scheme(&self) -> &str;
    /// 本 scheme 资源是否不可编辑。
    fn immutable(&self) -> bool {
        false
    }
    /// 把 `scheme://` 之后的部分解析成内容。
    async fn resolve(&self, rest: &str) -> anyhow::Result<ResourceDoc>;
}

/// scheme → handler 路由（借鉴 oh-my-pi router）。
#[derive(Default)]
pub struct ResourceRouter {
    handlers: HashMap<String, Arc<dyn Resource>>,
}

impl ResourceRouter {
    pub fn new() -> Self {
        Self::default()
    }
    /// 装一个 handler；同 scheme 后者覆盖前者。链式可用。
    pub fn register(&mut self, h: Arc<dyn Resource>) -> &mut Self {
        self.handlers.insert(h.scheme().to_string(), h);
        self
    }
    /// 解析一个 url（裸路径按 `file://`）。未知 scheme 返回清晰错误。
    pub async fn resolve(&self, url: &str) -> anyhow::Result<ResourceDoc> {
        let (scheme, rest) = split_url(url);
        match self.handlers.get(scheme) {
            Some(h) => h.resolve(rest).await,
            None => Err(anyhow::anyhow!("unknown resource scheme: {scheme}://")),
        }
    }
}

/// 拆 `scheme://rest`；无 `://` 的裸路径归 `file`。
fn split_url(url: &str) -> (&str, &str) {
    match url.find("://") {
        Some(i) => (&url[..i], &url[i + 3..]),
        None if url.starts_with("blob:") => ("blob", &url["blob:".len()..]),
        None => ("file", url),
    }
}

/// 单一归一化入口（§2.12，Postel「宽进严判」）：把 `file://` 之后的路径段（或裸路径）
/// 收敛成统一形态——**策略检查与实际读取都调它**，消灭「两套规范化」的正确性/安全错位。
///
/// 吃下所有畸形：反斜杠转正斜杠、剥 `\\?\` 长路径前缀、折叠前导斜杠后按是否含盘符区分
/// 绝对/相对——`/D:/x`→`D:/x`（啰嗦但正确的绝对路径）、`/Cargo.toml`→`Cargo.toml`
/// （多斜杠 → 折成 workdir 相对）。选择器后缀（`:N`/`:raw`/`:A-B`）原样保留给 `FileSelector`。
pub fn normalize_file_path(after_scheme: &str) -> String {
    let slashed = after_scheme.replace('\\', "/");
    // 剥 Windows 长路径前缀 \\?\ （转斜杠后为 //?/）。
    let body = slashed.strip_prefix("//?/").unwrap_or(slashed.as_str());
    // 折叠所有前导斜杠：剩余以盘符（X:）开头则为绝对盘符路径，否则视为 workdir 相对。
    // 这样 `/D:/...`→`D:/...`（绝对）、`/Cargo.toml`→`Cargo.toml`（相对），消灭「第三个斜杠
    // 被当绝对路径」的多斜杠 bug；真正越界的盘符路径（如 C:\Users\...）仍是绝对，由 workdir 判越界。
    body.trim_start_matches('/').to_string()
}

/// `file://` 处理器：读文本文件（裸路径也走这里）。
pub struct FileResource;

#[async_trait]
impl Resource for FileResource {
    fn scheme(&self) -> &str {
        "file"
    }
    async fn resolve(&self, path: &str) -> anyhow::Result<ResourceDoc> {
        // §2.12：先过单一归一化入口（与策略检查同一函数），再切选择器。
        let normalized = normalize_file_path(path);
        let selector = FileSelector::parse(&normalized)?;
        let content = tokio::fs::read_to_string(selector.path).await.map_err(|e| {
            anyhow::anyhow!(
                "读取 {} 失败: {e}。提示：本地文件请用相对 workdir 的路径（如 \"Cargo.toml\"），不要加 file:// 或前导 /",
                selector.path
            )
        })?;
        let content = selector.render(&content)?;
        Ok(ResourceDoc {
            url: format!("file://{normalized}"),
            content,
            content_type: "text/plain",
            immutable: false,
        })
    }
}

#[async_trait]
impl crate::context::ContextSource for FileResource {
    fn scheme(&self) -> &str {
        "file"
    }

    fn facets(&self) -> crate::context::ContextFacets {
        use crate::context::*;
        // 文件 = 当前世界态、可自由改、按路径精确查；无全局目录可常驻。
        ContextFacets {
            trust: Trust::Observation,
            volatility: Volatility::WorldState,
            retrieval: Retrieval::ExactKey,
            writeback: Writeback::Free,
        }
    }

    /// `file://` 永不常驻——没有「所有文件」的全局蒸馏目录。
    async fn handle(&self, _budget: usize) -> Option<crate::context::Handle> {
        None
    }

    async fn expand(&self, query: &str) -> anyhow::Result<ResourceDoc> {
        Resource::resolve(self, query).await
    }
}

/// Strip a trailing read selector from a file path, if present.
///
/// Supported selectors are `:raw`, `:N`, and `:A-B`. The final colon is ignored
/// when it is the Windows drive separator (`C:\...` / `C:/...`).
pub fn strip_file_selector(path: &str) -> &str {
    match split_file_selector(path) {
        Some((base, _)) => base,
        None => path,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileMode {
    Hashline,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineSelection {
    All,
    One(usize),
    Range(usize, usize),
}

#[derive(Debug, Clone, Copy)]
struct FileSelector<'a> {
    path: &'a str,
    mode: FileMode,
    lines: LineSelection,
}

impl<'a> FileSelector<'a> {
    fn parse(raw: &'a str) -> anyhow::Result<Self> {
        let Some((path, suffix)) = split_file_selector(raw) else {
            return Ok(Self {
                path: raw,
                mode: FileMode::Hashline,
                lines: LineSelection::All,
            });
        };
        if suffix == "raw" {
            return Ok(Self {
                path,
                mode: FileMode::Raw,
                lines: LineSelection::All,
            });
        }
        let lines = parse_line_selection(suffix)?;
        Ok(Self {
            path,
            mode: FileMode::Hashline,
            lines,
        })
    }

    fn render(&self, content: &str) -> anyhow::Result<String> {
        if self.mode == FileMode::Raw {
            return Ok(content.to_string());
        }
        let lines: Vec<&str> = content.lines().collect();
        let selected = selected_line_bounds(self.lines, lines.len())?;
        let mut out = String::new();
        for idx in selected {
            if !out.is_empty() {
                out.push('\n');
            }
            let line_no = idx + 1;
            let line = lines[idx];
            let hash = hashline(self.path, line_no, line);
            out.push_str(&format!(
                "¶{}#{} L{} | {}",
                display_file_path(self.path),
                hash,
                line_no,
                line
            ));
        }
        Ok(out)
    }
}

fn split_file_selector(path: &str) -> Option<(&str, &str)> {
    let idx = path.rfind(':')?;
    if idx == 1 && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let suffix = &path[idx + 1..];
    let is_selector = suffix == "raw"
        || suffix.parse::<usize>().is_ok()
        || suffix
            .split_once('-')
            .is_some_and(|(a, b)| a.parse::<usize>().is_ok() && b.parse::<usize>().is_ok());
    is_selector.then_some((&path[..idx], suffix))
}

fn parse_line_selection(suffix: &str) -> anyhow::Result<LineSelection> {
    if let Some((start, end)) = suffix.split_once('-') {
        let start = start.parse::<usize>()?;
        let end = end.parse::<usize>()?;
        if start == 0 || end == 0 || start > end {
            anyhow::bail!("invalid line range selector: :{suffix}");
        }
        return Ok(LineSelection::Range(start, end));
    }
    let line = suffix.parse::<usize>()?;
    if line == 0 {
        anyhow::bail!("line selector is 1-based: :0 is invalid");
    }
    Ok(LineSelection::One(line))
}

fn selected_line_bounds(
    selection: LineSelection,
    total: usize,
) -> anyhow::Result<std::ops::Range<usize>> {
    match selection {
        LineSelection::All => Ok(0..total),
        LineSelection::One(line) => {
            if line > total {
                anyhow::bail!("line selector :{line} is outside file with {total} lines");
            }
            Ok(line - 1..line)
        }
        LineSelection::Range(start, end) => {
            if start > total {
                anyhow::bail!(
                    "line range selector :{start}-{end} starts outside file with {total} lines"
                );
            }
            let end = end.min(total);
            Ok(start - 1..end)
        }
    }
}

pub fn hashline_hash(path: &str, line_no: usize, line: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(display_file_path(path));
    hasher.update([0]);
    hasher.update(line_no.to_string());
    hasher.update([0]);
    hasher.update(line);
    let digest = hasher.finalize();
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn hashline(path: &str, line_no: usize, line: &str) -> String {
    hashline_hash(path, line_no, line)
}

fn display_file_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// `http://` / `https://` 处理器：只读抓取文本资源。
pub struct HttpResource {
    scheme: &'static str,
    client: reqwest::Client,
    max_bytes: usize,
}

impl HttpResource {
    pub fn http() -> Self {
        Self::new("http")
    }

    pub fn https() -> Self {
        Self::new("https")
    }

    fn new(scheme: &'static str) -> Self {
        // 总超时兜底：read(http://…) 无 per-call 超时，慢/挂死端点会卡住整个 turn（自主运行无人解救）。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            scheme,
            client,
            max_bytes: MAX_HTTP_BYTES,
        }
    }

    fn classify_content_type(raw: Option<&str>) -> &'static str {
        let raw = raw.unwrap_or_default().to_ascii_lowercase();
        if raw.contains("json") {
            "application/json"
        } else if raw.contains("markdown") || raw.contains("md") {
            "text/markdown"
        } else {
            "text/plain"
        }
    }
}

#[async_trait]
impl Resource for HttpResource {
    fn scheme(&self) -> &str {
        self.scheme
    }

    fn immutable(&self) -> bool {
        true
    }

    async fn resolve(&self, rest: &str) -> anyhow::Result<ResourceDoc> {
        let url = format!("{}://{}", self.scheme, rest);
        let response = self.client.get(&url).send().await?.error_for_status()?;
        if let Some(len) = response.content_length() {
            anyhow::ensure!(
                len <= self.max_bytes as u64,
                "http resource too large: {len} bytes > {} bytes",
                self.max_bytes
            );
        }
        let content_type = Self::classify_content_type(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
        );
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            bytes.len() <= self.max_bytes,
            "http resource too large: {} bytes > {} bytes",
            bytes.len(),
            self.max_bytes
        );
        Ok(ResourceDoc {
            url,
            content: String::from_utf8_lossy(&bytes).into_owned(),
            content_type,
            immutable: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn split_url_handles_scheme_and_bare() {
        assert_eq!(split_url("file://Cargo.toml"), ("file", "Cargo.toml"));
        assert_eq!(split_url("skill://foo"), ("skill", "foo"));
        assert_eq!(split_url("blob:sha256:abc"), ("blob", "sha256:abc"));
        assert_eq!(
            split_url("https://example.com/a"),
            ("https", "example.com/a")
        );
        assert_eq!(split_url("Cargo.toml"), ("file", "Cargo.toml"));
        assert_eq!(split_url("a/b.txt"), ("file", "a/b.txt"));
    }

    #[tokio::test]
    async fn file_resource_reads_via_router_both_forms() {
        let mut r = ResourceRouter::new();
        r.register(Arc::new(FileResource));
        // 本 crate 一定有 Cargo.toml。
        let bare = r.resolve("Cargo.toml").await.expect("裸路径应可读");
        let url = r
            .resolve("file://Cargo.toml")
            .await
            .expect("file:// 应可读");
        assert!(bare.content.contains("agent-act"));
        assert_eq!(bare.content, url.content, "两种写法内容一致");
    }

    #[tokio::test]
    async fn file_resource_defaults_to_hashline_snapshot() {
        let dir = std::env::temp_dir().join(format!("botobot-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();

        let doc = FileResource
            .resolve(file.to_str().unwrap())
            .await
            .expect("file should read");

        assert!(doc.content.contains("¶"));
        assert!(doc.content.contains("#"));
        assert!(doc.content.contains("L1 | alpha"));
        assert!(doc.content.contains("L2 | beta"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn file_resource_supports_line_selectors_and_raw() {
        let dir = std::env::temp_dir().join(format!("botobot-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let path = file.to_str().unwrap();

        let one = FileResource
            .resolve(&format!("{path}:2"))
            .await
            .expect("line selector should read");
        assert!(!one.content.contains("alpha"));
        assert!(one.content.contains("L2 | beta"));
        assert!(!one.content.contains("gamma"));

        let range = FileResource
            .resolve(&format!("{path}:2-3"))
            .await
            .expect("range selector should read");
        assert!(range.content.contains("L2 | beta"));
        assert!(range.content.contains("L3 | gamma"));

        let raw = FileResource
            .resolve(&format!("{path}:raw"))
            .await
            .expect("raw selector should read");
        assert_eq!(raw.content, "alpha\nbeta\ngamma\n");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn normalize_file_path_folds_malformed_inputs() {
        // §2.12 实战瀑布的各种畸形写法 → 统一形态。
        // 多斜杠：第三个斜杠不再被当绝对路径，折成 workdir 相对。
        assert_eq!(normalize_file_path("/Cargo.toml"), "Cargo.toml");
        assert_eq!(normalize_file_path("///AGENTS.md"), "AGENTS.md");
        // 啰嗦但正确的盘符绝对路径：前导斜杠折掉，盘符保留为绝对。
        assert_eq!(
            normalize_file_path("/D:/botobot/Cargo.toml"),
            "D:/botobot/Cargo.toml"
        );
        // 反斜杠 → 正斜杠；越界盘符路径仍是绝对（交给 workdir 判越界）。
        assert_eq!(
            normalize_file_path("/C:\\Users\\botobot\\Cargo.toml"),
            "C:/Users/botobot/Cargo.toml"
        );
        // 裸相对路径原样。
        assert_eq!(
            normalize_file_path("crates/foo/src/lib.rs"),
            "crates/foo/src/lib.rs"
        );
        // 裸盘符绝对路径原样（无前导斜杠）。
        assert_eq!(normalize_file_path("D:/botobot/x"), "D:/botobot/x");
        // Windows 长路径前缀 \\?\ 被剥除。
        assert_eq!(normalize_file_path("\\\\?\\D:\\botobot\\x"), "D:/botobot/x");
        // 选择器后缀原样保留给 FileSelector。
        assert_eq!(normalize_file_path("/D:/foo.rs:12"), "D:/foo.rs:12");
        assert_eq!(normalize_file_path("/Cargo.toml:raw"), "Cargo.toml:raw");
    }

    #[tokio::test]
    async fn file_resource_reads_via_multislash_and_drive_forms() {
        // 同一文件多种啰嗦写法都应读到同一内容（消灭试错瀑布）。
        let dir = std::env::temp_dir().join(format!("botobot-norm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let abs = file.to_str().unwrap().replace('\\', "/"); // e.g. C:/.../note.txt

        let plain = FileResource.resolve(&abs).await.expect("裸绝对应可读");
        // 前面塞一个多余斜杠（模型常见写法 file:/// 剥后的样子）。
        let slashed = FileResource
            .resolve(&format!("/{abs}"))
            .await
            .expect("前导斜杠的绝对路径应折叠后可读");
        assert!(plain.content.contains("hello"));
        assert_eq!(plain.content, slashed.content, "啰嗦写法与裸写法内容一致");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strip_file_selector_leaves_windows_drive_colon() {
        assert_eq!(strip_file_selector("C:/repo/file.rs"), "C:/repo/file.rs");
        assert_eq!(strip_file_selector("C:/repo/file.rs:12"), "C:/repo/file.rs");
        assert_eq!(strip_file_selector("src/lib.rs:1-3"), "src/lib.rs");
        assert_eq!(strip_file_selector("src/lib.rs:raw"), "src/lib.rs");
    }

    #[tokio::test]
    async fn file_context_source_never_resident() {
        use crate::context::{ContextSource, Trust, Volatility, Writeback};
        let f = ContextSource::facets(&FileResource);
        assert_eq!(f.trust, Trust::Observation);
        assert_eq!(f.volatility, Volatility::WorldState);
        assert_eq!(f.writeback, Writeback::Free);
        // file:// 永不常驻。
        assert!(ContextSource::handle(&FileResource, 9999).await.is_none());
        // expand == Resource::resolve（本 crate 一定有 Cargo.toml）。
        let doc = ContextSource::expand(&FileResource, "Cargo.toml")
            .await
            .unwrap();
        assert!(doc.content.contains("agent-act"));
    }

    #[tokio::test]
    async fn unknown_scheme_errors_clearly() {
        let r = ResourceRouter::new();
        let e = r.resolve("memory://x").await.unwrap_err().to_string();
        assert!(e.contains("memory"), "应报未知 scheme");
    }

    #[tokio::test]
    async fn http_resource_reads_text_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"ok\":true}\n",
                )
                .await
                .unwrap();
        });

        let mut router = ResourceRouter::new();
        router.register(Arc::new(HttpResource::http()));
        let doc = router
            .resolve(&format!("http://{addr}/health"))
            .await
            .expect("http:// resource should resolve");

        assert_eq!(doc.content, "{\"ok\":true}\n");
        assert_eq!(doc.content_type, "application/json");
        assert!(doc.immutable);
    }
}
