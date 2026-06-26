use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::patch::{ApplyPatchOut, apply_patch};
use crate::resource::hashline_hash;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditByHashlineArgs {
    /// File path under the workspace.
    pub path: String,
    /// Hashline anchor copied from read output, for example "¶src/lib.rs#abc123 L42 | old".
    pub anchor: String,
    /// Replacement text for the anchored line. May contain multiple lines; empty text deletes it.
    pub replacement: String,
    /// Validate and preview the generated patch without writing files. Defaults to false.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct EditByHashlineOut {
    pub applied: bool,
    pub dry_run: bool,
    pub path: String,
    pub line: usize,
    pub old_hash: String,
    pub new_hashes: Vec<String>,
    pub patch: String,
    pub patch_result: ApplyPatchOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedAnchor {
    hash: String,
    line: usize,
    text: Option<String>,
}

pub struct EditByHashlineTool;

#[async_trait]
impl Tool for EditByHashlineTool {
    fn name(&self) -> &str {
        "edit_by_hashline"
    }

    fn description(&self) -> &str {
        "Edit a file by a hashline anchor from read output. Verifies the current line hash, rejects drift, generates a patch, and applies it through apply_patch."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(EditByHashlineArgs))
            .unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: EditByHashlineArgs = serde_json::from_value(args)?;
        edit_by_hashline(&ctx.workdir, args).map(|out| json!(out))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: EditByHashlineArgs = serde_json::from_value(args)?;
        edit_by_hashline(&std::env::current_dir()?, args).map(|out| json!(out))
    }
}

pub fn edit_by_hashline(
    workdir: &Path,
    args: EditByHashlineArgs,
) -> anyhow::Result<EditByHashlineOut> {
    let workspace = workspace_root(workdir);
    let target = resolve_under_workdir(&workspace, &args.path)?;
    let display_path = display_path(&workspace, &target);
    let anchor = parse_anchor(&args.anchor)?;
    let content = std::fs::read_to_string(&target)?;
    let mut lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
    if content.ends_with('\n') && lines.is_empty() {
        lines.push(String::new());
    }
    anyhow::ensure!(
        (1..=lines.len()).contains(&anchor.line),
        "anchor line {} is outside file with {} lines",
        anchor.line,
        lines.len()
    );

    let old_line = &lines[anchor.line - 1];
    let current_hash = hashline_hash(&args.path, anchor.line, old_line);
    if current_hash != anchor.hash {
        let current_display_hash = hashline_hash(&display_path, anchor.line, old_line);
        if current_display_hash == anchor.hash {
            anyhow::bail!(
                "anchor path mismatch: hash matches workspace-relative path `{display_path}` but args.path was `{}`",
                args.path
            );
        }
        let candidates = anchor
            .text
            .as_deref()
            .map(|text| candidate_lines(&args.path, &lines, text))
            .unwrap_or_default();
        anyhow::bail!(
            "hashline drift at {}:{}: expected {}, got {}; candidates: {}",
            args.path,
            anchor.line,
            anchor.hash,
            current_hash,
            candidates.join(", ")
        );
    }

    let replacement: Vec<String> = if args.replacement.is_empty() {
        Vec::new()
    } else {
        args.replacement.lines().map(ToOwned::to_owned).collect()
    };
    let patch = build_patch(&display_path, &lines, anchor.line, &replacement)?;
    let patch_result = apply_patch(&workspace, &patch, args.dry_run.unwrap_or(false))?;

    let new_hashes = replacement
        .iter()
        .enumerate()
        .map(|(idx, line)| hashline_hash(&args.path, anchor.line + idx, line))
        .collect();

    Ok(EditByHashlineOut {
        applied: patch_result.applied,
        dry_run: patch_result.dry_run,
        path: display_path,
        line: anchor.line,
        old_hash: anchor.hash,
        new_hashes,
        patch,
        patch_result,
    })
}

fn parse_anchor(raw: &str) -> anyhow::Result<ParsedAnchor> {
    let hash_start = raw
        .find('#')
        .map(|idx| idx + 1)
        .or_else(|| raw.find("hash=").map(|idx| idx + "hash=".len()))
        .unwrap_or(0);
    let hash: String = raw[hash_start..]
        .chars()
        .take_while(|ch| ch.is_ascii_hexdigit())
        .collect();
    anyhow::ensure!(hash.len() >= 6, "anchor is missing a 6+ hex hash");

    // 行号标记 `L<digits>` 恒在 hash 之后（格式 `…#<hash> L<line> | text`）。从 hash 末尾起扫描，
    // 取**第一个后接数字的 'L'**——否则路径里的大写 L（如 `src/Lexer.rs`）会被 `find('L')` 误中、
    // 读不到数字而整条解析失败（无法编辑任何含大写 L 路径的文件，真实高频 bug）。
    let after_hash = hash_start + hash.len();
    let line = raw[after_hash..]
        .match_indices('L')
        .find_map(|(idx, _)| {
            raw[after_hash + idx + 1..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
                .parse::<usize>()
                .ok()
                .filter(|&n| n > 0)
        })
        .ok_or_else(|| anyhow::anyhow!("anchor is missing L<line>"))?;

    let text = raw.split_once(" | ").map(|(_, text)| text.to_string());
    Ok(ParsedAnchor { hash, line, text })
}

fn build_patch(
    path: &str,
    lines: &[String],
    line: usize,
    replacement: &[String],
) -> anyhow::Result<String> {
    let idx = line - 1;
    let start = idx.saturating_sub(3);
    let end = (idx + 4).min(lines.len());
    let needle = &lines[start..end];
    let matches = lines
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(pos, _)| pos)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matches == [start],
        "hashline context is ambiguous; use apply_patch directly"
    );

    let mut patch = String::from("*** Begin Patch\n");
    patch.push_str(&format!("*** Update File: {path}\n@@\n"));
    for line in &lines[start..idx] {
        patch.push_str(&format!(" {line}\n"));
    }
    patch.push_str(&format!("-{}\n", lines[idx]));
    for line in replacement {
        patch.push_str(&format!("+{line}\n"));
    }
    for line in &lines[idx + 1..end] {
        patch.push_str(&format!(" {line}\n"));
    }
    patch.push_str("*** End Patch\n");
    Ok(patch)
}

fn candidate_lines(path: &str, lines: &[String], text: &str) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.as_str() == text)
        .map(|(idx, line)| {
            let line_no = idx + 1;
            format!("L{}#{}", line_no, hashline_hash(path, line_no, line))
        })
        .collect()
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

fn display_path(workdir: &Path, path: &Path) -> String {
    path.strip_prefix(workdir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("botobot-edit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn edit_by_hashline_replaces_anchored_line_through_patch() {
        let root = temp_workspace();
        let file = root.join("a.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let hash = hashline_hash("a.txt", 2, "beta");
        let out = edit_by_hashline(
            &root,
            EditByHashlineArgs {
                path: "a.txt".into(),
                anchor: format!("¶a.txt#{hash} L2 | beta"),
                replacement: "BETA\nsecond".into(),
                dry_run: None,
            },
        )
        .unwrap();

        assert!(out.applied);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\nsecond\ngamma\n"
        );
        assert_eq!(out.new_hashes.len(), 2);
        assert!(out.patch.contains("*** Update File: a.txt"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn edit_by_hashline_rejects_drift_with_candidates() {
        let root = temp_workspace();
        let file = root.join("a.txt");
        std::fs::write(&file, "alpha\nchanged\nbeta\n").unwrap();
        let old_hash = hashline_hash("a.txt", 2, "beta");
        let err = edit_by_hashline(
            &root,
            EditByHashlineArgs {
                path: "a.txt".into(),
                anchor: format!("¶a.txt#{old_hash} L2 | beta"),
                replacement: "BETA".into(),
                dry_run: None,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("drift"));
        assert!(err.contains("L3#"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nchanged\nbeta\n"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn edit_by_hashline_rejects_path_escape() {
        let root = temp_workspace();
        let err = edit_by_hashline(
            &root,
            EditByHashlineArgs {
                path: "../outside.txt".into(),
                anchor: "abc123 L1 | nope".into(),
                replacement: "x".into(),
                dry_run: None,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("escapes workdir"));
        std::fs::remove_dir_all(root).ok();
    }

    // 回归：路径含大写 L（Lexer.rs）时，行号 'L' 标记仍正确解析（旧 find('L') 误中路径 L）。
    #[test]
    fn parse_anchor_handles_capital_l_in_path() {
        let a = parse_anchor("¶src/Lexer.rs#abc123 L42 | code").unwrap();
        assert_eq!(a.line, 42, "应取行标记 L42 而非路径里的 L");
        assert_eq!(a.hash, "abc123");
        assert_eq!(a.text.as_deref(), Some("code"));
        // 多个大写 L（List/Layout）也不误中。
        let b = parse_anchor("¶crates/Layout/List.rs#deadbeef L7 | x").unwrap();
        assert_eq!(b.line, 7);
    }

    // 端到端：编辑大写 L 路径下的文件（旧实现会因 anchor 解析失败而无法编辑）。
    #[test]
    fn edit_by_hashline_works_for_capital_l_path() {
        let root = temp_workspace();
        let sub = root.join("Lexer");
        std::fs::create_dir_all(&sub).unwrap();
        let rel = "Lexer/Mod.rs";
        std::fs::write(root.join(rel), "one\ntwo\nthree\n").unwrap();
        let hash = hashline_hash(rel, 2, "two");
        let out = edit_by_hashline(
            &root,
            EditByHashlineArgs {
                path: rel.into(),
                anchor: format!("¶{rel}#{hash} L2 | two"),
                replacement: "TWO".into(),
                dry_run: None,
            },
        )
        .unwrap();
        assert!(out.applied);
        assert_eq!(out.line, 2);
        assert_eq!(
            std::fs::read_to_string(root.join(rel)).unwrap(),
            "one\nTWO\nthree\n"
        );
        std::fs::remove_dir_all(root).ok();
    }
}
