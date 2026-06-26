//! 嵌入的 webui 静态资源 handler。
//!
//! 编译期 `include_dir!` 把整棵 webui 编进二进制；按 URL 路径在内存中查找文件，
//! 找不到时 fallback 到 `index.html`（SPA 行为）。
//!
//! 独立成 crate（不在 `bot-api` 内）的原因：`bot-api` 只管 WS 协议与 session 生命周期，
//! 不该知道 webui 资源在哪。资源归 `webui-bin`（"UI 资源 + 二进制入口"），在装配 router
//! 时由调用方把 [`webui_handler`] 当 axum fallback 挂上即可。
//!
//! 缓存策略（避免用户被迫 Ctrl+F5 清缓存）：
//!   - 指纹 = 参与指纹的嵌入资源内容的哈希，**内容变才变**（即每次有效编译后变化）。
//!   - `index.html` 在服务时把各资源引用改写为 `style.css?v=<指纹>` 等，并以 `no-cache`
//!     下发（始终重新校验，体积小）；浏览器加载到的资源 URL 因 `?v=` 变化而被当成新资源。
//!   - 其余资源（带 `?v=`）以 `immutable` 长缓存下发：URL 一变即重新拉取，不变则永久命中。
//!   - `req.uri().path()` 已剥离 query，故 `?v=` 不影响文件查找。

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Response, StatusCode, header};
use include_dir::{Dir, include_dir};
use std::sync::OnceLock;

pub static WEBUI: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/webui");

/// 需要被注入 `?v=` 指纹的资源引用（index.html 里以 `"name"` 形式出现）。
const VERSIONED_ASSETS: &[&str] = &["style.css", "app.js", "tailwind.js", "marked.min.js"];

const NO_CACHE: &str = "no-cache";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// 编译指纹：对全部参与指纹的资源内容做一次哈希，进程内只算一次。
fn build_fingerprint() -> &'static str {
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // index.html 也纳入：模板自身变化时同样要击穿缓存。
        for name in std::iter::once("index.html").chain(VERSIONED_ASSETS.iter().copied()) {
            if let Some(f) = WEBUI.get_file(name) {
                f.contents().hash(&mut hasher);
            }
        }
        format!("{:016x}", hasher.finish())
    })
}

/// 改写后的 index.html（资源引用带 `?v=<指纹>`），进程内只算一次。
fn index_html() -> &'static [u8] {
    static HTML: OnceLock<Vec<u8>> = OnceLock::new();
    HTML.get_or_init(|| {
        let raw = WEBUI
            .get_file("index.html")
            .map(|f| f.contents())
            .unwrap_or_default();
        let mut s = String::from_utf8_lossy(raw).into_owned();
        let fp = build_fingerprint();
        for asset in VERSIONED_ASSETS {
            // 精确匹配带引号的引用，避免误伤注释/文本里出现的同名子串。
            s = s.replace(&format!("\"{asset}\""), &format!("\"{asset}?v={fp}\""));
        }
        s.into_bytes()
    })
    .as_slice()
}

fn build(status: StatusCode, mime: &str, cache: &str, body: Body) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .body(body)
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap()
        })
}

fn serve_index() -> Response<Body> {
    build(
        StatusCode::OK,
        "text/html; charset=utf-8",
        NO_CACHE,
        Body::from(index_html()),
    )
}

pub async fn webui_handler(req: Request) -> Response<Body> {
    let path = req.uri().path().trim_start_matches('/');
    if path.is_empty() || path == "index.html" {
        return serve_index();
    }
    match WEBUI.get_file(path) {
        Some(f) => build(
            StatusCode::OK,
            guess_mime(f.path()),
            IMMUTABLE,
            Body::from(f.contents()),
        ),
        // SPA fallback：仅对“无扩展名”的导航路径回退 index.html；带扩展名的资源
        // 未命中应是真 404（避免把缺失的 .js 当 HTML 返回，导致脚本加载错误）。
        None if !has_extension(path) => serve_index(),
        None => build(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            NO_CACHE,
            Body::empty(),
        ),
    }
}

/// 末段（最后一个 `/` 之后）是否含 `.` —— 粗判这是带扩展名的资源请求。
fn has_extension(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .map(|seg| seg.contains('.'))
        .unwrap_or(false)
}

fn guess_mime(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}
