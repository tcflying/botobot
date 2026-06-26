//! botobot：CLI 入口（参数/stdin → 装配 → 走 serve 或 run_once）。
//!
//! 用法（二进制名 `bots`）：
//!   bots                         # 无参数 → 启动 Web UI（serve）
//!   bots serve                   # 显式启动 Web UI
//!   bots mcp                     # 启动 stdio MCP server
//!   bots gc [--apply]            # §2.9④ 清理 .bot/artifacts 孤儿工件（默认 dry-run）
//!   bots "把 docs/now.md 读出来讲讲"     # 带参数 → 单次 CLI
//!   bots --image shot.png "这张图里有什么?"
//! 端点默认本地 unsloth Qwen3.6（见 config.rs / config.example.toml）。

use base_types::ContentPart;
use base64::Engine;
use std::io::Write;
use tracing_subscriber::prelude::*;
use webui_bin::bot::Bot;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    // 日志级别：`BOTOBOT_LOG=info|debug|trace` → botobot::* 那组 target（事件分级按此过滤）；
    // 也认 `RUST_LOG`（更细粒度）；默认只显 botobot 自己的 info 及以上。
    // trace = 含 token/reasoning 流式增量；info = 生命周期+动作；warn/error = 异常/终态。
    let filter = std::env::var("BOTOBOT_LOG")
        .ok()
        .map(|l| format!("botobot={l}"))
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "botobot=info".into());
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(filter))
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(bot_api::logs::BotobotLogLayer)
        .init();

    let first = std::env::args().nth(1);
    // §2.9 ④ 孤儿 GC：不装配 Bot（纯文件维护），扫 `.bot/sessions` 引用后 sweep `.bot/artifacts`。
    // 默认 dry-run；`bots gc --apply` 实删。
    if first.as_deref() == Some("gc") {
        let apply = std::env::args().any(|a| a == "--apply");
        return webui_bin::gc::run_gc(".bot", apply);
    }
    let bot = Bot::from_env().await?;
    if first.is_none() || first.as_deref() == Some("serve") {
        return bot.serve().await;
    }
    if first.as_deref() == Some("mcp") {
        return bot.run_mcp_stdio().await;
    }

    let parts = read_input()?;
    bot.run_once(parts).await
}

/// 解析参数：`--image <path>`（可重复）转为 data-URL 图像，其余拼成文本。
/// 无参数时从 stdin 读一行作为文本。
fn read_input() -> anyhow::Result<Vec<ContentPart>> {
    let mut images: Vec<String> = Vec::new();
    let mut words: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--image" | "-i" => {
                let path = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--image 需要一个路径参数"))?;
                images.push(image_data_url(&path)?);
            }
            _ => words.push(a),
        }
    }

    let text = if !words.is_empty() {
        words.join(" ")
    } else if images.is_empty() {
        eprint!("> ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim().to_string()
    } else {
        String::new()
    };

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(ContentPart::Text(text));
    }
    for url in images {
        parts.push(ContentPart::ImageUrl(url));
    }
    Ok(parts)
}

/// 读图像文件并编码为 `data:<mime>;base64,...`。
fn image_data_url(path: &str) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mime = match path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}
