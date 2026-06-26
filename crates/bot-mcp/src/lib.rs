//! MCP adapter for exposing botobot as a stdio JSON-RPC tool server.

use base_types::{AgentEvent, ContentPart, LlmOpts};
use bot_sdk::protocol::{EventMsg, SessionId};
use bot_sdk::{BotSdk, TurnOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "botobot-mcp";

#[derive(Clone)]
pub struct McpServer {
    sdk: BotSdk,
}

impl McpServer {
    pub fn new(sdk: BotSdk) -> Self {
        Self { sdk }
    }

    pub async fn handle_message(&self, input: &str) -> Option<Value> {
        let msg: RpcMessage = match serde_json::from_str(input) {
            Ok(msg) => msg,
            Err(err) => {
                return Some(error_response(
                    Value::Null,
                    -32700,
                    format!("parse error: {err}"),
                ));
            }
        };

        let id = msg.id.clone();
        match (id, msg.method.as_str()) {
            (Some(id), "initialize") => Some(success_response(id, self.initialize(msg.params))),
            (Some(id), "ping") => Some(success_response(id, json!({}))),
            (Some(id), "tools/list") => Some(success_response(id, self.list_tools())),
            (Some(id), "tools/call") => {
                Some(success_response(id, self.call_tool(msg.params).await))
            }
            (Some(id), "resources/list") => Some(success_response(
                id,
                json!({
                    "resources": [],
                    "nextCursor": null
                }),
            )),
            (Some(id), "resources/templates/list") => Some(success_response(
                id,
                json!({
                    "resourceTemplates": [],
                    "nextCursor": null
                }),
            )),
            (Some(id), "prompts/list") => Some(success_response(
                id,
                json!({
                    "prompts": [],
                    "nextCursor": null
                }),
            )),
            (Some(id), method) => Some(error_response(
                id,
                -32601,
                format!("method not found: {method}"),
            )),
            (None, _) => None,
        }
    }

    fn initialize(&self, params: Option<Value>) -> Value {
        let protocol_version = params
            .as_ref()
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION);
        json!({
            "protocolVersion": protocol_version,
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION")
            }
        })
    }

    fn list_tools(&self) -> Value {
        json!({
            "tools": [
                botobot_tool("botobot", "Start or continue a botobot coder bot thread with a prompt.", false),
                botobot_tool("botobot-reply", "Continue an existing botobot MCP thread.", true)
            ],
            "nextCursor": null
        })
    }

    async fn call_tool(&self, params: Option<Value>) -> Value {
        let Some(params) = params else {
            return tool_error("tools/call missing params");
        };
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

        match name {
            "botobot" => self.run_botobot(arguments, false).await,
            "botobot-reply" => self.run_botobot(arguments, true).await,
            "" => tool_error("tools/call missing tool name"),
            other => tool_error(format!("unknown tool: {other}")),
        }
    }

    async fn run_botobot(&self, arguments: Value, require_thread: bool) -> Value {
        let args: BotobotArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid botobot arguments: {err}")),
        };
        if require_thread && args.thread_id.as_deref().unwrap_or_default().is_empty() {
            return tool_error("botobot-reply requires thread_id");
        }
        if args.prompt.trim().is_empty() {
            return tool_error("prompt must not be empty");
        }

        let thread_id = args
            .thread_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| format!("mcp-{}", uuid::Uuid::new_v4()));
        let opts = LlmOpts {
            thinking: args.thinking,
            web_search: args.web_search,
            code_execution: args.code_execution,
            ..Default::default()
        };
        let output = self
            .sdk
            .run_parts_with_opts(
                thread_id.clone(),
                vec![ContentPart::Text(args.prompt)],
                opts,
            )
            .await;

        match output {
            Ok(turn) => turn_result(thread_id, turn),
            Err(err) => tool_error(format!("botobot turn failed: {err}")),
        }
    }
}

pub async fn run_stdio(sdk: BotSdk) -> anyhow::Result<()> {
    let server = McpServer::new(sdk);
    let stdin = io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = io::stdout();

    while let Some(line) = lines.next_line().await? {
        let Some(response) = server.handle_message(&line).await else {
            continue;
        };
        let bytes = serde_json::to_vec(&response)?;
        stdout.write_all(&bytes).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RpcMessage {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct BotobotArgs {
    prompt: String,
    #[serde(default, alias = "threadId")]
    thread_id: Option<SessionId>,
    #[serde(default)]
    thinking: Option<bool>,
    #[serde(default)]
    web_search: Option<bool>,
    #[serde(default)]
    code_execution: Option<bool>,
}

#[derive(Debug, Serialize)]
struct BotobotResult {
    thread_id: SessionId,
    output: String,
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

fn botobot_tool(name: &str, description: &str, require_thread: bool) -> Value {
    let mut required = vec!["prompt"];
    if require_thread {
        required.push("thread_id");
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "User prompt for the botobot coder bot."
                },
                "thread_id": {
                    "type": "string",
                    "description": "MCP thread id returned by a previous botobot call."
                },
                "thinking": {
                    "type": "boolean",
                    "description": "Optional per-turn thinking override."
                },
                "web_search": {
                    "type": "boolean",
                    "description": "Optional per-turn web search tool exposure."
                },
                "code_execution": {
                    "type": "boolean",
                    "description": "Optional per-turn code/shell tool exposure."
                }
            },
            "required": required
        }
    })
}

fn turn_result(thread_id: SessionId, turn: TurnOutput) -> Value {
    let mut final_output = String::new();
    let mut error = None::<String>;
    for ev in turn.events {
        match ev.msg {
            EventMsg::Agent(AgentEvent::Done { output, .. }) => final_output = output,
            EventMsg::Agent(AgentEvent::Error { message, .. }) => error = Some(message),
            EventMsg::Error { message } => error = Some(message),
            _ => {}
        }
    }

    if let Some(message) = error {
        return tool_error(message);
    }

    let data = BotobotResult {
        thread_id,
        output: final_output,
    };
    let text = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".into());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{Decision, Llm, LlmError, LlmEvent, Message, ToolSpec};
    use serde_json::json;
    use std::sync::Arc;

    struct EchoLlm;

    #[async_trait]
    impl Llm for EchoLlm {
        async fn infer(
            &self,
            messages: &[Message],
            _tools: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            let text = messages
                .last()
                .map(message_text)
                .unwrap_or_else(|| "empty".into());
            let decision = Decision {
                text: format!("echo: {text}"),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::TextDelta(decision.text.clone())),
                Ok(LlmEvent::Done(decision)),
            ])))
        }
    }

    #[tokio::test]
    async fn lists_botobot_tools() {
        let server = test_server();
        let response = server
            .handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "botobot");
        assert_eq!(tools[1]["name"], "botobot-reply");
    }

    #[tokio::test]
    async fn calls_botobot_and_returns_thread_id() {
        let server = test_server();
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":"call","method":"tools/call","params":{"name":"botobot","arguments":{"prompt":"hello"}}}"#,
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], false);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("mcp-"));
        assert!(text.contains("echo: hello"));
    }

    #[tokio::test]
    async fn reply_requires_thread_id() {
        let server = test_server();
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"botobot-reply","arguments":{"prompt":"hello"}}}"#,
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "botobot-reply requires thread_id"
        );
    }

    #[tokio::test]
    async fn initialize_echoes_protocol_version() {
        let server = test_server();
        let response = server
            .handle_message(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "protocolVersion": "test-version" }
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "test-version");
        assert_eq!(response["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    fn test_server() -> McpServer {
        let agent = agent_loop::Agent::builder().llm(Arc::new(EchoLlm)).build();
        McpServer::new(BotSdk::new(agent))
    }

    fn message_text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.as_str()),
                ContentPart::ImageUrl(_) => None,
            })
            .collect()
    }
}
