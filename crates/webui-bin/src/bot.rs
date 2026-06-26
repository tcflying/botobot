//! 装配入口：把 env/config/工具集/LLM 装成 `Arc<Agent>`，再驱动 serve 或 run_once。

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_act::resource::ResourceRouter;
use agent_infer::OpenAiCompat;
use agent_loop::{Agent, AgentEvent};
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base_types::ContentPart;
use bot_api::protocol::EventMsg;
use bot_sdk::BotSdk;
use serde::{Deserialize, Serialize};

use crate::config::{self, Endpoint};
use crate::profile::CoderBotProfile;

/// 装配完成的 bot：持有已建好的 Agent。
pub struct Bot {
    agent: Arc<Agent>,
    resources: Arc<ResourceRouter>,
    profile: CoderBotProfile,
    /// 共享 Switchboard（§4.5）：team 工具与 Hub 同一棵协作树。
    switchboard: Arc<std::sync::Mutex<team_core::Switchboard>>,
    /// 共享 cron 任务表（§2.10）：cron 工具与 Hub 的 CronHandler 同一份表。
    cron_jobs: bot_api::cron::CronJobs,
    /// Skill 仓库（§1.6）：供 `GET /api/skills` 列本地已装。
    skill_store: Arc<agent_act::skill::SkillStore>,
    /// 语义嵌入器加载状态，供 `GET /api/status` 暴露给 webui 显示加载动画。
    /// 0=loading（后台线程加载中）/1=ready（已注入，召回升余弦）/2=failed（降级关键词）。
    embedder_state: Arc<AtomicU8>,
}

const EMBEDDER_LOADING: u8 = 0;
const EMBEDDER_READY: u8 = 1;
const EMBEDDER_FAILED: u8 = 2;

#[derive(Debug, Deserialize)]
struct ResourceQuery {
    url: String,
}

/// §1.6 S2：安装/更新 skill 的请求体。`skill_md` = 目录式 SKILL.md 正文（含 frontmatter）。
#[derive(Debug, Deserialize)]
struct InstallSkillRequest {
    id: String,
    skill_md: String,
}

/// §1.6 S3：添加市场源。
#[derive(Debug, Deserialize)]
struct AddSourceRequest {
    name: String,
    url: String,
}

/// §1.6 S3：拉远端 catalog 的查询（`source` = 远端基址）。
#[derive(Debug, Deserialize)]
struct MarketCatalogQuery {
    source: String,
}

/// §1.6 S3：从某市场源装一个包。
#[derive(Debug, Deserialize)]
struct MarketInstallRequest {
    source: String,
    id: String,
}

/// §1.6 S3：catalog 项 + 本地比对（已装/可更新）。
#[derive(Debug, Serialize)]
struct CatalogEntry {
    #[serde(flatten)]
    remote: agent_act::market::RemoteSkill,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_version: Option<u32>,
    update_available: bool,
}

#[derive(Debug, Serialize)]
struct ResourcePreview {
    url: String,
    content: String,
    content_type: &'static str,
    immutable: bool,
}

#[derive(Debug, Serialize)]
struct ResourceError {
    error: String,
}

/// 常驻前缀 token 预算（§1.8.2 ContextAssembler）。取足够大以**不截断**当前
/// skill/book 常驻内容（忠实迁移）；待 memory 概要等更多源接入再按窗口收紧。
const RESIDENT_BUDGET: usize = 24_000;

/// §2.11 Always 持久化：把永久放行的 dedup_key 落 `.bot/approvals.json`（JSON 字符串数组）。
/// 失败静默（容错，不崩 agent）；read-modify-write 加锁防并发丢更新。
struct FileApprovalStore {
    path: std::path::PathBuf,
    lock: std::sync::Mutex<()>,
}

impl FileApprovalStore {
    fn new(root: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: root.as_ref().join("approvals.json"),
            lock: std::sync::Mutex::new(()),
        }
    }
}

impl base_types::ApprovalStore for FileApprovalStore {
    fn load_always(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
    }

    fn persist_always(&self, key: &str) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut keys = self.load_always();
        if keys.iter().any(|k| k == key) {
            return;
        }
        keys.push(key.to_string());
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&keys) {
            let _ = std::fs::write(&self.path, json);
        }
    }
}

impl Bot {
    /// 从 env + 配置文件装配。`BOTOBOT_SUMMARIZE` / `BOTOBOT_CONTEXT` / `BOTOBOT_HISTORY_SUMMARY` 走 env。
    pub async fn from_env() -> anyhow::Result<Self> {
        // §2.9 ② / T1b：数据根 `.botobot` 改名为 `.bot`。一次性迁移旧目录（整目录 rename，
        // 须在任何 store 打开 `.bot` 之前跑）。失败不致命：继续用 `.bot`（新装无旧目录时本就跳过）。
        {
            let (old, new) = (
                std::path::Path::new(".botobot"),
                std::path::Path::new(".bot"),
            );
            if old.exists() && !new.exists() {
                match std::fs::rename(old, new) {
                    Ok(()) => eprintln!("(migrate: .botobot → .bot 完成)"),
                    Err(e) => eprintln!("(migrate: .botobot → .bot 失败，继续用 .bot: {e})"),
                }
            }
        }
        let profile = CoderBotProfile::from_env()?;
        eprintln!("(profile: {} · {})", profile.id(), profile.display_name());
        eprintln!(
            "(profile tools: {})",
            profile.tool_preset_names().join(", ")
        );

        let ep = Endpoint::resolve();
        eprintln!(
            "(llm: {} @ {} · thinking={:?})",
            ep.model, ep.base_url, ep.thinking
        );

        let mut provider =
            OpenAiCompat::new(ep.base_url, ep.api_key, ep.model).with_temperature(ep.temperature);
        if let Some(b) = ep.thinking {
            provider = provider.with_thinking(b);
        }
        let mut llm: Arc<dyn agent_infer::Llm> = Arc::new(provider);
        // 瞬时错误重试（P-1）：默认 2 次，`BOTOBOT_RETRY=0` 关闭、其它数覆盖。
        let retries = std::env::var("BOTOBOT_RETRY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2);
        if retries > 0 {
            eprintln!("(llm: 瞬时错误最多重试 {retries} 次)");
            llm = Arc::new(agent_infer::RetryLlm::new(llm, retries));
        }

        // 技能/书（§2.9③ 收敛到 `.bot/`）：运行期家在 `.bot/skills` / `.bot/books`，与 sessions/
        // memory/artifacts/bin 同处一棵 `.bot/` 树。首次运行从 git 跟踪的仓库基线 `./skills`/`./books`
        // **播种**（缺则拷入），之后从 `.bot/` 加载。仓库副本仍是分发基线（删 `.bot/skills` 可重播种）。
        let skills_dir = crate::config::seed_bot_assets("skills");
        let books_dir = crate::config::seed_bot_assets("books");
        let skill_store = Arc::new(agent_act::skill::SkillStore::new(&skills_dir));
        let skills = skill_store.load_all();
        // 一次构造 SkillResource：既给 assembler 当常驻源，又给 router 当 skill:// handler。
        let skill_res = Arc::new(agent_act::skill::SkillResource::with_store(
            &skills,
            skill_store.clone(),
        ));
        let books = agent_act::book::load_books(&books_dir);
        let book_res =
            (!books.is_empty()).then(|| Arc::new(agent_act::book::BookResource::new(&books)));
        let workdir = config::resolve_workdir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| ".".into());
        eprintln!(
            "(skills: {} loaded, books: {} loaded, workdir: {})",
            skills.len(),
            books.len(),
            workdir.display()
        );

        // 记忆（P-5）：跨对话知识库。recall=read(memory://<query>)，retain=retain 工具。
        // ★ 落 .bot/memory/store.jsonl（§2.9）。**在 assemble 之前创建**：记忆概要要作为常驻源
        //   进开场白（§1.8.3① T2a①——修「本地 Qwen 首轮不主动召回」：靠看见钩子,不靠主动查）。
        let mem_path = std::path::Path::new(".bot/memory/store.jsonl");
        let old_mem = std::path::Path::new(".bot/memory.txt"); // 旧单文件形态一次性迁移
        if old_mem.exists() && !mem_path.exists() {
            if let Some(parent) = mem_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(old_mem, mem_path);
        }
        let memory = Arc::new(agent_act::memory::MemoryStore::open(mem_path)?);
        // 后台加载内嵌 bge 嵌入器（不阻塞启动），加载完注入 + 回填旧向量；失败保持关键词降级。
        let embedder_state = Arc::new(AtomicU8::new(EMBEDDER_LOADING));
        {
            let mem = memory.clone();
            let state = embedder_state.clone();
            let book_emb = book_res.clone(); // §1.8.8 S5：同一 embedder 也喂给 book 语义索引
            let skill_emb = skill_res.clone(); // §1.8.3b：也喂给 skill 概要索引（能力提示）
            std::thread::spawn(move || {
                // 守卫：线程**无论如何退出**(正常/panic/早返回)，只要 state 仍停在 LOADING
                // 就翻成 FAILED——否则一次 candle backfill panic 会让 /api/status 永报 loading，
                // webui 的「记忆语义加载中…」徽章永远转圈(降级关键词召回仍可用，故标 FAILED 即可)。
                struct LoadGuard(Arc<AtomicU8>);
                impl Drop for LoadGuard {
                    fn drop(&mut self) {
                        let _ = self.0.compare_exchange(
                            EMBEDDER_LOADING,
                            EMBEDDER_FAILED,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                }
                let _guard = LoadGuard(state.clone());
                let t0 = std::time::Instant::now();
                match model_embed::EmbedCore::load() {
                    Ok(core) => {
                        let load_ms = t0.elapsed().as_millis();
                        // §1.8.3b：包一层单 query 缓存——memory/skill/book 共享，force_recall 默认开后
                        // 同一 turn 同句 query 被多路嵌入（recall + skill 提示 + book 提示）只真算一次。
                        let emb: Arc<dyn base_types::Embedder> =
                            Arc::new(agent_act::embed::CachingEmbedder::new(Arc::new(core)));
                        mem.set_embedder(emb.clone()); // 含旧记忆向量回填
                        if let Some(b) = &book_emb {
                            b.set_embedder(emb.clone()); // 构建 book 语义索引
                        }
                        skill_emb.set_embedder(emb.clone()); // §1.8.3b 构建 skill 概要索引
                        state.store(EMBEDDER_READY, Ordering::Relaxed);
                        eprintln!(
                            "(memory: 语义嵌入器已加载，召回升级为余弦 — load {load_ms}ms, 总 {}ms)",
                            t0.elapsed().as_millis()
                        );
                    }
                    Err(e) => {
                        state.store(EMBEDDER_FAILED, Ordering::Relaxed);
                        eprintln!("(memory: 嵌入器加载失败，降级关键词召回: {e})");
                    }
                }
            });
        }
        let memory_res = Arc::new(agent_act::memory::MemoryResource::new(memory.clone()));

        // §1.8.2 第3步·方案B + §1.8.3 A：经 ContextAssembler 统一装配常驻前缀——收齐
        // skill/book/memory 源的 handle()，按可信度降序排（book/Authority → skill → memory），
        // 拼带可信度头的前缀。memory 概要 = 钉住的身份事实（逐字）+ 最近 N 条占位，让本地模型
        // 开口前即见钉住事实（修「首轮不主动召回」）。⚠️ 启动快照：中途 retain 下次重启才进概要。
        // §1.8.7：**稳定段**（skill + book）烤进 system 前缀一次；**易变段**（memory「现在记着的」）
        // 不烤——改由 live_prefix 每轮临时注入（见下 builder.live_prefix），故新会话/中途即时反映记忆。
        let mut sources: Vec<Arc<dyn agent_act::context::ContextSource>> = vec![skill_res.clone()];
        if let Some(b) = &book_res {
            sources.push(b.clone());
        }
        // §4.9 B4：静/动两段装配——不可变源（skill/book）进**静态前缀**（保 provider 前缀 KV 缓存），
        // 易变源（WorldState/Realtime）进**动态段**置于尾部。当前 sources 全为静态，故动态段通常为空；
        // 留此缝便于将来把易变源挂进常驻而不破前缀缓存（memory 现走 force_recall，不在此）。
        let (resident_prefix, dynamic_suffix) =
            agent_act::context::ContextAssembler::new(RESIDENT_BUDGET)
                .assemble_split(&sources)
                .await;

        // 环境上下文块（§4.6 env-tech）：把 cwd/os 注入 system，让 agent 自带环境信息。
        // 角色 prompt 走 profile（仅角色，skills/books 常驻改由 assembler 承担）。
        // 顺序：角色 + 静态常驻前缀（缓存区）+ env + 动态常驻段（易变，置于稳定段之后）。
        let mut system = profile.system_prompt(None, None);
        if !resident_prefix.is_empty() {
            system.push_str("\n\n");
            system.push_str(&resident_prefix);
        }
        system.push_str("\n\n");
        system.push_str(&agent_act::env::EnvCore::here(&workdir).environment_block());
        if !dynamic_suffix.is_empty() {
            system.push_str("\n\n");
            system.push_str(&dynamic_suffix);
        }

        let mut builder = Agent::builder()
            .llm(llm.clone())
            .system(system)
            .policy(profile.policy())
            // 单轮工具调用步数上限（§安全闸，防无限自循环）。文档/多步任务（如 officecli 建 pptx
            // 需大量 add 命令）易撞低上限——拍板提到 100；窗口压缩在 turn 内自动处理 token，与此正交。
            .max_steps(100);
        builder = builder.workdir(workdir.clone());
        // §2.6 缺陷2 收尾：流中途失败重放次数（默认 1；`BOTOBOT_STREAM_REPLAY=0` 关闭）。
        if let Some(n) = std::env::var("BOTOBOT_STREAM_REPLAY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            eprintln!("(stream: mid-stream 失败最多重放 {n} 次)");
            builder = builder.stream_replays(n);
        }
        // §2.5b：注入 subsession 落盘端口。root 与 bot_api::router 的 .bot 一致（同一棵 sessions/ 树），
        // subagent run 经 task-local 传播在任意深度落盘为 kind=subagent 的 subsession。
        builder = builder.subsession_store(Arc::new(bot_api::SessionStoreSubsessions::new(".bot")));
        // §2.11：Always 批准落 .bot/approvals.json（跨会话永久放行）。
        builder = builder.approval_store(Arc::new(FileApprovalStore::new(".bot")));
        // §1.8.8 + §1.8.3b：统一召回源——记忆图 + skill/book 能力提示，组合成一个 QueryRecall。
        // composer `force_recall` 开时按 query 检索、增广进当前 user 消息（不再每轮塞 system 块；KV 友好）。
        // 关时靠 prompt 引导 LLM 自调 read(memory://)。能力提示让本地小模型看见「自己有可用 skill/书节」。
        let mut hints: Vec<Arc<dyn agent_act::recall::CapabilityHint>> = vec![skill_res.clone()];
        if let Some(b) = &book_res {
            hints.push(b.clone());
        }
        let unified = Arc::new(agent_act::recall::UnifiedRecall::new(
            memory_res.clone(),
            hints,
        ));
        builder = builder.recall(unified);
        // §1.8.8 S4：每 turn 收口后异步角色条件化抽取 episode（复用同一 LLM + 记忆库，限流 2）。
        builder = builder.episodic(Arc::new(agent_act::episode::EpisodeWriter::new(
            llm.clone(),
            memory.clone(),
            2,
        )));

        // 工件存储（P-4）：大工具输出外置，历史留 artifact:// 引用。
        // ★ 持久化根 = .bot/（与 SessionStore 同根，CWD 相对）：放 temp_dir 会被系统清理，
        //   导致持久化 session 历史里的 artifact:// 折叠指针重启后悬空。
        let artifacts = Arc::new(agent_act::artifact::ArtifactStore::new(".bot/artifacts")?);
        // artifact_list（Read）：枚举已 spill 文本工件，供压缩后找回 artifact:// id（bot 级，需 artifacts）。
        builder
            .tools_mut()
            .register(Arc::new(agent_act::artifact::ArtifactListTool::new(
                artifacts.clone(),
            )));

        // 记忆工具（store 已在 assemble 前创建并注入概要源）：retain/forget。
        builder
            .tools_mut()
            .register(Arc::new(agent_act::memory::RetainTool::new(memory.clone())));
        builder
            .tools_mut()
            .register(Arc::new(agent_act::memory::ForgetMemoryTool::new(
                memory.clone(),
            )));
        builder
            .tools_mut()
            .register(Arc::new(agent_act::skill::SkillPatchTool::new(
                skill_store.clone(),
            )));
        // §4.6 步④：浏览器工具（browser_navigate/snapshot/click）——仅 `browser` feature 构建时注册
        //（默认关：需真 Chrome + tokio-tungstenite 网络栈）。首次调用懒启动 Chrome。
        #[cfg(feature = "browser")]
        for t in agent_act::browser::tools::browser_tools(9222) {
            builder.tools_mut().register(t);
        }
        // §4.7 OfficeCLI 薄壳工具（officecli feature 现并入 default——零依赖成本、用户重度使用；
        // 需 officecli 二进制在场，缺则工具调用时优雅报错）。
        #[cfg(feature = "officecli")]
        {
            builder
                .tools_mut()
                .register(Arc::new(agent_act::officecli::OfficeCliViewTool));
            builder
                .tools_mut()
                .register(Arc::new(agent_act::officecli::OfficeCliRawTool));
        }
        // §4.8 PDF 文字版工具（feature 默认关）。
        #[cfg(feature = "pdf")]
        builder
            .tools_mut()
            .register(Arc::new(agent_act::pdf::PdfReadTool));
        // §4.6 knowledge-tech：book_search（LLM 推理选节点）——仅在有书时注册（无书则不暴露空工具）。
        if let Some(b) = &book_res {
            builder
                .tools_mut()
                .register(Arc::new(agent_act::book::BookSearchTool::new(
                    b.clone(),
                    llm.clone(),
                )));
        }

        // 资源路由：file:// 默认 + 技能 skill:// + 工件 artifact:// + 记忆 memory://。
        // 复用上面装配时构造的 skill_res / book_res（同一实例既当常驻源又当 read handler）。
        let mut router = agent_act::tools::default_router();
        router.register(skill_res);
        if let Some(b) = book_res {
            router.register(b);
        }
        router.register(Arc::new(agent_act::artifact::ArtifactResource::new(
            artifacts.clone(),
        )));
        router.register(Arc::new(agent_act::artifact::BlobResource::new(
            artifacts.clone(),
        )));
        router.register(memory_res);
        let resources = Arc::new(router);
        profile.register_tools(
            builder.tools_mut(),
            resources.clone(),
            memory.clone(),
            skill_store.clone(),
        );

        // 只读 explore 子 agent（§2.5a）：隔离上下文做只读理解/检索，主只收蒸馏结论。
        // 限定 read/search/find/lsp + ReadOnly 双闸 + Exclusive 串行（防打爆本地端点）。叶子、volatile。
        let mut explore_b = Agent::builder()
            .llm(llm.clone())
            .system(profile.explore_system_prompt())
            .policy(Arc::new(agent_loop::ReadOnly))
            .max_steps(8)
            .workdir(workdir.clone());
        profile.register_explore_tools(explore_b.tools_mut(), resources.clone());
        let explore = explore_b.build();
        builder = builder.sub_agent_exclusive("explore", profile.explore_description(), explore);
        eprintln!("(subagent: explore [read-only, exclusive] 已装配)");

        // §2.5b 编辑子 agent（逃生阀）：隔离上下文执行聚焦的编辑/重构，主只收变更摘要——大改动不撑爆主上下文。
        // read+edit 工具（apply_patch/edit_by_hashline/rename_file，无 shell/exec），用 profile.policy()
        // （Write tier→Allow，沙箱内编辑）。叶子、volatile。max_steps 比 explore 高（重构需更多步）。
        let mut editor_b = Agent::builder()
            .llm(llm.clone())
            .system(profile.editor_system_prompt())
            .policy(profile.policy())
            .max_steps(40)
            .workdir(workdir.clone());
        profile.register_editor_tools(editor_b.tools_mut(), resources.clone());
        let editor = editor_b.build();
        builder = builder.sub_agent("editor", profile.editor_description(), editor);
        eprintln!("(subagent: editor [read+edit] 已装配)");

        // 默认观察：外置大输出（P-4）。`BOTOBOT_ARTIFACT=N` 调阈值，`BOTOBOT_SUMMARIZE` 覆盖为摘要式。
        let inline_max = std::env::var("BOTOBOT_ARTIFACT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8000);
        builder = builder.observe(Arc::new(agent_observe::ArtifactObserver::new(
            artifacts.clone(),
            inline_max,
        )));
        if let Some(max) = std::env::var("BOTOBOT_SUMMARIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            eprintln!("(observer: summarize tool outputs > {max} chars)");
            builder = builder.observe(Arc::new(agent_observe::SummarizingObserver::new(
                llm.clone(),
                max,
            )));
        }
        // 压缩默认开（决策⑧）：真实窗口三级解析(env>config>32768，见 Endpoint::resolve)，
        // Compactor 内部派生 soft=0.75W / hard=0.9W / keep=0.4W；注入 ArtifactStore 启用可回溯折叠。
        let window = ep.context_window;
        builder = builder
            .compaction_artifacts(artifacts.clone())
            .context_window(window);
        eprintln!(
            "(window: compaction ON | window={window} soft={} hard={})",
            window * 3 / 4,
            window * 9 / 10
        );
        if let Some(max) = std::env::var("BOTOBOT_HISTORY_SUMMARY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            let keep_recent = std::env::var("BOTOBOT_HISTORY_KEEP")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(12);
            eprintln!(
                "(history: summarize older messages above {max} chars, keep {keep_recent} recent)"
            );
            builder = builder.summary_history(max, keep_recent);
        }
        if let Some(max) = std::env::var("BOTOBOT_TOKEN_BUDGET")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            eprintln!("(budget: stop run after ~{max} estimated tokens)");
            builder = builder.token_budget(max);
        }

        // §4.5：共享 Switchboard + 4 个 team 工具（leader 在对话里观察/发言/委派）。
        // 同一 Switchboard 经 router_with_switchboard 注入 Hub，工具与 Hub 共用一棵协作树。
        let switchboard = Arc::new(std::sync::Mutex::new(team_core::Switchboard::default()));
        let team_cx =
            bot_api::TeamToolCtx::new(switchboard.clone(), Some(team_core::TeamStore::new(".bot")));
        builder
            .tools_mut()
            .register(Arc::new(bot_api::TeamMembersTool::new(team_cx.clone())));
        builder
            .tools_mut()
            .register(Arc::new(bot_api::TeamReadTool::new(team_cx.clone())));
        builder
            .tools_mut()
            .register(Arc::new(bot_api::TeamPostTool::new(team_cx.clone())));
        builder
            .tools_mut()
            .register(Arc::new(bot_api::TeamDelegateTool::new(team_cx)));

        // §2.10：共享 cron 表 + 3 个 cron 工具（schedule/list/cancel）。
        // 同一表经 router_with_switchboard_and_cron 注入 Hub，工具排的任务到点被心跳触发。
        let cron_jobs: bot_api::cron::CronJobs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cron_cx = bot_api::CronToolCtx::new(cron_jobs.clone());
        builder
            .tools_mut()
            .register(Arc::new(bot_api::ScheduleTaskTool::new(cron_cx.clone())));
        builder
            .tools_mut()
            .register(Arc::new(bot_api::ListTasksTool::new(cron_cx.clone())));
        builder
            .tools_mut()
            .register(Arc::new(bot_api::CancelTaskTool::new(cron_cx)));

        Ok(Self {
            agent: builder.build(),
            resources,
            profile,
            switchboard,
            cron_jobs,
            skill_store,
            embedder_state,
        })
    }

    /// 启动 Web UI（WS + 嵌入 webui + 打开浏览器），阻塞到退出。
    /// 端口默认固定为 8787（可用 `BOTOBOT_PORT` 覆盖；占用时回退随机），固定端口便于刷新/重连/收藏。
    pub async fn serve(self) -> anyhow::Result<()> {
        // §5.6 C10：browser feature 构建 → 告知 capabilities 投屏可用（/browser-ws 在场）。
        // serve 启动早期、单线程、无并发读此 var，set_var 安全。
        #[cfg(feature = "browser")]
        unsafe {
            std::env::set_var("BOTOBOT_CAP_BROWSER", "1");
        }
        let want_port: u16 = std::env::var("BOTOBOT_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8787);
        let listener = bot_api::bind_listener(want_port).await?;
        let port = listener.local_addr()?.port();
        // §5.7：通用助手 base agent = 共享同一工具/记忆的 coder agent，换通用角色 prompt。
        // Hub 按 bot.profile 路由（编程 bot→self.agent，通用 bot→general），市场模板由此产生真行为差异。
        // 真异构多模型：若用户配了 `BOTOBOT_GENERAL_MODEL`，再 `with_llm` 接到独立模型端点
        // （否则共用编程 bot 的 llm，向后兼容零变化）。
        let mut general_agent = self.agent.with_system(crate::profile::general_system_prompt());
        if let Some(gep) = crate::config::Endpoint::resolve().resolve_general() {
            eprintln!("(general bot: 独立模型 {} @ {})", gep.model, gep.base_url);
            let mut gprov = agent_infer::OpenAiCompat::new(gep.base_url, gep.api_key, gep.model)
                .with_temperature(gep.temperature);
            if let Some(b) = gep.thinking {
                gprov = gprov.with_thinking(b);
            }
            let mut gllm: Arc<dyn agent_infer::Llm> = Arc::new(gprov);
            let retries = std::env::var("BOTOBOT_RETRY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(2);
            if retries > 0 {
                gllm = Arc::new(agent_infer::RetryLlm::new(gllm, retries));
            }
            general_agent = general_agent.with_llm(gllm);
        }
        let general = std::sync::Arc::new(general_agent);
        let app = bot_api::router_with_switchboard_cron_profiles(
            self.agent,
            self.switchboard,
            self.cron_jobs,
            vec![("general".to_string(), general)],
        )
                .merge(resource_api_router(self.resources))
                .merge(status_api_router(self.embedder_state))
                // §5.5 B8：/api/capabilities 由 bot_api::router 统一提供（B8 shape），
                // 此处不再重复 merge（旧 §1.6 capabilities_api_router 已退场，避免路由重叠 panic）。
                .merge(skills_api_router(self.skill_store.clone()))
                .merge(market_api_router(
                    self.skill_store,
                    agent_act::market::MarketSources::new(".bot/market.json"),
                ));
        // §5.6 C10：浏览器投屏 WS（feature `browser` 才接，需真 Edge/Chrome）。
        #[cfg(feature = "browser")]
        let app = app.merge(crate::browser_mirror::browser_mirror_router());
        let app = app.fallback(crate::webui::webui_handler);
        let url = format!("http://127.0.0.1:{port}");
        eprintln!("botobot ready at {url} ({})", self.profile.display_name());
        let server = tokio::spawn(async move { bot_api::run(listener, app).await });
        tokio::task::yield_now().await;
        // 自动开浏览器：BOTOBOT_NO_OPEN 置任意值则跳过。端口固定(8787)可收藏，
        // 直接刷新已开标签页是 0 冷启动；自动拉起会触发默认浏览器冷启动(可能数十秒)。
        // open() 只「请求」系统打开、不等浏览器，用时日志可证实卡顿在浏览器侧而非此处。
        if std::env::var_os("BOTOBOT_NO_OPEN").is_some() {
            eprintln!("(已跳过自动开浏览器；在浏览器打开 {url}，端口固定可收藏后刷新即用)");
        } else {
            let t = std::time::Instant::now();
            match crate::open::open(&url) {
                Ok(()) => eprintln!(
                    "(已请求打开浏览器，用时 {}ms；若页面迟迟不出多为浏览器冷启动——可设 BOTOBOT_NO_OPEN=1 改用已开标签页刷新)",
                    t.elapsed().as_millis()
                ),
                Err(e) => eprintln!("(打不开浏览器: {e}；请手动打开 {url})"),
            }
        }
        match server.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(e) => Err(anyhow::anyhow!("server task failed: {e}")),
        }
    }

    /// 单次 CLI 模式：流式跑完并把事件渲染到终端。
    pub async fn run_once(self, parts: Vec<ContentPart>) -> anyhow::Result<()> {
        if parts.is_empty() {
            eprintln!("(empty prompt; nothing to do)");
            return Ok(());
        }

        let sdk = BotSdk::new(self.agent);
        let session_id = cli_session_id();
        let out = sdk
            .run_parts(session_id, parts, None)
            .await
            .map_err(|e| anyhow::anyhow!("failed to run CLI turn through SDK: {e}"))?;

        for ev in out.events {
            match ev.msg {
                EventMsg::Agent(agent_event) => render(&agent_event),
                EventMsg::TurnComplete => break,
                EventMsg::CancelComplete => break,
                EventMsg::ShutdownComplete => break,
                EventMsg::History { .. } => {}
                EventMsg::Error { message } => eprintln!("\n  ✗ error: {message}"),
                _ => {}
            }
        }
        println!();
        Ok(())
    }

    /// 启动 stdio MCP server，把 botobot 作为 MCP tool 暴露给外部 host。
    pub async fn run_mcp_stdio(self) -> anyhow::Result<()> {
        bot_mcp::run_stdio(BotSdk::new(self.agent)).await
    }
}

/// `GET /api/status` —— 轻量就绪探针，目前只报语义嵌入器加载状态，供 webui 启动时
/// 显示「记忆语义加载中」动画（嵌入器在后台线程加载，期间召回降级关键词，不阻塞使用）。
#[derive(Serialize)]
struct StatusResponse {
    embedder: &'static str,
}

fn status_api_router(embedder_state: Arc<AtomicU8>) -> Router {
    Router::new().route(
        "/api/status",
        get(move || {
            let embedder_state = embedder_state.clone();
            async move {
                let embedder = match embedder_state.load(Ordering::Relaxed) {
                    EMBEDDER_READY => "ready",
                    EMBEDDER_FAILED => "failed",
                    _ => "loading",
                };
                Json(StatusResponse { embedder })
            }
        }),
    )
}

/// §1.6 S1+S2 本地侧：
/// - `GET /api/skills` 列本地已装（embedded + overlay，标 source/version）
/// - `POST /api/skills/install`、`POST /api/skills/update` 写磁盘 overlay（同一语义=覆盖）
/// - `DELETE /api/skills/:id` 删 overlay（= 回退出厂 / 彻底移除）
///
/// 远端 catalog 拉取/下载（S3/S4）与市场页 UI（S5）属后续切片。
fn skills_api_router(skill_store: Arc<agent_act::skill::SkillStore>) -> Router {
    let list_store = skill_store.clone();
    let install_store = skill_store.clone();
    let update_store = skill_store.clone();
    let delete_store = skill_store.clone();
    let package_store = skill_store;
    Router::new()
        .route(
            "/api/skills",
            get(move || {
                let s = list_store.clone();
                async move { Json(s.list_installed()) }
            }),
        )
        .route(
            // §1.6 S3：包下载——返回原始 SKILL.md，供市场客户端拉取后 install_overlay。
            "/api/skills/:id/package",
            get(move |Path(id): Path<String>| {
                let s = package_store.clone();
                async move { crate::server_market::package_response(&s, &id) }
            }),
        )
        .route(
            "/api/skills/install",
            post(move |Json(req): Json<InstallSkillRequest>| {
                let s = install_store.clone();
                async move { install_skill(&s, req) }
            }),
        )
        .route(
            "/api/skills/update",
            post(move |Json(req): Json<InstallSkillRequest>| {
                let s = update_store.clone();
                async move { install_skill(&s, req) }
            }),
        )
        .route(
            "/api/skills/:id",
            delete(move |Path(id): Path<String>| {
                let s = delete_store.clone();
                async move {
                    match s.remove_overlay(&id) {
                        Ok(removed) => Json(serde_json::json!({ "id": id, "removed": removed }))
                            .into_response(),
                        Err(e) => (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": e.to_string() })),
                        )
                            .into_response(),
                    }
                }
            }),
        )
}

/// §1.6 S3 客户端侧：市场源配置 + 拉远端 catalog + 从源装包。
/// - `GET /api/market/sources` / `POST /api/market/sources` / `DELETE /api/market/sources/:name`
/// - `GET /api/market/catalog?source=<base>` 拉远端清单 + 本地比对（installed/update_available）
/// - `POST /api/market/install` {source,id} 下载包 → 落本地 overlay
fn market_api_router(
    skill_store: Arc<agent_act::skill::SkillStore>,
    sources: agent_act::market::MarketSources,
) -> Router {
    use agent_act::market::{MarketClient, MarketSource};

    let list_sources = sources.clone();
    let add_sources = sources.clone();
    let del_sources = sources;
    let catalog_store = skill_store.clone();
    let install_store = skill_store;

    Router::new()
        .route(
            "/api/market/sources",
            get(move || {
                let s = list_sources.clone();
                async move { Json(s.list()) }
            })
            .post(move |Json(req): Json<AddSourceRequest>| {
                let s = add_sources.clone();
                async move {
                    match s.add(MarketSource {
                        name: req.name,
                        url: req.url,
                    }) {
                        Ok(all) => Json(all).into_response(),
                        Err(e) => market_err(e),
                    }
                }
            }),
        )
        .route(
            "/api/market/sources/:name",
            delete(move |Path(name): Path<String>| {
                let s = del_sources.clone();
                async move {
                    match s.remove(&name) {
                        Ok(removed) => {
                            Json(serde_json::json!({ "name": name, "removed": removed }))
                                .into_response()
                        }
                        Err(e) => market_err(e),
                    }
                }
            }),
        )
        .route(
            "/api/market/catalog",
            get(move |Query(q): Query<MarketCatalogQuery>| {
                let store = catalog_store.clone();
                async move {
                    match MarketClient::new().fetch_catalog(&q.source).await {
                        Ok(remote) => {
                            let entries: Vec<CatalogEntry> = remote
                                .into_iter()
                                .map(|r| annotate_catalog(&store, r))
                                .collect();
                            Json(entries).into_response()
                        }
                        Err(e) => market_err(e),
                    }
                }
            }),
        )
        .route(
            "/api/market/install",
            post(move |Json(req): Json<MarketInstallRequest>| {
                let store = install_store.clone();
                async move {
                    let md = match MarketClient::new()
                        .fetch_package(&req.source, &req.id)
                        .await
                    {
                        Ok(md) => md,
                        Err(e) => return market_err(e),
                    };
                    match store.install_overlay(&req.id, &md) {
                        Ok(path) => Json(serde_json::json!({
                            "id": req.id,
                            "source": req.source,
                            "path": path.display().to_string(),
                        }))
                        .into_response(),
                        Err(e) => market_err(e),
                    }
                }
            }),
        )
}

/// 远端 catalog 一项 + 本地比对：installed = 本地已装；update_available = 远端版本号高于本地。
fn annotate_catalog(
    store: &agent_act::skill::SkillStore,
    remote: agent_act::market::RemoteSkill,
) -> CatalogEntry {
    let local_version = store.current_version(&remote.id);
    let installed = store.list_installed().iter().any(|d| d.id == remote.id);
    // 仅当两侧都有版本号且远端更高才算可更新；缺版本信息时保守为 false。
    let update_available = match (remote.version, local_version) {
        (Some(rv), Some(lv)) => rv > lv,
        _ => false,
    };
    CatalogEntry {
        remote,
        installed,
        local_version,
        update_available,
    }
}

fn market_err(e: anyhow::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

fn install_skill(store: &agent_act::skill::SkillStore, req: InstallSkillRequest) -> Response {
    match store.install_overlay(&req.id, &req.skill_md) {
        Ok(path) => Json(serde_json::json!({
            "id": req.id,
            "path": path.display().to_string(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn resource_api_router(resources: Arc<ResourceRouter>) -> Router {
    Router::new().route(
        "/api/resource",
        get(move |Query(query): Query<ResourceQuery>| {
            let resources = resources.clone();
            async move { resource_preview(resources, query).await }
        }),
    )
}

async fn resource_preview(resources: Arc<ResourceRouter>, query: ResourceQuery) -> Response {
    if !is_canvas_resource_url(&query.url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ResourceError {
                error: "canvas preview only accepts artifact:// or blob:sha256: URLs".into(),
            }),
        )
            .into_response();
    }

    match resources.resolve(&query.url).await {
        Ok(doc) => Json(ResourcePreview {
            url: doc.url,
            content: doc.content,
            content_type: doc.content_type,
            immutable: doc.immutable,
        })
        .into_response(),
        Err(err) => (
            StatusCode::NOT_FOUND,
            Json(ResourceError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

fn is_canvas_resource_url(url: &str) -> bool {
    url.starts_with("artifact://") || url.starts_with("blob:sha256:")
}

fn cli_session_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("cli-{n}")
}

/// 把事件渲染到终端：回答→stdout，思考/工具→stderr（嵌套靠 parent_id，本切片单层）。
fn render(ev: &AgentEvent) {
    match ev {
        AgentEvent::Token { text, .. } => {
            print!("{text}");
            std::io::stdout().flush().ok();
        }
        AgentEvent::Reasoning { text, .. } => {
            eprint!("\x1b[2m{text}\x1b[0m");
            std::io::stderr().flush().ok();
        }
        AgentEvent::ToolStart { name, args, .. } => {
            eprintln!("\n  ⛏ {name}({args})");
        }
        AgentEvent::ToolEnd { ok, result, .. } => {
            let mark = if *ok { "✓" } else { "✗" };
            eprintln!("  {mark} -> {result}");
        }
        AgentEvent::Diagnostics { ok, summary, .. } => {
            let mark = if *ok { "✓" } else { "✗" };
            eprintln!("  {mark} diagnostics -> {summary}");
        }
        AgentEvent::ApprovalRequest {
            approval_id,
            name,
            reason,
            ..
        } => {
            eprintln!("\n  ? approval {approval_id} for {name}: {reason}");
        }
        AgentEvent::ApprovalResolved {
            approval_id,
            approved,
            ..
        } => {
            let state = if *approved { "approved" } else { "denied" };
            eprintln!("  approval {approval_id}: {state}");
        }
        AgentEvent::Error { message, .. } => {
            eprintln!("\n  ✗ error: {message}");
        }
        // debug 细节(llm_request/tool_result)CLI 不渲染（用 BOTOBOT_LOG=debug 看服务端日志）。
        // stream_reset：流重放信号，CLI 无部分渲染需清（一次性打印），忽略。
        AgentEvent::Start { .. }
        | AgentEvent::Done { .. }
        | AgentEvent::Usage { .. }
        | AgentEvent::StreamReset { .. }
        | AgentEvent::Debug { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_types::{Decision, Llm, LlmError, LlmEvent, LlmOpts, Message, ToolSpec};

    struct OneShotLlm;

    #[async_trait]
    impl Llm for OneShotLlm {
        async fn infer(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _opts: &LlmOpts,
        ) -> Result<base_types::LlmStream, LlmError> {
            let decision = Decision {
                text: "cli ok".into(),
                finish_reason: Some("stop".into()),
                ..Default::default()
            };
            let events = vec![
                Ok(LlmEvent::TextDelta(decision.text.clone())),
                Ok(LlmEvent::Done(decision)),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn cli_parts_map_to_hub_user_message() {
        let op = bot_sdk::user_message_op_from_parts(
            vec![
                ContentPart::Text("hello".into()),
                ContentPart::ImageUrl("data:image/png;base64,abc".into()),
                ContentPart::Text("world".into()),
            ],
            None,
        );

        match op {
            bot_api::protocol::Op::UserMessage {
                text,
                images,
                thinking,
                web_search,
                code_execution,
                ..
            } => {
                assert_eq!(text, "hello\nworld");
                assert_eq!(images, vec!["data:image/png;base64,abc".to_string()]);
                assert_eq!(thinking, None);
                assert_eq!(web_search, None);
                assert_eq!(code_execution, None);
            }
            _ => panic!("CLI should submit user_message ops"),
        }
    }

    #[tokio::test]
    async fn cli_run_once_uses_hub_path() {
        let bot = Bot {
            agent: Agent::builder().llm(Arc::new(OneShotLlm)).build(),
            resources: Arc::new(agent_act::tools::default_router()),
            profile: CoderBotProfile::builtin(),
            switchboard: Arc::new(std::sync::Mutex::new(team_core::Switchboard::default())),
            cron_jobs: Arc::new(std::sync::Mutex::new(Vec::new())),
            skill_store: Arc::new(agent_act::skill::SkillStore::new("skills")),
            embedder_state: Arc::new(AtomicU8::new(EMBEDDER_READY)),
        };

        bot.run_once(vec![ContentPart::Text("hello".into())])
            .await
            .unwrap();
    }

    /// §1.6 S3 端到端：实例 A 当市场源（serve skills_api_router），客户端经 MarketClient
    /// 拉 catalog + 下载包，落进实例 B 的本地 overlay。
    #[tokio::test]
    async fn market_fetch_from_a_running_source_and_install_locally() {
        use agent_act::market::MarketClient;
        use agent_act::skill::SkillStore;

        let src_root = std::env::temp_dir().join(format!(
            "botobot-market-src-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store_a = Arc::new(SkillStore::new(&src_root));
        store_a
            .install_overlay("deploy", "---\ndescription: ship safely\n---\nChecklist.\n")
            .unwrap();

        // 实例 A：把 skills_api_router serve 在临时端口。
        let app = skills_api_router(store_a.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // 客户端：拉 catalog + 包。
        let client = MarketClient::new();
        let catalog = client.fetch_catalog(&base).await.unwrap();
        assert!(
            catalog.iter().any(|r| r.id == "deploy"),
            "catalog 应含 deploy"
        );
        let md = client.fetch_package(&base, "deploy").await.unwrap();
        assert!(md.contains("ship safely"));

        // 装进实例 B。
        let dst_root = std::env::temp_dir().join(format!(
            "botobot-market-dst-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store_b = SkillStore::new(&dst_root);
        store_b.install_overlay("deploy", &md).unwrap();
        assert_eq!(
            store_b
                .load_current("deploy")
                .unwrap()
                .unwrap()
                .fm
                .description
                .as_deref(),
            Some("ship safely")
        );

        server.abort();
        let _ = std::fs::remove_dir_all(src_root);
        let _ = std::fs::remove_dir_all(dst_root);
    }

    #[test]
    fn canvas_resource_api_is_limited_to_managed_artifacts() {
        assert!(is_canvas_resource_url("artifact://a0"));
        assert!(is_canvas_resource_url(
            "blob:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(!is_canvas_resource_url("file://Cargo.toml"));
        assert!(!is_canvas_resource_url("https://example.com"));
    }
}
