//! In-process project search tools for coder profiles.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use base_types::{Tool, ToolCtx, ToolResult, ToolTier};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Text or regex pattern to search for.
    pub query: String,
    /// Directory or file to search, relative to the workspace. Defaults to ".".
    pub path: Option<String>,
    /// Optional glob filter, for example "src/**/*.rs".
    pub glob: Option<String>,
    /// Treat query as a regular expression. Defaults to false.
    pub regex: Option<bool>,
    /// Case-sensitive matching. Defaults to true.
    pub case_sensitive: Option<bool>,
    /// Context lines before/after each match. Defaults to 1, capped at 5.
    pub context_lines: Option<usize>,
    /// Maximum matches to return. Defaults to 50, capped at 500.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchOut {
    pub root: String,
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub scanned_files: usize,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub match_text: String,
    pub line_text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    /// Directory to search, relative to the workspace. Defaults to ".".
    pub path: Option<String>,
    /// Glob pattern to match paths, for example "crates/**/*.rs". Defaults to "*".
    pub glob: Option<String>,
    /// Optional substring that must appear in the file name.
    pub name: Option<String>,
    /// Include directories as well as files. Defaults to false.
    pub include_dirs: Option<bool>,
    /// Maximum paths to return. Defaults to 100, capped at 1000.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct FindOut {
    pub root: String,
    pub paths: Vec<FindMatch>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct FindMatch {
    pub path: String,
    pub is_dir: bool,
}

pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search workspace text files in-process. Returns structured path, line, column, match, and context."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(SearchArgs))
            .unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: SearchArgs = serde_json::from_value(args)?;
        search_workspace(&ctx.workdir, args).map(|out| json!(out))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: SearchArgs = serde_json::from_value(args)?;
        search_workspace(&std::env::current_dir()?, args).map(|out| json!(out))
    }
}

pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files in the workspace with ignore-aware traversal and optional glob/name filters."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schema_for!(FindArgs)).unwrap_or_else(|_| json!({ "type": "object" }))
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }

    async fn call_with_context(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let args: FindArgs = serde_json::from_value(args)?;
        find_workspace(&ctx.workdir, args).map(|out| json!(out))
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: FindArgs = serde_json::from_value(args)?;
        find_workspace(&std::env::current_dir()?, args).map(|out| json!(out))
    }
}

fn search_workspace(workdir: &Path, args: SearchArgs) -> anyhow::Result<SearchOut> {
    let workspace = workspace_root(workdir);
    let root = resolve_under_workdir(&workspace, args.path.as_deref().unwrap_or("."))?;
    let glob = compile_glob(args.glob.as_deref())?;
    let matcher = QueryMatcher::new(
        &args.query,
        args.regex.unwrap_or(false),
        args.case_sensitive.unwrap_or(true),
    )?;
    let context = args.context_lines.unwrap_or(1).min(5);
    let limit = args.limit.unwrap_or(50).clamp(1, 500);

    let mut matches = Vec::new();
    let mut scanned_files = 0usize;
    let mut truncated = false;
    for entry in WalkBuilder::new(&root).standard_filters(true).build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() || !glob_matches(&glob, &workspace, path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned_files += 1;
        let lines: Vec<&str> = text.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            for hit in matcher.find_iter(line) {
                matches.push(SearchMatch {
                    path: display_path(&workspace, path),
                    line: idx + 1,
                    column: hit.start + 1,
                    match_text: hit.text.to_string(),
                    line_text: (*line).to_string(),
                    context_before: context_window_before(&lines, idx, context),
                    context_after: context_window_after(&lines, idx, context),
                });
                if matches.len() >= limit {
                    truncated = true;
                    return Ok(SearchOut {
                        root: display_path(&workspace, &root),
                        query: args.query,
                        matches,
                        scanned_files,
                        truncated,
                    });
                }
            }
        }
    }

    Ok(SearchOut {
        root: display_path(&workspace, &root),
        query: args.query,
        matches,
        scanned_files,
        truncated,
    })
}

fn find_workspace(workdir: &Path, args: FindArgs) -> anyhow::Result<FindOut> {
    let workspace = workspace_root(workdir);
    let root = resolve_under_workdir(&workspace, args.path.as_deref().unwrap_or("."))?;
    let glob = compile_glob(args.glob.as_deref())?;
    let name = args.name.unwrap_or_default();
    let include_dirs = args.include_dirs.unwrap_or(false);
    let limit = args.limit.unwrap_or(100).clamp(1, 1000);

    let mut paths = Vec::new();
    let mut truncated = false;
    for entry in WalkBuilder::new(&root).standard_filters(true).build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let is_dir = path.is_dir();
        if is_dir && !include_dirs {
            continue;
        }
        if !is_dir && !path.is_file() {
            continue;
        }
        if !name.is_empty()
            && !path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.contains(&name))
        {
            continue;
        }
        if !glob_matches(&glob, &workspace, path) {
            continue;
        }
        paths.push(FindMatch {
            path: display_path(&workspace, path),
            is_dir,
        });
        if paths.len() >= limit {
            truncated = true;
            break;
        }
    }

    Ok(FindOut {
        root: display_path(&workspace, &root),
        paths,
        truncated,
    })
}

struct MatchHit<'a> {
    start: usize,
    text: &'a str,
}

enum QueryMatcher {
    Regex(Regex),
    Literal {
        needle: String,
        folded_needle: String,
        case_sensitive: bool,
    },
}

impl QueryMatcher {
    fn new(query: &str, regex: bool, case_sensitive: bool) -> anyhow::Result<Self> {
        if query.is_empty() {
            anyhow::bail!("search query must not be empty");
        }
        if regex {
            let matcher = RegexBuilder::new(query)
                .case_insensitive(!case_sensitive)
                .build()?;
            return Ok(Self::Regex(matcher));
        }
        Ok(Self::Literal {
            needle: query.to_string(),
            folded_needle: query.to_lowercase(),
            case_sensitive,
        })
    }

    fn find_iter<'a>(&'a self, line: &'a str) -> Vec<MatchHit<'a>> {
        match self {
            Self::Regex(re) => re
                .find_iter(line)
                .map(|m| MatchHit {
                    start: m.start(),
                    text: m.as_str(),
                })
                .collect(),
            Self::Literal {
                needle,
                folded_needle,
                case_sensitive,
            } => {
                if *case_sensitive {
                    literal_matches(line, needle)
                } else {
                    let folded = line.to_lowercase();
                    literal_matches(&folded, folded_needle)
                        .into_iter()
                        .filter_map(|m| {
                            let end = m.start + folded_needle.len();
                            line.get(m.start..end).map(|text| MatchHit {
                                start: m.start,
                                text,
                            })
                        })
                        .collect()
                }
            }
        }
    }
}

fn literal_matches<'a>(haystack: &'a str, needle: &str) -> Vec<MatchHit<'a>> {
    let mut hits = Vec::new();
    let mut offset = 0usize;
    while let Some(pos) = haystack[offset..].find(needle) {
        let start = offset + pos;
        let end = start + needle.len();
        if let Some(text) = haystack.get(start..end) {
            hits.push(MatchHit { start, text });
        }
        offset = end.max(start + 1);
        if offset >= haystack.len() {
            break;
        }
    }
    hits
}

fn compile_glob(glob: Option<&str>) -> anyhow::Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let pattern = glob.filter(|s| !s.trim().is_empty()).unwrap_or("**");
    builder.add(Glob::new(pattern)?);
    Ok(builder.build()?)
}

fn glob_matches(glob: &GlobSet, workdir: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(workdir).unwrap_or(path);
    glob.is_match(rel) || glob.is_match(path)
}

fn context_window_before(lines: &[&str], idx: usize, context: usize) -> Vec<String> {
    let start = idx.saturating_sub(context);
    lines[start..idx].iter().map(|s| (*s).to_string()).collect()
}

fn context_window_after(lines: &[&str], idx: usize, context: usize) -> Vec<String> {
    let end = (idx + 1 + context).min(lines.len());
    lines[idx + 1..end]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
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
        let root = std::env::temp_dir().join(format!("botobot-search-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn search_returns_structured_matches_with_context() {
        let root = temp_workspace();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "alpha\nfn target() {}\nlet target_value = 1;\nomega\n",
        )
        .unwrap();

        let out = search_workspace(
            &root,
            SearchArgs {
                query: "target".into(),
                path: Some("src".into()),
                glob: Some("**/*.rs".into()),
                regex: None,
                case_sensitive: None,
                context_lines: Some(1),
                limit: Some(10),
            },
        )
        .unwrap();

        assert_eq!(out.matches.len(), 2);
        assert_eq!(out.matches[0].path, "src/lib.rs");
        assert_eq!(out.matches[0].line, 2);
        assert_eq!(out.matches[0].column, 4);
        assert_eq!(out.matches[0].context_before, vec!["alpha"]);
        assert_eq!(out.matches[0].context_after, vec!["let target_value = 1;"]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn find_filters_by_glob_and_name() {
        let root = temp_workspace();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/search.rs"), "").unwrap();
        std::fs::write(root.join("src/other.txt"), "").unwrap();

        let out = find_workspace(
            &root,
            FindArgs {
                path: Some(".".into()),
                glob: Some("**/*.rs".into()),
                name: Some("search".into()),
                include_dirs: None,
                limit: Some(10),
            },
        )
        .unwrap();

        assert_eq!(out.paths.len(), 1);
        assert_eq!(out.paths[0].path, "src/search.rs");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_rejects_paths_outside_workdir() {
        let root = temp_workspace();
        let err = search_workspace(
            &root,
            SearchArgs {
                query: "x".into(),
                path: Some("..".into()),
                glob: None,
                regex: None,
                case_sensitive: None,
                context_lines: None,
                limit: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("escapes workdir"));

        let _ = std::fs::remove_dir_all(root);
    }
}
