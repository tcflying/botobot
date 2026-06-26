//! Session-local todo scratchpad for agent planning.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TodoItem {
    /// Optional stable item id chosen by the model.
    pub id: Option<String>,
    /// Short task text.
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoWriteArgs {
    /// Complete replacement todo list for the current session.
    pub items: Vec<TodoItem>,
    /// Optional short note explaining the current plan.
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TodoWriteOut {
    pub session_id: String,
    pub path: String,
    pub items: Vec<TodoItem>,
    pub note: Option<String>,
    pub updated_unix_ms: u128,
}

pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Replace the current session todo scratchpad. Stores a small JSON todo list at .bot/sessions/<id>/todos.json."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(TodoWriteArgs))
            .unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: TodoWriteArgs = serde_json::from_value(args)?;
        run_todo_write(&ctx.session_id, args).await
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: TodoWriteArgs = serde_json::from_value(args)?;
        run_todo_write("local", args).await
    }
}

/// §2.9：todos 锚定到 **session-store-root**（CWD 相对 `.bot/sessions/<id>/todos.json`，
/// 与 SessionStore 同根、与会话同位），修掉旧 `<workdir>/.bot/todos/` 在多 bot 不同
/// workdir 下散落的锚点漂移；随 session 删除一并清理。
pub async fn run_todo_write(session_id: &str, args: TodoWriteArgs) -> ToolResult {
    validate_items(&args.items)?;
    let session_id = if session_id.trim().is_empty() {
        "local"
    } else {
        session_id
    };
    let path = todo_path(session_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let updated_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let out = TodoWriteOut {
        session_id: session_id.to_string(),
        path: display_path(&path),
        items: args.items,
        note: args.note.filter(|note| !note.trim().is_empty()),
        updated_unix_ms,
    };
    let bytes = serde_json::to_vec_pretty(&out)?;
    tokio::fs::write(&path, bytes).await?;
    Ok(json!(out))
}

/// `todo_read`（Read）：读回当前 session 的 todo scratchpad。补 `todo_write` 的盲区——
/// 上下文压缩后 agent 丢失 in-context 计划、又不知 `todos.json` 路径，无从恢复。
pub struct TodoReadTool;

#[async_trait]
impl Tool for TodoReadTool {
    fn name(&self) -> &str {
        "todo_read"
    }
    fn description(&self) -> &str {
        "Read back the current session's todo scratchpad (items + note). Use to recover your plan \
         after a context compaction or to check progress before updating it with todo_write."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    async fn call_with_context(&self, _args: Value, ctx: &ToolCtx) -> ToolResult {
        run_todo_read(&ctx.session_id).await
    }
    async fn call(&self, _args: Value) -> ToolResult {
        run_todo_read("local").await
    }
}

/// 读回 session todos（缺/坏文件 → 空列表，不报错）。
pub async fn run_todo_read(session_id: &str) -> ToolResult {
    let session_id = if session_id.trim().is_empty() {
        "local"
    } else {
        session_id
    };
    let path = todo_path(session_id);
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => Ok(v),
            Err(_) => Ok(json!({ "session_id": session_id, "items": [], "note": null })),
        },
        Err(_) => Ok(json!({ "session_id": session_id, "items": [], "note": null })),
    }
}

fn validate_items(items: &[TodoItem]) -> anyhow::Result<()> {
    anyhow::ensure!(items.len() <= 100, "todo_write supports at most 100 items");
    let in_progress = items
        .iter()
        .filter(|item| item.status == TodoStatus::InProgress)
        .count();
    anyhow::ensure!(
        in_progress <= 1,
        "todo_write supports at most one in_progress item"
    );
    for (index, item) in items.iter().enumerate() {
        let content = item.content.trim();
        anyhow::ensure!(!content.is_empty(), "todo item {index} has empty content");
        anyhow::ensure!(
            content.chars().count() <= 500,
            "todo item {index} is too long"
        );
    }
    Ok(())
}

fn todo_path(session_id: &str) -> PathBuf {
    // CWD 相对，对齐 SessionStore 的 `.bot` 根（lib.rs router 也用相对 `.bot`）。
    PathBuf::from(".bot")
        .join("sessions")
        .join(safe_file_component(session_id))
        .join("todos.json")
}

fn safe_file_component(raw: &str) -> String {
    let safe: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches('.');
    if safe.is_empty() {
        "local".into()
    } else {
        safe.to_string()
    }
}

fn display_path(path: impl AsRef<Path>) -> String {
    path.as_ref().display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: None,
            content: content.into(),
            status,
        }
    }

    #[tokio::test]
    async fn todo_write_persists_session_file() {
        // §2.9：CWD 相对 .bot/sessions/<safe_sid>/todos.json（用唯一 sid 避免与真实数据冲突）。
        let sid = format!("test-{}/x", uuid::Uuid::new_v4());
        let out = run_todo_write(
            &sid,
            TodoWriteArgs {
                items: vec![
                    item("inspect files", TodoStatus::Completed),
                    item("patch code", TodoStatus::InProgress),
                ],
                note: Some("keep it tight".into()),
            },
        )
        .await
        .unwrap();

        let path = todo_path(&sid);
        assert!(path.is_file(), "应落 .bot/sessions/<sid>/todos.json");
        assert!(path.ends_with("todos.json"));
        assert!(path.to_string_lossy().contains("sessions"));
        assert_eq!(out["items"].as_array().unwrap().len(), 2);
        // 清理本测试目录
        if let Some(session_dir) = path.parent() {
            let _ = std::fs::remove_dir_all(session_dir);
        }
    }

    // todo_read 读回 todo_write 写的内容；无文件 → 空列表。
    #[tokio::test]
    async fn todo_read_returns_written_or_empty() {
        let sid = format!("test-read-{}", uuid::Uuid::new_v4());
        // 无文件 → 空。
        let empty = run_todo_read(&sid).await.unwrap();
        assert_eq!(empty["items"].as_array().unwrap().len(), 0);
        // 写后读回。
        run_todo_write(
            &sid,
            TodoWriteArgs {
                items: vec![item("plan a", TodoStatus::InProgress)],
                note: Some("go".into()),
            },
        )
        .await
        .unwrap();
        let read = run_todo_read(&sid).await.unwrap();
        assert_eq!(read["items"].as_array().unwrap().len(), 1);
        assert_eq!(read["items"][0]["content"], "plan a");
        assert_eq!(read["note"], "go");
        if let Some(dir) = todo_path(&sid).parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn todo_write_rejects_multiple_in_progress_items() {
        let err = run_todo_write(
            "s",
            TodoWriteArgs {
                items: vec![
                    item("one", TodoStatus::InProgress),
                    item("two", TodoStatus::InProgress),
                ],
                note: None,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("at most one in_progress"));
    }
}
