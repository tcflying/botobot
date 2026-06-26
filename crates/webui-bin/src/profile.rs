//! Built-in bot profiles.
//!
//! Profiles are an assembly-layer concern: the agent harness stays generic,
//! while a profile decides prompt shape, default policy, workspace guidance,
//! and which tool preset is exposed.

use std::path::Path;
use std::sync::Arc;

use agent_act::resource::ResourceRouter;
use agent_act::{ToolRegistry, memory, skill, tools};
use agent_loop::Policy;
use serde::Deserialize;

/// §5.7 通用助手 profile 的角色 system prompt（市场模板 `general`）。
/// **与 coder 同工具集**，只是身份/纪律不同：通用助手不注入编程专属 SOP（brainstorm/plan/TDD/
/// 根因调试/验证那套），定位日常问答·调研·文档·杂活。记忆纪律保留（跨任务有用）。
pub fn general_system_prompt() -> String {
    String::from(
        "You are botobot, a general-purpose assistant backed by a capable agent harness.\n\
         Help with whatever the user needs — questions, research, writing, planning, analysis, and light hands-on tasks. \
         You have the full tool set (read/search/edit/shell/memory/skills/books/web) available; use a tool only when it directly helps, and explain briefly when an action is non-trivial or risky.\n\
         Prefer grounding answers in real sources (files, the web, books) over guessing; say when you are unsure.\n\
         When a task needs a lot of reading or cross-source understanding, dispatch the read-only `explore` sub-agent so your main context stays lean.\n\
         Be concise and direct.\n\
         \n\
         Memory: at the start of a task that could depend on remembered context — the user's stated preferences, earlier decisions, or standing facts — search memory first with `read(memory://<topic>)` before assuming or asking again. Recalls are low-confidence: verify before relying, and `retain` durable new facts. When the user states an identity or standing preference about themselves, `retain` it with `pin=true` so it stays always-visible.",
    )
}

/// First built-in role: a coding-capable bot backed by the generic harness.
#[derive(Debug, Clone)]
pub struct CoderBotProfile {
    id: String,
    display_name: String,
    custom_system: Option<String>,
    replace_system: bool,
}

#[derive(Debug, Deserialize)]
struct ProfileToml {
    id: Option<String>,
    display_name: Option<String>,
    system: Option<String>,
    replace_system: Option<bool>,
}

impl CoderBotProfile {
    pub const ID: &'static str = "coder";
    pub const DISPLAY_NAME: &'static str = "Coder Bot";

    pub fn builtin() -> Self {
        Self {
            id: Self::ID.to_string(),
            display_name: Self::DISPLAY_NAME.to_string(),
            custom_system: None,
            replace_system: false,
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("BOTOBOT_PROFILE") {
            Ok(path) if !path.trim().is_empty() => Self::from_file(path),
            _ => Ok(Self::builtin()),
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)?;
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("toml") => Self::from_toml(&raw),
            Some("md") | Some("markdown") => Ok(Self::from_markdown(&raw)),
            other => anyhow::bail!(
                "unsupported bot profile extension {:?}; expected .toml or .md",
                other
            ),
        }
    }

    fn from_toml(raw: &str) -> anyhow::Result<Self> {
        let file: ProfileToml = toml::from_str(raw)?;
        let mut profile = Self::builtin();
        if let Some(id) = file.id.filter(|v| !v.trim().is_empty()) {
            profile.id = id;
        }
        if let Some(display_name) = file.display_name.filter(|v| !v.trim().is_empty()) {
            profile.display_name = display_name;
        }
        profile.custom_system = file.system.filter(|v| !v.trim().is_empty());
        profile.replace_system = file.replace_system.unwrap_or(false);
        Ok(profile)
    }

    fn from_markdown(raw: &str) -> Self {
        let mut profile = Self::builtin();
        if let Some(title) = raw
            .lines()
            .find_map(|line| line.strip_prefix("# ").map(str::trim))
            .filter(|title| !title.is_empty())
        {
            profile.display_name = title.to_string();
            profile.id = title
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_string();
            if profile.id.is_empty() {
                profile.id = Self::ID.to_string();
            }
        }
        profile.custom_system = Some(raw.trim().to_string()).filter(|v| !v.is_empty());
        profile
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn policy(&self) -> Arc<dyn Policy> {
        // §4：声明式前缀规则表（Allow/Forbidden/Prompt）替代「Exec 一律人审」。
        // §4 覆盖加载：`.bot/exec_policy.toml` 存在则把其 allow/forbidden **并入**默认表
        //（只增不减，不静默削弱安全）；缺/坏文件回退纯默认。
        let rules = load_exec_overrides(".bot/exec_policy.toml");
        Arc::new(agent_loop::RuleTableExecPolicy::new(rules))
    }

    /// Role prompt + workspace rules. Skills/books are injected as optional
    /// knowledge affordances, but the profile remains usable without them.
    pub fn system_prompt(&self, skills_prompt: Option<&str>, books_prompt: Option<&str>) -> String {
        let builtin = String::from(
            "You are botobot's Coder Bot, the first built-in role of a general agent harness.\n\
             Your job is to help with software projects: understand files, search precisely, edit audibly, run tools only when useful, and keep changes scoped.\n\
             Treat the workspace as the source of truth. Prefer reading relevant files before changing them. Explain risks briefly when a command or edit is non-trivial.\n\
             For code changes, prefer patch-style edits and preserve unrelated user changes. Use memory, skills, books, artifacts, and web/code tools only when they directly help the current task.\n\
             When a task needs a lot of reading, searching, or cross-file understanding, prefer dispatching the read-only `explore` sub-agent: it investigates in an isolated context and returns only conclusions with path:line anchors, so your main context stays lean.\n\
             For a large, well-scoped edit/refactor that would otherwise bloat your context with many diffs, dispatch the `editor` sub-agent: it applies the changes in an isolated context and returns only a concise change summary. Keep doing small edits yourself; reach for `editor` only when the change is big and clearly specifiable.\n\
             Be concise, but do not skip important verification details.\n\
             \n\
             Memory: at the start of a task that could depend on remembered context — the user's stated preferences, earlier decisions, or this project's conventions — search your memory first with `read(memory://<topic>)` before assuming or asking the user again. Recalls are low-confidence: verify before relying, and `retain` durable new facts worth remembering. When the user states an identity or standing preference about themselves (their name, what to call you, a lasting preference), `retain` it with `pin=true` so it stays always-visible.\n\
             \n\
             Engineering discipline (§1.7):\n\
             - For non-trivial changes, briefly outline the approach (affected files, data flow) before editing; for large or cross-cutting changes, plan it first.\n\
             - When fixing a bug, locate the root cause before patching — do not paper over symptoms.\n\
             - Prefer adding or running a test when you change logic. Keep changes scoped.\n\
             - For long-running commands (builds, tests, dev servers) that would otherwise block the turn, start them with `shell_background` and poll `job_status`, so you can keep working; cancel with `job_cancel`.\n\
             - Before claiming something works or is done, verify it (run the build/tests, or reproduce the original symptom) and report results with evidence, not assumptions.",
        );
        let mut system = if self.replace_system {
            self.custom_system.clone().unwrap_or(builtin)
        } else {
            let mut system = builtin;
            if let Some(custom) = &self.custom_system {
                system.push_str("\n\nProfile instructions:\n");
                system.push_str(custom);
            }
            system
        };

        if let Some(sp) = skills_prompt {
            system.push_str("\n\n");
            system.push_str(sp);
        }
        if let Some(bp) = books_prompt {
            system.push_str("\n\n");
            system.push_str(bp);
        }
        system
    }

    /// Register the current coder tool preset. Future non-coder profiles can
    /// expose a different subset without changing the generic tool library.
    pub fn register_tools(
        &self,
        reg: &mut ToolRegistry,
        resources: Arc<ResourceRouter>,
        memory: Arc<memory::MemoryStore>,
        skill_store: Arc<skill::SkillStore>,
    ) {
        reg.register(Arc::new(memory::RetainTool::new(memory.clone())));
        reg.register(Arc::new(memory::MemoryListTool::new(memory.clone())));
        // §1.8.6 B 结构化批量编撰（事务式校验后落盘），与单条 retain 并存。
        reg.register(Arc::new(memory::MemoryOpsTool::new(memory.clone())));
        reg.register(Arc::new(memory::ForgetMemoryTool::new(memory)));
        reg.register(Arc::new(skill::SkillListTool::new(skill_store.clone())));
        reg.register(Arc::new(skill::SkillPatchTool::new(skill_store)));
        tools::register_read(reg, resources);
        reg.register(Arc::new(agent_act::search::SearchTool));
        reg.register(Arc::new(agent_act::search::FindTool));
        reg.register(Arc::new(agent_act::patch::ApplyPatchTool));
        reg.register(Arc::new(agent_act::edit::EditByHashlineTool));
        reg.register(Arc::new(agent_act::rename::RenameFileTool));
        reg.register(Arc::new(agent_act::lsp::LspTool));
        reg.register(Arc::new(agent_act::dap::DapTool));
        reg.register(Arc::new(agent_act::dap_session::DebugTool));
        reg.register(Arc::new(agent_act::todo::TodoWriteTool));
        reg.register(Arc::new(agent_act::todo::TodoReadTool));
        reg.register_typed(tools::WebSearchTool::new());
        reg.register(Arc::new(agent_act::shell::ShellCommandTool));
        reg.register_typed(tools::CodeExecutionTool);
        reg.register_typed(tools::HttpRequestTool::new());
        // §4.9 B1 后台命令：三工具共享一个 per-agent BackgroundJobs 注册表（长构建不阻塞 turn）。
        let jobs = Arc::new(agent_act::background::BackgroundJobs::default());
        reg.register(Arc::new(agent_act::background::ShellBackgroundTool::new(
            jobs.clone(),
        )));
        reg.register(Arc::new(agent_act::background::JobStatusTool::new(
            jobs.clone(),
        )));
        reg.register(Arc::new(agent_act::background::JobCancelTool::new(
            jobs.clone(),
        )));
        reg.register(Arc::new(agent_act::background::JobListTool::new(jobs)));
    }

    /// explore 子 agent 的只读工具集（§2.5a）：只暴露 read/search/find/lsp，
    /// 不含任何 write/exec/edit，也不含 explore/sub_agent（叶子）。
    pub fn register_explore_tools(&self, reg: &mut ToolRegistry, resources: Arc<ResourceRouter>) {
        tools::register_read(reg, resources);
        reg.register(Arc::new(agent_act::search::SearchTool));
        reg.register(Arc::new(agent_act::search::FindTool));
        reg.register(Arc::new(agent_act::lsp::LspTool));
    }

    /// explore 子 agent 的 system prompt（§2.5a）：强制蒸馏——只回结论 + `path:line`，不贴大段原文。
    pub fn explore_system_prompt(&self) -> String {
        String::from(
            "You are botobot's read-only `explore` sub-agent. Investigate the codebase with read/search/find/lsp only.\n\
             报告纪律（强制蒸馏）：只回**结论** + `path:line` 锚点，绝不复述大段原文。父 agent 只需要你的结论与定位，不需要你看到的全文。\n\
             你不能编辑、执行命令或写文件——只读理解与检索。完成调查后用简洁结论回复。",
        )
    }

    /// explore 工具对模型可见的描述（§2.5a）：告知何时派。
    pub fn explore_description(&self) -> &'static str {
        "派一个只读探索子 agent 调查代码库。当你需要大量读取/检索、跨文件理解、找定义/追用法/定位某段逻辑时用它——\
         它在隔离上下文里跑 read/search/find/lsp，只回结论 + `path:line`，不占用你的主上下文。`task` = 要调查的问题。"
    }

    /// §2.5b 编辑子 agent 的工具集：read/search/find/lsp + apply_patch/edit_by_hashline/rename_file。
    /// **能读能编辑，不含 shell/exec/http/background**（纯编辑逃生阀），也不含 explore/editor（叶子）。
    pub fn register_editor_tools(&self, reg: &mut ToolRegistry, resources: Arc<ResourceRouter>) {
        tools::register_read(reg, resources);
        reg.register(Arc::new(agent_act::search::SearchTool));
        reg.register(Arc::new(agent_act::search::FindTool));
        reg.register(Arc::new(agent_act::patch::ApplyPatchTool));
        reg.register(Arc::new(agent_act::edit::EditByHashlineTool));
        reg.register(Arc::new(agent_act::rename::RenameFileTool));
        reg.register(Arc::new(agent_act::lsp::LspTool));
    }

    /// §2.5b 编辑子 agent 的 system prompt：隔离上下文执行编辑，只回**简洁变更摘要**（非完整 diff）。
    pub fn editor_system_prompt(&self) -> String {
        String::from(
            "You are botobot's `editor` sub-agent. Apply the requested code edits in an isolated context \
             using read/search/find/lsp + apply_patch/edit_by_hashline/rename_file. You can read and edit \
             files in the workspace; you cannot run commands or a shell.\n\
             报告纪律：完成后只回**简洁变更摘要**——改了哪些文件、改了什么、为什么，附 `path:line` 锚点；\
             绝不复述完整 diff 或大段代码。父 agent 只需知道你改了什么与定位。\n\
             改动最小且聚焦任务；遇到不确定的大决策，宁可少改并在摘要里说明，让父 agent 定夺。",
        )
    }

    /// §2.5b 编辑子 agent 对模型可见的描述：告知何时派（大改动逃生阀）。
    pub fn editor_description(&self) -> &'static str {
        "派一个编辑子 agent 在隔离上下文执行**一项聚焦的编辑/重构任务**。当一次跨多文件的大改动会撑爆你的主上下文时用它——\
         它能 read/search/find/lsp + apply_patch/edit_by_hashline/rename_file（不能跑命令/shell），改完只回简洁变更摘要 + `path:line`。\
         `task` = 要执行的明确编辑任务（说清目标、范围与约束）。"
    }

    pub fn tool_preset_names(&self) -> &'static [&'static str] {
        &[
            "read",
            "search",
            "find",
            "apply_patch",
            "edit_by_hashline",
            "rename_file",
            "lsp",
            "dap",
            "debug",
            "todo_write",
            "todo_read",
            "retain",
            "memory_list",
            "forget_memory",
            "skill_list",
            "skill_patch",
            "web_search",
            "shell_command",
            "code_execution",
            "http_request",
            "shell_background",
            "job_status",
            "job_cancel",
            "job_list",
        ]
    }
}

/// §4 exec policy 覆盖文件结构：`[exec] allow=[...] forbidden=[...]`。
#[derive(Debug, Default, Deserialize)]
struct ExecOverrideToml {
    #[serde(default)]
    exec: ExecOverrideSection,
}
#[derive(Debug, Default, Deserialize)]
struct ExecOverrideSection {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    forbidden: Vec<String>,
}

/// §4：从 TOML 加载 exec 规则覆盖，**并入**默认表（只增不减）。缺文件/解析失败 → 纯默认表
/// （永不静默放宽）。路径相对 CWD（与 `.bot` 同根）。
fn load_exec_overrides(path: impl AsRef<Path>) -> agent_loop::ExecRules {
    let base = agent_loop::ExecRules::default_coder();
    match std::fs::read_to_string(path.as_ref()) {
        Ok(text) => match toml::from_str::<ExecOverrideToml>(&text) {
            Ok(cfg) => base.with_overrides(cfg.exec.allow, cfg.exec.forbidden),
            Err(e) => {
                eprintln!(
                    "(exec policy: {} 解析失败，回退默认表: {e})",
                    path.as_ref().display()
                );
                base
            }
        },
        Err(_) => base, // 无覆盖文件 = 纯默认（常态，不告警）
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explore_tools_are_read_only_four() {
        let profile = CoderBotProfile::builtin();
        let mut reg = ToolRegistry::default();
        let resources = Arc::new(tools::default_router());
        profile.register_explore_tools(&mut reg, resources);
        for t in ["read", "search", "find", "lsp"] {
            assert!(reg.has(t), "explore 工具集应含 {t}");
        }
        for t in [
            "apply_patch",
            "edit_by_hashline",
            "rename_file",
            "shell_command",
            "debug",
            "todo_write",
            "code_execution",
        ] {
            assert!(!reg.has(t), "explore 工具集不应含写/执行工具 {t}");
        }
        // 叶子：不含 explore / sub_agent
        assert!(
            !reg.has("explore"),
            "explore 子 agent 不应再含 explore（叶子）"
        );
    }

    // §2.5b 编辑子 agent：read+edit 工具齐全；不含 shell/exec/http/background；叶子（无 explore/editor）。
    #[test]
    fn editor_tools_are_read_plus_edit_no_exec() {
        let profile = CoderBotProfile::builtin();
        let mut reg = ToolRegistry::default();
        let resources = Arc::new(tools::default_router());
        profile.register_editor_tools(&mut reg, resources);
        for t in [
            "read",
            "search",
            "find",
            "lsp",
            "apply_patch",
            "edit_by_hashline",
            "rename_file",
        ] {
            assert!(reg.has(t), "editor 工具集应含 {t}");
        }
        for t in [
            "shell_command",
            "code_execution",
            "http_request",
            "shell_background",
            "explore",
            "editor",
        ] {
            assert!(
                !reg.has(t),
                "editor 工具集不应含 {t}（无 exec/shell，叶子）"
            );
        }
    }

    #[test]
    fn system_prompt_carries_engineering_discipline() {
        let sys = CoderBotProfile::builtin().system_prompt(None, None);
        assert!(sys.contains("Engineering discipline"));
        assert!(sys.contains("root cause"));
        assert!(sys.contains("verify"));
        // 主动记忆召回纪律（修「首轮不自查记忆」）。
        assert!(sys.contains("memory://"), "应指导主动 read(memory://) 召回");
    }

    #[test]
    fn explore_prompts_carry_distillation_and_guidance() {
        let profile = CoderBotProfile::builtin();
        let sys = profile.explore_system_prompt();
        assert!(
            sys.contains("结论") && sys.contains("path:line"),
            "explore system prompt 应含蒸馏纪律（结论 + path:line）"
        );
        let desc = profile.explore_description();
        assert!(
            desc.contains("explore") || desc.contains("探索"),
            "explore_description 应描述何时派"
        );
        // 主 coder prompt 含 explore 引导词
        let main = profile.system_prompt(None, None);
        assert!(
            main.contains("explore"),
            "主 coder prompt 应含 explore 引导词"
        );
    }

    #[test]
    fn exec_overrides_load_merge_and_fallback() {
        use agent_loop::{Verdict, classify};
        // 沙箱模型：缺文件 → 纯默认表。workdir 内放行；越界（绝对路径）→ Prompt。
        let def = load_exec_overrides("/nonexistent/exec_policy.toml");
        assert!(
            matches!(classify("ls -la", &def), Verdict::Allow),
            "缺省下 workdir 内放行"
        );
        assert!(
            matches!(classify("cat ../secret", &def), Verdict::Prompt { .. }),
            "缺省下越界 Prompt"
        );

        // 覆盖文件：allow 信任越界命令、forbidden 新增生效、默认 forbidden 仍在。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("botobot-execpol-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("exec_policy.toml");
        std::fs::write(
            &path,
            "[exec]\nallow = [\"cat\"]\nforbidden = [\"git push\"]\n",
        )
        .unwrap();
        let rules = load_exec_overrides(&path);
        assert!(
            matches!(classify("cat ../secret", &rules), Verdict::Allow),
            "覆盖 allow 信任越界命令"
        );
        assert!(
            matches!(classify("git push origin", &rules), Verdict::Deny(_)),
            "覆盖 forbidden 生效"
        );
        assert!(
            matches!(classify("rm -rf /x", &rules), Verdict::Deny(_)),
            "默认 forbidden 不被删除"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coder_prompt_names_role_and_workspace_rules() {
        let profile = CoderBotProfile::builtin();
        let prompt = profile.system_prompt(Some("SKILLS"), Some("BOOKS"));

        assert!(prompt.contains("Coder Bot"));
        assert!(prompt.contains("workspace"));
        assert!(prompt.contains("preserve unrelated user changes"));
        assert!(prompt.contains("SKILLS"));
        assert!(prompt.contains("BOOKS"));
    }

    /// 特征化测试：锁住 `profile.system_prompt(skills, books)` 自身的装配顺序与分隔
    /// （角色 → skills 段 → books 段，各 `\n\n` 分隔，无悬空尾）。
    /// 注：§1.8.2 第 3 步（方案 B，2026-06-23）后，**bot.rs 的常驻装配已改走
    /// `ContextAssembler`**（按可信度排序：book 在前、skill 在后 + 可信度头），
    /// bot.rs 调 `system_prompt(None, None)` 只取角色段。本测试仍守此方法的独立契约
    /// （其 skills/books 参数路径供其它 profile/调用方备用）。
    #[test]
    fn system_prompt_assembly_order_is_role_then_skills_then_books() {
        let profile = CoderBotProfile::builtin();
        let prompt = profile.system_prompt(Some("SKILLS-BLOCK"), Some("BOOKS-BLOCK"));

        let role_at = prompt.find("Coder Bot").expect("角色段应在");
        let skills_at = prompt.find("SKILLS-BLOCK").expect("skills 段应在");
        let books_at = prompt.find("BOOKS-BLOCK").expect("books 段应在");
        // 顺序不变量：角色 → skills → books。
        assert!(role_at < skills_at, "角色应在 skills 之前");
        assert!(skills_at < books_at, "skills 应在 books 之前");
        // 分隔不变量：各段以 \n\n 衔接。
        assert!(
            prompt.contains("\n\nSKILLS-BLOCK"),
            "skills 段前应有空行分隔"
        );
        assert!(prompt.contains("\n\nBOOKS-BLOCK"), "books 段前应有空行分隔");

        // 缺省（无 skills/books）时不应残留空段标记或多余分隔尾。
        let bare = profile.system_prompt(None, None);
        assert!(!bare.contains("SKILLS-BLOCK") && !bare.contains("BOOKS-BLOCK"));
        assert!(!bare.ends_with("\n\n"), "无附加段时不应留悬空分隔");
    }

    #[test]
    fn toml_profile_can_append_custom_system() {
        let profile = CoderBotProfile::from_toml(
            r#"
id = "reviewer"
display_name = "Reviewer"
system = "Focus on reviews."
"#,
        )
        .unwrap();
        assert_eq!(profile.id(), "reviewer");
        assert_eq!(profile.display_name(), "Reviewer");
        let prompt = profile.system_prompt(None, None);
        assert!(prompt.contains("Coder Bot"));
        assert!(prompt.contains("Focus on reviews."));
    }

    #[test]
    fn markdown_profile_derives_name() {
        let profile = CoderBotProfile::from_markdown("# Ops Bot\nKeep runs tidy.");
        assert_eq!(profile.id(), "ops-bot");
        assert_eq!(profile.display_name(), "Ops Bot");
        assert!(
            profile
                .system_prompt(None, None)
                .contains("Keep runs tidy.")
        );
    }

    #[test]
    fn coder_tool_preset_is_stable_and_minimal() {
        let profile = CoderBotProfile::builtin();
        assert_eq!(profile.id(), "coder");
        assert_eq!(profile.display_name(), "Coder Bot");
        assert_eq!(
            profile.tool_preset_names(),
            &[
                "read",
                "search",
                "find",
                "apply_patch",
                "edit_by_hashline",
                "rename_file",
                "lsp",
                "dap",
                "debug",
                "todo_write",
                "todo_read",
                "retain",
                "memory_list",
                "forget_memory",
                "skill_list",
                "skill_patch",
                "web_search",
                "shell_command",
                "code_execution",
                "http_request",
                "shell_background",
                "job_status",
                "job_cancel",
                "job_list",
            ]
        );
    }
}
