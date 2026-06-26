use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::lsp::{did_rename_files, path_from_file_uri, will_rename_files};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameFileArgs {
    /// Existing file path under the workspace.
    pub old_path: String,
    /// New file path under the workspace.
    pub new_path: String,
    /// Create missing destination parent directories. Defaults to false.
    pub create_dirs: Option<bool>,
    /// Overwrite an existing destination file. Defaults to false.
    pub overwrite: Option<bool>,
    /// Preview LSP edits and rename without writing. Defaults to false.
    pub dry_run: Option<bool>,
    /// LSP timeout in milliseconds. Defaults to 30000ms and is capped at 120000ms.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct RenameFileOut {
    pub renamed: bool,
    pub dry_run: bool,
    pub old_path: String,
    pub new_path: String,
    pub workspace_edit: WorkspaceEditSummary,
    pub lsp_error: Option<String>,
    pub did_rename_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct WorkspaceEditSummary {
    pub changed_files: Vec<String>,
    pub edit_count: usize,
}

#[derive(Debug, Clone)]
struct TextEdit {
    range: LspTextRange,
    new_text: String,
}

#[derive(Debug, Clone)]
struct LspTextRange {
    start_line: usize,
    start_character: usize,
    end_line: usize,
    end_character: usize,
}

pub struct RenameFileTool;

#[async_trait]
impl Tool for RenameFileTool {
    fn name(&self) -> &str {
        "rename_file"
    }

    fn description(&self) -> &str {
        "Rename a file under the workspace and apply safe LSP workspace/willRenameFiles text edits when available."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(RenameFileArgs))
            .unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: RenameFileArgs = serde_json::from_value(args)?;
        rename_file(&ctx.workdir, args).await.map(|out| json!(out))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: RenameFileArgs = serde_json::from_value(args)?;
        rename_file(&std::env::current_dir()?, args)
            .await
            .map(|out| json!(out))
    }
}

pub async fn rename_file(workdir: &Path, args: RenameFileArgs) -> anyhow::Result<RenameFileOut> {
    let workspace = workspace_root(workdir);
    let old_path = resolve_existing_file(&workspace, &args.old_path)?;
    let new_path = resolve_new_path(&workspace, &args.new_path)?;
    anyhow::ensure!(old_path != new_path, "old_path and new_path are the same");
    let dry_run = args.dry_run.unwrap_or(false);
    let overwrite = args.overwrite.unwrap_or(false);
    let create_dirs = args.create_dirs.unwrap_or(false);
    if new_path.exists() && !overwrite {
        anyhow::bail!(
            "destination already exists: {}",
            display_path(&workspace, &new_path)
        );
    }
    if let Some(parent) = new_path.parent() {
        if !parent.exists() && !create_dirs {
            anyhow::bail!(
                "destination parent does not exist: {}",
                display_path(&workspace, parent)
            );
        }
    }

    let deadline = Duration::from_millis(args.timeout_ms.unwrap_or(30_000).clamp(1, 120_000));
    let mut lsp_error = None;
    let workspace_edit = if workspace.join("Cargo.toml").is_file() {
        match will_rename_files(&workspace, &old_path, &new_path, deadline).await {
            Ok(edit) => apply_workspace_edit(&workspace, &edit, dry_run)?,
            Err(err) => {
                lsp_error = Some(err.to_string());
                WorkspaceEditSummary::default()
            }
        }
    } else {
        WorkspaceEditSummary::default()
    };

    if !dry_run {
        if create_dirs {
            if let Some(parent) = new_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        if new_path.exists() && overwrite {
            std::fs::remove_file(&new_path)?;
        }
        std::fs::rename(&old_path, &new_path)?;
    }

    let did_rename_error = if !dry_run && workspace.join("Cargo.toml").is_file() {
        did_rename_files(&workspace, &old_path, &new_path, deadline)
            .await
            .err()
            .map(|err| err.to_string())
    } else {
        None
    };

    Ok(RenameFileOut {
        renamed: !dry_run,
        dry_run,
        old_path: display_path(&workspace, &old_path),
        new_path: display_path(&workspace, &new_path),
        workspace_edit,
        lsp_error,
        did_rename_error,
    })
}

fn apply_workspace_edit(
    workspace: &Path,
    edit: &Value,
    dry_run: bool,
) -> anyhow::Result<WorkspaceEditSummary> {
    let workspace = workspace_root(workspace);
    let mut edits_by_file: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();
    collect_workspace_edits(&workspace, edit, &mut edits_by_file)?;

    let mut summary = WorkspaceEditSummary::default();
    for (path, edits) in edits_by_file {
        if edits.is_empty() {
            continue;
        }
        let original = std::fs::read_to_string(&path)?;
        let next = apply_text_edits(&original, &edits)?;
        summary.edit_count += edits.len();
        summary.changed_files.push(display_path(&workspace, &path));
        if !dry_run && next != original {
            std::fs::write(path, next)?;
        }
    }
    Ok(summary)
}

fn collect_workspace_edits(
    workspace: &Path,
    edit: &Value,
    out: &mut BTreeMap<PathBuf, Vec<TextEdit>>,
) -> anyhow::Result<()> {
    if edit.is_null() {
        return Ok(());
    }
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            collect_text_edits_for_uri(workspace, uri, edits, out)?;
        }
    }
    if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            if let (Some(uri), Some(edits)) = (
                change
                    .get("textDocument")
                    .and_then(|doc| doc.get("uri"))
                    .and_then(Value::as_str),
                change.get("edits"),
            ) {
                collect_text_edits_for_uri(workspace, uri, edits, out)?;
            }
        }
    }
    Ok(())
}

fn collect_text_edits_for_uri(
    workspace: &Path,
    uri: &str,
    edits: &Value,
    out: &mut BTreeMap<PathBuf, Vec<TextEdit>>,
) -> anyhow::Result<()> {
    let path = path_from_file_uri(uri).ok_or_else(|| anyhow::anyhow!("unsupported uri: {uri}"))?;
    let path = resolve_under_workdir(workspace, &path)?;
    let parsed = edits
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("workspace edit for {uri} is not an array"))?
        .iter()
        .map(parse_text_edit)
        .collect::<anyhow::Result<Vec<_>>>()?;
    out.entry(path).or_default().extend(parsed);
    Ok(())
}

fn parse_text_edit(value: &Value) -> anyhow::Result<TextEdit> {
    let range = value
        .get("range")
        .ok_or_else(|| anyhow::anyhow!("text edit missing range"))?;
    let start = range
        .get("start")
        .ok_or_else(|| anyhow::anyhow!("text edit missing start"))?;
    let end = range
        .get("end")
        .ok_or_else(|| anyhow::anyhow!("text edit missing end"))?;
    Ok(TextEdit {
        range: LspTextRange {
            start_line: json_usize(start, "line")?,
            start_character: json_usize(start, "character")?,
            end_line: json_usize(end, "line")?,
            end_character: json_usize(end, "character")?,
        },
        new_text: value
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn json_usize(value: &Value, key: &str) -> anyhow::Result<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .ok_or_else(|| anyhow::anyhow!("missing numeric {key}"))
}

fn apply_text_edits(text: &str, edits: &[TextEdit]) -> anyhow::Result<String> {
    let mut resolved = edits
        .iter()
        .map(|edit| {
            let start =
                offset_for_lsp_position(text, edit.range.start_line, edit.range.start_character)?;
            let end = offset_for_lsp_position(text, edit.range.end_line, edit.range.end_character)?;
            anyhow::ensure!(start <= end, "text edit start is after end");
            Ok((start, end, edit.new_text.clone()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    resolved.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in resolved.windows(2) {
        anyhow::ensure!(
            pair[0].1 <= pair[1].0,
            "overlapping text edits are unsupported"
        );
    }
    let mut out = text.to_string();
    for (start, end, new_text) in resolved.into_iter().rev() {
        out.replace_range(start..end, &new_text);
    }
    Ok(out)
}

fn offset_for_lsp_position(text: &str, line: usize, character: usize) -> anyhow::Result<usize> {
    let starts = line_starts(text);
    let start = *starts
        .get(line)
        .ok_or_else(|| anyhow::anyhow!("line {line} is outside file"))?;
    let raw_end = starts.get(line + 1).copied().unwrap_or(text.len());
    let mut line_text = &text[start..raw_end];
    if let Some(stripped) = line_text.strip_suffix("\r\n") {
        line_text = stripped;
    } else if let Some(stripped) = line_text.strip_suffix('\n') {
        line_text = stripped;
    }
    let mut utf16 = 0usize;
    for (idx, ch) in line_text.char_indices() {
        if utf16 == character {
            return Ok(start + idx);
        }
        utf16 += ch.len_utf16();
    }
    if utf16 == character {
        return Ok(start + line_text.len());
    }
    anyhow::bail!("character {character} is outside line {line}")
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn resolve_existing_file(workdir: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let path = resolve_under_workdir(workdir, raw)?;
    anyhow::ensure!(path.is_file(), "source file does not exist: {raw}");
    Ok(path)
}

fn resolve_new_path(workdir: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        workdir.join(raw_path)
    };
    let target = normalize(joined);
    if !target.starts_with(workdir) {
        anyhow::bail!(
            "path escapes workdir: {raw} (workdir: {})",
            workdir.display()
        );
    }
    Ok(target)
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

fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_workspace_edit_changes_shape() {
        let root = std::env::temp_dir().join(format!("botobot-rename-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("lib.rs");
        std::fs::write(&file, "mod old;\nuse crate::old::Thing;\n").unwrap();
        let edit = json!({
            "changes": {
                format!("file:///{}", file.to_string_lossy().replace('\\', "/")): [{
                    "range": {
                        "start": { "line": 1, "character": 11 },
                        "end": { "line": 1, "character": 14 }
                    },
                    "newText": "new"
                }]
            }
        });

        let summary = apply_workspace_edit(&root, &edit, false).unwrap();

        assert_eq!(summary.edit_count, 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "mod old;\nuse crate::new::Thing;\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rename_file_moves_file_without_lsp_workspace() {
        let root = std::env::temp_dir().join(format!("botobot-rename-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/old.rs"), "pub fn old() {}\n").unwrap();

        let out = rename_file(
            &root,
            RenameFileArgs {
                old_path: "src/old.rs".into(),
                new_path: "src/new.rs".into(),
                create_dirs: None,
                overwrite: None,
                dry_run: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();

        assert!(out.renamed);
        assert!(!root.join("src/old.rs").exists());
        assert!(root.join("src/new.rs").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rename_file_rejects_paths_escaping_workdir() {
        let root =
            std::env::temp_dir().join(format!("botobot-rename-esc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "x\n").unwrap();

        // dst 越界（../ 逃出 workdir）→ 拒绝，不把文件移出去
        let out = rename_file(
            &root,
            RenameFileArgs {
                old_path: "a.rs".into(),
                new_path: "../escaped.rs".into(),
                create_dirs: Some(true),
                overwrite: None,
                dry_run: None,
                timeout_ms: None,
            },
        )
        .await;
        assert!(
            out.is_err() && out.unwrap_err().to_string().contains("escapes workdir"),
            "dst 越界应被拒绝"
        );
        assert!(root.join("a.rs").exists(), "源文件不应被移走");

        // src 越界（绝对路径指向 workdir 外）→ 拒绝，不把外部文件拉进来
        let outside =
            std::env::temp_dir().join(format!("botobot-outside-{}.rs", uuid::Uuid::new_v4()));
        std::fs::write(&outside, "secret\n").unwrap();
        let out2 = rename_file(
            &root,
            RenameFileArgs {
                old_path: outside.to_string_lossy().to_string(),
                new_path: "pulled.rs".into(),
                create_dirs: None,
                overwrite: None,
                dry_run: None,
                timeout_ms: None,
            },
        )
        .await;
        assert!(out2.is_err(), "src 越界应被拒绝");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(root);
    }
}
