//! Minimal live Debug Adapter Protocol session manager.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base_types::{Tool, ToolConcurrency, ToolCtx, ToolLoadMode, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio::time::timeout;

use crate::dap_wire::{DapMessageDecoder, encode_dap_message};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_EVENT_CACHE: usize = 256;

type PendingRequests = Arc<AsyncMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type SourceBreakpointCache = Arc<AsyncMutex<HashMap<String, Vec<DapSourceBreakpointRecord>>>>;
type FunctionBreakpointCache = Arc<AsyncMutex<Vec<DapFunctionBreakpointRecord>>>;
type InstructionBreakpointCache = Arc<AsyncMutex<Vec<DapInstructionBreakpointRecord>>>;
type DataBreakpointCache = Arc<AsyncMutex<Vec<DapDataBreakpointRecord>>>;

static GLOBAL_DAP_MANAGER: OnceLock<DapSessionManager> = OnceLock::new();

pub fn global_dap_session_manager() -> &'static DapSessionManager {
    GLOBAL_DAP_MANAGER.get_or_init(DapSessionManager::new)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DebugAction {
    LaunchAdapter,
    Launch,
    Attach,
    ConfigurationDone,
    Status,
    WaitForEvent,
    Sessions,
    CustomRequest,
    Events,
    Output,
    Threads,
    StackTrace,
    Scopes,
    Variables,
    Evaluate,
    Continue,
    StepOver,
    StepIn,
    StepOut,
    Pause,
    SetBreakpoint,
    RemoveBreakpoint,
    SetFunctionBreakpoint,
    RemoveFunctionBreakpoint,
    SetInstructionBreakpoint,
    RemoveInstructionBreakpoint,
    DataBreakpointInfo,
    SetDataBreakpoint,
    RemoveDataBreakpoint,
    Terminate,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DebugArgs {
    pub action: DebugAction,
    /// Debug adapter display name. Defaults to the adapter command.
    pub adapter: Option<String>,
    /// Adapter executable for launch_adapter. May be a PATH command or a workdir-relative path.
    pub adapter_command: Option<String>,
    /// Adapter argv for launch_adapter.
    pub args: Option<Vec<String>>,
    /// Program path for launch. Relative paths are resolved under workdir.
    pub program: Option<String>,
    /// Debuggee argv for launch.
    pub program_args: Option<Vec<String>>,
    /// Optional working directory for launch_adapter. Defaults to the bot workdir.
    pub cwd: Option<String>,
    /// Raw DAP command for custom_request, for example "threads".
    pub dap_command: Option<String>,
    /// Raw DAP request arguments for custom_request.
    pub arguments: Option<Value>,
    /// Optional thread id for stack_trace.
    pub thread_id: Option<u64>,
    /// Optional frame id for scopes/evaluate. Defaults to current top frame when available.
    pub frame_id: Option<u64>,
    /// Variables reference for variables.
    pub variable_ref: Option<u64>,
    /// Expression for evaluate.
    pub expression: Option<String>,
    /// Evaluate context. Defaults to "repl".
    pub context: Option<String>,
    /// Optional stack frame count for stack_trace.
    pub levels: Option<u64>,
    /// Source file for set_breakpoint/remove_breakpoint.
    pub file: Option<String>,
    /// 1-based source line for set_breakpoint/remove_breakpoint.
    pub line: Option<u64>,
    /// Optional breakpoint condition for set_breakpoint.
    pub condition: Option<String>,
    /// Function name for set_function_breakpoint/remove_function_breakpoint.
    pub function: Option<String>,
    /// Instruction reference for set_instruction_breakpoint/remove_instruction_breakpoint.
    pub instruction_ref: Option<String>,
    /// Optional instruction offset for set_instruction_breakpoint/remove_instruction_breakpoint.
    pub offset: Option<i64>,
    /// Optional breakpoint hit condition.
    pub hit_condition: Option<String>,
    /// Variable name for data_breakpoint_info.
    pub data_name: Option<String>,
    /// Adapter data id for set_data_breakpoint/remove_data_breakpoint.
    pub data_id: Option<String>,
    /// Optional data breakpoint access type, for example read, write, or readWrite.
    pub access_type: Option<String>,
    /// Event names for wait_for_event. Defaults to stopped/terminated/exited.
    pub event_names: Option<Vec<String>>,
    /// Event cache index to start waiting after. Defaults to the current event count.
    pub event_start_index: Option<usize>,
    /// Optional timeout in milliseconds. Defaults to 30000.
    pub timeout_ms: Option<u64>,
}

pub struct DebugTool;

#[async_trait]
impl Tool for DebugTool {
    fn name(&self) -> &str {
        "debug"
    }

    fn description(&self) -> &str {
        "Drive a live Debug Adapter Protocol session. Current actions: launch_adapter, launch, attach, configuration_done, status, wait_for_event, sessions, custom_request, events, output, threads, stack_trace, scopes, variables, evaluate, continue, step_over, step_in, step_out, pause, set_breakpoint, remove_breakpoint, set_function_breakpoint, remove_function_breakpoint, set_instruction_breakpoint, remove_instruction_breakpoint, data_breakpoint_info, set_data_breakpoint, remove_data_breakpoint, terminate."
    }

    fn summary(&self) -> &str {
        "Drive a live DAP debug adapter session"
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(DebugArgs)).unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Exec
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Exclusive
    }

    fn load_mode(&self) -> ToolLoadMode {
        ToolLoadMode::Discoverable
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: DebugArgs = serde_json::from_value(args)?;
        run_debug_tool(global_dap_session_manager(), &ctx.workdir, args).await
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: DebugArgs = serde_json::from_value(args)?;
        run_debug_tool(
            global_dap_session_manager(),
            &std::env::current_dir()?,
            args,
        )
        .await
    }
}

pub async fn run_debug_tool(
    manager: &DapSessionManager,
    workdir: &Path,
    args: DebugArgs,
) -> ToolResult {
    match args.action {
        DebugAction::LaunchAdapter => {
            let adapter_command = args
                .adapter_command
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("adapter_command is required for launch_adapter"))?;
            let cwd = resolve_cwd(workdir, args.cwd.as_deref().unwrap_or("."))?;
            let command = resolve_adapter_command(workdir, &adapter_command)?;
            let adapter = args.adapter.unwrap_or_else(|| adapter_command.clone());
            Ok(json!(
                manager
                    .start_adapter(
                        DapAdapterLaunch {
                            name: adapter,
                            command,
                            args: args.args.unwrap_or_default(),
                            cwd,
                        },
                        args.timeout_ms,
                    )
                    .await?
            ))
        }
        DebugAction::Launch => {
            let arguments = normalized_debuggee_arguments(
                workdir,
                "launch",
                args.arguments,
                args.program,
                args.program_args,
                args.cwd,
            )?;
            Ok(json!(
                manager
                    .launch_or_attach("launch", arguments, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::Attach => {
            let arguments = normalized_debuggee_arguments(
                workdir,
                "attach",
                args.arguments,
                args.program,
                args.program_args,
                args.cwd,
            )?;
            Ok(json!(
                manager
                    .launch_or_attach("attach", arguments, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::ConfigurationDone => {
            Ok(json!(manager.configuration_done(args.timeout_ms).await?))
        }
        DebugAction::Status => Ok(json!(manager.status_snapshot().await)),
        DebugAction::WaitForEvent => Ok(json!(
            manager
                .wait_for_event(args.event_names, args.event_start_index, args.timeout_ms)
                .await?
        )),
        DebugAction::Sessions => Ok(json!(manager.list_sessions().await)),
        DebugAction::Events => Ok(json!(manager.events().await?)),
        DebugAction::Output => Ok(json!(manager.output().await?)),
        DebugAction::Threads => Ok(json!(manager.threads(args.timeout_ms).await?)),
        DebugAction::StackTrace => Ok(json!(
            manager
                .stack_trace(args.thread_id, args.levels, args.timeout_ms)
                .await?
        )),
        DebugAction::Scopes => Ok(json!(manager.scopes(args.frame_id, args.timeout_ms).await?)),
        DebugAction::Variables => {
            let variable_ref = args
                .variable_ref
                .ok_or_else(|| anyhow::anyhow!("variable_ref is required for variables"))?;
            Ok(json!(
                manager.variables(variable_ref, args.timeout_ms).await?
            ))
        }
        DebugAction::Evaluate => {
            let expression = args
                .expression
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("expression is required for evaluate"))?;
            Ok(json!(
                manager
                    .evaluate(
                        expression,
                        args.context.unwrap_or_else(|| "repl".into()),
                        args.frame_id,
                        args.timeout_ms,
                    )
                    .await?
            ))
        }
        DebugAction::Continue => Ok(json!(
            manager
                .continue_run(args.thread_id, args.timeout_ms)
                .await?
        )),
        DebugAction::StepOver => Ok(json!(
            manager
                .step("next", args.thread_id, args.timeout_ms)
                .await?
        )),
        DebugAction::StepIn => Ok(json!(
            manager
                .step("stepIn", args.thread_id, args.timeout_ms)
                .await?
        )),
        DebugAction::StepOut => Ok(json!(
            manager
                .step("stepOut", args.thread_id, args.timeout_ms)
                .await?
        )),
        DebugAction::Pause => Ok(json!(manager.pause(args.thread_id, args.timeout_ms).await?)),
        DebugAction::SetBreakpoint => {
            let file = resolve_debug_file(workdir, args.file.as_deref())?;
            let line = args
                .line
                .ok_or_else(|| anyhow::anyhow!("line is required for set_breakpoint"))?;
            Ok(json!(
                manager
                    .set_breakpoint(file, line, args.condition, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::RemoveBreakpoint => {
            let file = resolve_debug_file(workdir, args.file.as_deref())?;
            let line = args
                .line
                .ok_or_else(|| anyhow::anyhow!("line is required for remove_breakpoint"))?;
            Ok(json!(
                manager
                    .remove_breakpoint(file, line, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::SetFunctionBreakpoint => {
            let function = required_nonempty(args.function, "function", "set_function_breakpoint")?;
            Ok(json!(
                manager
                    .set_function_breakpoint(function, args.condition, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::RemoveFunctionBreakpoint => {
            let function =
                required_nonempty(args.function, "function", "remove_function_breakpoint")?;
            Ok(json!(
                manager
                    .remove_function_breakpoint(function, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::SetInstructionBreakpoint => {
            let instruction_ref = required_nonempty(
                args.instruction_ref,
                "instruction_ref",
                "set_instruction_breakpoint",
            )?;
            Ok(json!(
                manager
                    .set_instruction_breakpoint(
                        instruction_ref,
                        args.offset,
                        args.condition,
                        args.hit_condition,
                        args.timeout_ms,
                    )
                    .await?
            ))
        }
        DebugAction::RemoveInstructionBreakpoint => {
            let instruction_ref = required_nonempty(
                args.instruction_ref,
                "instruction_ref",
                "remove_instruction_breakpoint",
            )?;
            Ok(json!(
                manager
                    .remove_instruction_breakpoint(instruction_ref, args.offset, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::DataBreakpointInfo => {
            let variable_ref = args.variable_ref.ok_or_else(|| {
                anyhow::anyhow!("variable_ref is required for data_breakpoint_info")
            })?;
            let data_name = required_nonempty(args.data_name, "data_name", "data_breakpoint_info")?;
            Ok(json!(
                manager
                    .data_breakpoint_info(variable_ref, data_name, args.frame_id, args.timeout_ms,)
                    .await?
            ))
        }
        DebugAction::SetDataBreakpoint => {
            let data_id = required_nonempty(args.data_id, "data_id", "set_data_breakpoint")?;
            Ok(json!(
                manager
                    .set_data_breakpoint(
                        data_id,
                        args.access_type,
                        args.condition,
                        args.hit_condition,
                        args.timeout_ms,
                    )
                    .await?
            ))
        }
        DebugAction::RemoveDataBreakpoint => {
            let data_id = required_nonempty(args.data_id, "data_id", "remove_data_breakpoint")?;
            Ok(json!(
                manager
                    .remove_data_breakpoint(data_id, args.timeout_ms)
                    .await?
            ))
        }
        DebugAction::Terminate => Ok(json!(manager.terminate(args.timeout_ms).await?)),
        DebugAction::CustomRequest => {
            let command = args
                .dap_command
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("dap_command is required for custom_request"))?;
            Ok(json!(
                manager
                    .send_request(&command, args.arguments, args.timeout_ms)
                    .await?
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub struct DapAdapterLaunch {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DapSessionStatus {
    Starting,
    Running,
    Stopped,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapEventRecord {
    pub event: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapSessionSummary {
    pub id: String,
    pub adapter: String,
    pub command: String,
    pub cwd: String,
    pub status: DapSessionStatus,
    pub launched_unix_ms: u128,
    pub last_event: Option<String>,
    pub pending_requests: usize,
    pub capabilities: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapRequestOutcome {
    pub session: DapSessionSummary,
    pub body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapRunOutcome {
    pub session: DapSessionSummary,
    pub state: DapSessionStatus,
    pub timed_out: bool,
    pub event: Option<DapEventRecord>,
    pub event_count: usize,
    pub output_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapStatusSnapshot {
    pub session: Option<DapSessionSummary>,
    pub event_count: usize,
    pub recent_events: Vec<DapEventRecord>,
    pub output_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapWaitOutcome {
    pub session: DapSessionSummary,
    pub event_count: usize,
    pub matched_events: Vec<String>,
    pub timed_out: bool,
    pub event: Option<DapEventRecord>,
    pub output_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DapSourceBreakpointRecord {
    pub line: u64,
    pub condition: Option<String>,
    pub verified: bool,
    pub id: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapBreakpointOutcome {
    pub session: DapSessionSummary,
    pub source_path: String,
    pub breakpoints: Vec<DapSourceBreakpointRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DapFunctionBreakpointRecord {
    pub name: String,
    pub condition: Option<String>,
    pub verified: bool,
    pub id: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapFunctionBreakpointOutcome {
    pub session: DapSessionSummary,
    pub breakpoints: Vec<DapFunctionBreakpointRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DapInstructionBreakpointRecord {
    pub instruction_ref: String,
    pub offset: Option<i64>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub verified: bool,
    pub id: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapInstructionBreakpointOutcome {
    pub session: DapSessionSummary,
    pub breakpoints: Vec<DapInstructionBreakpointRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DapDataBreakpointRecord {
    pub data_id: String,
    pub access_type: Option<String>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub verified: bool,
    pub id: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapDataBreakpointOutcome {
    pub session: DapSessionSummary,
    pub breakpoints: Vec<DapDataBreakpointRecord>,
}

#[derive(Default, Clone)]
pub struct DapSessionManager {
    active: Arc<AsyncMutex<Option<Arc<DapSession>>>>,
}

impl DapSessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn start_adapter(
        &self,
        launch: DapAdapterLaunch,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapSessionSummary> {
        let deadline = request_timeout(timeout_ms);
        {
            let active = self.active.lock().await;
            if let Some(session) = active.as_ref()
                && session.status().await != DapSessionStatus::Terminated
                && session.is_alive().await
            {
                anyhow::bail!(
                    "debug session {} is still active; terminate it before starting another",
                    session.id
                );
            }
        }

        let session = DapSession::spawn(launch).await?;
        {
            let mut active = self.active.lock().await;
            *active = Some(session.clone());
        }
        let capabilities = session
            .send_request(
                "initialize",
                Some(json!({
                    "clientID": "botobot",
                    "clientName": "botobot",
                    "adapterID": session.adapter,
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                    "supportsRunInTerminalRequest": false,
                    "supportsVariableType": true,
                })),
                deadline,
            )
            .await?;
        session.set_capabilities(capabilities).await;
        session.set_status(DapSessionStatus::Running).await;
        Ok(session.summary().await)
    }

    pub async fn active_summary(&self) -> Option<DapSessionSummary> {
        let session = self.active.lock().await.as_ref()?.clone();
        Some(session.summary().await)
    }

    pub async fn list_sessions(&self) -> Vec<DapSessionSummary> {
        match self.active_summary().await {
            Some(summary) => vec![summary],
            None => Vec::new(),
        }
    }

    pub async fn status_snapshot(&self) -> DapStatusSnapshot {
        let session = self.active.lock().await.as_ref().cloned();
        let Some(session) = session else {
            return DapStatusSnapshot {
                session: None,
                event_count: 0,
                recent_events: Vec::new(),
                output_tail: String::new(),
            };
        };
        DapStatusSnapshot {
            session: Some(session.summary().await),
            event_count: session.event_len().await,
            recent_events: session.recent_events(20).await,
            output_tail: tail_chars(&session.output().await, 4_000),
        }
    }

    pub async fn wait_for_event(
        &self,
        event_names: Option<Vec<String>>,
        start_index: Option<usize>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapWaitOutcome> {
        let session = self.active_session().await?;
        let start_index = start_index.unwrap_or(session.event_len().await);
        let names = normalized_event_names(event_names);
        let name_refs = names.iter().map(String::as_str).collect::<Vec<_>>();
        let event = session
            .wait_for_event_after(start_index, &name_refs, request_timeout(timeout_ms))
            .await?;
        Ok(DapWaitOutcome {
            session: session.summary().await,
            event_count: session.event_len().await,
            matched_events: names,
            timed_out: event.is_none(),
            event,
            output_tail: tail_chars(&session.output().await, 4_000),
        })
    }

    pub async fn send_request(
        &self,
        command: &str,
        arguments: Option<Value>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        let session = self.active_session().await?;
        let body = session
            .send_request(command, arguments, request_timeout(timeout_ms))
            .await?;
        Ok(DapRequestOutcome {
            session: session.summary().await,
            body,
        })
    }

    pub async fn launch_or_attach(
        &self,
        command: &str,
        arguments: Value,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        match command {
            "launch" | "attach" => {}
            other => anyhow::bail!("unsupported debuggee start command: {other}"),
        }
        let session = self.active_session().await?;
        let body = session
            .send_request(command, Some(arguments), request_timeout(timeout_ms))
            .await?;
        session.set_status(DapSessionStatus::Running).await;
        Ok(DapRequestOutcome {
            session: session.summary().await,
            body,
        })
    }

    pub async fn configuration_done(
        &self,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        self.send_request("configurationDone", None, timeout_ms)
            .await
    }

    pub async fn events(&self) -> anyhow::Result<Vec<DapEventRecord>> {
        let session = self.active_session().await?;
        Ok(session.events().await)
    }

    pub async fn output(&self) -> anyhow::Result<DapOutputSnapshot> {
        let session = self.active_session().await?;
        Ok(DapOutputSnapshot {
            session: session.summary().await,
            output: session.output().await,
        })
    }

    pub async fn threads(&self, timeout_ms: Option<u64>) -> anyhow::Result<DapRequestOutcome> {
        self.send_request("threads", None, timeout_ms).await
    }

    pub async fn stack_trace(
        &self,
        thread_id: Option<u64>,
        levels: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        let session = self.active_session().await?;
        let thread_id = self
            .resolve_thread_id(&session, thread_id, timeout_ms)
            .await?;
        let mut args = json!({ "threadId": thread_id });
        if let Some(levels) = levels {
            args["levels"] = json!(levels);
        }
        let body = session
            .send_request("stackTrace", Some(args), request_timeout(timeout_ms))
            .await?;
        Ok(DapRequestOutcome {
            session: session.summary().await,
            body,
        })
    }

    pub async fn scopes(
        &self,
        frame_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        let session = self.active_session().await?;
        let frame_id = self
            .resolve_frame_id(&session, frame_id, timeout_ms)
            .await?;
        let body = session
            .send_request(
                "scopes",
                Some(json!({ "frameId": frame_id })),
                request_timeout(timeout_ms),
            )
            .await?;
        Ok(DapRequestOutcome {
            session: session.summary().await,
            body,
        })
    }

    pub async fn variables(
        &self,
        variable_ref: u64,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        self.send_request(
            "variables",
            Some(json!({ "variablesReference": variable_ref })),
            timeout_ms,
        )
        .await
    }

    pub async fn data_breakpoint_info(
        &self,
        variable_ref: u64,
        name: String,
        frame_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        let mut args = json!({
            "variablesReference": variable_ref,
            "name": name,
        });
        if let Some(frame_id) = frame_id {
            args["frameId"] = json!(frame_id);
        }
        self.send_request("dataBreakpointInfo", Some(args), timeout_ms)
            .await
    }

    pub async fn evaluate(
        &self,
        expression: String,
        context: String,
        frame_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRequestOutcome> {
        let session = self.active_session().await?;
        let mut args = json!({
            "expression": expression,
            "context": context,
        });
        if let Some(frame_id) = self
            .maybe_resolve_frame_id(&session, frame_id, timeout_ms)
            .await?
        {
            args["frameId"] = json!(frame_id);
        }
        let body = session
            .send_request("evaluate", Some(args), request_timeout(timeout_ms))
            .await?;
        Ok(DapRequestOutcome {
            session: session.summary().await,
            body,
        })
    }

    pub async fn continue_run(
        &self,
        thread_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRunOutcome> {
        self.run_and_wait("continue", thread_id, timeout_ms).await
    }

    pub async fn step(
        &self,
        command: &str,
        thread_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRunOutcome> {
        match command {
            "next" | "stepIn" | "stepOut" => {
                self.run_and_wait(command, thread_id, timeout_ms).await
            }
            other => anyhow::bail!("unsupported step command: {other}"),
        }
    }

    pub async fn pause(
        &self,
        thread_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRunOutcome> {
        self.run_and_wait("pause", thread_id, timeout_ms).await
    }

    pub async fn set_breakpoint(
        &self,
        source_path: String,
        line: u64,
        condition: Option<String>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.source_breakpoints.lock().await;
            let entries = cache.entry(source_path.clone()).or_default();
            entries.retain(|entry| entry.line != line);
            entries.push(DapSourceBreakpointRecord {
                line,
                condition: condition.filter(|s| !s.trim().is_empty()),
                verified: false,
                id: None,
                message: None,
            });
            entries.sort_by_key(|entry| entry.line);
            entries.clone()
        };
        self.sync_source_breakpoints(session, source_path, current, timeout_ms)
            .await
    }

    pub async fn remove_breakpoint(
        &self,
        source_path: String,
        line: u64,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.source_breakpoints.lock().await;
            let entries = cache.entry(source_path.clone()).or_default();
            entries.retain(|entry| entry.line != line);
            let current = entries.clone();
            if current.is_empty() {
                cache.remove(&source_path);
            }
            current
        };
        self.sync_source_breakpoints(session, source_path, current, timeout_ms)
            .await
    }

    pub async fn set_function_breakpoint(
        &self,
        name: String,
        condition: Option<String>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapFunctionBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.function_breakpoints.lock().await;
            cache.retain(|entry| entry.name != name);
            cache.push(DapFunctionBreakpointRecord {
                name,
                condition: condition.filter(|s| !s.trim().is_empty()),
                verified: false,
                id: None,
                message: None,
            });
            cache.sort_by(|a, b| a.name.cmp(&b.name));
            cache.clone()
        };
        self.sync_function_breakpoints(session, current, timeout_ms)
            .await
    }

    pub async fn remove_function_breakpoint(
        &self,
        name: String,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapFunctionBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.function_breakpoints.lock().await;
            cache.retain(|entry| entry.name != name);
            cache.clone()
        };
        self.sync_function_breakpoints(session, current, timeout_ms)
            .await
    }

    pub async fn set_instruction_breakpoint(
        &self,
        instruction_ref: String,
        offset: Option<i64>,
        condition: Option<String>,
        hit_condition: Option<String>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapInstructionBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.instruction_breakpoints.lock().await;
            cache
                .retain(|entry| entry.instruction_ref != instruction_ref || entry.offset != offset);
            cache.push(DapInstructionBreakpointRecord {
                instruction_ref,
                offset,
                condition: condition.filter(|s| !s.trim().is_empty()),
                hit_condition: hit_condition.filter(|s| !s.trim().is_empty()),
                verified: false,
                id: None,
                message: None,
            });
            cache.sort_by(|a, b| {
                a.instruction_ref
                    .cmp(&b.instruction_ref)
                    .then(a.offset.cmp(&b.offset))
            });
            cache.clone()
        };
        self.sync_instruction_breakpoints(session, current, timeout_ms)
            .await
    }

    pub async fn remove_instruction_breakpoint(
        &self,
        instruction_ref: String,
        offset: Option<i64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapInstructionBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.instruction_breakpoints.lock().await;
            cache
                .retain(|entry| entry.instruction_ref != instruction_ref || entry.offset != offset);
            cache.clone()
        };
        self.sync_instruction_breakpoints(session, current, timeout_ms)
            .await
    }

    pub async fn set_data_breakpoint(
        &self,
        data_id: String,
        access_type: Option<String>,
        condition: Option<String>,
        hit_condition: Option<String>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapDataBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.data_breakpoints.lock().await;
            cache.retain(|entry| entry.data_id != data_id);
            cache.push(DapDataBreakpointRecord {
                data_id,
                access_type: access_type.filter(|s| !s.trim().is_empty()),
                condition: condition.filter(|s| !s.trim().is_empty()),
                hit_condition: hit_condition.filter(|s| !s.trim().is_empty()),
                verified: false,
                id: None,
                message: None,
            });
            cache.sort_by(|a, b| a.data_id.cmp(&b.data_id));
            cache.clone()
        };
        self.sync_data_breakpoints(session, current, timeout_ms)
            .await
    }

    pub async fn remove_data_breakpoint(
        &self,
        data_id: String,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapDataBreakpointOutcome> {
        let session = self.active_session().await?;
        let current = {
            let mut cache = session.data_breakpoints.lock().await;
            cache.retain(|entry| entry.data_id != data_id);
            cache.clone()
        };
        self.sync_data_breakpoints(session, current, timeout_ms)
            .await
    }

    pub async fn terminate(
        &self,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<Option<DapSessionSummary>> {
        let session = {
            let mut active = self.active.lock().await;
            active.take()
        };
        let Some(session) = session else {
            return Ok(None);
        };
        let _ = session
            .send_request(
                "disconnect",
                Some(json!({ "terminateDebuggee": true })),
                request_timeout(timeout_ms),
            )
            .await;
        session.dispose().await;
        Ok(Some(session.summary().await))
    }

    async fn active_session(&self) -> anyhow::Result<Arc<DapSession>> {
        let session = self.active.lock().await.as_ref().cloned();
        let Some(session) = session else {
            anyhow::bail!("no active DAP session; start an adapter first");
        };
        if session.status().await == DapSessionStatus::Terminated || !session.is_alive().await {
            anyhow::bail!("active DAP session {} is not running", session.id);
        }
        Ok(session)
    }

    async fn resolve_thread_id(
        &self,
        session: &Arc<DapSession>,
        provided: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<u64> {
        if let Some(thread_id) = provided {
            return Ok(thread_id);
        }
        if let Some(thread_id) = session.last_stopped_thread_id().await {
            return Ok(thread_id);
        }
        let body = session
            .send_request("threads", None, request_timeout(timeout_ms))
            .await?;
        body.get("threads")
            .and_then(Value::as_array)
            .and_then(|threads| threads.first())
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("debugger reported no threads"))
    }

    async fn resolve_frame_id(
        &self,
        session: &Arc<DapSession>,
        provided: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<u64> {
        self.maybe_resolve_frame_id(session, provided, timeout_ms)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("no active stack frame; run stack_trace or provide frame_id")
            })
    }

    async fn maybe_resolve_frame_id(
        &self,
        session: &Arc<DapSession>,
        provided: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<Option<u64>> {
        if provided.is_some() {
            return Ok(provided);
        }
        let thread_id = match self.resolve_thread_id(session, None, timeout_ms).await {
            Ok(thread_id) => thread_id,
            Err(_) => return Ok(None),
        };
        let body = session
            .send_request(
                "stackTrace",
                Some(json!({ "threadId": thread_id, "levels": 1 })),
                request_timeout(timeout_ms),
            )
            .await?;
        Ok(body
            .get("stackFrames")
            .and_then(Value::as_array)
            .and_then(|frames| frames.first())
            .and_then(|frame| frame.get("id"))
            .and_then(Value::as_u64))
    }

    async fn run_and_wait(
        &self,
        command: &str,
        thread_id: Option<u64>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapRunOutcome> {
        let session = self.active_session().await?;
        let thread_id = self
            .resolve_thread_id(&session, thread_id, timeout_ms)
            .await?;
        let start_index = session.event_len().await;
        if command != "pause" {
            session.set_status(DapSessionStatus::Running).await;
        }
        session
            .send_request(
                command,
                Some(json!({ "threadId": thread_id })),
                request_timeout(timeout_ms),
            )
            .await?;
        let event = session
            .wait_for_event_after(
                start_index,
                &["stopped", "terminated", "exited"],
                request_timeout(timeout_ms),
            )
            .await?;
        let state = session.status().await;
        Ok(DapRunOutcome {
            session: session.summary().await,
            state,
            timed_out: event.is_none(),
            event,
            event_count: session.event_len().await,
            output_tail: tail_chars(&session.output().await, 4_000),
        })
    }

    async fn sync_source_breakpoints(
        &self,
        session: Arc<DapSession>,
        source_path: String,
        current: Vec<DapSourceBreakpointRecord>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapBreakpointOutcome> {
        let breakpoints = current
            .iter()
            .map(|entry| {
                let mut bp = json!({ "line": entry.line });
                if let Some(condition) = &entry.condition {
                    bp["condition"] = json!(condition);
                }
                bp
            })
            .collect::<Vec<_>>();
        let response = session
            .send_request(
                "setBreakpoints",
                Some(json!({
                    "source": {
                        "path": source_path.clone(),
                        "name": Path::new(&source_path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("source"),
                    },
                    "breakpoints": breakpoints,
                })),
                request_timeout(timeout_ms),
            )
            .await?;
        let adapter_breakpoints = response
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mapped = current
            .into_iter()
            .enumerate()
            .map(|(index, mut entry)| {
                if let Some(adapter) = adapter_breakpoints.get(index) {
                    entry.verified = adapter
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    entry.id = adapter.get("id").and_then(Value::as_u64);
                    entry.message = adapter
                        .get("message")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                entry
            })
            .collect::<Vec<_>>();
        {
            let mut cache = session.source_breakpoints.lock().await;
            if mapped.is_empty() {
                cache.remove(&source_path);
            } else {
                cache.insert(source_path.clone(), mapped.clone());
            }
        }
        Ok(DapBreakpointOutcome {
            session: session.summary().await,
            source_path,
            breakpoints: mapped,
        })
    }

    async fn sync_function_breakpoints(
        &self,
        session: Arc<DapSession>,
        current: Vec<DapFunctionBreakpointRecord>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapFunctionBreakpointOutcome> {
        let breakpoints = current
            .iter()
            .map(|entry| {
                let mut bp = json!({ "name": entry.name });
                if let Some(condition) = &entry.condition {
                    bp["condition"] = json!(condition);
                }
                bp
            })
            .collect::<Vec<_>>();
        let response = session
            .send_request(
                "setFunctionBreakpoints",
                Some(json!({ "breakpoints": breakpoints })),
                request_timeout(timeout_ms),
            )
            .await?;
        let adapter_breakpoints = response
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mapped = current
            .into_iter()
            .enumerate()
            .map(|(index, mut entry)| {
                if let Some(adapter) = adapter_breakpoints.get(index) {
                    entry.verified = adapter
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    entry.id = adapter.get("id").and_then(Value::as_u64);
                    entry.message = adapter
                        .get("message")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                entry
            })
            .collect::<Vec<_>>();
        *session.function_breakpoints.lock().await = mapped.clone();
        Ok(DapFunctionBreakpointOutcome {
            session: session.summary().await,
            breakpoints: mapped,
        })
    }

    async fn sync_instruction_breakpoints(
        &self,
        session: Arc<DapSession>,
        current: Vec<DapInstructionBreakpointRecord>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapInstructionBreakpointOutcome> {
        let breakpoints = current
            .iter()
            .map(|entry| {
                let mut bp = json!({ "instructionReference": entry.instruction_ref });
                if let Some(offset) = entry.offset {
                    bp["offset"] = json!(offset);
                }
                if let Some(condition) = &entry.condition {
                    bp["condition"] = json!(condition);
                }
                if let Some(hit_condition) = &entry.hit_condition {
                    bp["hitCondition"] = json!(hit_condition);
                }
                bp
            })
            .collect::<Vec<_>>();
        let response = session
            .send_request(
                "setInstructionBreakpoints",
                Some(json!({ "breakpoints": breakpoints })),
                request_timeout(timeout_ms),
            )
            .await?;
        let adapter_breakpoints = response
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mapped = current
            .into_iter()
            .enumerate()
            .map(|(index, mut entry)| {
                if let Some(adapter) = adapter_breakpoints.get(index) {
                    entry.verified = adapter
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    entry.id = adapter.get("id").and_then(Value::as_u64);
                    entry.message = adapter
                        .get("message")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                entry
            })
            .collect::<Vec<_>>();
        *session.instruction_breakpoints.lock().await = mapped.clone();
        Ok(DapInstructionBreakpointOutcome {
            session: session.summary().await,
            breakpoints: mapped,
        })
    }

    async fn sync_data_breakpoints(
        &self,
        session: Arc<DapSession>,
        current: Vec<DapDataBreakpointRecord>,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<DapDataBreakpointOutcome> {
        let breakpoints = current
            .iter()
            .map(|entry| {
                let mut bp = json!({ "dataId": entry.data_id });
                if let Some(access_type) = &entry.access_type {
                    bp["accessType"] = json!(access_type);
                }
                if let Some(condition) = &entry.condition {
                    bp["condition"] = json!(condition);
                }
                if let Some(hit_condition) = &entry.hit_condition {
                    bp["hitCondition"] = json!(hit_condition);
                }
                bp
            })
            .collect::<Vec<_>>();
        let response = session
            .send_request(
                "setDataBreakpoints",
                Some(json!({ "breakpoints": breakpoints })),
                request_timeout(timeout_ms),
            )
            .await?;
        let adapter_breakpoints = response
            .get("breakpoints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mapped = current
            .into_iter()
            .enumerate()
            .map(|(index, mut entry)| {
                if let Some(adapter) = adapter_breakpoints.get(index) {
                    entry.verified = adapter
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    entry.id = adapter.get("id").and_then(Value::as_u64);
                    entry.message = adapter
                        .get("message")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                entry
            })
            .collect::<Vec<_>>();
        *session.data_breakpoints.lock().await = mapped.clone();
        Ok(DapDataBreakpointOutcome {
            session: session.summary().await,
            breakpoints: mapped,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapOutputSnapshot {
    pub session: DapSessionSummary,
    pub output: String,
}

struct DapSession {
    id: String,
    adapter: String,
    command: String,
    cwd: PathBuf,
    launched_unix_ms: u128,
    child: Arc<AsyncMutex<Child>>,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    next_seq: AtomicU64,
    pending: PendingRequests,
    events: Arc<AsyncMutex<VecDeque<DapEventRecord>>>,
    event_notify: Arc<Notify>,
    source_breakpoints: SourceBreakpointCache,
    function_breakpoints: FunctionBreakpointCache,
    instruction_breakpoints: InstructionBreakpointCache,
    data_breakpoints: DataBreakpointCache,
    status: Arc<AsyncMutex<DapSessionStatus>>,
    capabilities: Arc<AsyncMutex<Option<Value>>>,
}

impl DapSession {
    async fn spawn(launch: DapAdapterLaunch) -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new(&launch.command)
            .args(&launch.args)
            .current_dir(&launch.cwd)
            // 与 lsp/shell 一致：drop 即杀，防 setup 错误路径或会话未显式 terminate 就被 drop 时
            // 调试适配器子进程**泄漏成孤儿**（terminate 的显式 kill 仍在，这是兜底）。
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("DAP adapter stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("DAP adapter stdout was not piped"))?;
        let session = Arc::new(Self {
            id: format!("debug-{}", unix_ms()),
            adapter: launch.name,
            command: launch.command,
            cwd: launch.cwd,
            launched_unix_ms: unix_ms(),
            child: Arc::new(AsyncMutex::new(child)),
            stdin: Arc::new(AsyncMutex::new(stdin)),
            next_seq: AtomicU64::new(0),
            pending: Arc::new(AsyncMutex::new(HashMap::new())),
            events: Arc::new(AsyncMutex::new(VecDeque::new())),
            event_notify: Arc::new(Notify::new()),
            source_breakpoints: Arc::new(AsyncMutex::new(HashMap::new())),
            function_breakpoints: Arc::new(AsyncMutex::new(Vec::new())),
            instruction_breakpoints: Arc::new(AsyncMutex::new(Vec::new())),
            data_breakpoints: Arc::new(AsyncMutex::new(Vec::new())),
            status: Arc::new(AsyncMutex::new(DapSessionStatus::Starting)),
            capabilities: Arc::new(AsyncMutex::new(None)),
        });
        tokio::spawn(read_loop(session.clone(), stdout));
        Ok(session)
    }

    async fn send_request(
        &self,
        command: &str,
        arguments: Option<Value>,
        deadline: Duration,
    ) -> anyhow::Result<Value> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let request = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments.unwrap_or(Value::Null),
        });
        let payload = encode_dap_message(&request)?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(seq, tx);
        let mut pending_guard = PendingRequestGuard::new(self.pending.clone(), seq);
        let write_result = async {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(&payload).await?;
            stdin.flush().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = write_result {
            pending_guard.remove_now().await;
            return Err(error);
        }
        let response = match timeout(deadline, rx).await {
            Ok(response) => {
                pending_guard.disarm();
                response
                    .map_err(|_| anyhow::anyhow!("DAP adapter closed before answering {command}"))?
            }
            Err(_) => {
                pending_guard.remove_now().await;
                anyhow::bail!(
                    "DAP request {command} timed out after {}ms",
                    deadline.as_millis()
                );
            }
        };
        response.map_err(|message| anyhow::anyhow!(message))
    }

    async fn summary(&self) -> DapSessionSummary {
        DapSessionSummary {
            id: self.id.clone(),
            adapter: self.adapter.clone(),
            command: self.command.clone(),
            cwd: display_path(&self.cwd),
            status: self.status().await,
            launched_unix_ms: self.launched_unix_ms,
            last_event: self
                .events
                .lock()
                .await
                .back()
                .map(|event| event.event.clone()),
            pending_requests: self.pending.lock().await.len(),
            capabilities: self.capabilities.lock().await.clone(),
        }
    }

    async fn events(&self) -> Vec<DapEventRecord> {
        self.events.lock().await.iter().cloned().collect()
    }

    async fn recent_events(&self, max: usize) -> Vec<DapEventRecord> {
        let events = self.events.lock().await;
        let skip = events.len().saturating_sub(max);
        events.iter().skip(skip).cloned().collect()
    }

    async fn event_len(&self) -> usize {
        self.events.lock().await.len()
    }

    async fn wait_for_event_after(
        &self,
        start_index: usize,
        names: &[&str],
        deadline: Duration,
    ) -> anyhow::Result<Option<DapEventRecord>> {
        let wait = async {
            loop {
                if let Some(event) = self.match_event_after(start_index, names).await {
                    return Some(event);
                }
                if self.status().await == DapSessionStatus::Terminated {
                    return self.match_event_after(start_index, names).await;
                }
                self.event_notify.notified().await;
            }
        };
        match timeout(deadline, wait).await {
            Ok(event) => Ok(event),
            Err(_) => Ok(None),
        }
    }

    async fn match_event_after(
        &self,
        start_index: usize,
        names: &[&str],
    ) -> Option<DapEventRecord> {
        self.events
            .lock()
            .await
            .iter()
            .skip(start_index)
            .find(|event| names.iter().any(|name| event.event == *name))
            .cloned()
    }

    async fn output(&self) -> String {
        self.events
            .lock()
            .await
            .iter()
            .filter(|event| event.event == "output")
            .filter_map(|event| {
                event
                    .body
                    .as_ref()
                    .and_then(|body| body.get("output"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("")
    }

    async fn last_stopped_thread_id(&self) -> Option<u64> {
        self.events
            .lock()
            .await
            .iter()
            .rev()
            .find(|event| event.event == "stopped")
            .and_then(|event| event.body.as_ref())
            .and_then(|body| body.get("threadId"))
            .and_then(Value::as_u64)
    }

    async fn status(&self) -> DapSessionStatus {
        self.status.lock().await.clone()
    }

    async fn set_status(&self, status: DapSessionStatus) {
        *self.status.lock().await = status;
    }

    async fn set_capabilities(&self, capabilities: Value) {
        *self.capabilities.lock().await = Some(capabilities);
    }

    async fn is_alive(&self) -> bool {
        self.child
            .lock()
            .await
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(false)
    }

    async fn dispose(&self) {
        self.set_status(DapSessionStatus::Terminated).await;
        reject_all_pending(&self.pending, "DAP session terminated").await;
        let mut child = self.child.lock().await;
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill().await;
        }
    }
}

struct PendingRequestGuard {
    pending: PendingRequests,
    seq: u64,
    armed: bool,
}

impl PendingRequestGuard {
    fn new(pending: PendingRequests, seq: u64) -> Self {
        Self {
            pending,
            seq,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    async fn remove_now(&mut self) {
        if self.armed {
            self.pending.lock().await.remove(&self.seq);
            self.armed = false;
        }
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            let pending = self.pending.clone();
            let seq = self.seq;
            tokio::spawn(async move {
                pending.lock().await.remove(&seq);
            });
        }
    }
}

async fn read_loop(session: Arc<DapSession>, mut stdout: tokio::process::ChildStdout) {
    let mut decoder = DapMessageDecoder::new();
    let mut buf = [0_u8; 8192];
    loop {
        match stdout.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => match decoder.push(&buf[..n]) {
                Ok(messages) => {
                    for message in messages {
                        handle_message(&session, message).await;
                    }
                }
                Err(error) => {
                    reject_all_pending(&session.pending, &format!("DAP decode error: {error}"))
                        .await;
                    session.set_status(DapSessionStatus::Terminated).await;
                    return;
                }
            },
            Err(error) => {
                reject_all_pending(&session.pending, &format!("DAP read error: {error}")).await;
                session.set_status(DapSessionStatus::Terminated).await;
                return;
            }
        }
    }
    reject_all_pending(&session.pending, "DAP adapter stdout closed").await;
    if session.status().await != DapSessionStatus::Terminated {
        session.set_status(DapSessionStatus::Terminated).await;
    }
}

async fn handle_message(session: &Arc<DapSession>, message: Value) {
    match message.get("type").and_then(Value::as_str) {
        Some("response") => handle_response(session, message).await,
        Some("event") => handle_event(session, message).await,
        Some("request") => handle_reverse_request(session, message).await,
        _ => {}
    }
}

async fn handle_response(session: &Arc<DapSession>, message: Value) {
    let Some(request_seq) = message.get("request_seq").and_then(Value::as_u64) else {
        return;
    };
    let Some(tx) = session.pending.lock().await.remove(&request_seq) else {
        return;
    };
    let success = message
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if success {
        let _ = tx.send(Ok(message.get("body").cloned().unwrap_or(Value::Null)));
    } else {
        let message = message
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("DAP request failed")
            .to_string();
        let _ = tx.send(Err(message));
    }
}

async fn handle_event(session: &Arc<DapSession>, message: Value) {
    let Some(event) = message.get("event").and_then(Value::as_str) else {
        return;
    };
    match event {
        "stopped" => session.set_status(DapSessionStatus::Stopped).await,
        "continued" | "initialized" => session.set_status(DapSessionStatus::Running).await,
        "terminated" | "exited" => session.set_status(DapSessionStatus::Terminated).await,
        _ => {}
    }
    let mut events = session.events.lock().await;
    events.push_back(DapEventRecord {
        event: event.to_string(),
        body: message.get("body").cloned(),
    });
    while events.len() > MAX_EVENT_CACHE {
        events.pop_front();
    }
    drop(events);
    session.event_notify.notify_waiters();
}

async fn handle_reverse_request(session: &Arc<DapSession>, message: Value) {
    let seq = message.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let command = message
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let response = json!({
        "seq": session.next_seq.fetch_add(1, Ordering::SeqCst) + 1,
        "type": "response",
        "request_seq": seq,
        "success": false,
        "command": command,
        "message": format!("unsupported reverse DAP request: {command}"),
    });
    if let Ok(payload) = encode_dap_message(&response) {
        let mut stdin = session.stdin.lock().await;
        let _ = stdin.write_all(&payload).await;
        let _ = stdin.flush().await;
    }
}

async fn reject_all_pending(pending: &PendingRequests, message: &str) {
    for (_, tx) in pending.lock().await.drain() {
        let _ = tx.send(Err(message.to_string()));
    }
}

fn request_timeout(timeout_ms: Option<u64>) -> Duration {
    timeout_ms
        .map(|ms| Duration::from_millis(ms.clamp(1, 300_000)))
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
}

fn normalized_event_names(event_names: Option<Vec<String>>) -> Vec<String> {
    let names = event_names
        .unwrap_or_else(|| vec!["stopped".into(), "terminated".into(), "exited".into()])
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if names.is_empty() {
        vec!["stopped".into(), "terminated".into(), "exited".into()]
    } else {
        names
    }
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    text.chars().skip(char_count - max_chars).collect()
}

fn resolve_cwd(workdir: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let root = workspace_root(workdir);
    let raw_path = Path::new(raw);
    let target = if raw_path.is_absolute() {
        normalize(raw_path)
    } else {
        normalize(root.join(raw_path))
    };
    let target = std::fs::canonicalize(&target).unwrap_or(target);
    if !target.starts_with(&root) {
        anyhow::bail!("cwd escapes workdir: {raw} (workdir: {})", root.display());
    }
    Ok(target)
}

fn resolve_adapter_command(workdir: &Path, raw: &str) -> anyhow::Result<String> {
    let raw_path = Path::new(raw);
    let has_path_component =
        raw_path.components().count() > 1 || raw.contains('/') || raw.contains('\\');
    if has_path_component || raw_path.is_absolute() {
        return Ok(display_path(resolve_cwd(workdir, raw)?));
    }
    Ok(raw.to_string())
}

fn resolve_debug_file(workdir: &Path, raw: Option<&str>) -> anyhow::Result<String> {
    let raw = raw
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("file is required"))?;
    Ok(display_path(resolve_cwd(workdir, raw)?))
}

fn normalized_debuggee_arguments(
    workdir: &Path,
    action: &str,
    arguments: Option<Value>,
    program: Option<String>,
    program_args: Option<Vec<String>>,
    cwd: Option<String>,
) -> anyhow::Result<Value> {
    let mut map = match arguments {
        Some(Value::Object(map)) => map,
        Some(_) => anyhow::bail!("arguments must be an object for {action}"),
        None => Map::new(),
    };

    if let Some(program) = program.filter(|s| !s.trim().is_empty()) {
        map.insert(
            "program".into(),
            Value::String(resolve_debug_path(workdir, &program)?),
        );
    }
    if let Some(program_args) = program_args {
        map.insert("args".into(), json!(program_args));
    }
    if let Some(cwd) = cwd.filter(|s| !s.trim().is_empty()) {
        map.insert(
            "cwd".into(),
            Value::String(display_path(resolve_cwd(workdir, &cwd)?)),
        );
    }

    normalize_path_field(workdir, &mut map, "program")?;
    normalize_path_field(workdir, &mut map, "cwd")?;

    if action == "launch" && !map.contains_key("program") {
        anyhow::bail!("program is required for launch");
    }

    Ok(Value::Object(map))
}

fn normalize_path_field(
    workdir: &Path,
    map: &mut Map<String, Value>,
    field: &str,
) -> anyhow::Result<()> {
    let raw = match map.get(field) {
        Some(Value::String(raw)) => raw.clone(),
        _ => return Ok(()),
    };
    if raw.contains("${") || raw.trim().is_empty() {
        return Ok(());
    }
    let normalized = if field == "cwd" {
        display_path(resolve_cwd(workdir, &raw)?)
    } else {
        resolve_debug_path(workdir, &raw)?
    };
    map.insert(field.to_string(), Value::String(normalized));
    Ok(())
}

fn resolve_debug_path(workdir: &Path, raw: &str) -> anyhow::Result<String> {
    Ok(display_path(resolve_cwd(workdir, raw)?))
}

fn required_nonempty(raw: Option<String>, field: &str, action: &str) -> anyhow::Result<String> {
    raw.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{field} is required for {action}"))
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

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn display_path(path: impl AsRef<Path>) -> String {
    path.as_ref().display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn manager_spawns_initializes_requests_and_terminates() {
        let root = temp_dir("botobot-dap-session");
        fs::create_dir_all(&root).unwrap();
        let adapter = compile_fake_adapter(&root);
        let manager = DapSessionManager::new();

        let started = manager
            .start_adapter(
                DapAdapterLaunch {
                    name: "fake".into(),
                    command: adapter.display().to_string(),
                    args: Vec::new(),
                    cwd: root.clone(),
                },
                Some(5_000),
            )
            .await
            .unwrap();

        assert_eq!(started.adapter, "fake");
        assert_eq!(started.status, DapSessionStatus::Running);
        assert!(started.capabilities.is_some());

        let response = manager
            .send_request("threads", None, Some(5_000))
            .await
            .unwrap();
        assert_eq!(response.body["threads"][0]["name"], "main");

        let timeout = manager
            .send_request("hang", None, Some(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(timeout.contains("timed out"));
        assert_eq!(
            manager.active_summary().await.unwrap().pending_requests,
            0,
            "timed-out requests should not leak pending entries"
        );

        let events = manager.events().await.unwrap();
        assert!(events.iter().any(|event| event.event == "initialized"));

        let terminated = manager.terminate(Some(5_000)).await.unwrap().unwrap();
        assert_eq!(terminated.status, DapSessionStatus::Terminated);
        assert!(manager.list_sessions().await.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn manager_enforces_single_active_session() {
        let root = temp_dir("botobot-dap-session-single");
        fs::create_dir_all(&root).unwrap();
        let adapter = compile_fake_adapter(&root);
        let manager = DapSessionManager::new();
        let launch = DapAdapterLaunch {
            name: "fake".into(),
            command: adapter.display().to_string(),
            args: Vec::new(),
            cwd: root.clone(),
        };
        manager
            .start_adapter(launch.clone(), Some(5_000))
            .await
            .unwrap();
        let err = manager
            .start_adapter(launch, Some(5_000))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("still active"));
        let _ = manager.terminate(Some(5_000)).await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn debug_tool_launches_requests_lists_events_and_terminates() {
        let root = temp_dir("botobot-debug-tool");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let adapter = compile_fake_adapter(&root);
        let manager = DapSessionManager::new();

        let launched = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::LaunchAdapter,
                adapter: Some("fake".into()),
                adapter_command: Some(adapter.display().to_string()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::LaunchAdapter)
            },
        )
        .await
        .unwrap();
        assert_eq!(launched["adapter"], "fake");

        let status = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Status,
                ..debug_args(DebugAction::Status)
            },
        )
        .await
        .unwrap();
        assert_eq!(status["session"]["adapter"], "fake");
        assert!(status["event_count"].as_u64().unwrap() >= 1);
        assert!(status["output_tail"].as_str().unwrap().contains("hello"));

        let timed_out = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::WaitForEvent,
                event_names: Some(vec!["never".into()]),
                timeout_ms: Some(5),
                ..debug_args(DebugAction::WaitForEvent)
            },
        )
        .await
        .unwrap();
        assert_eq!(timed_out["timed_out"], true);
        assert!(timed_out["event"].is_null());

        let launch = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Launch,
                program: Some("main.rs".into()),
                program_args: Some(vec!["--flag".into()]),
                arguments: Some(json!({ "stopOnEntry": true })),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Launch)
            },
        )
        .await
        .unwrap();
        assert_eq!(launch["body"]["accepted"], true);

        let configuration_done = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::ConfigurationDone,
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::ConfigurationDone)
            },
        )
        .await
        .unwrap();
        assert_eq!(configuration_done["body"]["configured"], true);

        let attach = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Attach,
                arguments: Some(json!({ "processId": 42 })),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Attach)
            },
        )
        .await
        .unwrap();
        assert_eq!(attach["body"]["accepted"], true);

        let threads = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Threads,
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Threads)
            },
        )
        .await
        .unwrap();
        assert_eq!(threads["body"]["threads"][0]["name"], "main");

        let stack = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::StackTrace,
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::StackTrace)
            },
        )
        .await
        .unwrap();
        assert_eq!(stack["body"]["stackFrames"][0]["name"], "main");

        let scopes = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Scopes,
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Scopes)
            },
        )
        .await
        .unwrap();
        assert_eq!(scopes["body"]["scopes"][0]["name"], "Locals");

        let variables = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Variables,
                variable_ref: Some(7),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Variables)
            },
        )
        .await
        .unwrap();
        assert_eq!(variables["body"]["variables"][0]["name"], "x");

        let data_info = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::DataBreakpointInfo,
                variable_ref: Some(7),
                data_name: Some("x".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::DataBreakpointInfo)
            },
        )
        .await
        .unwrap();
        assert_eq!(data_info["body"]["dataId"], "data:x");

        let evaluation = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Evaluate,
                expression: Some("x + 1".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Evaluate)
            },
        )
        .await
        .unwrap();
        assert_eq!(evaluation["body"]["result"], "8");

        let output = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Output,
                ..debug_args(DebugAction::Output)
            },
        )
        .await
        .unwrap();
        assert!(output["output"].as_str().unwrap().contains("hello"));

        let set_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::SetBreakpoint,
                file: Some("main.rs".into()),
                line: Some(1),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::SetBreakpoint)
            },
        )
        .await
        .unwrap();
        assert_eq!(set_breakpoint["breakpoints"][0]["line"], 1);
        assert_eq!(set_breakpoint["breakpoints"][0]["verified"], true);

        let remove_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::RemoveBreakpoint,
                file: Some("main.rs".into()),
                line: Some(1),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::RemoveBreakpoint)
            },
        )
        .await
        .unwrap();
        assert!(
            remove_breakpoint["breakpoints"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let set_function_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::SetFunctionBreakpoint,
                function: Some("main".into()),
                condition: Some("argc > 0".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::SetFunctionBreakpoint)
            },
        )
        .await
        .unwrap();
        assert_eq!(set_function_breakpoint["breakpoints"][0]["name"], "main");
        assert_eq!(set_function_breakpoint["breakpoints"][0]["verified"], true);

        let remove_function_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::RemoveFunctionBreakpoint,
                function: Some("main".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::RemoveFunctionBreakpoint)
            },
        )
        .await
        .unwrap();
        assert!(
            remove_function_breakpoint["breakpoints"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let set_instruction_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::SetInstructionBreakpoint,
                instruction_ref: Some("0x1000".into()),
                offset: Some(4),
                condition: Some("rax == 1".into()),
                hit_condition: Some("2".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::SetInstructionBreakpoint)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            set_instruction_breakpoint["breakpoints"][0]["instruction_ref"],
            "0x1000"
        );
        assert_eq!(
            set_instruction_breakpoint["breakpoints"][0]["verified"],
            true
        );

        let remove_instruction_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::RemoveInstructionBreakpoint,
                instruction_ref: Some("0x1000".into()),
                offset: Some(4),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::RemoveInstructionBreakpoint)
            },
        )
        .await
        .unwrap();
        assert!(
            remove_instruction_breakpoint["breakpoints"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let set_data_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::SetDataBreakpoint,
                data_id: Some("data:x".into()),
                access_type: Some("write".into()),
                condition: Some("x > 0".into()),
                hit_condition: Some("1".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::SetDataBreakpoint)
            },
        )
        .await
        .unwrap();
        assert_eq!(set_data_breakpoint["breakpoints"][0]["data_id"], "data:x");
        assert_eq!(set_data_breakpoint["breakpoints"][0]["verified"], true);

        let remove_data_breakpoint = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::RemoveDataBreakpoint,
                data_id: Some("data:x".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::RemoveDataBreakpoint)
            },
        )
        .await
        .unwrap();
        assert!(
            remove_data_breakpoint["breakpoints"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        for (action, label) in [
            (DebugAction::Continue, "continue"),
            (DebugAction::StepOver, "step_over"),
            (DebugAction::StepIn, "step_in"),
            (DebugAction::StepOut, "step_out"),
            (DebugAction::Pause, "pause"),
        ] {
            let outcome = run_debug_tool(
                &manager,
                &root,
                DebugArgs {
                    action,
                    timeout_ms: Some(5_000),
                    ..debug_args(DebugAction::Events)
                },
            )
            .await
            .unwrap();
            assert_eq!(outcome["state"], "stopped", "{label} should stop");
            assert_eq!(outcome["timed_out"], false);
            assert!(outcome["event_count"].as_u64().unwrap() >= 1);
            assert!(outcome["output_tail"].as_str().unwrap().contains("hello"));
        }

        let waited = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::WaitForEvent,
                event_names: Some(vec!["stopped".into()]),
                event_start_index: Some(0),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::WaitForEvent)
            },
        )
        .await
        .unwrap();
        assert_eq!(waited["timed_out"], false);
        assert_eq!(waited["event"]["event"], "stopped");

        let raw_threads = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::CustomRequest,
                dap_command: Some("threads".into()),
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::CustomRequest)
            },
        )
        .await
        .unwrap();
        assert_eq!(raw_threads["body"]["threads"][0]["name"], "main");

        let events = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Events,
                ..debug_args(DebugAction::Events)
            },
        )
        .await
        .unwrap();
        assert!(
            events
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["event"] == "initialized")
        );

        let terminated = run_debug_tool(
            &manager,
            &root,
            DebugArgs {
                action: DebugAction::Terminate,
                timeout_ms: Some(5_000),
                ..debug_args(DebugAction::Terminate)
            },
        )
        .await
        .unwrap();
        assert_eq!(terminated["status"], "terminated");

        let _ = fs::remove_dir_all(root);
    }

    fn debug_args(action: DebugAction) -> DebugArgs {
        DebugArgs {
            action,
            adapter: None,
            adapter_command: None,
            args: None,
            program: None,
            program_args: None,
            cwd: None,
            dap_command: None,
            arguments: None,
            thread_id: None,
            frame_id: None,
            variable_ref: None,
            expression: None,
            context: None,
            levels: None,
            file: None,
            line: None,
            condition: None,
            function: None,
            instruction_ref: None,
            offset: None,
            hit_condition: None,
            data_name: None,
            data_id: None,
            access_type: None,
            event_names: None,
            event_start_index: None,
            timeout_ms: None,
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
    }

    fn compile_fake_adapter(root: &Path) -> PathBuf {
        let source = root.join("fake_dap_adapter.rs");
        let exe = root.join(if cfg!(windows) {
            "fake_dap_adapter.exe"
        } else {
            "fake_dap_adapter"
        });
        fs::write(&source, FAKE_ADAPTER).unwrap();
        let output = std::process::Command::new("rustc")
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "rustc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    }

    const FAKE_ADAPTER: &str = r##"
use std::io::{Read, Write};

fn main() {
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut buffer = Vec::<u8>::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let n = stdin.read(&mut chunk).unwrap();
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
        while let Some((message, used)) = next_message(&buffer) {
            buffer.drain(..used);
            let seq = json_number(&message, "seq").unwrap_or(0);
            let command = json_string(&message, "command").unwrap_or_default();
            match command.as_str() {
                "initialize" => {
                    write_json(&mut stdout, r#"{"seq":1,"type":"event","event":"initialized"}"#);
                    write_json(&mut stdout, r#"{"seq":6,"type":"event","event":"output","body":{"output":"hello debug\n"}}"#);
                    write_json(&mut stdout, &format!(r#"{{"seq":2,"type":"response","request_seq":{seq},"success":true,"command":"initialize","body":{{"supportsConfigurationDoneRequest":false}}}}"#));
                }
                "launch" => {
                    let ok = message.contains("\"program\"") && message.contains("main.rs") && message.contains("--flag");
                    write_json(&mut stdout, &format!(r#"{{"seq":15,"type":"response","request_seq":{seq},"success":{ok},"command":"launch","body":{{"accepted":{ok}}},"message":"invalid launch request"}}"#));
                }
                "configurationDone" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":16,"type":"response","request_seq":{seq},"success":true,"command":"configurationDone","body":{{"configured":true}}}}"#));
                }
                "attach" => {
                    let ok = message.contains("\"processId\":42");
                    write_json(&mut stdout, &format!(r#"{{"seq":17,"type":"response","request_seq":{seq},"success":{ok},"command":"attach","body":{{"accepted":{ok}}},"message":"invalid attach request"}}"#));
                }
                "hang" => {}
                "threads" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":3,"type":"response","request_seq":{seq},"success":true,"command":"threads","body":{{"threads":[{{"id":1,"name":"main"}}]}}}}"#));
                }
                "stackTrace" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":7,"type":"response","request_seq":{seq},"success":true,"command":"stackTrace","body":{{"stackFrames":[{{"id":11,"name":"main","line":3,"column":1,"source":{{"path":"main.rs"}}}}],"totalFrames":1}}}}"#));
                }
                "scopes" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":8,"type":"response","request_seq":{seq},"success":true,"command":"scopes","body":{{"scopes":[{{"name":"Locals","variablesReference":7,"expensive":false}}]}}}}"#));
                }
                "variables" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":9,"type":"response","request_seq":{seq},"success":true,"command":"variables","body":{{"variables":[{{"name":"x","value":"7","variablesReference":0}}]}}}}"#));
                }
                "dataBreakpointInfo" => {
                    let ok = message.contains("\"variablesReference\":7") && message.contains("\"name\":\"x\"");
                    write_json(&mut stdout, &format!(r#"{{"seq":19,"type":"response","request_seq":{seq},"success":{ok},"command":"dataBreakpointInfo","body":{{"dataId":"data:x","description":"x","accessTypes":["read","write"],"canPersist":true}},"message":"invalid data breakpoint info request"}}"#));
                }
                "evaluate" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":10,"type":"response","request_seq":{seq},"success":true,"command":"evaluate","body":{{"result":"8","variablesReference":0}}}}"#));
                }
                "setBreakpoints" => {
                    let count = message.matches("\"line\":").count();
                    let breakpoints = (0..count)
                        .map(|index| format!(r#"{{"id":{},"verified":true}}"#, index + 1))
                        .collect::<Vec<_>>()
                        .join(",");
                    write_json(&mut stdout, &format!(r#"{{"seq":13,"type":"response","request_seq":{seq},"success":true,"command":"setBreakpoints","body":{{"breakpoints":[{breakpoints}]}}}}"#));
                }
                "setFunctionBreakpoints" => {
                    let count = message.matches("\"name\":").count();
                    let breakpoints = (0..count)
                        .map(|index| format!(r#"{{"id":{},"verified":true}}"#, index + 101))
                        .collect::<Vec<_>>()
                        .join(",");
                    write_json(&mut stdout, &format!(r#"{{"seq":14,"type":"response","request_seq":{seq},"success":true,"command":"setFunctionBreakpoints","body":{{"breakpoints":[{breakpoints}]}}}}"#));
                }
                "setInstructionBreakpoints" => {
                    let count = message.matches("\"instructionReference\":").count();
                    let breakpoints = (0..count)
                        .map(|index| format!(r#"{{"id":{},"verified":true}}"#, index + 201))
                        .collect::<Vec<_>>()
                        .join(",");
                    write_json(&mut stdout, &format!(r#"{{"seq":18,"type":"response","request_seq":{seq},"success":true,"command":"setInstructionBreakpoints","body":{{"breakpoints":[{breakpoints}]}}}}"#));
                }
                "setDataBreakpoints" => {
                    let count = message.matches("\"dataId\":").count();
                    let breakpoints = (0..count)
                        .map(|index| format!(r#"{{"id":{},"verified":true}}"#, index + 301))
                        .collect::<Vec<_>>()
                        .join(",");
                    write_json(&mut stdout, &format!(r#"{{"seq":20,"type":"response","request_seq":{seq},"success":true,"command":"setDataBreakpoints","body":{{"breakpoints":[{breakpoints}]}}}}"#));
                }
                "continue" | "next" | "stepIn" | "stepOut" | "pause" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":11,"type":"response","request_seq":{seq},"success":true,"command":"{command}"}}"#));
                    write_json(&mut stdout, r#"{"seq":12,"type":"event","event":"stopped","body":{"threadId":1,"reason":"step"}}"#);
                }
                "disconnect" => {
                    write_json(&mut stdout, &format!(r#"{{"seq":4,"type":"response","request_seq":{seq},"success":true,"command":"disconnect"}}"#));
                    return;
                }
                other => {
                    write_json(&mut stdout, &format!(r#"{{"seq":5,"type":"response","request_seq":{seq},"success":false,"command":"{other}","message":"unsupported"}}"#));
                }
            }
        }
    }
}

fn next_message(buffer: &[u8]) -> Option<(String, usize)> {
    let header_end = buffer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header = std::str::from_utf8(&buffer[..header_end]).ok()?;
    let len = header.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
    })?;
    let start = header_end + 4;
    let end = start + len;
    if buffer.len() < end {
        return None;
    }
    Some((String::from_utf8_lossy(&buffer[start..end]).into_owned(), end))
}

fn write_json(stdout: &mut std::io::Stdout, body: &str) {
    write!(stdout, "Content-Length: {}\r\n\r\n{}", body.as_bytes().len(), body).unwrap();
    stdout.flush().unwrap();
}

fn json_number(message: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = message.find(&needle)? + needle.len();
    let digits: String = message[start..].chars().skip_while(|ch| ch.is_whitespace()).take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn json_string(message: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = message.find(&needle)? + needle.len();
    let end = message[start..].find('"')?;
    Some(message[start..start + end].to_string())
}
"##;
}
