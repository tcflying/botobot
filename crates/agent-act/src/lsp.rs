use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LspOp {
    Status,
    Diagnostics,
    References,
    Rename,
    CodeActions,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LspArgs {
    pub op: LspOp,
    /// Optional subdirectory under the workspace for status/diagnostics, or file path for semantic ops.
    pub path: Option<String>,
    /// Optional file path for references/rename/code_actions. Defaults to path when omitted.
    pub file: Option<String>,
    /// 1-based line for references/rename/code_actions.
    pub line: Option<u32>,
    /// 1-based column for references/rename/code_actions.
    pub column: Option<u32>,
    /// Optional 1-based range end line for code_actions.
    pub end_line: Option<u32>,
    /// Optional 1-based range end column for code_actions.
    pub end_column: Option<u32>,
    /// New symbol name for rename.
    pub new_name: Option<String>,
    /// Optional timeout for diagnostics. Defaults to 30000ms and is capped at 120000ms.
    pub timeout_ms: Option<u64>,
    /// Optional maximum diagnostic count. Defaults to 100 and is capped at 500.
    pub max_diagnostics: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct LspStatusOut {
    pub workspace: String,
    pub rust: RustStatus,
}

#[derive(Debug, Serialize)]
pub struct RustStatus {
    pub cargo_toml: bool,
    pub cargo_available: bool,
    pub cargo_version: Option<String>,
    pub rust_analyzer_available: bool,
    pub rust_analyzer_version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LspDiagnosticsOut {
    pub workspace: String,
    pub command: String,
    pub status: Option<i32>,
    pub diagnostics: Vec<LspDiagnostic>,
    pub truncated: bool,
    pub stderr: String,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub level: String,
    pub message: String,
    pub code: Option<String>,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct LspReferencesOut {
    pub workspace: String,
    pub file: String,
    pub position: LspPosition,
    pub references: Vec<LspLocation>,
}

#[derive(Debug, Serialize)]
pub struct LspRenameOut {
    pub workspace: String,
    pub file: String,
    pub position: LspPosition,
    pub new_name: String,
    pub workspace_edit: Value,
}

#[derive(Debug, Serialize)]
pub struct LspCodeActionsOut {
    pub workspace: String,
    pub file: String,
    pub range: LspRange,
    pub actions: Vec<LspCodeActionSummary>,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspLocation {
    pub uri: String,
    pub path: Option<String>,
    pub range: LspRange,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspPosition {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LspCodeActionSummary {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: Option<bool>,
}

pub struct LspTool;

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "IDE-style workspace semantics. Supports status, Rust diagnostics, and rust-analyzer references/rename/code_actions."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(LspArgs)).unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: LspArgs = serde_json::from_value(args)?;
        run_lsp(&ctx.workdir, args).await.map(|out| json!(out))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: LspArgs = serde_json::from_value(args)?;
        run_lsp(&std::env::current_dir()?, args)
            .await
            .map(|out| json!(out))
    }
}

pub async fn run_lsp(workdir: &Path, args: LspArgs) -> anyhow::Result<Value> {
    let workspace = workspace_root(workdir);
    let target = resolve_under_workdir(&workspace, args.path.as_deref().unwrap_or("."))?;
    let deadline = Duration::from_millis(args.timeout_ms.unwrap_or(30_000).clamp(1, 120_000));
    match args.op {
        LspOp::Status => Ok(json!(status(&target).await)),
        LspOp::Diagnostics => Ok(json!(diagnostics(&target, args).await?)),
        LspOp::References => timeout(deadline, references(&workspace, args))
            .await
            .map_err(|_| {
                anyhow::anyhow!("lsp references timed out after {}ms", deadline.as_millis())
            })?,
        LspOp::Rename => timeout(deadline, rename(&workspace, args))
            .await
            .map_err(|_| {
                anyhow::anyhow!("lsp rename timed out after {}ms", deadline.as_millis())
            })?,
        LspOp::CodeActions => timeout(deadline, code_actions(&workspace, args))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "lsp code_actions timed out after {}ms",
                    deadline.as_millis()
                )
            })?,
    }
}

async fn status(workspace: &Path) -> LspStatusOut {
    let cargo_version = command_output(
        workspace,
        "cargo",
        &["--version"],
        Duration::from_millis(5_000),
    )
    .await
    .ok()
    .and_then(|out| (out.status == Some(0)).then_some(out.stdout.trim().to_string()));
    let rust_analyzer_version = command_output(
        workspace,
        "rust-analyzer",
        &["--version"],
        Duration::from_millis(5_000),
    )
    .await
    .ok()
    .and_then(|out| (out.status == Some(0)).then_some(out.stdout.trim().to_string()));
    LspStatusOut {
        workspace: display_path(workspace),
        rust: RustStatus {
            cargo_toml: workspace.join("Cargo.toml").is_file(),
            cargo_available: cargo_version.is_some(),
            cargo_version,
            rust_analyzer_available: rust_analyzer_version.is_some(),
            rust_analyzer_version,
        },
    }
}

async fn diagnostics(workspace: &Path, args: LspArgs) -> anyhow::Result<LspDiagnosticsOut> {
    let timeout_ms = args.timeout_ms.unwrap_or(30_000).clamp(1, 120_000);
    let max = args.max_diagnostics.unwrap_or(100).clamp(1, 500);
    anyhow::ensure!(
        workspace.join("Cargo.toml").is_file(),
        "diagnostics currently requires a Rust Cargo.toml workspace"
    );
    let output = command_output(
        workspace,
        "cargo",
        &["check", "--message-format=json"],
        Duration::from_millis(timeout_ms),
    )
    .await?;
    let mut diagnostics = parse_cargo_diagnostics(&output.stdout);
    let truncated = diagnostics.len() > max;
    diagnostics.truncate(max);
    Ok(LspDiagnosticsOut {
        workspace: display_path(workspace),
        command: "cargo check --message-format=json".into(),
        status: output.status,
        diagnostics,
        truncated,
        stderr: output.stderr,
        timed_out: output.timed_out,
    })
}

async fn references(workspace: &Path, args: LspArgs) -> anyhow::Result<Value> {
    let target = LspFileTarget::from_args(workspace, &args).await?;
    let mut client = LspClient::start(workspace).await?;
    client.did_open(&target).await?;
    client.wait_after_open().await;
    let result = client
        .request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": target.uri },
                "position": target.position_json,
                "context": { "includeDeclaration": true }
            }),
        )
        .await?;
    client.shutdown().await.ok();
    let references = result
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(location_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!(LspReferencesOut {
        workspace: display_path(workspace),
        file: display_path(&target.path),
        position: target.position,
        references,
    }))
}

async fn rename(workspace: &Path, args: LspArgs) -> anyhow::Result<Value> {
    let new_name = args
        .new_name
        .clone()
        .ok_or_else(|| anyhow::anyhow!("rename requires new_name"))?;
    let target = LspFileTarget::from_args(workspace, &args).await?;
    let mut client = LspClient::start(workspace).await?;
    client.did_open(&target).await?;
    client.wait_after_open().await;
    let workspace_edit = client
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": target.uri },
                "position": target.position_json,
                "newName": new_name,
            }),
        )
        .await?;
    client.shutdown().await.ok();
    Ok(json!(LspRenameOut {
        workspace: display_path(workspace),
        file: display_path(&target.path),
        position: target.position,
        new_name,
        workspace_edit,
    }))
}

async fn code_actions(workspace: &Path, args: LspArgs) -> anyhow::Result<Value> {
    let target = LspFileTarget::from_args(workspace, &args).await?;
    let end_line = args.end_line.unwrap_or(target.position.line);
    let end_column = args.end_column.unwrap_or(target.position.column);
    let range = LspRange {
        start: target.position.clone(),
        end: one_based_position(end_line, end_column)?,
    };
    let mut client = LspClient::start(workspace).await?;
    client.did_open(&target).await?;
    client.wait_after_open().await;
    let raw = client
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": target.uri },
                "range": range_to_lsp_json(&range),
                "context": { "diagnostics": [] }
            }),
        )
        .await?;
    client.shutdown().await.ok();
    let actions = raw
        .as_array()
        .map(|items| items.iter().filter_map(code_action_summary).collect())
        .unwrap_or_default();
    Ok(json!(LspCodeActionsOut {
        workspace: display_path(workspace),
        file: display_path(&target.path),
        range,
        actions,
        raw,
    }))
}

pub async fn will_rename_files(
    workspace: &Path,
    old_path: &Path,
    new_path: &Path,
    deadline: Duration,
) -> anyhow::Result<Value> {
    timeout(deadline, async {
        let workspace = workspace_root(workspace);
        let mut client = LspClient::start(&workspace).await?;
        let result = client
            .request(
                "workspace/willRenameFiles",
                json!({
                    "files": [{
                        "oldUri": file_uri(old_path),
                        "newUri": file_uri(new_path),
                    }]
                }),
            )
            .await?;
        client.shutdown().await.ok();
        Ok(result)
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "lsp workspace/willRenameFiles timed out after {}ms",
            deadline.as_millis()
        )
    })?
}

pub async fn did_rename_files(
    workspace: &Path,
    old_path: &Path,
    new_path: &Path,
    deadline: Duration,
) -> anyhow::Result<()> {
    timeout(deadline, async {
        let workspace = workspace_root(workspace);
        let mut client = LspClient::start(&workspace).await?;
        client
            .notify(
                "workspace/didRenameFiles",
                json!({
                    "files": [{
                        "oldUri": file_uri(old_path),
                        "newUri": file_uri(new_path),
                    }]
                }),
            )
            .await?;
        client.shutdown().await.ok();
        Ok(())
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "lsp workspace/didRenameFiles timed out after {}ms",
            deadline.as_millis()
        )
    })?
}

#[derive(Debug)]
struct CommandOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

async fn command_output(
    cwd: &Path,
    program: &str,
    args: &[&str],
    deadline: Duration,
) -> anyhow::Result<CommandOutput> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = cmd.spawn()?;
    match timeout(deadline, child.wait_with_output()).await {
        Ok(output) => {
            let output = output?;
            Ok(CommandOutput {
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: false,
            })
        }
        Err(_) => Ok(CommandOutput {
            status: None,
            stdout: String::new(),
            stderr: format!("command timed out after {}ms", deadline.as_millis()),
            timed_out: true,
        }),
    }
}

struct LspFileTarget {
    path: PathBuf,
    uri: String,
    text: String,
    position: LspPosition,
    position_json: Value,
}

impl LspFileTarget {
    async fn from_args(workspace: &Path, args: &LspArgs) -> anyhow::Result<Self> {
        let raw_file = args
            .file
            .as_deref()
            .or(args.path.as_deref())
            .ok_or_else(|| anyhow::anyhow!("semantic lsp ops require file or path"))?;
        let path = resolve_under_workdir(workspace, raw_file)?;
        anyhow::ensure!(
            path.is_file(),
            "lsp file does not exist: {}",
            path.display()
        );
        let line = args
            .line
            .ok_or_else(|| anyhow::anyhow!("semantic lsp ops require 1-based line"))?;
        let column = args
            .column
            .ok_or_else(|| anyhow::anyhow!("semantic lsp ops require 1-based column"))?;
        let position = one_based_position(line, column)?;
        let text = tokio::fs::read_to_string(&path).await?;
        Ok(Self {
            uri: file_uri(&path),
            path,
            text,
            position_json: position_to_lsp_json(&position),
            position,
        })
    }
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LspClient {
    async fn start(workspace: &Path) -> anyhow::Result<Self> {
        let mut child = Command::new("rust-analyzer");
        child
            .current_dir(workspace)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = child.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("rust-analyzer stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("rust-analyzer stdout unavailable"))?;
        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        client
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "clientInfo": { "name": "botobot" },
                    "rootUri": file_uri(workspace),
                    "rootPath": display_path(workspace),
                    "workspaceFolders": [{
                        "uri": file_uri(workspace),
                        "name": workspace
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("workspace")
                    }],
                    "capabilities": {
                        "workspace": {
                            "configuration": false,
                            "workspaceEdit": { "documentChanges": true },
                            "fileOperations": {
                                "willRename": true,
                                "didRename": true
                            }
                        },
                        "textDocument": {
                            "codeAction": { "dynamicRegistration": false },
                            "rename": { "dynamicRegistration": false },
                            "references": { "dynamicRegistration": false }
                        }
                    }
                }),
            )
            .await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    async fn did_open(&mut self, target: &LspFileTarget) -> anyhow::Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": target.uri,
                    "languageId": language_id(&target.path),
                    "version": 1,
                    "text": target.text,
                }
            }),
        )
        .await
    }

    async fn wait_after_open(&self) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let message = self.read().await?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    anyhow::bail!("lsp {method} failed: {error}");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if message.get("id").is_some() && message.get("method").is_some() {
                let response_id = message.get("id").cloned().unwrap_or(Value::Null);
                let result = server_request_result(&message);
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": response_id,
                    "result": result,
                }))
                .await?;
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.request("shutdown", Value::Null).await?;
        self.notify("exit", Value::Null).await?;
        let _ = self.child.wait().await;
        Ok(())
    }

    async fn send(&mut self, message: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_vec(message)?;
        self.stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        self.stdin.write_all(&body).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read(&mut self) -> anyhow::Result<Value> {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).await?;
            anyhow::ensure!(n != 0, "rust-analyzer closed stdout");
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
        let len =
            content_length.ok_or_else(|| anyhow::anyhow!("lsp message missing Content-Length"))?;
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

fn server_request_result(message: &Value) -> Value {
    match message.get("method").and_then(Value::as_str) {
        Some("workspace/configuration") => {
            let count = message
                .get("params")
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            Value::Array((0..count).map(|_| json!({})).collect())
        }
        _ => Value::Null,
    }
}

fn parse_cargo_diagnostics(text: &str) -> Vec<LspDiagnostic> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("reason").and_then(Value::as_str) == Some("compiler-message"))
        .filter_map(|value| value.get("message").cloned())
        .filter_map(cargo_message_to_diagnostic)
        .collect()
}

fn cargo_message_to_diagnostic(message: Value) -> Option<LspDiagnostic> {
    let level = message.get("level")?.as_str()?.to_string();
    let text = message.get("message")?.as_str()?.to_string();
    let code = message
        .get("code")
        .and_then(|code| code.get("code"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let primary = message
        .get("spans")
        .and_then(Value::as_array)?
        .iter()
        .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true));
    let (file, line, column) = match primary {
        Some(span) => (
            span.get("file_name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            span.get("line_start")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            span.get("column_start")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
        ),
        None => (None, None, None),
    };
    Some(LspDiagnostic {
        level,
        message: text,
        code,
        file,
        line,
        column,
    })
}

fn location_from_value(value: &Value) -> Option<LspLocation> {
    let uri = value.get("uri")?.as_str()?.to_string();
    let range = range_from_value(value.get("range")?)?;
    Some(LspLocation {
        path: path_from_file_uri(&uri),
        uri,
        range,
    })
}

fn code_action_summary(value: &Value) -> Option<LspCodeActionSummary> {
    Some(LspCodeActionSummary {
        title: value.get("title")?.as_str()?.to_string(),
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        is_preferred: value.get("isPreferred").and_then(Value::as_bool),
    })
}

fn range_from_value(value: &Value) -> Option<LspRange> {
    Some(LspRange {
        start: position_from_value(value.get("start")?)?,
        end: position_from_value(value.get("end")?)?,
    })
}

fn position_from_value(value: &Value) -> Option<LspPosition> {
    Some(LspPosition {
        line: value.get("line")?.as_u64()?.checked_add(1)? as u32,
        column: value.get("character")?.as_u64()?.checked_add(1)? as u32,
    })
}

fn one_based_position(line: u32, column: u32) -> anyhow::Result<LspPosition> {
    anyhow::ensure!(line > 0, "line must be 1-based and greater than 0");
    anyhow::ensure!(column > 0, "column must be 1-based and greater than 0");
    Ok(LspPosition { line, column })
}

fn position_to_lsp_json(position: &LspPosition) -> Value {
    json!({
        "line": position.line - 1,
        "character": position.column - 1,
    })
}

fn range_to_lsp_json(range: &LspRange) -> Value {
    json!({
        "start": position_to_lsp_json(&range.start),
        "end": position_to_lsp_json(&range.end),
    })
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        _ => "plaintext",
    }
}

fn file_uri(path: &Path) -> String {
    let path = workspace_root(path);
    let mut raw = slash_path_without_windows_prefix(&path);
    if let Some(stripped) = raw.strip_prefix("//?/UNC/") {
        raw = format!("//{stripped}");
    } else if let Some(stripped) = raw.strip_prefix("//?/") {
        raw = stripped.to_string();
    }
    if raw.starts_with('/') {
        format!("file://{}", percent_encode_uri_path(&raw))
    } else {
        format!("file:///{}", percent_encode_uri_path(&raw))
    }
}

fn display_path(path: &Path) -> String {
    let slash = slash_path_without_windows_prefix(path);
    #[cfg(windows)]
    {
        slash.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        slash
    }
}

fn slash_path_without_windows_prefix(path: &Path) -> String {
    let mut raw = path.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = raw.strip_prefix("//?/UNC/") {
        raw = format!("//{stripped}");
    } else if let Some(stripped) = raw.strip_prefix("//?/") {
        raw = stripped.to_string();
    }
    raw
}

pub(crate) fn path_from_file_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = percent_decode_uri_path(rest);
    #[cfg(windows)]
    {
        let path = path.trim_start_matches('/');
        Some(path.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        Some(path)
    }
}

fn percent_encode_uri_path(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode_uri_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn resolve_under_workdir(workdir: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        workdir.join(raw_path)
    };
    let target = std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize(joined));
    if !target.starts_with(workdir) {
        anyhow::bail!(
            "path escapes workdir: {raw} (workdir: {})",
            workdir.display()
        );
    }
    Ok(target)
}

fn workspace_root(workdir: &Path) -> PathBuf {
    std::fs::canonicalize(workdir).unwrap_or_else(|_| normalize(workdir))
}

fn normalize(path: impl AsRef<Path>) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.as_ref().components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_compiler_messages() {
        let line = serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "cannot find value `x` in this scope",
                "code": { "code": "E0425" },
                "spans": [{
                    "file_name": "src/lib.rs",
                    "line_start": 3,
                    "column_start": 9,
                    "is_primary": true
                }]
            }
        });
        let parsed = parse_cargo_diagnostics(&line.to_string());
        assert_eq!(
            parsed,
            vec![LspDiagnostic {
                level: "error".into(),
                message: "cannot find value `x` in this scope".into(),
                code: Some("E0425".into()),
                file: Some("src/lib.rs".into()),
                line: Some(3),
                column: Some(9),
            }]
        );
    }

    #[test]
    fn ignores_non_compiler_messages() {
        let parsed = parse_cargo_diagnostics(r#"{"reason":"build-finished"}"#);
        assert!(parsed.is_empty());
    }

    #[test]
    fn positions_roundtrip_between_user_and_lsp_coordinates() {
        let pos = one_based_position(3, 9).unwrap();
        assert_eq!(
            position_to_lsp_json(&pos),
            json!({ "line": 2, "character": 8 })
        );
        assert_eq!(
            position_from_value(&json!({ "line": 2, "character": 8 })),
            Some(pos)
        );
    }

    #[test]
    fn file_uri_encodes_spaces_and_decodes_paths() {
        let uri = file_uri(Path::new("D:/botobot/a b.rs"));
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("a%20b.rs"));
        assert_eq!(
            path_from_file_uri("file:///D:/botobot/a%20b.rs"),
            Some("D:\\botobot\\a b.rs".into())
        );
    }

    #[tokio::test]
    async fn references_uses_rust_analyzer_when_available() {
        if Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let root = std::env::temp_dir().join(format!("botobot-lsp-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(root.join("src")).await.unwrap();
        tokio::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"botobot_lsp_smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join("src/lib.rs"),
            "pub fn f() -> i32 { 1 }\npub fn g() -> i32 { f() }\n",
        )
        .await
        .unwrap();
        let out = run_lsp(
            &root,
            LspArgs {
                op: LspOp::References,
                path: None,
                file: Some("src/lib.rs".into()),
                line: Some(2),
                column: Some(21),
                end_line: None,
                end_column: None,
                new_name: None,
                timeout_ms: Some(30_000),
                max_diagnostics: None,
            },
        )
        .await
        .unwrap();
        let refs = out
            .get("references")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(out.get("workspace").and_then(Value::as_str).is_some());
        assert!(out.get("file").and_then(Value::as_str).is_some());
        assert!(
            refs.is_empty() || refs[0].get("range").is_some(),
            "got {out}"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
