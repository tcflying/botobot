//! Debug Adapter Protocol helpers.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DapOp {
    Status,
    LaunchConfig,
    CargoTestPlan,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DapArgs {
    pub op: DapOp,
    /// Optional workspace subdirectory. Defaults to the current bot workdir.
    pub path: Option<String>,
    /// Optional debug adapter name. Auto-detected when omitted.
    pub adapter: Option<String>,
    /// Program path for launch_config. Relative paths are resolved under workdir.
    pub program: Option<String>,
    /// Arguments for launch_config or cargo_test_plan test filter.
    pub args: Option<Vec<String>>,
    /// Cargo package for cargo_test_plan.
    pub package: Option<String>,
    /// Cargo test target/bin name for cargo_test_plan.
    pub test: Option<String>,
    /// Optional timeout for adapter/cargo probes. Defaults to 5000ms.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct DapStatusOut {
    pub workspace: String,
    pub rust: RustDebugStatus,
    pub client_ready: bool,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct RustDebugStatus {
    pub cargo_toml: bool,
    pub cargo_available: bool,
    pub cargo_version: Option<String>,
    pub adapters: Vec<DebugAdapterStatus>,
    pub selected_adapter: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DebugAdapterStatus {
    pub name: String,
    pub command: String,
    pub available: bool,
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DapLaunchConfigOut {
    pub workspace: String,
    pub adapter: String,
    pub configuration: Value,
    pub client_ready: bool,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct DapCargoTestPlanOut {
    pub workspace: String,
    pub adapter: String,
    pub cargo_command: Vec<String>,
    pub launch_config: Value,
    pub client_ready: bool,
    pub note: String,
}

pub struct DapTool;

#[async_trait]
impl Tool for DapTool {
    fn name(&self) -> &str {
        "dap"
    }

    fn description(&self) -> &str {
        "Debug Adapter Protocol planning for Rust debugging. Supports status, launch_config, and cargo_test_plan."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(DapArgs)).unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: DapArgs = serde_json::from_value(args)?;
        run_dap(&ctx.workdir, args).await
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: DapArgs = serde_json::from_value(args)?;
        run_dap(&std::env::current_dir()?, args).await
    }
}

pub async fn run_dap(workdir: &Path, args: DapArgs) -> ToolResult {
    let workspace = workspace_root(workdir);
    let target = resolve_under_workdir(&workspace, args.path.as_deref().unwrap_or("."))?;
    let deadline = Duration::from_millis(args.timeout_ms.unwrap_or(5_000).clamp(1, 30_000));
    match args.op {
        DapOp::Status => Ok(json!(status(&target, deadline).await)),
        DapOp::LaunchConfig => Ok(json!(launch_config(&workspace, args, deadline).await?)),
        DapOp::CargoTestPlan => Ok(json!(cargo_test_plan(&target, args, deadline).await?)),
    }
}

async fn status(workspace: &Path, deadline: Duration) -> DapStatusOut {
    let cargo_version = command_output(workspace, "cargo", &["--version"], deadline)
        .await
        .ok()
        .and_then(|out| (out.status == Some(0)).then_some(out.stdout.trim().to_string()));
    let adapters = adapter_statuses(workspace, deadline).await;
    let selected_adapter = adapters
        .iter()
        .find(|adapter| adapter.available)
        .map(|adapter| adapter.name.clone());
    DapStatusOut {
        workspace: display_path(workspace),
        rust: RustDebugStatus {
            cargo_toml: workspace.join("Cargo.toml").is_file(),
            cargo_available: cargo_version.is_some(),
            cargo_version,
            adapters,
            selected_adapter,
        },
        client_ready: false,
        note: "DAP planning is available; interactive DAP client sessions are not implemented yet."
            .into(),
    }
}

async fn launch_config(
    workspace: &Path,
    args: DapArgs,
    deadline: Duration,
) -> anyhow::Result<DapLaunchConfigOut> {
    let adapter = select_adapter(workspace, args.adapter.as_deref(), deadline).await;
    let program = args
        .program
        .as_deref()
        .map(|program| resolve_under_workdir(workspace, program))
        .transpose()?;
    let program = program
        .map(display_path)
        .unwrap_or_else(|| "${workspaceFolder}/target/debug/<binary>".to_string());
    let configuration = launch_configuration(&adapter, &program, args.args.unwrap_or_default());
    Ok(DapLaunchConfigOut {
        workspace: display_path(workspace),
        adapter,
        configuration,
        client_ready: false,
        note: "Use this configuration with a DAP-capable IDE/client; botobot does not drive the live session yet."
            .into(),
    })
}

async fn cargo_test_plan(
    workspace: &Path,
    args: DapArgs,
    deadline: Duration,
) -> anyhow::Result<DapCargoTestPlanOut> {
    let adapter = select_adapter(workspace, args.adapter.as_deref(), deadline).await;
    let mut cargo_command = vec!["cargo".to_string(), "test".to_string()];
    if let Some(package) = args.package.filter(|s| !s.trim().is_empty()) {
        cargo_command.extend(["-p".into(), package]);
    }
    cargo_command.extend(["--no-run".into(), "--message-format=json".into()]);
    if let Some(test) = args.test.filter(|s| !s.trim().is_empty()) {
        cargo_command.push(test);
    }
    if let Some(filters) = args.args {
        cargo_command.push("--".into());
        cargo_command.extend(filters);
    }
    let program = "${workspaceFolder}/target/debug/<test-binary-from-cargo-json>".to_string();
    let launch_config = launch_configuration(&adapter, &program, Vec::new());
    Ok(DapCargoTestPlanOut {
        workspace: display_path(workspace),
        adapter,
        cargo_command,
        launch_config,
        client_ready: false,
        note: "Run cargo_command, pick the produced executable from compiler-artifact JSON, then launch it with the returned config."
            .into(),
    })
}

fn launch_configuration(adapter: &str, program: &str, args: Vec<String>) -> Value {
    match adapter {
        "codelldb" => json!({
            "type": "lldb",
            "request": "launch",
            "name": "botobot Rust debug",
            "program": program,
            "args": args,
            "cwd": "${workspaceFolder}",
            "stopOnEntry": false,
        }),
        "lldb-dap" | "lldb-vscode" => json!({
            "type": "lldb",
            "request": "launch",
            "name": "botobot Rust debug",
            "program": program,
            "args": args,
            "cwd": "${workspaceFolder}",
        }),
        "cppvsdbg" => json!({
            "type": "cppvsdbg",
            "request": "launch",
            "name": "botobot Rust debug",
            "program": program,
            "args": args,
            "cwd": "${workspaceFolder}",
            "stopAtEntry": false,
        }),
        other => json!({
            "type": other,
            "request": "launch",
            "name": "botobot debug",
            "program": program,
            "args": args,
            "cwd": "${workspaceFolder}",
        }),
    }
}

async fn select_adapter(workspace: &Path, requested: Option<&str>, deadline: Duration) -> String {
    if let Some(requested) = requested.filter(|s| !s.trim().is_empty()) {
        return requested.to_string();
    }
    adapter_statuses(workspace, deadline)
        .await
        .into_iter()
        .find(|adapter| adapter.available)
        .map(|adapter| adapter.name)
        .unwrap_or_else(default_adapter_name)
}

async fn adapter_statuses(workspace: &Path, deadline: Duration) -> Vec<DebugAdapterStatus> {
    let mut out = Vec::new();
    for &(name, command, version_args) in adapter_candidates() {
        let probe = command_output(workspace, command, version_args, deadline)
            .await
            .ok()
            .filter(|out| out.status == Some(0));
        let version = probe
            .as_ref()
            .and_then(|out| first_non_empty(&[&out.stdout, &out.stderr]));
        out.push(DebugAdapterStatus {
            name: name.into(),
            command: command.into(),
            available: probe.is_some(),
            version,
        });
    }
    out
}

fn first_non_empty(parts: &[&str]) -> Option<String> {
    parts
        .iter()
        .map(|part| part.trim())
        .find(|part| !part.is_empty())
        .map(ToOwned::to_owned)
}

fn adapter_candidates() -> &'static [(&'static str, &'static str, &'static [&'static str])] {
    #[cfg(windows)]
    {
        &[
            ("codelldb", "codelldb", &["--version"]),
            ("lldb-dap", "lldb-dap", &["--version"]),
            ("lldb-vscode", "lldb-vscode", &["--version"]),
            ("cppvsdbg", "vsdbg", &["--version"]),
        ]
    }
    #[cfg(not(windows))]
    {
        &[
            ("codelldb", "codelldb", &["--version"]),
            ("lldb-dap", "lldb-dap", &["--version"]),
            ("lldb-vscode", "lldb-vscode", &["--version"]),
            ("gdb", "gdb", &["--version"]),
        ]
    }
}

#[cfg(windows)]
fn default_adapter_name() -> String {
    "cppvsdbg".into()
}

#[cfg(not(windows))]
fn default_adapter_name() -> String {
    "lldb-dap".into()
}

struct CommandOut {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn command_output(
    cwd: &Path,
    program: &str,
    args: &[&str],
    deadline: Duration,
) -> anyhow::Result<CommandOut> {
    let output = timeout(
        deadline,
        Command::new(program).args(args).current_dir(cwd).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{program} timed out after {}ms", deadline.as_millis()))??;
    Ok(CommandOut {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn resolve_under_workdir(workdir: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let root = workspace_root(workdir);
    let raw_path = Path::new(raw);
    let target = if raw_path.is_absolute() {
        normalize(raw_path)
    } else {
        normalize(root.join(raw_path))
    };
    if !target.starts_with(&root) {
        anyhow::bail!("path escapes workdir: {raw} (workdir: {})", root.display());
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

fn display_path(path: impl AsRef<Path>) -> String {
    path.as_ref().display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dap_status_reports_workspace() {
        let out = run_dap(
            Path::new("."),
            DapArgs {
                op: DapOp::Status,
                path: None,
                adapter: None,
                program: None,
                args: None,
                package: None,
                test: None,
                timeout_ms: Some(100),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["client_ready"], false);
        assert!(out["rust"]["adapters"].as_array().is_some());
    }

    #[tokio::test]
    async fn launch_config_rejects_path_escape() {
        let root = std::env::temp_dir().join(format!("botobot-dap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let err = run_dap(
            &root,
            DapArgs {
                op: DapOp::LaunchConfig,
                path: None,
                adapter: Some("lldb-dap".into()),
                program: Some("../outside".into()),
                args: None,
                package: None,
                test: None,
                timeout_ms: Some(100),
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("escapes workdir"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cargo_test_plan_shapes_command() {
        let out = run_dap(
            Path::new("."),
            DapArgs {
                op: DapOp::CargoTestPlan,
                path: None,
                adapter: Some("codelldb".into()),
                program: None,
                args: Some(vec!["my_filter".into()]),
                package: Some("agent-act".into()),
                test: Some("some_test".into()),
                timeout_ms: Some(100),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["adapter"], "codelldb");
        assert!(
            out["cargo_command"]
                .as_array()
                .unwrap()
                .contains(&json!("-p"))
        );
        assert!(
            out["cargo_command"]
                .as_array()
                .unwrap()
                .contains(&json!("--no-run"))
        );
        assert_eq!(out["client_ready"], false);
    }
}
