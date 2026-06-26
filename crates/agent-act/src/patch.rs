//! Patch editing tool for coder profiles.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyPatchArgs {
    /// Patch text in the Codex-style "*** Begin Patch" format.
    pub patch: String,
    /// Validate and preview changes without writing files. Defaults to false.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ApplyPatchOut {
    pub applied: bool,
    pub dry_run: bool,
    pub operations: Vec<PatchOpSummary>,
}

#[derive(Debug, Serialize)]
pub struct PatchOpSummary {
    pub op: &'static str,
    pub path: String,
    pub new_path: Option<String>,
    pub hunks: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
}

pub struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply or preview a Codex-style patch. Supports Add File, Delete File, Update File, Move to, and @@ hunks."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(ApplyPatchArgs))
            .unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: ApplyPatchArgs = serde_json::from_value(args)?;
        apply_patch(&ctx.workdir, &args.patch, args.dry_run.unwrap_or(false)).map(|out| json!(out))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ApplyPatchArgs = serde_json::from_value(args)?;
        apply_patch(
            &std::env::current_dir()?,
            &args.patch,
            args.dry_run.unwrap_or(false),
        )
        .map(|out| json!(out))
    }
}

pub fn apply_patch(workdir: &Path, patch: &str, dry_run: bool) -> anyhow::Result<ApplyPatchOut> {
    let workspace = workspace_root(workdir);
    let ops = parse_patch(patch)?;
    anyhow::ensure!(!ops.is_empty(), "patch contains no operations");

    let mut materialized = Vec::new();
    for op in ops {
        materialized.push(materialize_op(&workspace, op)?);
    }

    let summaries = materialized.iter().map(MaterializedOp::summary).collect();
    if !dry_run {
        for op in materialized {
            op.write()?;
        }
    }

    Ok(ApplyPatchOut {
        applied: !dry_run,
        dry_run,
        operations: summaries,
    })
}

#[derive(Debug)]
enum PatchOp {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug)]
struct Hunk {
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

#[derive(Debug)]
enum MaterializedOp {
    Add {
        path: PathBuf,
        display: String,
        content: String,
        added_lines: usize,
    },
    Delete {
        path: PathBuf,
        display: String,
    },
    Update {
        path: PathBuf,
        display: String,
        move_to: Option<(PathBuf, String)>,
        content: String,
        hunks: usize,
        added_lines: usize,
        removed_lines: usize,
    },
}

impl MaterializedOp {
    fn summary(&self) -> PatchOpSummary {
        match self {
            Self::Add {
                display,
                added_lines,
                ..
            } => PatchOpSummary {
                op: "add",
                path: display.clone(),
                new_path: None,
                hunks: 0,
                added_lines: *added_lines,
                removed_lines: 0,
            },
            Self::Delete { display, .. } => PatchOpSummary {
                op: "delete",
                path: display.clone(),
                new_path: None,
                hunks: 0,
                added_lines: 0,
                removed_lines: 0,
            },
            Self::Update {
                display,
                move_to,
                hunks,
                added_lines,
                removed_lines,
                ..
            } => PatchOpSummary {
                op: if move_to.is_some() {
                    "move_update"
                } else {
                    "update"
                },
                path: display.clone(),
                new_path: move_to.as_ref().map(|(_, display)| display.clone()),
                hunks: *hunks,
                added_lines: *added_lines,
                removed_lines: *removed_lines,
            },
        }
    }

    fn write(self) -> anyhow::Result<()> {
        match self {
            Self::Add { path, content, .. } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, content)?;
            }
            Self::Delete { path, .. } => {
                anyhow::ensure!(
                    path.is_file(),
                    "delete target is not a file: {}",
                    path.display()
                );
                std::fs::remove_file(path)?;
            }
            Self::Update {
                path,
                move_to,
                content,
                ..
            } => {
                let target = move_to.as_ref().map(|(path, _)| path).unwrap_or(&path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, content)?;
                if let Some((target, _)) = move_to {
                    if target != path && path.exists() {
                        std::fs::remove_file(path)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn parse_patch(patch: &str) -> anyhow::Result<Vec<PatchOp>> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut i = 0usize;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    anyhow::ensure!(
        i < lines.len() && lines[i].trim() == "*** Begin Patch",
        "patch must start with *** Begin Patch"
    );
    i += 1;

    let mut ops = Vec::new();
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.trim() == "*** End Patch" {
            return Ok(ops);
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut add_lines = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let raw = lines[i];
                let content = raw.strip_prefix('+').ok_or_else(|| {
                    anyhow::anyhow!("add file {} line {} must start with '+'", path, i + 1)
                })?;
                add_lines.push(content.to_string());
                i += 1;
            }
            ops.push(PatchOp::Add {
                path: path.trim().to_string(),
                lines: add_lines,
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOp::Delete {
                path: path.trim().to_string(),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            i += 1;
            let mut move_to = None;
            if i < lines.len() {
                if let Some(new_path) = lines[i].strip_prefix("*** Move to: ") {
                    move_to = Some(new_path.trim().to_string());
                    i += 1;
                }
            }
            let mut hunks = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let header = lines[i].trim();
                anyhow::ensure!(
                    header.starts_with("@@"),
                    "expected @@ hunk header at patch line {}",
                    i + 1
                );
                i += 1;
                let mut hunk_lines = Vec::new();
                while i < lines.len()
                    && !lines[i].starts_with("@@")
                    && !lines[i].starts_with("*** ")
                {
                    let raw = lines[i];
                    let (tag, content) = raw.split_at(raw.len().min(1));
                    let content = content.to_string();
                    match tag {
                        " " => hunk_lines.push(HunkLine::Context(content)),
                        "+" => hunk_lines.push(HunkLine::Add(content)),
                        "-" => hunk_lines.push(HunkLine::Remove(content)),
                        _ => {
                            anyhow::bail!("invalid hunk line {}: expected ' ', '+', or '-'", i + 1)
                        }
                    }
                    i += 1;
                }
                anyhow::ensure!(
                    !hunk_lines.is_empty(),
                    "empty update hunk for {path} before patch line {}",
                    i + 1
                );
                hunks.push(Hunk { lines: hunk_lines });
            }
            ops.push(PatchOp::Update {
                path,
                move_to,
                hunks,
            });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            anyhow::bail!("unknown patch directive at line {}: {line}", i + 1);
        }
    }
    anyhow::bail!("patch missing *** End Patch");
}

fn materialize_op(workspace: &Path, op: PatchOp) -> anyhow::Result<MaterializedOp> {
    match op {
        PatchOp::Add { path, lines } => {
            let target = resolve_under_workdir(workspace, &path)?;
            let added_lines = lines.len();
            Ok(MaterializedOp::Add {
                path: target,
                display: path,
                content: lines_to_text(&lines),
                added_lines,
            })
        }
        PatchOp::Delete { path } => {
            let target = resolve_under_workdir(workspace, &path)?;
            anyhow::ensure!(target.is_file(), "delete target does not exist: {path}");
            Ok(MaterializedOp::Delete {
                path: target,
                display: path,
            })
        }
        PatchOp::Update {
            path,
            move_to,
            hunks,
        } => {
            let target = resolve_under_workdir(workspace, &path)?;
            anyhow::ensure!(target.is_file(), "update target does not exist: {path}");
            let before = std::fs::read_to_string(&target)?;
            let mut lines: Vec<String> = before.lines().map(str::to_string).collect();
            let had_trailing_newline = before.ends_with('\n');
            // 保留原文件换行风格：`str::lines()` 剥掉了 `\r`，若不还原会把 CRLF 文件整体转成 LF——
            // 一次补丁静默改写全文件换行（超出补丁范围，违背"保留无关改动"）。本仓库即 CRLF 工作树。
            let newline = if before.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let added_lines = hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .filter(|l| matches!(l, HunkLine::Add(_)))
                .count();
            let removed_lines = hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .filter(|l| matches!(l, HunkLine::Remove(_)))
                .count();
            for (idx, hunk) in hunks.iter().enumerate() {
                apply_hunk(&mut lines, hunk).map_err(|err| {
                    anyhow::anyhow!("failed to apply hunk {} for {}: {err}", idx + 1, path)
                })?;
            }
            let mut content = lines.join(newline);
            if had_trailing_newline || !content.is_empty() {
                content.push_str(newline);
            }
            let move_to = move_to
                .map(|new_path| {
                    let resolved = resolve_under_workdir(workspace, &new_path)?;
                    Ok::<_, anyhow::Error>((resolved, new_path))
                })
                .transpose()?;
            Ok(MaterializedOp::Update {
                path: target,
                display: path,
                move_to,
                content,
                hunks: hunks.len(),
                added_lines,
                removed_lines,
            })
        }
    }
}

fn apply_hunk(lines: &mut Vec<String>, hunk: &Hunk) -> anyhow::Result<()> {
    let old: Vec<String> = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.clone()),
            HunkLine::Add(_) => None,
        })
        .collect();
    let new: Vec<String> = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(s) | HunkLine::Add(s) => Some(s.clone()),
            HunkLine::Remove(_) => None,
        })
        .collect();

    if old.is_empty() {
        lines.splice(0..0, new);
        return Ok(());
    }
    let Some(pos) = find_sequence(lines, &old) else {
        anyhow::bail!("context not found");
    };
    lines.splice(pos..pos + old.len(), new);
    Ok(())
}

fn find_sequence(lines: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    lines
        .windows(needle.len())
        .position(|window| window.iter().zip(needle).all(|(a, b)| a == b))
}

fn lines_to_text(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

fn resolve_under_workdir(workdir: &Path, raw: &str) -> anyhow::Result<PathBuf> {
    let root = workspace_root(workdir);
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        root.join(raw_path)
    };
    let target = std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize(joined));
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("botobot-patch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn apply_patch_add_update_delete_and_dry_run() {
        let root = temp_workspace();
        std::fs::write(root.join("modify.txt"), "line1\nline2\nline3\n").unwrap();
        std::fs::write(root.join("delete.txt"), "bye\n").unwrap();
        let patch = r#"*** Begin Patch
*** Add File: nested/new.txt
+created
*** Update File: modify.txt
@@
-line2
+changed
*** Delete File: delete.txt
*** End Patch
"#;

        let dry = apply_patch(&root, patch, true).unwrap();
        assert!(!dry.applied);
        assert!(!root.join("nested/new.txt").exists());
        assert!(root.join("delete.txt").exists());

        let out = apply_patch(&root, patch, false).unwrap();
        assert!(out.applied);
        assert_eq!(
            std::fs::read_to_string(root.join("nested/new.txt")).unwrap(),
            "created\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("modify.txt")).unwrap(),
            "line1\nchanged\nline3\n"
        );
        assert!(!root.join("delete.txt").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    // 回归：CRLF 文件打补丁后保留 CRLF 换行（旧实现 join("\n") 会把全文件静默转 LF）。
    #[test]
    fn apply_patch_preserves_crlf_line_endings() {
        let root = temp_workspace();
        std::fs::write(root.join("win.txt"), "line1\r\nline2\r\nline3\r\n").unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: win.txt\n@@\n-line2\n+changed\n*** End Patch\n";
        apply_patch(&root, patch, false).unwrap();
        let after = std::fs::read_to_string(root.join("win.txt")).unwrap();
        assert_eq!(
            after, "line1\r\nchanged\r\nline3\r\n",
            "应保留 CRLF，不静默转 LF"
        );
        // LF 文件仍保持 LF（不误加 \r）。
        std::fs::write(root.join("nix.txt"), "a\nb\nc\n").unwrap();
        let p2 = "*** Begin Patch\n*** Update File: nix.txt\n@@\n-b\n+B\n*** End Patch\n";
        apply_patch(&root, p2, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("nix.txt")).unwrap(),
            "a\nB\nc\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_patch_reports_missing_context() {
        let root = temp_workspace();
        std::fs::write(root.join("modify.txt"), "line1\nline2\n").unwrap();
        let patch = r#"*** Begin Patch
*** Update File: modify.txt
@@
-missing
+changed
*** End Patch
"#;

        let err = apply_patch(&root, patch, false).unwrap_err();
        assert!(err.to_string().contains("hunk 1"));
        assert!(err.to_string().contains("context not found"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_patch_rejects_path_escape() {
        let root = temp_workspace();
        let patch = r#"*** Begin Patch
*** Add File: ../outside.txt
+nope
*** End Patch
"#;

        let err = apply_patch(&root, patch, false).unwrap_err();
        assert!(err.to_string().contains("escapes workdir"));

        let _ = std::fs::remove_dir_all(root);
    }
}
