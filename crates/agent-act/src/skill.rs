//! Lightweight skill SOPs plus versioned skill evolution.
//!
//! Skills are markdown instructions with a small Claude-compatible frontmatter
//! subset. They are listed in the system prompt and can be read through
//! `skill://<name>`. The `skill_patch` tool evolves skills by bounded patches
//! and stores append-only version snapshots under `skills/.managed/`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::resource::{Resource, ResourceDoc};
use base_types::{Tool, ToolResult, ToolTier};

#[derive(Debug, Default, Clone)]
pub struct SkillFrontmatter {
    pub description: Option<String>,
    pub always_apply: bool,
    pub hide: bool,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub content: String,
    pub fm: SkillFrontmatter,
}

fn split_frontmatter(raw: &str) -> (&str, &str) {
    let t = raw.trim_start_matches(['\u{feff}']).trim_start();
    if let Some(rest) = t.strip_prefix("---") {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let body = rest[end + 4..].trim_start_matches(['\n', '\r']);
            return (fm, body);
        }
    }
    ("", raw)
}

pub fn parse_skill(default_name: &str, raw: &str) -> Skill {
    let (fm_text, body) = split_frontmatter(raw);
    let mut name = default_name.to_string();
    let mut fm = SkillFrontmatter::default();
    for line in fm_text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        match k.trim() {
            "name" if !v.is_empty() => name = v.to_string(),
            "description" => fm.description = Some(v.to_string()),
            "alwaysApply" => fm.always_apply = v == "true",
            "hide" => fm.hide = v == "true",
            _ => {}
        }
    }
    Skill {
        name,
        content: body.to_string(),
        fm,
    }
}

pub fn load_skills(dir: impl AsRef<Path>) -> Vec<Skill> {
    SkillStore::new(dir).load_all()
}

fn render_skill(skill: &Skill) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", skill.name));
    if let Some(description) = &skill.fm.description {
        out.push_str(&format!("description: {}\n", description));
    }
    if skill.fm.always_apply {
        out.push_str("alwaysApply: true\n");
    }
    if skill.fm.hide {
        out.push_str("hide: true\n");
    }
    out.push_str("---\n");
    out.push_str(skill.content.trim());
    out.push('\n');
    out
}

/// 常驻技能正文（§1.8.2 第3步·方案B）：指令 + 可见 skill 列表 + always_apply 全文，
/// **不含** `## Available skills` markdown 头（统一格式由 `ContextAssembler` 的可信度头承担）。
/// `skills_prompt` 在此基础上加回 markdown 头以保持旧输出逐字不变（向后兼容）。
pub fn skills_resident_body(skills: &[Skill]) -> Option<String> {
    let listed: Vec<&Skill> = skills.iter().filter(|s| !s.fm.hide).collect();
    let always: Vec<&Skill> = skills.iter().filter(|s| s.fm.always_apply).collect();
    if listed.is_empty() && always.is_empty() {
        return None;
    }
    let mut s = String::new();
    if !listed.is_empty() {
        s.push_str(
            "Read a skill's full instructions with the `read` tool using `skill://<name>`. Improve a skill with `skill_patch` using add/delete/replace patches.\n",
        );
        for sk in &listed {
            let desc = sk.fm.description.as_deref().unwrap_or("");
            s.push_str(&format!("- `skill://{}` - {desc}\n", sk.name));
        }
    }
    for sk in &always {
        s.push_str(&format!("\n### Skill: {}\n{}\n", sk.name, sk.content));
    }
    Some(s)
}

pub fn skills_prompt(skills: &[Skill]) -> Option<String> {
    let body = skills_resident_body(skills)?;
    // 旧输出：listed 非空时前缀 "## Available skills\n"；只有 always 时无头。
    if skills.iter().any(|s| !s.fm.hide) {
        Some(format!("## Available skills\n{body}"))
    } else {
        Some(body)
    }
}

#[derive(Debug, Clone)]
pub struct SkillStore {
    root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl SkillStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn load_all(&self) -> Vec<Skill> {
        let mut by_name = BTreeMap::<String, Skill>::new();
        for skill in self.load_top_level() {
            by_name.insert(skill.name.clone(), skill);
        }
        for skill in self.load_managed() {
            by_name.insert(skill.name.clone(), skill);
        }
        by_name.into_values().collect()
    }

    pub fn load_current(&self, name: &str) -> anyhow::Result<Option<Skill>> {
        // 防路径穿越：top-level 分支用原始 name 拼路径，`skill://../../x` 会越出 skills 目录。
        // 合法 skill 名不含分隔符/`..`（本机模型另有 file://，仍保一致防意外）。
        if !is_safe_skill_name(name) {
            return Ok(None);
        }
        if let Some(path) = self.current_managed_path(name)? {
            let raw = std::fs::read_to_string(path)?;
            return Ok(Some(parse_skill(name, &raw)));
        }
        // 目录式富 skill（§1.7 决策4）：skills/<name>/SKILL.md 优先于单文件。
        let dir_md = self.root.join(name).join("SKILL.md");
        if dir_md.exists() {
            let raw = std::fs::read_to_string(dir_md)?;
            return Ok(Some(parse_skill(name, &raw)));
        }
        let path = self.root.join(format!("{name}.md"));
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            return Ok(Some(parse_skill(name, &raw)));
        }
        Ok(None)
    }

    pub fn apply_patch(&self, args: SkillPatchArgs) -> anyhow::Result<SkillPatchOut> {
        let _guard = self.write_lock.lock().unwrap();
        let before = self
            .load_current(&args.name)?
            .unwrap_or_else(|| empty_skill(&args.name, args.description.clone()));
        let after = patch_skill(before.clone(), &args)?;
        let version = self.next_version(&args.name)?;
        let raw = render_skill(&after);
        let dir = self.managed_dir(&args.name);
        std::fs::create_dir_all(&dir)?;
        let file_name = format!("skill_v{version:04}.md");
        let path = dir.join(&file_name);
        std::fs::write(&path, raw)?;
        std::fs::write(dir.join("current.txt"), &file_name)?;
        Ok(SkillPatchOut {
            name: args.name,
            version,
            path: path.display().to_string(),
            url: format!("skill://{}", after.name),
        })
    }

    fn load_top_level(&self) -> Vec<Skill> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            // 目录式富 skill（§1.7 决策4）：skills/<name>/SKILL.md（+ 未来 scripts/、子 prompt）。
            // 跳过 `.managed`（演进版本由 load_managed 处理）。
            if path.is_dir() {
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if name == ".managed" {
                    continue;
                }
                if let Ok(raw) = std::fs::read_to_string(path.join("SKILL.md")) {
                    out.push(parse_skill(name, &raw));
                }
                continue;
            }
            // 单文件旧形态（视为只有 SKILL.md 的退化目录）。
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(raw) = std::fs::read_to_string(&path) {
                out.push(parse_skill(stem, &raw));
            }
        }
        out
    }

    fn load_managed(&self) -> Vec<Skill> {
        let managed = self.root.join(".managed");
        let Ok(entries) = std::fs::read_dir(managed) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(ToString::to_string) else {
                continue;
            };
            if let Ok(Some(skill)) = self.load_current(&name) {
                out.push(skill);
            }
        }
        out
    }

    fn current_managed_path(&self, name: &str) -> anyhow::Result<Option<PathBuf>> {
        let dir = self.managed_dir(name);
        let current = dir.join("current.txt");
        if !current.exists() {
            return Ok(None);
        }
        let file = std::fs::read_to_string(current)?;
        Ok(Some(dir.join(file.trim())))
    }

    /// 当前生效版本号（仅 managed/overlay 演进 skill 有；从 current.txt 的 `skill_vNNNN.md` 解析）。
    /// 出厂/单文件/目录式未经 skill_patch 的 skill 返回 `None`（§1.5：无版本可省略）。
    pub fn current_version(&self, name: &str) -> Option<u32> {
        let current = self.managed_dir(name).join("current.txt");
        let file = std::fs::read_to_string(current).ok()?;
        file.trim()
            .strip_prefix("skill_v")
            .and_then(|s| s.strip_suffix(".md"))
            .and_then(|s| s.parse::<u32>().ok())
    }

    fn next_version(&self, name: &str) -> anyhow::Result<u32> {
        let dir = self.managed_dir(name);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(1);
        };
        let max = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
            .filter_map(|file| {
                file.strip_prefix("skill_v")
                    .and_then(|s| s.strip_suffix(".md"))
                    .and_then(|s| s.parse::<u32>().ok())
            })
            .max()
            .unwrap_or(0);
        Ok(max + 1)
    }

    fn managed_dir(&self, name: &str) -> PathBuf {
        self.root.join(".managed").join(safe_name(name))
    }
}

/// §1.6 一条已装 skill 的清单项（**从现状派生**，不引 manifest.toml）。
/// id/description 取自 frontmatter；version 取自 `.managed` 演进（无则省略）；
/// source 区分出厂内嵌 vs 磁盘 overlay。kind v1 恒为 "skill"。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillDescriptor {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub source: SkillSource,
    pub kind: &'static str,
    pub hidden: bool,
}

/// skill 的来源面：`Overlay`=磁盘（用户编辑 / skill_patch 演进 / §1.6 市场包）；
/// `Embedded`=出厂内嵌（§1.7 include_dir!，未落地前不会出现）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Embedded,
    Overlay,
}

impl SkillStore {
    /// §1.6 S2：安装/更新一个 skill 到磁盘 overlay（`skills/<id>/SKILL.md`）。
    /// 目录式富 skill 的最小落点——v1 只写 SKILL.md 正文（含 frontmatter）；
    /// 已存在即覆盖（= 更新）。返回写入路径。市场包后续可在此目录补 scripts/ 子文件。
    pub fn install_overlay(&self, id: &str, skill_md: &str) -> anyhow::Result<PathBuf> {
        let _guard = self.write_lock.lock().unwrap();
        let safe = safe_name(id);
        if safe != id {
            return Err(anyhow::anyhow!(
                "invalid skill id `{id}` (use lowercase letters/digits/-/_)"
            ));
        }
        let dir = self.root.join(&safe);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        std::fs::write(&path, skill_md)?;
        Ok(path)
    }

    /// §1.6 S3：取一个 skill 的**原始包正文**（用于市场下载 / 客户端 install_overlay）。
    /// 取真实落盘的 SKILL.md（目录式）或单文件 / `.managed` 当前版本——保留原始字节，
    /// 不经 parse→render 往返（避免 frontmatter 归一化漂移）。无则 `None`。
    pub fn package_md(&self, id: &str) -> anyhow::Result<Option<String>> {
        let safe = safe_name(id);
        // 演进过的优先取 .managed 当前版本（与 read(skill://) 一致）。
        if let Some(path) = self.current_managed_path(&safe)? {
            return Ok(Some(std::fs::read_to_string(path)?));
        }
        let dir_md = self.root.join(&safe).join("SKILL.md");
        if dir_md.exists() {
            return Ok(Some(std::fs::read_to_string(dir_md)?));
        }
        let single = self.root.join(format!("{safe}.md"));
        if single.exists() {
            return Ok(Some(std::fs::read_to_string(single)?));
        }
        Ok(None)
    }

    /// §1.6 S2：删除磁盘 overlay（`skills/<id>/` 整个目录）。返回是否真的删了。
    /// 「删 overlay = 回退出厂」语义：内嵌基线（§1.7）落地后，删 overlay 即影子消失、
    /// 回退到内嵌 default；当前无内嵌则等于彻底移除。`.managed` 演进快照一并清除。
    pub fn remove_overlay(&self, id: &str) -> anyhow::Result<bool> {
        let _guard = self.write_lock.lock().unwrap();
        let safe = safe_name(id);
        let mut removed = false;
        // 目录式 overlay。
        let dir = self.root.join(&safe);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
            removed = true;
        }
        // 单文件旧形态。
        let single = self.root.join(format!("{safe}.md"));
        if single.is_file() {
            std::fs::remove_file(&single)?;
            removed = true;
        }
        // skill_patch 演进快照。
        let managed = self.managed_dir(&safe);
        if managed.is_dir() {
            std::fs::remove_dir_all(&managed)?;
            removed = true;
        }
        Ok(removed)
    }

    /// §1.6 S1：列出本地已装 skill（embedded + overlay 合并，标 source/version/kind）。
    /// 当前内嵌基线（§1.7）未落地，故磁盘上的一律 `Overlay`。
    pub fn list_installed(&self) -> Vec<SkillDescriptor> {
        self.load_all()
            .into_iter()
            .map(|s| {
                let version = self.current_version(&s.name);
                SkillDescriptor {
                    id: s.name.clone(),
                    description: s.fm.description.clone(),
                    version,
                    // 内嵌基线尚未实现 → 磁盘上的都算 overlay。
                    source: SkillSource::Overlay,
                    kind: "skill",
                    hidden: s.fm.hide,
                }
            })
            .collect()
    }
}

fn empty_skill(name: &str, description: Option<String>) -> Skill {
    Skill {
        name: name.to_string(),
        content: String::new(),
        fm: SkillFrontmatter {
            description,
            ..SkillFrontmatter::default()
        },
    }
}

/// skill 名是否安全用于直接拼路径（top-level `skills/<name>` 查找）：非空、无分隔符/`..`/NUL。
/// 合法名如 `officecli-pptx`/`law`/`MySkill` 通过；`../x`、`a/b` 拒。
fn is_safe_skill_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains('\0')
}

fn safe_name(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "skill".into() } else { out }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SkillPatchArgs {
    pub name: String,
    pub op: SkillPatchOp,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub old: Option<String>,
    #[serde(default)]
    pub new: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillPatchOp {
    Add,
    Delete,
    Replace,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillPatchOut {
    pub name: String,
    pub version: u32,
    pub path: String,
    pub url: String,
}

fn patch_skill(mut before: Skill, args: &SkillPatchArgs) -> anyhow::Result<Skill> {
    if let Some(description) = &args.description {
        before.fm.description = Some(description.clone());
    }
    match args.op {
        SkillPatchOp::Add => {
            let text = required_arg(args.text.as_deref(), "text")?;
            if !before.content.ends_with('\n') && !before.content.is_empty() {
                before.content.push('\n');
            }
            before.content.push_str(text);
            before.content.push('\n');
        }
        SkillPatchOp::Delete => {
            let old = required_arg(args.old.as_deref(), "old")?;
            before.content = replace_once(&before.content, old, "")?;
        }
        SkillPatchOp::Replace => {
            let old = required_arg(args.old.as_deref(), "old")?;
            let new = required_arg(args.new.as_deref(), "new")?;
            before.content = replace_once(&before.content, old, new)?;
        }
    }
    Ok(before)
}

fn required_arg<'a>(value: Option<&'a str>, name: &str) -> anyhow::Result<&'a str> {
    match value {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(anyhow::anyhow!("skill_patch: missing non-empty `{name}`")),
    }
}

fn replace_once(content: &str, old: &str, new: &str) -> anyhow::Result<String> {
    let Some(pos) = content.find(old) else {
        return Err(anyhow::anyhow!("skill_patch: target text not found"));
    };
    let mut out = String::new();
    out.push_str(&content[..pos]);
    out.push_str(new);
    out.push_str(&content[pos + old.len()..]);
    Ok(out)
}

pub trait SkillAcceptGate: Send + Sync {
    fn accept(&self, before: &Skill, after: &Skill) -> anyhow::Result<()>;
}

pub struct AcceptAllGate;

impl SkillAcceptGate for AcceptAllGate {
    fn accept(&self, _before: &Skill, _after: &Skill) -> anyhow::Result<()> {
        Ok(())
    }
}

/// `skill_list` 工具（读）：**枚举**本地已装 skill（id + description + version）。补盲区——
/// 常驻 skill 列表受 RESIDENT_BUDGET 截断，skill 多时（用户已 15+）prompt 里看不全；此工具
/// 让 agent 按需列全部，再 `read(skill://<id>)` 取正文。隐藏 skill 不列。
pub struct SkillListTool {
    store: Arc<SkillStore>,
}

impl SkillListTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }
    fn description(&self) -> &str {
        "List all installed skills (id + description + version). Use to discover skills not shown in \
         the always-on list (it is budget-truncated when there are many), then read(skill://<id>)."
    }
    fn tier(&self) -> ToolTier {
        ToolTier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> ToolResult {
        let skills: Vec<SkillDescriptor> = self
            .store
            .list_installed()
            .into_iter()
            .filter(|s| !s.hidden)
            .collect();
        Ok(json!({ "skills": skills }))
    }
}

pub struct SkillPatchTool {
    store: Arc<SkillStore>,
    gate: Arc<dyn SkillAcceptGate>,
}

impl SkillPatchTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            store,
            gate: Arc::new(AcceptAllGate),
        }
    }

    pub fn with_gate(store: Arc<SkillStore>, gate: Arc<dyn SkillAcceptGate>) -> Self {
        Self { store, gate }
    }
}

#[async_trait]
impl Tool for SkillPatchTool {
    fn name(&self) -> &str {
        "skill_patch"
    }

    fn description(&self) -> &str {
        "Evolve a managed skill using a bounded add/delete/replace patch. Writes append-only skill_vXXXX.md snapshots under skills/.managed and updates current.txt."
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Write
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name, without skill://." },
                "op": { "type": "string", "enum": ["add", "delete", "replace"] },
                "text": { "type": "string", "description": "Text to append when op=add." },
                "old": { "type": "string", "description": "Exact text to delete or replace." },
                "new": { "type": "string", "description": "Replacement text when op=replace." },
                "description": { "type": "string", "description": "Optional skill description/frontmatter." }
            },
            "required": ["name", "op"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: SkillPatchArgs = serde_json::from_value(args)?;
        let before = self
            .store
            .load_current(&parsed.name)?
            .unwrap_or_else(|| empty_skill(&parsed.name, parsed.description.clone()));
        let after = patch_skill(before.clone(), &parsed)?;
        self.gate.accept(&before, &after)?;
        let out = self.store.apply_patch(parsed)?;
        Ok(json!(out))
    }
}

/// §1.8.3b skill 语义索引一条 = 一个 skill 的「名+描述」概要嵌入 + citation(`skill://<name>`)。
struct SkillCard {
    name: String,
    summary: String,
    vec: Vec<f32>,
}

pub struct SkillResource {
    bodies: HashMap<String, String>,
    store: Option<Arc<SkillStore>>,
    /// §1.8.2：常驻把手正文（指令 + 列表 + always_apply 全文）。空=无可常驻 skill。
    resident: Option<String>,
    /// §1.8.3b 概要语料 `(name, "name — description")`——供 `set_embedder` 后台建索引（隐藏 skill 不入）。
    summaries: Vec<(String, String)>,
    /// §1.8.3b 语义索引（注入 embedder 后构建；与名表并存，只做粗筛）。
    embedder: Mutex<Option<Arc<dyn base_types::Embedder>>>,
    index: Mutex<Vec<SkillCard>>,
}

impl SkillResource {
    pub fn new(skills: &[Skill]) -> Self {
        Self::with_optional_store(skills, None)
    }

    pub fn with_store(skills: &[Skill], store: Arc<SkillStore>) -> Self {
        Self::with_optional_store(skills, Some(store))
    }

    fn with_optional_store(skills: &[Skill], store: Option<Arc<SkillStore>>) -> Self {
        Self {
            bodies: skills
                .iter()
                .map(|s| (s.name.clone(), s.content.clone()))
                .collect(),
            store,
            // §1.8.2 第3步·方案B：常驻正文 = 指令 + 列表 + always_apply 全文（忠实迁移
            // skills_prompt 内容，仅去掉 markdown 头，交给 assembler 的可信度头）。
            resident: skills_resident_body(skills),
            // §1.8.3b 概要语料：name + description（无描述退化为仅 name）；隐藏 skill 不参与提示。
            summaries: skills
                .iter()
                .filter(|s| !s.fm.hide)
                .map(|s| {
                    let summary = match &s.fm.description {
                        Some(d) if !d.trim().is_empty() => format!("{} — {}", s.name, d.trim()),
                        _ => s.name.clone(),
                    };
                    (s.name.clone(), summary)
                })
                .collect(),
            embedder: Mutex::new(None),
            index: Mutex::new(Vec::new()),
        }
    }

    /// §1.8.3b 注入嵌入器并构建概要索引（嵌入每个 skill 的「名+描述」一条向量）。
    /// 装配时后台 bge 加载完调用（与 `BookResource::set_embedder` 同模式）。
    pub fn set_embedder(&self, embedder: Arc<dyn base_types::Embedder>) {
        if !self.summaries.is_empty() {
            let texts: Vec<&str> = self.summaries.iter().map(|(_, s)| s.as_str()).collect();
            if let Ok(vecs) = embedder.embed(&texts) {
                let index: Vec<SkillCard> = self
                    .summaries
                    .iter()
                    .zip(vecs)
                    .map(|((name, summary), vec)| SkillCard {
                        name: name.clone(),
                        summary: summary.clone(),
                        vec,
                    })
                    .collect();
                *self.index.lock().unwrap() = index;
            }
        }
        *self.embedder.lock().unwrap() = Some(embedder);
    }

    /// §1.8.3b 语义粗筛：返回 top-K `(score, name, summary)`（余弦降序，≥0.3）。
    /// 无 embedder/空索引 → 空。
    pub fn semantic_search(&self, query: &str, top_k: usize) -> Vec<(f32, String, String)> {
        let qvec = {
            let emb = self.embedder.lock().unwrap();
            let Some(e) = emb.as_ref() else {
                return Vec::new();
            };
            match e.embed(&[query]) {
                Ok(mut v) => match v.drain(..).next() {
                    Some(q) => q,
                    None => return Vec::new(),
                },
                Err(_) => return Vec::new(),
            }
        };
        let index = self.index.lock().unwrap();
        let mut scored: Vec<(f32, &SkillCard)> = index
            .iter()
            .map(|c| (skill_cosine(&qvec, &c.vec), c))
            .filter(|(s, _)| *s >= 0.3)
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(top_k)
            .map(|(s, c)| (s, c.name.clone(), c.summary.clone()))
            .collect()
    }
}

/// §1.8.3b 能力提示：按 query 语义粗筛相关 skill，回 `skill://<name>` citation 供下钻。
#[async_trait]
impl crate::recall::CapabilityHint for SkillResource {
    async fn hint(&self, query: &str, max: usize) -> Vec<crate::recall::CapHint> {
        self.semantic_search(query, max)
            .into_iter()
            .map(|(_, name, summary)| crate::recall::CapHint {
                kind: "skill",
                label: summary,
                citation: format!("skill://{name}"),
            })
            .collect()
    }
}

/// 余弦相似度（L2 归一化输入即点积）。
fn skill_cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[async_trait]
impl crate::context::ContextSource for SkillResource {
    fn scheme(&self) -> &str {
        "skill"
    }

    fn facets(&self) -> crate::context::ContextFacets {
        use crate::context::*;
        // Skill 中高可信、不可变基线、精确键查、经 gate 写回。
        ContextFacets {
            trust: Trust::Skill,
            volatility: Volatility::Immutable,
            retrieval: Retrieval::ExactKey,
            writeback: Writeback::Gated,
        }
    }

    async fn handle(&self, budget: usize) -> Option<crate::context::Handle> {
        use crate::context::*;
        let resident = self.resident.as_ref()?;
        // §1.8.7「能去翻的」钩子小节：技能名+描述（指针）+ always_apply 全文常驻。
        let framed = format!("## Skills you can apply\n{resident}");
        let digest = fit_budget(&framed, budget);
        Some(Handle {
            est_tokens: est_tokens(&digest),
            digest,
            trust: Trust::Skill,
        })
    }

    async fn expand(&self, query: &str) -> anyhow::Result<ResourceDoc> {
        Resource::resolve(self, query).await
    }
}

#[async_trait]
impl Resource for SkillResource {
    fn scheme(&self) -> &str {
        "skill"
    }

    fn immutable(&self) -> bool {
        true
    }

    async fn resolve(&self, name: &str) -> anyhow::Result<ResourceDoc> {
        // §1.5 read 输出纪律：skill 中高可信，带 version + source（overlay/embedded）。
        if let Some(store) = &self.store {
            if let Some(skill) = store.load_current(name)? {
                // version 仅 skill_patch 演进过的 managed skill 有（current.txt）；否则省略。
                let ver = store
                    .current_version(name)
                    .map(|v| format!(" version={v}"))
                    .unwrap_or_default();
                return Ok(ResourceDoc {
                    url: format!("skill://{name}"),
                    content: format!(
                        "[SKILL procedural/gated] source=overlay{ver}\n{}",
                        skill.content
                    ),
                    content_type: "text/markdown",
                    immutable: true,
                });
            }
        }
        match self.bodies.get(name) {
            Some(body) => Ok(ResourceDoc {
                url: format!("skill://{name}"),
                content: format!("[SKILL procedural/gated] source=embedded\n{body}"),
                content_type: "text/markdown",
                immutable: true,
            }),
            None => Err(anyhow::anyhow!("unknown skill: {name}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\nname: greet\ndescription: say hello\nalwaysApply: false\n---\nDo the greeting SOP.\n";
        let sk = parse_skill("file-stem", raw);
        assert_eq!(sk.name, "greet");
        assert_eq!(sk.fm.description.as_deref(), Some("say hello"));
        assert!(!sk.fm.always_apply);
        assert_eq!(sk.content.trim(), "Do the greeting SOP.");
    }

    #[test]
    fn no_frontmatter_uses_filename() {
        let sk = parse_skill("foo", "just a body, no fm");
        assert_eq!(sk.name, "foo");
        assert!(sk.content.contains("just a body"));
    }

    #[tokio::test]
    async fn skill_context_source_handle_lists_visible_only() {
        use crate::context::{ContextSource, Retrieval, Trust, Writeback};
        let skills = vec![
            parse_skill("a", "---\ndescription: alpha\n---\nA body"),
            parse_skill("sec", "---\nhide: true\n---\nhidden body"),
        ];
        let res = SkillResource::new(&skills);
        // facets：Skill 可信、gate 写回、精确键查。
        let f = ContextSource::facets(&res);
        assert_eq!(f.trust, Trust::Skill);
        assert_eq!(f.writeback, Writeback::Gated);
        assert_eq!(f.retrieval, Retrieval::ExactKey);
        // handle：常驻列表含可见 skill 元信息，隐藏的不列。
        let h = ContextSource::handle(&res, 1000)
            .await
            .expect("应有常驻把手");
        assert!(h.digest.contains("skill://a"));
        assert!(h.digest.contains("alpha"));
        assert!(!h.digest.contains("skill://sec"));
        assert!(h.est_tokens > 0);
        // expand 与 Resource::resolve 等价。
        let doc = ContextSource::expand(&res, "a").await.unwrap();
        assert!(doc.content.starts_with("[SKILL"));
    }

    // §1.8.3b skill 能力提示：注入 embedder 后按 query 语义粗筛，回 skill://<name> citation；
    // 隐藏 skill 不入索引；无 embedder → 空。
    #[tokio::test]
    async fn skill_hint_semantic_search_matches_and_skips_hidden() {
        use crate::recall::CapabilityHint;

        // 复用 memory 测试同款主题 stub embedder（cat/car/food 三正交主题）。
        struct StubEmb;
        impl base_types::Embedder for StubEmb {
            fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
                Ok(texts
                    .iter()
                    .map(|t| {
                        let t = t.to_lowercase();
                        let mut v: [f32; 3] = [
                            t.contains("cat") as i32 as f32,
                            t.contains("car") as i32 as f32,
                            t.contains("food") as i32 as f32,
                        ];
                        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                        if n > 0.0 {
                            for x in &mut v {
                                *x /= n;
                            }
                        } else {
                            v = [0.577, 0.577, 0.577];
                        }
                        v.to_vec()
                    })
                    .collect())
            }
            fn dim(&self) -> usize {
                3
            }
        }

        let skills = vec![
            parse_skill("feline", "---\ndescription: all about cat care\n---\nbody"),
            parse_skill("driving", "---\ndescription: car and drive tips\n---\nbody"),
            parse_skill("sec", "---\nhide: true\ndescription: cat secret\n---\nbody"),
        ];
        let res = SkillResource::new(&skills);
        // 无 embedder → 空。
        assert!(res.hint("cat", 2).await.is_empty());

        res.set_embedder(Arc::new(StubEmb));
        let hits = res.hint("cat", 2).await;
        assert!(!hits.is_empty(), "应命中 cat 主题 skill");
        assert_eq!(hits[0].kind, "skill");
        assert_eq!(hits[0].citation, "skill://feline");
        // 隐藏 skill 不入索引：不应出现 sec。
        assert!(
            !hits.iter().any(|h| h.citation.contains("sec")),
            "隐藏 skill 不提示"
        );
    }

    #[tokio::test]
    async fn skill_context_source_no_visible_means_no_handle() {
        use crate::context::ContextSource;
        let skills = vec![parse_skill("sec", "---\nhide: true\n---\nhidden")];
        let res = SkillResource::new(&skills);
        assert!(ContextSource::handle(&res, 1000).await.is_none());
    }

    #[tokio::test]
    async fn skill_read_has_provenance_header() {
        let skills = vec![parse_skill(
            "greet",
            "---\ndescription: hi\n---\nDo greeting",
        )];
        let res = SkillResource::new(&skills);
        let doc = res.resolve("greet").await.unwrap();
        assert!(
            doc.content.starts_with("[SKILL"),
            "skill read 应带来源头，got: {}",
            doc.content
        );
        assert!(doc.content.contains("source="));
        assert!(doc.content.contains("Do greeting"));
    }

    #[tokio::test]
    async fn skill_resource_resolves_and_lists() {
        let skills = vec![
            parse_skill("a", "---\ndescription: alpha\n---\nA body"),
            parse_skill("sec", "---\nhide: true\n---\nhidden body"),
        ];
        let res = SkillResource::new(&skills);
        let doc = res.resolve("a").await.unwrap();
        assert!(doc.content.starts_with("[SKILL"));
        assert!(doc.content.contains("A body"));
        assert!(res.resolve("missing").await.is_err());

        let prompt = skills_prompt(&skills).unwrap();
        assert!(prompt.contains("skill://a"));
        assert!(!prompt.contains("skill://sec"));
    }

    #[test]
    fn patch_requires_exact_target_text() {
        let skill = parse_skill("a", "alpha beta");
        let args = SkillPatchArgs {
            name: "a".into(),
            op: SkillPatchOp::Replace,
            old: Some("missing".into()),
            new: Some("gamma".into()),
            text: None,
            description: None,
        };
        assert!(patch_skill(skill, &args).is_err());
    }

    #[tokio::test]
    async fn skill_patch_writes_versions_and_resource_reads_current() {
        let root = temp_skill_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("greet.md"),
            "---\ndescription: greet people\n---\nSay hello.\n",
        )
        .unwrap();

        let store = Arc::new(SkillStore::new(&root));
        let out1 = store
            .apply_patch(SkillPatchArgs {
                name: "greet".into(),
                op: SkillPatchOp::Add,
                text: Some("Use the user's name.".into()),
                old: None,
                new: None,
                description: None,
            })
            .unwrap();
        assert_eq!(out1.version, 1);
        let out2 = store
            .apply_patch(SkillPatchArgs {
                name: "greet".into(),
                op: SkillPatchOp::Replace,
                old: Some("hello".into()),
                new: Some("hi".into()),
                text: None,
                description: None,
            })
            .unwrap();
        assert_eq!(out2.version, 2);

        let skills = store.load_all();
        let res = SkillResource::with_store(&skills, store.clone());
        let doc = res.resolve("greet").await.unwrap();
        assert!(doc.content.contains("Say hi."));
        assert!(doc.content.contains("Use the user's name."));
        // §1.5：skill_patch 演进过的 skill 读取头带版本号。
        assert!(
            doc.content.contains("version=2"),
            "managed skill 读取应带 version=2, got: {}",
            doc.content
        );

        let current = std::fs::read_to_string(root.join(".managed/greet/current.txt")).unwrap();
        assert_eq!(current.trim(), "skill_v0002.md");
        let _ = std::fs::remove_dir_all(root);
    }

    // skill_list 枚举已装 skill（id+description），隐藏的不列。
    #[tokio::test]
    async fn skill_list_enumerates_visible_skills() {
        let root = temp_skill_root();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("greet.md"),
            "---\ndescription: say hi\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("sec.md"),
            "---\nhide: true\ndescription: secret\n---\nbody\n",
        )
        .unwrap();
        let store = Arc::new(SkillStore::new(&root));
        let out = SkillListTool::new(store).call(json!({})).await.unwrap();
        let skills = out["skills"].as_array().unwrap();
        assert!(skills.iter().any(|s| s["id"] == "greet"), "应列 greet");
        assert!(!skills.iter().any(|s| s["id"] == "sec"), "隐藏 skill 不列");
        let _ = std::fs::remove_dir_all(root);
    }

    // 路径穿越：恶意 skill 名（含 ../、分隔符）的 load_current 返回 None，不越出 skills 目录读。
    #[test]
    fn load_current_rejects_traversal_names() {
        assert!(is_safe_skill_name("officecli-pptx"));
        assert!(is_safe_skill_name("law"));
        assert!(is_safe_skill_name("MySkill"));
        assert!(!is_safe_skill_name("../secret"));
        assert!(!is_safe_skill_name("a/b"));
        assert!(!is_safe_skill_name("a\\b"));
        assert!(!is_safe_skill_name("a..b"));
        assert!(!is_safe_skill_name(""));

        let root = temp_skill_root();
        std::fs::create_dir_all(&root).unwrap();
        let store = SkillStore::new(&root);
        assert!(
            store.load_current("../../etc/passwd").unwrap().is_none(),
            "穿越名应 None"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn loads_directory_style_skill_and_keeps_single_file_compat() {
        // §1.7 决策4：skills/<name>/SKILL.md 目录式 + 旧单文件并存。
        let root = temp_skill_root();
        std::fs::create_dir_all(root.join("deploy")).unwrap();
        std::fs::write(
            root.join("deploy/SKILL.md"),
            "---\nname: deploy\ndescription: ship safely\n---\nRun the deploy checklist.\n",
        )
        .unwrap();
        // 旧单文件仍应被加载。
        std::fs::write(
            root.join("greet.md"),
            "---\ndescription: say hi\n---\nGreet warmly.\n",
        )
        .unwrap();

        let store = Arc::new(SkillStore::new(&root));
        let skills = store.load_all();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"deploy"),
            "目录式 skill 应加载, got {names:?}"
        );
        assert!(names.contains(&"greet"), "单文件 skill 仍应加载");

        // skill:// 读目录式 skill 取 SKILL.md 正文。
        let res = SkillResource::with_store(&skills, store.clone());
        let doc = res.resolve("deploy").await.unwrap();
        assert!(doc.content.contains("deploy checklist"));
        // load_current 也走目录式。
        let cur = store.load_current("deploy").unwrap().unwrap();
        assert_eq!(cur.fm.description.as_deref(), Some("ship safely"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn list_installed_reports_source_and_version() {
        let root = temp_skill_root();
        std::fs::create_dir_all(&root).unwrap();
        // 普通磁盘 skill（无演进 → 无 version）。
        std::fs::write(
            root.join("greet.md"),
            "---\ndescription: say hi\n---\nGreet.\n",
        )
        .unwrap();
        // 隐藏 skill 仍列出，但标 hidden。
        std::fs::write(root.join("sec.md"), "---\nhide: true\n---\nhidden\n").unwrap();

        let store = SkillStore::new(&root);
        // 演进 greet → 产生 .managed version。
        store
            .apply_patch(SkillPatchArgs {
                name: "greet".into(),
                op: SkillPatchOp::Add,
                text: Some("Use name.".into()),
                old: None,
                new: None,
                description: None,
            })
            .unwrap();

        let list = store.list_installed();
        let greet = list.iter().find(|d| d.id == "greet").unwrap();
        assert_eq!(greet.version, Some(1), "演进过的 skill 应带 version");
        assert_eq!(greet.source, SkillSource::Overlay);
        assert_eq!(greet.kind, "skill");
        assert!(!greet.hidden);
        let sec = list.iter().find(|d| d.id == "sec").unwrap();
        assert!(sec.hidden, "隐藏 skill 仍列出但标 hidden");
        assert_eq!(sec.version, None);

        // 可序列化为 JSON（API 用），version 缺省时省略字段。
        let json = serde_json::to_string(sec).unwrap();
        assert!(
            !json.contains("version"),
            "无 version 应省略字段, got: {json}"
        );
        assert!(json.contains("\"source\":\"overlay\""));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_then_remove_overlay_roundtrip() {
        let root = temp_skill_root();
        let store = SkillStore::new(&root);
        // 安装：写 overlay 目录式 SKILL.md。
        let path = store
            .install_overlay("deploy", "---\ndescription: ship\n---\nChecklist.\n")
            .unwrap();
        assert!(path.ends_with("SKILL.md"));
        let list = store.list_installed();
        assert!(list.iter().any(|d| d.id == "deploy"));
        assert_eq!(
            store
                .load_current("deploy")
                .unwrap()
                .unwrap()
                .fm
                .description
                .as_deref(),
            Some("ship")
        );

        // 更新：覆盖正文。
        store
            .install_overlay("deploy", "---\ndescription: ship v2\n---\nNew checklist.\n")
            .unwrap();
        assert!(
            store
                .load_current("deploy")
                .unwrap()
                .unwrap()
                .content
                .contains("New checklist")
        );

        // 演进产生 .managed 快照，删除时应一并清。
        store
            .apply_patch(SkillPatchArgs {
                name: "deploy".into(),
                op: SkillPatchOp::Add,
                text: Some("more".into()),
                old: None,
                new: None,
                description: None,
            })
            .unwrap();
        assert!(root.join(".managed/deploy").is_dir());

        // 删除：overlay + managed 都清空。
        assert!(store.remove_overlay("deploy").unwrap());
        assert!(!root.join("deploy").exists());
        assert!(!root.join(".managed/deploy").exists());
        assert!(store.list_installed().iter().all(|d| d.id != "deploy"));
        // 再删不存在的返回 false。
        assert!(!store.remove_overlay("deploy").unwrap());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn package_md_returns_raw_and_roundtrips_through_install() {
        let src = temp_skill_root();
        let dst = temp_skill_root();
        let from = SkillStore::new(&src);
        let to = SkillStore::new(&dst);
        let raw = "---\ndescription: ship safely\n---\nDeploy checklist here.\n";
        from.install_overlay("deploy", raw).unwrap();

        // 取包 = 原始字节。
        let pkg = from.package_md("deploy").unwrap().unwrap();
        assert_eq!(pkg, raw, "package_md 应返回原始落盘字节");
        assert!(from.package_md("missing").unwrap().is_none());

        // 装到另一个 store（模拟市场拉取→本地安装）。
        to.install_overlay("deploy", &pkg).unwrap();
        assert_eq!(
            to.load_current("deploy")
                .unwrap()
                .unwrap()
                .fm
                .description
                .as_deref(),
            Some("ship safely")
        );

        let _ = std::fs::remove_dir_all(src);
        let _ = std::fs::remove_dir_all(dst);
    }

    #[test]
    fn install_rejects_unsafe_id() {
        let root = temp_skill_root();
        let store = SkillStore::new(&root);
        assert!(store.install_overlay("../escape", "x").is_err());
        assert!(store.install_overlay("Bad Name", "x").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_skill_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "botobot-skill-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
