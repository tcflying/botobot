//! §4.6 步② 上半：启动 Chrome（`--remote-debugging-port`）并发现 CDP `webSocketDebuggerUrl`。
//! 移植 `.oni/agent-browser` 的进程启动思路，精简为「找 chrome → spawn → 轮询 `/json/version`」。
//!
//! **feature-gated**（`browser`）。**可单测核心**：候选路径枚举 + `/json/version` JSON 解析；
//! **运行验证待真 Chrome**（spawn + HTTP 轮询需机器装 Chrome/Edge）。

use std::path::PathBuf;
use std::time::Duration;

/// 常见 Chrome/Edge/Chromium 可执行候选（按平台）。定位：先 `BOTOBOT_CHROME` env，再这些。
pub fn chrome_candidates() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("BOTOBOT_CHROME") {
        if !p.trim().is_empty() {
            return vec![PathBuf::from(p)];
        }
    }
    #[cfg(target_os = "windows")]
    let raw = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    #[cfg(target_os = "macos")]
    let raw = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let raw = [
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
    ];
    raw.iter().map(PathBuf::from).collect()
}

/// 找到第一个存在的候选 chrome 路径。
pub fn find_chrome() -> Option<PathBuf> {
    chrome_candidates().into_iter().find(|p| p.exists())
}

/// 从 `/json/version` 响应体解析 `webSocketDebuggerUrl`。
pub fn parse_ws_endpoint(json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get("webSocketDebuggerUrl")?
        .as_str()
        .map(str::to_string)
}

/// 启动 Chrome headless 并返回其 CDP `webSocketDebuggerUrl`。
/// 需真 Chrome（运行验证待环境）。`port`=remote debugging 端口。
pub async fn launch(port: u16) -> Result<(tokio::process::Child, String), String> {
    let chrome = find_chrome()
        .ok_or_else(|| "未找到 Chrome/Edge（设 BOTOBOT_CHROME 指定路径）".to_string())?;
    let child = tokio::process::Command::new(&chrome)
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--headless=new")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-gpu")
        // drop 即杀：CDP 就绪轮询超时返回 Err 时，child 在此被 drop——无此则 headless Chrome 泄漏成孤儿。
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("启动 Chrome 失败: {e}"))?;
    // 轮询 /json/version 直到 CDP 就绪（最多 ~5s）。
    let url = format!("http://127.0.0.1:{port}/json/version");
    let client = reqwest::Client::new();
    for _ in 0..50 {
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(body) = resp.text().await {
                if let Some(ws) = parse_ws_endpoint(&body) {
                    return Ok((child, ws));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("Chrome CDP 端点未就绪（/json/version 超时）".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_nonempty_and_env_override() {
        assert!(!chrome_candidates().is_empty(), "应有平台候选路径");
        // SAFETY: 测试串行设置/清除自有 env key。
        unsafe { std::env::set_var("BOTOBOT_CHROME", "/custom/chrome") };
        let c = chrome_candidates();
        unsafe { std::env::remove_var("BOTOBOT_CHROME") };
        assert_eq!(
            c,
            vec![PathBuf::from("/custom/chrome")],
            "env 覆盖应优先且唯一"
        );
    }

    #[test]
    fn parse_ws_endpoint_extracts_url() {
        let json = r#"{"Browser":"Chrome/120","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/abc"}"#;
        assert_eq!(
            parse_ws_endpoint(json).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc")
        );
        // 缺字段 / 坏 JSON → None。
        assert_eq!(parse_ws_endpoint(r#"{"Browser":"x"}"#), None);
        assert_eq!(parse_ws_endpoint("not json"), None);
    }
}
