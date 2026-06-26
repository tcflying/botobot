# todo.md — botobot 未来路线

> 本文件**只保留未来要做的事**。完成的条目立即从这里删除，事实摘要进 `docs/now.md`，明细查 git log。
> 工作流：大改动先画图 → TDD 写码 → 改完即 commit → 同步 now.md + todo.md。

---

## Active Queue（当前执行视图，2026-06-25 整理）

> 唯一执行顺序看本节。后文长设计只作**施工蓝本**，不暗示优先级。**已完成项一律不留**（见 now.md / git log）。

**当前焦点：bots.exe（本地完整工作台）**。server.exe / §1.6 远端站 / weui3 整体已闭环、**暂搁置**（见末尾「🅩 暂搁置」）。

**现状判断（2026-06-24 校准）**：可安全自主推进的高价值后端/逻辑工作已清空；下面所有剩余项都属「**不应现在自主 blast**」类——按 §0 铁律不预付，**待用户决策或真实痛点触发**。

### 排序后的剩余未做项

**⓪ 用户当前推动：shell 工具改造（2026-06-25 立项）**

```text
[A] ✅ 去掉 Code 开关（2026-06-25）—— shell_command/code_execution/http_request + 后台命令工具
    默认可见（删 tool_enabled_for 的 code_execution 门控，仅 web_search 仍按 Search pill）。
    默认可见≠免审批：Exec-tier 仍过 exec_policy。见 now.md。前端 Code pill 现 vestigial（仍驱动
    code_execution 透传），UI 清理留用户审美域。

[B] brush 纯 Rust bash 内核换 shell。**最小档 ✅（2026-06-25，feature-gated opt-in）**：
    可行性实测决定性通过——brush-core **0.3.5 crates.io 上游**在 edition 2024 + Windows 编译干净
    （旧「只有 vendored fork」判断已过时；0.5.0 需 rustc 1.88 故用 0.3.5）。落地：Cargo feature `brush`
    （默认关、optional dep，默认构建零成本）+ `run_brush_command`（Shell::new no_profile/no_rc +
    set_working_dir 沙箱cwd + run_string，临时文件捕获 stdout/stderr + exit_code + tokio::timeout，
    保 ShellCommandTool 形状）+ 运行期 `BOTOBOT_SHELL=brush` 选用（默认走系统 shell 不变）。
    端到端测（echo 经 brush 捕获）。见 now.md。
    · **中档 ✅（2026-06-25，持久会话）**：按 session_id 持久化 brush Shell（进程级注册表
      `Arc<tokio::Mutex<Shell>>`，同会话命令串行）——cd/export 粘住，cwd/env/变量跨命令保留；
      空 session_id 退化最小档每命令新 Shell（零行为变化）。仍 feature-gated + opt-in。2 单测。见 now.md。
    · **产品可达 ✅（2026-06-25）**：webui-bin feature `brush=["full","agent-act/brush"]` 透传，
      `cargo build -p webui-bin --features brush` 即得带 brush 内核的 bots.exe。
    · **完整档真实 gap 已修完**（✅ brush env 注入 `.bot/bin` PATH · ✅ cancel 接入（biased select 抢占、
      协作式边界已注明）· ✅ **Windows 命令路径翻译 2026-06-25**（实测驱动）——探查发现 brush 在 Windows
      不补 `.exe`/不查 PATHEXT **且** 按 `:` 切 PATH（盘符 `C:` 被撕碎）→ 连 cargo 都 127；绕法=把纯命令词
      解析成完整 exe 路径单引号替换（brush 含分隔符即直接执行，绕开其 PATH 解析），cargo 实测从 127→0）。
    · **非 gap（实测证伪，不做）**：`2>&1` brush 下本就正常（probe status 0）；`|head` 的 head 缺失是
      coreutils 真缺二进制（剥它改语义，不做）；minimizer 投机无对齐痛点。**剩 ls/cat/grep 等=用户机器
      未装 Git coreutils（装了 fixup 会一并解析），属环境而非代码。§⓪[B] brush 自主可达部分自此全完成。** 见 now.md。
```

**⓪-bis 用户当前推动：技能捷径 / Trail 导航缓存（2026-06-25 立项，设计见 §1.8.5）**

```text
外脑 v2：把「P → 渐进披露 → read(skill://X#B)」的导航路径学成一条低可信「捷径」条目，
下次近义请求直奔 B（省 meta→A→B 的导航 token）；思考模式并发权威重导自愈。
核心立论：捷径存的是「去哪找的指针」非「答案」——信路径不信内容，最坏回退正常披露，爆炸半径极小。
✅ Phase 0 实体别名共标（零风险 prompt，2026-06-25）→ ✅ Phase 1 Trail 写+读（2026-06-25）
  → ✅ Phase 2a 非思考最低相似度闸（2026-06-26，纯逻辑）：const `TRAIL_MIN_ACTIONABLE_SIM=0.62`，
    召回块按捷径余弦渲染「可直接打开 vs 仅参考·照常 meta→A→B」，模型据标办（render 单测）。
  → 仍待 Phase 2b（需真模型/跨 crate 验证）：思考模式并发两路·权威胜·supersede 旧 Trail（CoderBot
    prompt 纪律，硬度待 local Qwen 实测）；trail_version 召回版本门控（需把 skill 版本接到召回，跨 crate）。
全长在已有 episode 抽取 + force_recall + 软取代三件现成机件上：不破 KV / 不建图 / 不加新检索机制。见 now.md。
```

**① 用户/审美驱动（设计已定，等用户拍板或体验痛点）**

```text
- WebUI §5.5 体验硬化：**A3/A4/A5/A6/B7/B8/C9/C11/D5 均已 ✅**（2026-06-25，见 now.md）。
    **仅余 C10 浏览器镜像 CDP 画布**（绑 §4.6 browser，前后端成对——需 Chrome screencast 帧，
    与 browser-tech 全链运行验证同卡在「装 Chrome」）。
- WebUI §5.6 IM 化外壳布局（大布局重设计，尚未走 formal 流程 → 用户审美域）
- **§5.7 Bot 市场 ✅（2026-06-25，市场 UI + 模板真行为差异）**：nail `+` 弹「bot 市场」选模板（内置 通用助手/
  编程 bot）→ 指定工作目录注册。后端 `GET /api/bot-templates` + `create_bot_with_profile`（template→
  `BotEntry.profile`），前端 `openBotMarket` 卡片模态。**① 真行为差异 ✅（2026-06-25）**：`Agent::with_system`
  派生只换角色 prompt 的 agent（共享 llm/tools），Hub `profile_agents` 注册表按 `bot.profile` 路由——
  通用 bot 用通用 prompt（无 coder SOP）、编程 bot 用 coder SOP；`general_system_prompt()` + serve() 注入。
  实跑验证 general bot prompt≠coder bot prompt。单测+preview。见 now.md。
  **② 可切换 profile ✅（2026-06-25）**：C11 笼子 tab Profile 行做成下拉，切换 PATCH `/api/bots/:id`
  改 `BotEntry.profile`，因 `agent_for_session` 每轮现取 → **下一轮即生效、无需重建/重启**（实跑验证默认 bot
  切 general→bot.md 立即变 general prompt）。`Hub::set_bot_profile` + 单测。把「默认人格」选择权交用户而非
  硬编码（默认仍 coder——用户日常编程）。**③ 真异构多模型 ✅ 装配（2026-06-26）**：`Agent::with_llm` 派生
  只换底层 llm 的 agent（行为单测证实派生 agent 真用新模型）+ `Endpoint::resolve_general()`（`BOTOBOT_GENERAL_
  MODEL` 激活、默认 None=共用编程端点）+ serve() 给 general agent `with_llm` 接独立端点。**仍待**：端到端真跑
  差异需用户配第二个模型端点（本机单模型无法在此验证）；④ 模板「分发」语义（拉远端包，随 §1.6 server 线）。
```

**② 阻塞解锁链（等真实痛点触发 §2.5，再解锁 §1.7 下半身）**

```text
- §2.5 编辑型子 agent（逃生阀）✅（2026-06-25）：`editor` 子 agent——read+edit 工具（apply_patch/
  edit_by_hashline/rename，无 shell/exec，叶子）+ 隔离上下文只回变更摘要 + sub_agent("editor") 装配，
  主 prompt 引导（大改动逃生阀）。单测，见 now.md。§1.7 前置已解锁。
- §1.7 Coder bot SOP 下半身 ✅ 技能内容（2026-06-25）：executing-plans / subagent-driven-development /
  dispatching-parallel-agents / using-git-worktrees / finishing-a-development-branch 五个 botobot-concise
  SOP 技能已写入 skills/（随仓库走），见 now.md。**仍待**：① 富格式余项（scripts/ 惰性物化 + 子 prompt +
  include_dir! 出厂 skill + eject/overlay，属 §2.9③/§1.6 基建）② 激活纪律「硬度」待本地 Qwen 实测
  （local 模型是否服从 SOP，硬门控留后续）。
- §4.5 v2 leader 主动编排（✅ 核心 + ✅ **API 接线 2026-06-25**：`POST /api/teams/:id/conduct` 触发
  conduct_team_planned，端到端测；leader 拆分工→member 并行→汇总贴回 transcript。
  **真异构多模型 ✅ 装配完成（2026-06-26）**：行为差异（coder vs general 不同 system prompt）早已落地（§5.7
  profile_agents，promptsDiffer 实跑证实）；多**模型**基建现已补齐——`Agent::with_llm` 派生只换底层 LLM 的
  agent（共享 system/tools/记忆，行为单测证实派生 agent 真用新模型）+ `Endpoint::resolve_general()`（`BOTOBOT_
  GENERAL_MODEL` 激活，默认 None=共用编程端点）+ serve() 给 general agent `with_llm` 接独立端点。**仍待**：
  端到端实跑需用户配第二个模型端点（本机当前单模型，故无法在此验证真跑差异）；per-team/per-bot 任意模型路由（>2
  模型）规模触发再扩。见 now.md）。
```

**③ YAGNI / 规模或痛点触发（不预付）**

```text
- 记忆精耕余项（§1.8.3）：②b forget tombstone（O(1) 删，规模触发再开）。（✅ ③ consolidation pass / ✅ ④ Tier2 HNSW 接入召回 2026-06-25——feature hnsw + 超阈值走 ANN、代际缓存失效、默认零开销，见 now.md。）
- §2.6 缺陷3 阶1（token 级 rollout）/ 阶2（事件日志）；tool-protection 语义保护。（✅ 缺陷4 SSE 方言扩展 2026-06-25：`input_tokens`/`output_tokens` + `thinking` 别名，见 now.md。）
- §2.9 ③ skills 二进制自解压 + 进化 git（随 §1.6）。
- §2.10 心跳余项：PingSweep handler（conn 池 + last_pong 扫描）/ Blueprint 单源多表面渲染。
- §1.5 / §4 余项：Skill last_eval。（✅ Book PageIndex reasoning 检索 2026-06-25：`book_search` 工具 LLM 推理选节点取正文+citation，见 now.md。）
  （✅ 记忆非对称检索 query 指令前缀（bge 推荐）2026-06-25：`BOTOBOT_MEMORY_QUERY_PREFIX` env 默认关，query 端加前缀、存储端不加，见 now.md。）
- exec policy 余项：approval=never 上下文降级。（✅ TOML 覆盖加载 / ✅ **沙箱模型重做 2026-06-25**：用户拍板从白名单换成沙箱——workdir 内放行、越界 Prompt、破坏性恒 Deny；参数级解析并入；allow 新语义=信任越界命令，见 now.md。）
- §4.9 jcode B 组：B1 soft interrupt 三档（部分被现 CancellationToken+steer 覆盖）。（✅ 后台工具转后台 2026-06-25：shell_background/job_status/job_cancel + BackgroundJobs，长构建不阻塞 turn，见 now.md。）
  （✅ B2 压缩异步 summarize / ✅ B3 记忆认知模型 / ✅ B4 ContextAssembler static/dynamic 前缀段 —— 均 2026-06-25 完成，见 now.md。）
- §4.9 jcode C 组（远景）：C1 plan 依赖图+心跳 / C2 overnight 资源感知停止 / swarm 状态机。
```

**④ 架构触发器（命中条件才动手，非排期）**

```text
- model-* → adapter-* 泛化：当出现【第 2 个适配器、且非模型计算】时（向量库后端 / 远程 HTTP 嵌入器 /
  Postgres 记忆存储），把 model-* 泛化为按架构角色命名的 adapter-*，避免 db-*/net-* 前缀膨胀。在那之前不动。
```

**⑤ 未来工具能力（operator 方向，不预付）**

```text
- §4.6 浏览器引擎吸收为 Tool（agent-act/src/browser/）· §4.7 OfficeCLI 接入 · §4.8 PDF 解读 / text-to-sql
- §4 sandbox：✅ **trait + NoopSandbox 2026-06-25**（base-types `Sandbox`/`NoopSandbox` + shell `active_sandbox()` 接缝，Noop=零行为变化，见 now.md）；**仍待**真后端 0~1 个（主力平台一个，pi-iso 式 OS 隔离，中期）。· pi-shell / pi-ast 子集评估
- §4.6 回借器官：knowledge-tech（LLM 选择式 book 检索）· embed-tech（已落语义记忆，余非对称检索）
```

### 🅩 暂搁置：server.exe / §1.6 远端站 / weui3（聚焦 bots.exe 期间不动）

> 2026-06-24 用户拍板：先聚焦 bots.exe，server.exe 这条线整体放到最后。已闭环、可随时拾起（设计见 §1.6）。

```text
拾起时再做（非阻塞收尾，按需）：
  · weui3 是否（裁剪后）纳入 git + 固化「build dist + BOTOBOT_SPA_DIR 启动」脚本。
  · 其它平台页（公文/台账/书信/知识库…）后端逐页换 botobot（现连平台后端→404）。
  · dist 瘦身（~65MB，大头=ai-app 富渲染栈 + 知识页文件预览，深裁=产品取舍）。
  · §2.9 ③ skills 二进制自解压 + 进化 git（随 §1.6）。
```

---

## 0. 当前共识（2026-06 校准）

**根定位：botobot = 通用 agent 框架（harness），coder bot 是它的第一个、当前唯一的 bot。** 框架是身份定位；开发上以「自用、完全掌控、我懂每一行」为优先，差异化在「它是我的」而非功能数量。

- **通用是定位，不是预付**：world / multi-bot / researcher / operator 等未实现层**不先验预留抽象**，按真实需求再落。
- 判据从「oh-my-pi 有没有」改成「**我自己写代码时会不会用到**」——从参照系驱动转向目标驱动。
- **从简洁到扩展**：先把最小可用形态坐实，扩展按真实痛点叠加，不预付复杂度。

参照原则：

| 领域 | 主参照 |
|---|---|
| 工具体验、IDE 感、hashline、LSP/DAP、长期会话体验 | oh-my-pi |
| Rust patch、exec policy、thread/turn 协议、MCP、sandbox | Codex |
| oh-my-pi `pi-natives` N-API 外壳 / Codex `core` 整体内核 | 不搬 |

**移植纪律**：`.oni/oh-my-pi` 和 `E:\oni\rust\datoobot\.oni\codex\codex-rs` 为 pinned 只读参照，不进构建。**抄器官，不 fork 身体**——只移植自包含的算法/协议/格式，藏在 botobot 自己的 trait 接缝后；坚决不继承 codex 的 100-crate + Bazel 组织模型。**codex=Rust → 可直接抄代码（Apache-2.0）；oh-my-pi=TS → 抄设计、Rust 重写（MIT）；jcode=Rust（MIT）→ 可抄代码**。每次移植=一次 commit，注释写明来源，并在 now.md 记一行。

## 1.5 上下文分层：Tool / Skill / Book / Memory（接口纪律已落，余高级检索）

> 四类都可能进 LLM 视野，但身份、可信度、检索机制与变更模型不同。接口纪律已实现（见 now.md §3），本节留**仍待施工**项。

```text
Tool   = 能力：执行不是检索，不提供事实
Skill  = 可进化 SOP：reflect 提议 + gate 后优化
Book   = 可溯源依据：结构树 + 推理式检索 + citation
Memory = 快速联想：向量/关键词模糊召回，可信度随新近度衰减
```

**第一性原则**：可信度决定检索机制（高可信可解释可溯源，低可信才允许模糊联想）；相似不等于相关（Book 追求 relevance + citation，不追 embedding similarity）；变更模型决定写回通道。

**仍待施工**：
- **Memory 向量召回升级** → §1.8.3（巩固/分层/HNSW）。
- **Book 走结构化、可引用检索**：参考 `.oni/PageIndex` 的 tree index / reasoning-based retrieval。✅ outline 推理底座（2026-06-25）：`BookResource::outline()` + `read(book://?)` 返回跨书完整 outline 供 agent 推理选节点（零 embedding，对齐 PageIndex tree index）；连同 S5 语义粗筛 `book://?<query>`。**仍待**：真 reasoning tree-search（多跳/打分排序，规模触发）。
- **Skill last_eval**：进化升级路标（对照 `.oni/SkillOpt`）——某 skill 一旦攒出 eval 套件，给它接上 ① **held-out 验证 gate**（防「看着顺眼就收」退化）② **拒绝编辑缓冲**（记住试过没用的改，与 §2.11 批准 dedup 同一去重机件，复用 `dedup_key`）。SkillOpt 的 epoch/batch/lr 那套为无人值守批量训设计，单用户人在环不搬。

## 1.6 Skill 市场 + 双 bin 部署（bots.exe / server.exe）

> 🅩 **暂搁置（2026-06-24）**：本节属 server.exe 远端站线，已整体闭环（见 Active Queue 末「🅩 暂搁置」+ now.md）。
> 当前聚焦 bots.exe，本节作为已实现蓝本留存，聚焦期间不动。

**核心定位**：Skill 市场 = Skill / Book / 只读工具能力的**分发层**。本地 `bots.exe` 是完整 agent 工作台（市场客户端）；远端 `server.exe` 是可公开部署的只读助手站点（市场服务端 + 助手双帽子）。

**远端权限边界（硬规则）**：无 Memory；Session 在浏览器 IndexedDB（server 不存长期会话）；Skill 只读（不自动写回，至多提「待审核提案」）；Book 只读（可返回 citation，不改原始依据）；Tool 默认只读（read/search/book_search/skill_read，无 write/exec/shell/browser/lsp）；capability 驱动 UI（远端 SPA 读 `/api/capabilities` 显隐）。

**市场 overlay 模型**（接 §1.7）：安装 = 拉市场包写磁盘 overlay `./skills/<name>/`；更新 = 重拉 + bump 版本；删除 = 只删 overlay（内嵌 default 始终在，不提供 disable-list）。**安装信任**：用户配置过的 server 源即视为可信（一次有安全含义的信任决定——源被投毒则随之受影响），execution 仍走正常 exec policy + 惰性物化。

**拾起时待拍板**：Q1 server bin 位置（推荐 v1 放 webui-bin 双 bin）/ Q2 账号体系（推荐 v1 不做）/ Q3 IndexedDB 跨设备同步（推荐 v1 不同步）/ Q4 tool 包（推荐声明但默认禁用）/ Q6 在线优化（远端不允许自动写回）。

## 1.7 Coder bot 「superpowers 化」—— 原生 SOP 纪律

> 目标：让 coder bot **原生带着 SOP 纪律干活**——不靠外挂遵守。参照 `.oni/superpowers`。
> **进度**：① 目录式富 skill 加载器 ✅；② 单会话 SOP 8/8 ✅（brainstorming / writing-plans / TDD /
> systematic-debugging / verification-before-completion / requesting+receiving-code-review / writing-skills + commit-discipline）。
> **下半身 ⏸ 阻塞**于 §2.5 编辑型子 agent。

**仍待施工（下半身，依赖 §2.5 subagent 隔离 + §2.7 持久化——后者已完成，前者待立项）：**

```text
依赖 §2.5：executing-plans · subagent-driven-development · dispatching-parallel-agents
          · using-git-worktrees · finishing-a-development-branch
富格式余项：scripts/ 惰性物化 + 子 prompt + include_dir! 出厂 skill + eject/overlay
```

**⚠️ 承重风险**：superpowers 纪律为前沿模型调校；coder bot 默认跑本地 Qwen，可能不老实「先 brainstorm/先查 skill」、长链丢状态。**纪律「硬度」要靠 harness 强制**（元数据强制常驻注入 + 视实测的阶段化工具门控），非指望模型自觉。施工前先用本地模型实测服从度，再决定补硬门控。

**内嵌打包与释放（已拍板 = 虚拟解析 + 按需 eject）**：default skill 用 `include_dir!` 编进二进制，开箱即带出厂 SOP；`skill://` 优先解析磁盘 overlay（`./skills/<name>/`，用户编辑/skill_patch/市场包），回退内嵌基线（只读，永不被改写）。`bots skill eject <name>` 才释放到磁盘供编辑；skill_patch 一律写 overlay（删 overlay = factory reset）；可执行 `scripts/` 惰性物化到 `.bot/skill-cache/<name>/` 再按 exec policy 执行。

## 1.8 统一上下文系统：ContextSource 原语 + 记忆精耕

> §1.5「四分体系」的升维收口 + 记忆层精耕。ContextSource 原语 + ContextAssembler ✅（now.md §4）；
> 记忆系统 v2（S1→S6）✅（now.md）。本节留**记忆层仍待施工**项。

**核心洞察（升维）**：四个桶不是四种数据，是同一空间里的四个点，由五坐标描述（trust / volatility / retrieval / writeback / residency）。共享原语「蒸馏成把手 + 溢出到可寻址 + 按需展开」已造四遍（Skill/Memory/压缩/explore）→ 值得只造一次（已抽 `ContextSource` + `ContextAssembler`）。

### 1.8.3 记忆精耕余项（YAGNI 暂缓，规模触发再开）

> 已落地：S1–S6 记忆系统 v2（force_recall 按需 RAG 增广 / 每轮异步角色条件化 episode 抽取 + 质量门 /
> book 语义索引 / composer「记忆」pill）+ f16 向量边车 + pin 字段。下方为**未做**的两层深化。

```text
②b forget tombstone：forget 别整文件重写 → id 化 + tombstone（O(1) append），配 HNSW rebuild。
②c 实体别名归一（§1.8.5 Q2 方案2，等真 miss 触发）：用 pin 的身份事实（「局长=李四」）当别名种子，
   扩散求交前把名字归一到角色 → 让只标「李四」的记忆能与标「局长」的连上。Phase 0 已先靠抽取共标兜底。
③  consolidation pass ✅（2026-06-25）：`EpisodeWriter::consolidate`（够旧 episode→LLM gist 手记、原条软取代留盘）
   + **周期触发**（`EpisodeWriter` 自带 turn 计数，env `BOTOBOT_MEMORY_CONSOLIDATE_EVERY` 开启则每 N turn 收口顺带跑，
   默认关；自包含无需 Hub plumbing）。**仍待**：预算封顶/淘汰 top-N（规模触发再开）。见 now.md。
④  Tier2 HNSW（ANN）：**索引模块 ✅（2026-06-25）**——`agent-act::ann`（feature `hnsw`，纯 Rust
   `instant-distance` 守无 C 依赖、默认不拉）：`MemoryAnn::build(id↔vec)` + `query(vec,top_k)→(行索引,距离)`
   （L2 归一→欧氏 ∝ 余弦排序等价），2 单测（空/近邻升序）。**仍待**：MemoryStore 规模触发时用 ANN 替线性扫描
   （删点弱→forget 走 rebuild）+ serde 持久化索引。当前小规模 cosine 已足，规模触发再接线。
```

> ①概要块 plumbing 在前、ANN（④）在后：没有①模型根本不去召回，ANN 建再好也空转。注：§1.8.8 v2 已用
> 「force_recall 按需增广 + 自调 read(memory://)」替代了原①「每轮第二 system 块」（破 KV 缓存），故概要常驻这步形态已变。

### 1.8.3b 统一语义召回入口（✅ 2026-06-25，吸收「外脑」论）

> 来源：用户「外脑」笔记——"用户请求上来都要访问记忆，记忆里包含 skill 和 book 的向量"。
> 与现有 §1.8 高度同构（episode 额外 LLM 抽取 + 质量门 / 结构化实体关系 / 渐进披露 已全落），
> 对照后唯一真缺口=memory/book/skill 三套独立召回空间互不通。**已落地**（见 now.md）。

```text
✅ 召回图（决策2，图非树）：RecallGraph{facts,nodes,edges} + recall_graph()（复用 1-hop 扩散
   取子图、relations 解析成边）；recall_block 渲染「事实/节点/连接 + read(memory://<节点>)深挖」，
   把"是否扩大检索"交给 LLM。
✅ 统一召回（2.1）：UnifiedRecall(memory + CapabilityHint[]) 组合；SkillResource 补 set_embedder
   + 概要(name+description)向量索引 + impl CapabilityHint（决策：每 skill 一向量）；BookResource
   复用 semantic_search impl CapabilityHint。能力提示总是出现（各≤2 条，低可信）。bot.rs 接线。
✅ force_recall 默认开（决策1，开关保留）：http/ws 人面向请求 serde 默认 true（内部 cron/hub/team
   仍显式 false）；WebUI 召回 pill 默认 data-active=true（预览验证）。
取向注脚①（已决）：用户拍板默认开（上）；是否更激进（无条件硬编码、不可关）未做——开关已够。
取向注脚②（弱缺口·后置）：节点/连接已是图；若要更结构化（按实体聚类渲染）等真痛点再做。
```

### 1.8.4 情景记忆缺口（拍板：先不专设一层，走 B）

> 缺口：bot 干新活时无法自动想起「以前在这仓库怎么解过类似问题」（旧 session 是躺硬盘的死日志）。
> **方案 B（拍板）**：不造专层，需要时临时派 explore 子 agent（§2.5）去翻 `.bot/sessions/`。
> 理由：真值钱的教训本该走 Memory（已被记忆层接住）；原始旧对话长而噪；「上次怎么搞的」不高频。
> 红利：哪天真要专层，它只是又一个 ContextSource（scheme="episode"），挂上 ContextAssembler 即可。

### 1.8.5 技能捷径 / Trail —— 导航缓存（施工蓝本，2026-06-25 立项，用户「按你倾向设计」拍板）

> **来源**：用户「外脑」论延伸——skill/book 渐进披露是 `meta→A→B`；某次解决后，把请求意图直接链到深层节点 B，
> 下次同类问题走捷径。**已锁定全部决策**（用户授权按 Claude 倾向定），本节即施工蓝本，不再反问。

**核心立论（决定安全模型）**：捷径存的是「去哪找答案的指针 `skill://X#B`」，**不是答案本身**。跟它走信的是
*路径*不是*内容*；最坏情况指针过期→读到错/空节点→**回退正常渐进披露**，爆炸半径极小。故敢「低可信存、放心用」。
与现 trust 模型天然咬合：memory=低可信「需核实」，skill/book=「翻开即权威」——捷径=低可信指针，落点=权威资源。

**锁定的决策**：
```text
Q2 别名  : 方案1（抽取时共标角色+名字，如「局长」「李四」都进 entities），零 schema；
           方案2（pin 当别名种子归一）→ 记 §1.8.3 余项，等真 miss 再上。
Q3 key(P): 不建精确 P，复用现有向量召回。捷径=特殊 episode，靠余弦模糊命中近义 P。
Q3 写入  : 扩 EpisodeWriter，同一次抽取 LLM call 顺带吐 trail（零新基建）。
Q3 过期  : Phase 1 靠「权威恒胜」自愈；版本门控（带 skill version）留 Phase 2。
Q3 打架  : 思考模式并发两路，权威胜 + supersede 旧捷径写新的（复用现成软取代）。
非思考信任: 认同「捷径只省导航不损正确」，但加最低相似度闸——过闸才直奔 B，不过闸只展示、照常 meta→A→B。
```

**数据形态（极小改动）**：`MemoryEntry` 加第三种来源 `MemorySource::Trail`（serde lowercase；旧行默认 Episode 不受影响）。
不加新字段，全塞现有结构：
```text
content   = "想给 pptx 加柱状图 → skill://officecli-pptx#add-chart"   (人读 + 渲染)
relations = ["pptx 加图表 -solved-by-> skill://officecli-pptx#add-chart"]  (复用 parse_relation 出边)
entities  = ["pptx","图表","officecli-pptx"]                          (余弦/扩散命中)
source    = Trail ; ts = now                                          (用即回升复用)
```
Phase 2 才加 `trail_version: Option<u32>`（skill 已有 version，对不上即降级/丢弃）。

**捷径生命周期**：
```text
首次解决：P ──(meta→A→B)──> read(skill://X#B) ──> 干净收尾
  └ turn 收口·点④ EpisodeWriter 抽取（同一次 LLM call）：
       {entities, relations, trail?:{intent, target}}
       target 命中 skill://|book:// 且本轮咨询了它 → append_trail

复用(P' 近义)：force_recall 余弦命中该 Trail
  ├ 非思考：sim≥闸 → 直接 read(target)（省导航）；sim<闸 → 只展示，照常 meta→A→B
  └ 思考  ：并发 { read(target)[低可信] | book_search/skill 重定位[权威] }
            一致 → 用即回升(刷 ts)；不一致 → 权威胜 + supersede 旧 Trail + 写新 Trail（闭环自愈）
```

**落点（对照真实代码）**：
```text
✅ Phase 0（Q2 别名，零风险，2026-06-25）：episode.rs 抽取 prompt 加「人物同时标身份与名字」。
✅ Phase 1（Q3 写+读，2026-06-25）：MemorySource::Trail + append_trail（去重/边车/出边）+
   recall_ranked_with_trail（捷径保底，recall_block floor=2）+ render 独立「捷径/shortcuts」段 +
   episode Extract.trail + prompt JSON 契约「仅咨询了 skill://|book:// 并解决才吐」+ extract_now→append_trail
   （非指针 target 丢弃）。6 单测。见 now.md。

✅ Phase 2a（非思考相似度闸，2026-06-26）：memory.rs `pub const TRAIL_MIN_ACTIONABLE_SIM=0.62` +
   render_memory_graph 按捷径 score 分流措辞——≥闸标「可直接打开 target 省导航」、<闸标「相似度低·仅
   参考，请照常 meta→A→B」；段头注「非思考模式按下方相似度标办」。偏保守（最坏回退正常披露）。render
   单测（高分→可直接打开 / 低分→仅参考）。见 now.md。

仍待 Phase 2b（需真模型/跨 crate）：
  · CoderBotProfile 角色 prompt 加思考分支纪律（并发两路、权威胜、不一致 forget 旧+retain 新）——硬度
    待 local Qwen 实测（非思考分支已由召回块措辞覆盖）。
  · memory.rs trail_version + 召回版本对不上降级——需把当前 skill 版本接到召回路径（跨 crate 注入），
    痛点触发再做（skill 版本变更使旧 Trail 失效的真实场景出现时）。
```

### 1.8.6 外脑规格借鉴（exocore 提案评估后，§0 过滤的增量吸收 + 留痕不搬项）

> 来源：用户转来的第三方「Agent 外脑系统开发规格」（纯 Rust + candle/BGE + HNSW + SQLite 的
> 记忆宫殿框架，12-Phase 全量设计）。评估结论：方向与 botobot §1.8 高度重合（纯 Rust/candle/BGE/
> 本地/token 预算/结构化召回都已落），其工程哲学是**瀑布式预付**，与 §0「不预付/痛点驱动/最小可用」相反。
> 故**不照搬框架**，只把验证后值得的升维点记成增量待办，按痛点逐个上。

**值得增量吸收（按价值×痛点排序，均 §1.8 方向）：**
```text
A. 记忆 eval 套件 ✅（2026-06-26）：`webui-bin/tests/memory_eval.rs`（#[ignore]，真 bge + 16 条语料 +
   5 个带「相似≠相关」陷阱的 query）量 recall@5 与陷阱规避率，设基线门（mean recall@5≥0.6、陷阱
   规避≥0.5）防退化。基线：mean recall@5=0.867、陷阱规避 100%。**已暴露弱点**：「部署上线」多事实
   query 只 recall 1/3（留作 B/C/D 打分优化靶子）。**BCD 动召回打分时此门必须不降。** 见 now.md。
B. 记忆编撰契约升级（升级现 EpisodeWriter 抽取）：让 LLM 产出**严格 JSON 的结构化 MemoryOp**
   （create/update/link/supersede + Validator 校验 + 事务式 apply）替代自由抽 entities/relations。
   **✅ Validator + 事务式 apply 半身（2026-06-26，纯逻辑）**：`MemoryOp{Create,Link,Supersede}` 契约
   + `MemoryOp::validate`（内容非空/不超长 `MEMORY_CONTENT_MAX_CHARS=2000`、字段非空、Link 非自环）+
   `MemoryStore::apply_ops`（**事务门**：全量校验任一非法则整批拒绝不写一条，全过再逐条落盘，错误带索引
   定位 LLM 产物哪条坏）+ Supersede 走精确内容软取代（淡出召回留磁盘审计）。复用现 append_episode/软
   取代，不引队列/worker/死信重管道。4 单测（validate 各拒绝路径 / 事务原子性 / create+link+supersede
   落地）。**✅ 产出端工具 `memory_ops`（2026-06-26）**：agent-facing 写工具（JSON `ops` 数组→parse→
   事务校验→落盘，坏批回报 index 让模型改），注册进 coder profile 预设（实跑确认 40 工具含 memory_ops），
   即「LLM 产结构化 JSON」的可达产出端（schema 在工具边界强制、模型按错误重试）。**✅ 稳定 entry id +
   supersede-by-id（2026-06-26，纯逻辑·零格式变更）**：关键洞察——entries 不可变，故 id 可**派生**
   （`derive_entry_id`=DefaultHasher(bank,content,ts) 短 hex）而非持久，**不加字段不改 JSONL 格式**、重开
   确定性稳定。消费闭环：`memory_list` 每条带 id（`recent_with_ids/pinned_with_ids`）→ agent `memory_ops`
   `{op:supersede,id}` 精确取代（`SupersedeById`→`supersede_by_id` 按派生 id 匹配）。3 新单测（id 确定性+
   跨重开稳定 / supersede-by-id 端到端 / memory_list 带 id）。192 测绿。这同时解掉了 update-by-id / D 版本
   指针链的「需稳定 id」前置（派生 id 即合法稳定引用）。**✅ update-by-id（2026-06-26）**：`MemoryOp::Update
   {id,content,entities,relations}`=按 id 软取代旧条 + 写新内容的**原子封装**（entries 不可变，update=取代+
   新增），零格式变更；validate 复用 Create 内容校验 + id 非空，tool `{op:"update",id,content}`。单测（按 id
   修订→旧淡出新入库留盘 / 空 id 拒）。**B 的 create/update/link/supersede 全 op 集齐**。**仍待**：① episode
   自动抽取改产 ops（EpisodeWriter prompt 升级，需真模型调；现 agent 可显式调 memory_ops，B 写路径已系统化）。
   保持现"异步 fire-and-forget + 质量门 + 背压"成本控制。
C. 结构化记忆页面 + 双向链接 + 场景层级：把扁平 episode 升维为"页面（标题/摘要/内容/链接/场景）"，
   接 §1.8.3b RecallGraph（现已有节点/边的轻量图）。**增量在现 JSONL store 上加字段**，不上 SQLite。
   **✅ eval 实验 C 验证假设（2026-06-26）**：q「部署上线」纯余弦只 1/3，但把 3 条部署事实作共享实体
   「部署」的 episode 存后 `recall_expanded` 1-hop 扩散补回到 3/3——证「实体链接修多事实召回」成立，
   且**扩散机制已在**（recall_expanded）。**✅ recall 纳入扩散 facts（2026-06-26）**：`expand_facts`（复用
   主命中不二次嵌入）→ recall_block 取 ≤3 条相关事实 → 渲成独立「相关(同实体扩散·低可信)」小节（有界
   防膨胀）。**✅ 写侧（2026-06-26）**：retain 工具加可选 `entities` 参数（`retain_with_entities`）让 curated
   手记也带实体；扩散候选放宽到 episode + 带实体 retain（Trail 除外）。**§1.8.6 C 实体链接召回核心全
   production**（自动 episode + curated 手记都扩散）。186 测绿。**仍待**：结构化页面 schema（title/links/
   scenes，更大重构）+ 写路径 LLM 自动抽实体（接 B，gated；现靠 agent 经 retail 工具手标）。
D. 版本历史（记忆可回溯）：页面变更留不可变版本快照（接 §4.9 B3 superseded 软删链，自然延伸）。
   **前置已解**：稳定 entry id 已就绪（§1.8.6 B 派生 id），版本指针链可用 id 作引用（旧条 `superseded_by`=
   新条 id）。**仍待**：`superseded_by` 指针字段（这步才真改序列化格式，痛点触发——出现「想看某事实历次修订」
   的真实需求时再落）+ 链式回溯读面。
```

**明确不搬（留痕避免重复讨论）：**
```text
❌ 一上来 HNSW + SQLite：单用户 10万条线性扫描余弦+f16 边车够用（现状）；HNSW 已 feature-gated
   规模触发（§1.8.3 ④），SQLite 引入事务/migration/spawn_blocking 复杂度而无收益。
❌ 递归任务执行器（超上下文分解→计划→拓扑执行→汇总）：属**任务执行层**（botobot 放 subagent/
   §4.5 team），不该塞进"外脑"记忆层——范围蔓延。
❌ 硬编码 8 步混合检索打分（相似度×时间衰减+多种子加分+稳定版本加权）：权重全是直觉超参，
   无 eval 即无法验证对错（见 A）。先 eval 再谈打分。
❌ 12-Phase 瀑布全量框架 / 每轮重编撰管道（队列+worker+幂等+死信）/ Scene 双存（page+独立表冗余）。
```
> 顺序铁律：**A（eval）先行 → 再 B/C/D**。没有 eval，C/D 的结构化召回打分无从验证，等于重蹈"相似≠相关"。

## 2.5 P0：上下文窗口策略 —— subagent 隔离（阶0 已完成，余阶1-4）

> 阶0 ✅：coder profile 暴露只读 explore 子 agent（read/search/find/lsp + ReadOnly 双闸 + Exclusive 串行兜底
> + system 强制蒸馏 + ArtifactObserver size 兜底）。心智模型：subagent = N 个独立小窗口并行干活、主只收蒸馏。

**剩余阶梯（按真实痛点逐级叠加）：**

```text
阶1 : codex 式跨 session 并发信号量（N，放宽到 2~3 并发）精化 + 子 agent 数上限。
阶2 : 模型路由（explore 走小模型 / 大窗口模型，对齐 quick_task）。
阶3 : 切块分发 helper —— 父把超大读取机械切块 fan-out，reduce 合并。
阶4 : markdown 自定义子 agent（tools/spawns/model frontmatter）+ 强制 yield 结构化收尾。
```

**逃生阀（按痛点）**：**editor 子 agent（读+改下放）** 仅在频繁大重构、且确认主上下文扛不住时再加——是 §1.7 SOP 下半身的前置。论据：编辑本身不吃上下文，且「写正确 patch」最需要最强模型 = 主模型，下放 ROI 为负。

## 2.6 P0：稳定性硬化 —— 仍未完成项

> 已完成：缺陷1 压缩硬化 / 缺陷2 流恢复（含 mid-stream 重放）/ 缺陷3 阶0 turn-scratch 崩溃恢复 /
> 缺陷4 SSE 健壮性（部分）。事实见 now.md §2。

- **缺陷 3 阶1/阶2 = YAGNI**：阶1（token 级 rollout，崩溃后从半句续）边际价值低、复杂度高；阶2（rollout 事件日志，重放非 message 事件）当前 message 粒度够用。真有「单轮超长生成频繁被打断」痛点再做。
- **缺陷 4 剩余**：穷举更多 provider SSE 字段方言（按真实换 provider 痛点再加）。
- **tool-protection 语义保护**（⚠️ 降优先级·待议）：skill/memory/刚 read 的承重输出永不 prune/shake。因已落「可回溯折叠」（prune/shake 改 spill `artifact://` 可无损取回），价值从「防丢失」降为「省一次取回往返 + 保前缀稳定」；需要时再做（需 `tool_call_id`→工具名映射）。

## 2.7 P0：持久化补全 —— ✅ 全完成

> ✅ SessionStore v1 + durable subsession + team_member 写入 + **token live 逐 turn 累加**（2026-06-25）均已落地（now.md §4）。本节收口。

## 2.8 P0：持久化回读 —— ✅ 全完成

> ✅ 缺陷A 前端接上回读 + 空壳会话治理；缺陷B HTTP 路由命名收口（T1c）；**最近选中会话 localStorage 持久化**（2026-06-25，刷新回到上次会话）。本节收口，事实见 now.md §4。

## 2.9 P0：`.bot/` 目录重构 —— 仅剩 ③④

> ✅ ① threads/ 退场 + memory.txt→memory/store.jsonl + todos 重锚 + artifacts 拍平双层 wart；
> ② 重命名 `.botobot`→`.bot`（T1b，含启动迁移）。事实见 now.md §4。

**仍待施工**：
- **③ skills 二进制化 + 自解压 + 进化 git**（随 §1.6 双 bin）：**运行期家收敛 ✅（2026-06-26）**——skills/books 运行期家改 `.bot/skills`/`.bot/books`，`config::seed_bot_assets` 首启从仓库基线 `./skills`/`./books` 播种（见 now.md）。**仍待**：`include_dir!` 把仓库 `skills/` 编入二进制（真分发、无仓库时的播种源）；首次解压 + `git init` baseline；skillopt 进化走 `git -C .bot/skills commit`（**分发不靠 git，进化才靠 git**，两个不同的 git）。
- ✅ **④ artifacts 孤儿 GC（2026-06-25）**：`bots gc [--apply]` mark-sweep（扫会话引用→删未引用工件），dry-run 默认 + mtime 宽限防误删在途。见 now.md。
- ✂️ **不做**：`_index.json`、`meta.children[]`（已判定过早优化，扫 meta 现算替代）。

**目标布局（参考）**：
```text
.bot/   (主仓库 .gitignore 仅一行: .bot/)
├── skills/<name>/{SKILL.md, scripts/}    运行副本 + 进化（★自带独立 git: .bot/skills/.git）
├── skill-cache/<name>/scripts/           可执行脚本惰性物化（纯派生，不进 git）
├── books/<book>/{source/, build/}        纯运行时生成，不出厂自带
├── sessions/<id>/{meta.json, messages.jsonl, todos.json}   扁平存，父子树扫 meta 现算（meta 仅 parent_id）
├── artifacts/{text/, blobs/}             全局（跨 fork 复用）
├── memory/{store.jsonl, vectors/}        记忆
├── teams/{projects.json, <team_id>/…}    协作
└── bots.json                             bot 注册表
```

## 2.10 心跳内核 —— v1 晶振 + cron 完成，余 PingSweep / Blueprint

> ✅ v1 晶振（heartbeat.rs：TickHandler + spawn_heartbeat + Hub 注册表）+ cron handler（cron.rs/cron_tools.rs：
> 到点 submit / HTTP /api/cron / agent schedule_task·list_tasks·cancel_task）。事实见 now.md。

**核心建模**：把 botobot 运行时当 OS，Hub = 内核，心跳 = 唯一晶振（进程级常驻、与连接无关）。

**仍待施工（可插拔 handler，铁律③「注册即加，不改本体」）：**
- **PingSweep handler**：把 WS 连接注册进 Hub 的 conn 池（共享注册表）+ `last_pong` 逐条扫，WS ping 退化为它的订阅者（定时器从 N 降到 1）。⚠️ **YAGNI 暂缓**：WS 已有 app/协议双层 ping-pong + Hub 惰性回收 idle session，无「死连接堆积」痛点，且与铁律④「每连接存活自管」相左。真出现痛点再立。
- **Blueprint 单源多表面渲染**：一份定义渲染成 WebUI 表单 / IM 斜杠命令 / agent 追问（后置）。

**四条施工铁律（备查）**：① 心跳只派发不阻塞（tick 里绝不 `.await` 慢 hook/LLM）；② 单一最细粒度 + 计数器分频；③ tick handler 注册表（加定时行为 = 注册 handler）；④ ping 的 per-connection 存活态各连接自管。consent-first（任务先变「建议」让用户接受/永久拒绝 + dedup_key）已采纳进 cron。

## 4. P4：暂缓但保留方向

- **多 bot 协作（Team 层）**：v1 已完成（§4.5），v2 leader 主动编排待施工（见 §4.5）。
- **完整语义记忆后端**：✅ 阶段1+2（Embedder 端口 + candle bge 真语义余弦，now.md §3）。⏳ 后续：巩固/分层/向量存储升级（§1.8.3）；非对称检索 query 指令前缀（bge 推荐，当前对称召回）。
- **上下文四分体系**：接口纪律 ✅。仍待：memory 向量召回升级（§1.8.3）、Book PageIndex 式 reasoning 检索、Skill last_eval。
- **exec policy 余项**：`approval=never` 上下文降级（需先扩 `Policy::check` 加 approval-mode 上下文）；TOML/profile 覆盖文件加载（v1 留接口）；参数级深度语义解析。（数据化规则表 + workdir 全权限 ✅。）
- **⭐ exec policy 越界启发式误伤 officecli 节点路径（2026-06-25 真实痛点立项）**：
  - **症状**：跑 `officecli add "x.pptx" "/slide[1]" --type shape …` 每条都被降级 Prompt（`accesses outside workdir or runs an arbitrary subcommand`），逐条审批拖死建 PPT 流程。
  - **根因**：`exec_policy::is_outside_path` 把任何 `/` 开头 token 当 Unix 绝对路径判越界，而 officecli 用 `/`、`/slide[1]` 作**节点寻址 DSL**（非文件系统路径）；且 Windows 上 `/foo` 本就不是绝对路径（绝对=`C:\`/`\\`），该规则是 Unix-ism 误伤。放大因素：模型每条命令前缀套 `$env:PATH = ".bot\bin;"…; officecli …`（**多余**，run_shell_command 已自动前置 .bot/bin），使「显式信任 allow 表」也失效——`trusted` 匹配整条命令首前缀（`$env:PATH` 而非 `officecli`）。
  - **近期止血（A+C）✅（2026-06-25）**：A=`is_outside_path` 修——`/`-开头**仅非 Windows**算绝对（Windows 绝对=盘符/UNC）+ 含 `[` 的 token（officecli 节点 DSL）两平台都不当文件路径；C=`trusted` 改**按段首**匹配 allow（`$env:PATH=…; officecli …` 套壳后 officecli 在第 N 段也命中）+ 默认 allow 含 `officecli` + officecli-setup skill 加调用约定（别套 $env:PATH）。跨平台测改 `..`/盘符 + 新增 officecli 节点路径放行测。见 now.md。**治标**——`is_outside_path` 脆弱本质仍在（下个用 `/` 语法的工具又撞），终态见下。
  - **终态（学 oh-my-pi `pi-iso`，接 §4 sandbox + §0⓪[B] brush）**：**关键发现**——oh-my-pi `pi-shell` 全程 grep 无字符串级命令放行/拦截分类器（`deny/outside/escape` 命中全是 shell 引号转义），它的安全边界在 **`pi-iso` OS 级文件系统隔离**（Windows `ProjFS` / Linux `overlayfs` / macOS `clonefile`），命令跑在隔离的 merged 视图里物理出不了 workdir → 据此**删掉「从命令串抠路径猜越界」整套启发式**，Prompt 只留破坏性档（rm -rf 与路径无关）。brush 只是解析/执行内核（治不了本误判：`/slide[1]` 在 AST 里仍是 `/`-开头参数 token；且本场景是 PowerShell，brush 是 bash 解析器，错配）——真正根治的是 pi-iso 那一半。
  - **决策前必须**：大改动先画图（隔离视图生命周期 + diff 回收状态机）；pi-iso ProjFS 集成成本大，先 A+C 止血、中期再上隔离。
- **⭐ 子进程泄漏兜底：会话级进程树 reaping（2026-06-25 真实痛点立项，officecli 驻留锁文件触发）**：
  - **症状**：officecli `open`/`create` 启动**驻留守护进程**常驻内存攥着 .pptx，任务完没人 `close` → 文件一直被锁。
  - **根因**：现 shell spawn 仅 `kill_on_drop(true)`（[shell.rs:140]）**只杀直接子**（powershell/sh）；officecli 驻留是**脱离出去的孙进程/独立守护**，kill_on_drop 够不着 → 泄漏。
  - **方案 = OS 进程树作用域 reaping（不耦合任何工具）**：把会话下**每个** shell 子进程生在一个**会话级 OS 作用域**里，会话结束连根杀。Windows=`CreateJobObject`+`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`+`AssignProcessToJobObject`（子默认无法 breakaway 逃出 Job，比 Unix 强，连 double-fork 脱离的守护也跑不掉）；Unix=`setsid` 新进程组 + 会话末 `kill(-pgid)`。**零工具知识**，顺带兜住任何泄漏后台（dev server/watcher）。
  - **作用域 = 会话级**（非 per-命令）：驻留价值是「一个任务内多条命令复用」，per-命令杀会打断任务中途；**会话结束**才是它自然寿命终点。
  - **⚠️ 挂载高度纪律（必记，防焊错层）**：必须开在**会话/进程高度**（session driver 或 webui-bin 启动建 Job），让任何 spawn——经系统 shell **或将来经 brush（§0⓪[B]）**——都自动落进 Job；**严禁**焊死在当前 `tokio::process::Command` 调用点。理由：brush 换的是**解释器层**，spawn 改由 brush-core 内部做；本兜底守的是**OS 进程层**，两层正交。挂对高度则**brush-proof（换 brush 零返工）**，挂错则换 brush 时失效需重接。officecli 是外部 .exe，brush 无法进程内跑它、照样 OS spawn 出驻留——故 brush 下本问题一字不变，兜底依旧需要（oh-my-pi 用 brush 仍靠 `pi-shell::child_session_action` + pi-iso 管进程，佐证 brush 不消解进程管理）。
  - **决策前必须**：大改动跨 crate（shell spawn + 会话生命周期 + 平台 cfg）→ 先画图（进程树作用域 + 会话生命周期状态机 + 平台分支）。与下条 sandbox / 上条 pi-iso 终态同属「OS 进程/文件层兜底」，可一并设计。
- **sandbox**：参照 Codex sandboxing。**纪律（别学 codex 体量）**：立**一个 `Sandbox` trait**（`wrap(cmd,workdir)->cmd'` + `isolates()`），默认 `NoopSandbox`，**后端只做 0~1 个**（主力平台一个，其余 unsupported）。✅ **trait + NoopSandbox + shell `active_sandbox()` 接缝（2026-06-25）**，Noop=零行为变化；**仍待**真后端（中期，主力平台一个）。**升级路径**：上条「exec policy 越界误伤」的终态（pi-iso OS 级隔离）即本 trait 的第一个真后端——隔离到位后 exec policy 可从「猜路径 Prompt」退化为「只拦破坏性」。
- **pi-shell / pi-ast 子集评估**：不搬 N-API 外壳；`pi-shell` 仅当 `tokio::process` 版不够再评估；`pi-ast` 未来最多取 Rust/TS/Python/Markdown 子集。

## 4.5 Team 协作层 —— v1 + v2 leader 主动编排 ✅（仅剩 future 增强）

> **v2 ✅（2026-06-25）**：`ParticipantTracker`(三类终结防卡死) + `SessionRunner` 端口 + `TeamOrchestrator`(并行 join_all) + `HubSessionRunner`(真跑 member turn) + `conduct_team`(leader 自动汇总贴回 transcript)，全端到端测试（test hub 真 turn）。**仅剩 future**：broadcast 群聊轮次令牌/防抢话 · bot `adapter`(exe 路径/启动方式)建模 · 真多 bot(≥2 真 agent)压测。


> ✅ v1：`team-core`（实体 + Switchboard + TeamStore JSONL）+ hub 接线 + 4 工具（team_members/read/post/delegate）
> + 团队创建 HTTP + 命名（world-*→team-*）。事实见 now.md。

**v2 待办（leader 主动编排）**：把现 `team_delegate`「只记意图」升级为真并行编排——member session 自动执行 + leader `TurnComplete` 自动汇总 + member 结果回写 transcript。

- **✅ v2 编排端到端（2026-06-25）**：`team_core::{ParticipantTracker(三类终结防卡死), SessionRunner 端口, TeamOrchestrator::run_team}` + bot-api `HubSessionRunner`（team_delegate→subscribe→submit(UserMessage)→等 TurnComplete/Cancel/Error→TerminalKind）。**端到端测试**（test hub + OneShotLlm 真跑 member turn→Done）通过。**leader 自动汇总 ✅（2026-06-25）**：`bot_api::team_runner::conduct_team`（编排 + leader 把 `成员/完成/失败/取消` 汇总贴回 team transcript），端到端测试（test hub 真跑 + 断言汇总消息）通过。**仍待**（增强）：串行→并行派发（join_all）+ 真多 bot（≥2 member）压测。
- **方案 = C（用户 2026-06-22 拍板）**：同步 SessionRunner 端口 或 异步后台编排器，等真有多 bot 协作需求再做。
- **⚠️ 必记踩坑教训（照搬前身 mission-tech `MissionConductor`，已落 ParticipantTracker）**：后台聚合器**必须等三类终结事件之一**（`TurnDone` / `SubmissionFailed` / `SubmissionCancelled`），不能只等 `TurnDone`——否则失败/取消的 participant 永不递减计数，team 永卡 `Running`。
- **真未决（future）**：broadcast 群聊轮次令牌 / 防抢话；bot `adapter`（exe 路径/启动方式）建模（v1 `Bot` 只有 role/home）。

## 4.6 前身 datoobot 可借鉴 crate（器官清单，不预付，记账）

> 移植纪律同 §0（抄器官不 fork 身体，重写进自有 trait 接缝，每项一 commit 注明来源）。**不回借** lsp-tech/api-tech/action-tech（botobot 已重写且更强）。已借：env-tech ✅、embed-tech ✅（语义记忆）、mission-tech（→§4.5）。

- [x] **knowledge-tech ⭐⭐** ✅（2026-06-25）：`DocRetrieve` 落为 `book_search` 工具——outline 喂 LLM 挑 ≤max_sections 个 section id（非向量、按 outline 校验防幻觉）、取回权威正文 + citation。`DocCorpus` 角色由既有 `BookResource`（markdown 目录加载 + parse_nodes 切分 + citation）承担。映射 §1.5「Book = 结构树 + 推理式检索」，见 now.md。
- [ ] **浏览器能力 = 吸收外部 `agent-browser` 引擎为 Tool ⭐⭐**（operator 方向）：外部 OSS `E:\oni\rust\datoobot\.oni\agent-browser`（~30k 行 Rust）。
  - **定性 = Tool 非 Skill**；**落点 `crates/agent-act/src/browser/` 模块**（严禁 `world-*`/`-tech`/`-layer`，不发独立包）。
  - **只搬 5 个器官**：`CdpClient`（WS + id/oneshot 配对 + broadcast）+ `BrowserManager`（launch/connect/Target attach/navigate）+ `snapshot`（AX 全树 → 缩进文本 + RefMap）+ `element/RefMap`（ref/selector → 坐标/objectId，失效回退 role+name+nth 重查）+ `interaction`（click 真坐标 / fill insertText）。
  - **坚决不搬**：daemon / CLI / IPC / 云浏览器 providers / 录屏 / iOS·webdriver（botobot 常驻 harness，进程内直接持 BrowserManager）。
  - **施工分步**：① cdp.rs 精简 CdpClient + Chrome 启动 → 能连能发（**①上半 ✅**：`cdp.rs CdpDispatcher`——id 分配 + id↔oneshot 配对 + 事件 broadcast + encode，transport-agnostic、6 单测。**①下半 ✅ 2026-06-25**：`browser/connect.rs CdpConnection`（feature `browser`，默认构建不拉 tokio-tungstenite=守不预付）——`connect_async` WS 连接 Chrome CDP 端点 + 后台 reader 喂 dispatcher + `send(method,params,session_id)`。编译验证（含/不含 feature）；**运行验证待真 Chrome**（`chrome --remote-debugging-port` 的 `webSocketDebuggerUrl`）。**②上半 ✅ 2026-06-25**：`browser/launch.rs`——`find_chrome`(BOTOBOT_CHROME env + 平台候选) + `parse_ws_endpoint`(/json/version→webSocketDebuggerUrl，单测) + `launch(port)`(spawn `--headless=new --remote-debugging-port` + 轮询端点，运行待 Chrome)）；**②下半 ✅ 2026-06-25**：`browser/page.rs Browser`（launch_headless = launch+connect；`navigate(url)` = Page.enable+Page.navigate；drop 杀 Chrome）；**③ ✅ 2026-06-25**：`snapshot.rs render_ax_tree`（getFullAXTree→文本+RefMap[backendDOMNodeId]，2 单测）+ `Browser::snapshot()` 接活连接；`interaction.rs`（`quad_center` 2 单测 + `click_at/type_text`）；`Browser::click_ref`（ref→getBoxModel→center→click_at）。**browser ①②③ 结构完整**（cdp/connect/launch/navigate/snapshot/interaction/element 7 模块，可测核心全测，feature-gated 不污染默认构建）。**④ ✅ 2026-06-25**：`tools.rs` browser_navigate(Exec)/browser_snapshot(Read)/browser_click(Exec) + `BrowserHandle`（生命周期拍板=每 agent 一个、首次工具调用懒启动 Chrome、缓存 RefMap 供 click）+ `browser_tools(port)` 工厂。**browser-tech ①②③④ 结构完整**（8 模块）+ **webui-bin 接线 ✅ 2026-06-25**（`browser` feature = `["full","agent-act/browser"]`，默认关；bot.rs cfg-gated 注册 browser_tools；`cargo build -p webui-bin --features browser` 即得带浏览器工具的 bots.exe）。**仅剩**：**全链运行验证需真 Chrome**（feature 默认关，装 Chrome 开 feature 即用）。**§4.6 browser-tech 自此自主可达部分全完成。**
  - **待定**：ref 生命周期与 turn/session 的绑定（第②步拍板）。**与 §5.5 C10 浏览器镜像成对**（后端半=本模块，前端半=C10）。

## 4.7 外部二进制接入（operator 方向，只接入不吸收）

> 区别于 §4.6（前身器官，抄进自有 Rust）：本节是**外部成熟二进制**，因语言/体量无法变成「懂每一行」，只能当 vendored 黑盒后端接入（类比 ripgrep/git）。

- [ ] **OfficeCLI 文档能力 ⭐⭐**（[iOfficeAI/OfficeCLI](https://github.com/iOfficeAI/OfficeCLI)，C#/.NET，Apache-2.0）：操作 OpenXML 静态文件（.docx/.xlsx/.pptx，不连活 Office），自带 `--json` 全覆盖 + 渲染引擎。
  - **方案 = 薄壳 Tool（shell-out）+ `officecli.md` skill**（上游已自带 skill 版本，优先直接吃下，仅在不贴合 botobot idiom 时薄改）。**落点 `crates/agent-act/src/officecli.rs`**（严禁 world-*/-tech/-layer，不新发 crate）。
  - **关键收益 = tier 分级**：`officecli_view`/`officecli_get`/`officecli_query` 标 `tier=Read`（不打断）；`officecli_edit`/`officecli_raw` 标 `tier=Exec`（过 exec policy）。
  - **工具集（粗粒度）**：view(Read·L1) + get/query(Read·L2) + edit(Exec·L2) + raw(Exec·L3 XPath 兜底)。路径语法 `/slide[1]/shape[2]`、L1→L2→L3 升级靠 description + skill 教，不硬编码进 Rust。
  - **二进制分发 = 随 release 打包**（每平台内嵌，+几十 MB/平台；定位先随包路径其次 PATH）。
  - **施工分步**：①②**部分 ✅ 2026-06-25**：`agent-act/src/officecli.rs`——`officecli_path`(BOTOBOT_OFFICECLI env + PATH 裸名，单测) + `OfficeCliViewTool`(Read·`view --json`) + `OfficeCliRawTool`(Exec·透传 get/query/edit/raw)，tier 分级 + 错误归一，3 单测；③ skill 已存在(`skills/officecli*`)，**仍待**注册进 coder profile（需二进制在场）；④ release 打包内嵌。**运行待 officecli 二进制**（`BOTOBOT_OFFICECLI` 指定）。
  - [ ] **⭐ officecli 文档预览 → 画布（2026-06-25 立项，用户推动；C10 姊妹项）**：核对确认 officecli **有观看模式**——`watch <file>`=实时预览服务器（officecli 改文档时自动刷新，带 `mark/unmark/marks/goto` 跳转节点子命令；`unwatch` 停），另有 `view <file> html`=一次性静态 HTML 渲染文件。把预览投到 webui 画布，**三档按成本**：
    - **A 最便宜（静态，首选）**：`view html` 出静态 HTML → 画布 iframe/渲染该文件，编辑后重生成+重载。**无长驻进程、无进程泄漏、无需投屏引擎**；缺点=非实时（手动 refresh）。与 skill 已用的 `view html` 视觉审计同路。
    - **B 实时·干净**：`watch` 起预览服务器 → 画布 `<iframe src="http://localhost:PORT">`，自动刷新。代价：①引入长驻服务器=撞「子进程泄漏兜底」（靠 `unwatch`/会话末 reaping 收）②**待实跑确认**端口怎么拿(固定/打印/可配?)、X-Frame-Options 是否允许被 iframe。
    - **C 实时·重**：`watch` 服务器 → 复用 §5.5 C10 的 headless Chrome 加载预览 URL + `Page.startScreencast` 帧流推画布。**仅当** B 的 iframe 被 X-Frame-Options/CSP 挡住、或需服务端渲染时才走（officecli 预览本是本地网页，能 iframe 就不必截屏）。
    - **判断**：首选 A（性价比最高、零泄漏）；要 live 上 B；别先上 C。watch 服务器是长驻进程→生命周期绑会话（接「子进程泄漏兜底」条）。**决策前必须实跑** officecli `watch` 确认端口/是否需浏览器渲染/X-Frame-Options（当前 officecli 二进制不在场，待 `BOTOBOT_OFFICECLI` 到位）。**与 §5.5 C10**（CDP screencast 画布）共用画布投屏机件，C 档直接复用其引擎。

## 4.8 文档/数据解读工具扩展（PDF / text-to-sql）

> workdir 全权限 ✅（now.md §4）。本节余 PDF / text-to-sql。

- [ ] **PDF 解读（双路：文字版 / OCR 版）⭐⭐**
  - **文字版 ✅（2026-06-25，引擎升级）**：`agent-act/src/pdf.rs`（feature `pdf`，纯 Rust **`pdf-inspector`** v0.1.3·Firecrawl 守无 C 依赖、单依赖 lopdf——替换原 `pdf-extract`）——`read(path)→PdfRead` + `needs_ocr()` + `PdfReadTool`(Read·`pdf_read(path)`，spawn_blocking)。一趟既**分类**(text/scanned/image/mixed + 置信度 + 逐页 OCR 路由)又直出**干净 Markdown**(标题/表格/多栏)。2 单测 + 真 PDF 冒烟过。默认构建不拉。
  - **OCR 版立项（2026-06-26，分层方案已定）**：问题已收敛成**单一缺口「needs_ocr 的页 → 一张图」**——LLM 侧不用动（`base-types::ContentPart::ImageUrl(data:...base64)` 是现成入口，OpenAI 兼容 `image_url` wire）；路由侧不用建（pdf-inspector 的 `pages_needing_ocr` 已按页给好）。核心张力：通用「页→位图」渲染是重活，纯 Rust 渲染器不成熟，而 pdfium(C++)/mupdf(C)/poppler(C) **全破「无 C 依赖」红线**。**关键洞察**：needs_ocr 的页绝大多数是**扫描件 = 整页一个 full-page Image XObject**，根本不需渲染，只需**抽图搬运**。
    - **第一刀（主路）路线 E ⭐ 先做**：`agent-act` 新增 feature `pdf-ocr`——用 lopdf（已随 pdf-inspector 在场）抽 needs_ocr 页的内嵌 Image XObject → `/Filter=DCTDecode`(JPEG，扫描件最常见)**字节即 JPEG 直接 base64 成 `data:image/jpeg`，零解码零渲染纯搬运**；`/Filter=FlateDecode`(原始像素)配 `/Width /Height /ColorSpace` 用 `image` crate 打包 PNG → 拼成 `ContentPart::ImageUrl` 喂多模态。**纯 Rust 守红线**，覆盖扫描件主流。pdf-inspector 内部 `scan_xobjects_in_resources`/`collect_images_from_resources` 有逻辑但 `pub(crate)` 未暴露——自己用 lopdf 抽，或给上游提 PR。
    - **第二刀（兜底 feature）路线 B**：矢量页/坏字体编码页(长尾真渲染)——类比 officecli 薄壳：shell-out `pdftoppm`(poppler)/`mutool draw`(mupdf) 渲页成 PNG，**运行时检测二进制、缺则优雅报错**，不链接 C、只调外部进程。与已有 officecli/browser 的「feature-gate + 运行时检测外部运行时」同一妥协模式。
    - **备选不碰 路线 A**：feature-gate 链接/FFI pdfium——质量上限但破红线最深，不做。
    - **二阶决策（动手前定）**：① OCR 用哪个模型——本地 Qwen-VL(守离线/隐私需部署) vs 复用主 LLM vision(若主模型本身多模态则零额外部署)，取决于当前主 LLM 是否带 vision；② 成本闸——多模态按图收费贵，只搬 `pages_needing_ocr` 的页 + 设页数上限(>N 页要确认)，路线 E 天然支持。
    - **③ 与 `read(url)` 统一入口**：远端 PDF 走同一解读管线。
- [x] **text-to-sql ⭐⭐ v1 skill ✅（2026-06-25）**：`skills/text-to-sql.md`——introspect schema（`sqlite3 .schema`）→ 基于真实 schema 写查询 → LIMIT 验证 → `shell_command` 执行；只读默认、写操作过 exec policy；相似≠正确口径纪律。**升 Tool 触发**：频繁结构化查询 + 需自动 schema 注入再做。

## 4.9 jcode 可借鉴清单（B/C 组，A 组已完成）

> 来源 `.oni/jcode`（Rust · MIT · 商用级 coding agent）。移植纪律同 §0。**A 组已全完成**（A1 图片扁平 token / A2 413 剥图恢复 / A3 记忆 embedding_model 标签 / A4 subagent 完成报告，见 now.md §2）。
> **定调**：jcode 功能极度铺开，多数违反 §0「不预付」。真正该吸收的是「已替我们踩过的坑 + 对齐已知痛点的设计点」。

### B 组 · 中期重构（对齐已有方向，需动结构）

- [ ] **B1 ⭐⭐ soft interrupt 三档（软插话 / 转后台 / 硬停）** → 延伸现「优雅取消」。源 `agent-runtime`。`SoftInterruptMessage`（安全点注入消息、不取消整 turn）+ `InterruptSignal`（AtomicBool + Notify，消 spin-loop）+ `BackgroundToolSignal`（当前长工具转后台、对话继续）。botobot 现仅「全 turn cancel」一档。可与 §2.11 四档批准 / §5.5 B6 合流。
- ✅ **B2 后台异步 summarize（2026-06-25）**：soft（0.75W）触发**后台预摘要**（快照 append-only 老区、`tokio::spawn` 不阻塞 turn），hard（0.90W）即时套用预摘要、无则同步兜底（原行为）。`SummarizePlan`+`run_summarize`(自由 fn)+`apply_summary`(边界校验)。**未抽多压缩引擎 trait**（守拒预付）。⚠️ 「不卡用户」UX 收益需长会话 live 验证；swap 正确性（边界/无损）已离线单测。见 now.md。
- [ ] **B3 ⭐⭐ 记忆认知模型（信任度 + 衰减 + 取代链）** → 并入 §1.8.3。源 `memory-types` + `graph.rs`。`TrustLevel` + `confidence`（衰减、用即回升）+ `strength`（巩固）+ `superseded_by`（软删链）+ `EdgeKind` 带权 BFS 扩散召回。**直命中「类比人脑」哲学**。**step-1 软取代 ✅（2026-06-25）**：pin 取代由硬删改 `superseded` 软删（淡出召回/pinned/recent，留磁盘可审计）。**step-2/3 1-hop 实体+关系扩散 ✅（2026-06-25）**：`recall_expanded()` 以命中条目 entities 为种子补充共享实体（step-2）**或关系文本提及种子实体**（step-3，relations 参与）的关联 episode（SAG「event↔entities 多跳」最小形态），纯增量、force_recall 块加「相关上下文」分段。均见 now.md。**step-4 confidence 衰减 ✅（2026-06-25，env 开关默认关）**：`recall_facts_at` 对带 ts 的 episode 按 `0.5^(age/half_life)` 衰减分数（手记 ts=None / pinned 不衰减），足够旧即跌破下限淡出；`BOTOBOT_MEMORY_DECAY`=on 启用、`BOTOBOT_MEMORY_HALF_LIFE_SECS` 调半衰期（默认 30 天）。见 now.md。**step-5 用即回升 ✅（2026-06-25，随 decay）**：decay 开时召回把命中 episode 的 ts 刷到 now（从使用时刻重新衰减，常用不淡出），持久化；默认关零 I/O。见 now.md。**仍待**：`superseded_by` 指针链（现为软删 bool）/ 多跳（>1 hop）+ 带权重排序（参 `.oni/SAG` 完整图）——属规模/触发再开。

### C 组 · 远景 / 记账（大，按真实需求再落）

- [ ] **C1 ⭐⭐ plan 依赖图 + 心跳/检查点** → 映 §4.5 v2 编排。源 `plan`。`PlanItem` 带 `file_scope`（防多 agent 改同文件冲突）+ `blocked_by`（依赖排序）+ `assigned_to`；`SwarmTaskProgress` 带 heartbeat/checkpoint/stale_since。**当 team-core 真并行干活时必需**，是 §4.5 v2 的细化蓝本，现不做。
- [ ] **C2 ⭐ overnight 资源/额度感知停止条件（仅单摘）** → 映 §2.10。整套「无人值守时间盒自治」太大、明显预付**不做**；只单摘「按 token 预算 + 系统资源决定是否继续」，与现 token budget + §2.10 心跳契合。
- 🤔 **swarm 生命周期状态机 / ChannelIndex 频道 pub-sub**：比 team-core「机械传话」成熟；属 §4.5 v2 范畴，此处仅记，真做编排时回看。

### 不借（留痕避免重复讨论）

```text
❌ 多 provider 整片抽象（botobot 明确「只 OpenAI-compat、默认本地 Qwen」，整片=预付；仅单摘 complete_split=B4）
❌ selfdev 自举（自编译 + canary + 跨版本迁移会话，工程量巨大、近期零回报）
❌ import-core（导 Claude Code 会话，自用框架价值低）
❌ protocol-core 全套结构化协议（现 ad-hoc WS JSON 够用）
❌ productivity-core（开发活动 dashboard，与 agent 核心无关）
❌ 60+ *-types crate 细拆粒度（直接违命名铁律）
```

> §2.11 借鉴备忘（已评估·暂不做，留痕）：#6 ContextEngine 多压缩引擎可插拔（现 Compactor 已完善，抽 trait 是预付）；#7 Toolset 场景 profile（现仅一个 coder profile，等真有「研究/群聊模式」分化再做，去概率随机改确定性绑定）；PlatformEntry 平台注册表（暂无 channel 接入，真接多 IM 时用工厂闭包注册表替代 if/elif）。

## 5.5 WebUI 体验硬化 —— 剩余未做项（交用户驱动）

> 来源前身 `E:\oni\rust\datoobot\crates\bin-layer\webui`（vanilla，UX 成熟度领先）。零依赖 vanilla 路线（前身 React `webui-next` 已回退 vanilla）。移植纪律同 §0。
> 已完成：A1 活动行 / A2 详细度显隐 / A3 同步回显 / B6 pending-steer 队列 / D-fix1~3 / D1 XSS / D2 IME / D3 订阅 / D4 孤儿 session（见 now.md §4）。

**A 组 · 纯前端（A3/A4/A5/A6 ✅ 2026-06-25，见 now.md）。**
- [ ] **A6 ⭐⭐⭐ intent-aware 自动滚动（皇冠明珠）** — 已落核心：wheel↑/**触摸下滑** detach、近底 24px reattach、following 贴底、**滚动稳定器**（detached 时重渲染按高度差锚定 scrollTop，代码块重排不跳动）。preview 真引擎验证 detach/reattach/触摸机制。**仍待**（taste 余项，需真流式手感）：单 rAF follow 窗口 600ms 调优 / `visualViewport` 软键盘监听 / `--scroll-stabilizer` padding 式（现用 scrollTop 补偿，等价更简）。

**B 组 · 需后端配合：**
- ✅ **B7 per-sid busy 圆点 + 当前工具小文字（2026-06-25）**：会话项 busy 圆点（`[data-busy] .session-state-dot` blink，原已有）+ **当前工具/阶段小字**（`.session-tool`，顶层 run 的 activitySet 同步、收尾清空）。事件已带 session_id/run_id（subagent 走 parent_id 树、各 sub-run 自带活动行）。见 now.md。
- ✅ **B8 capabilities 探测（2026-06-25）**：后端 `GET /api/capabilities`（memory/skills/books/tools_write/tools_exec/teams/cron/browser）+ 前端 `probeCapabilities()` 启动探测，按能力隐藏 `[data-cap]` 元素（旧后端无端点则全可见，向后兼容）。记忆 pill 已挂 `data-cap=memory`。见 now.md。

**C 组 · 大子系统，绑既有路线，成对一起做（C11 后端+笼子+工具+subagent tab 已落）：**
- [ ] **C9 ⭐⭐⭐ mission/team 看板 UI** — header 看板按钮 + 全屏模态 3 泳道（Active/Done/Cancelled）✅ + **卡片可展开看 transcript ✅（2026-06-25，💬N chip + 点开看 Team.messages，作者映射）**（读 /api/teams 快照渲染）。**仍待**：甘特时间轴 + 参与者实时状态图（需 §4.5 team v2 时序数据）。与 §5.6 S2 同一看板。
- [ ] **C10 ⭐⭐ 浏览器镜像（CDP screencast 画布）** — 源 `app.js:48-195`。**门控解除 ✅（2026-06-26）**：Edge 在本机（`chrome_candidates` 含 msedge，Chromium 同 CDP）→ 不再卡「需 Chrome」。**投屏引擎 ✅（2026-06-26）**：`browser/screencast.rs ScreencastCore`（抄前身 datoobot：`Page.startScreencast`→主动推 base64 JPEG 帧→ack 背压→解码广播；订阅计数自动启停；FrameMeta 坐标元数据）+ `connect.rs CdpSender`（可克隆发送/订阅句柄）。feature `browser`，单测。**stage2 端到端 ✅ 真机验证（2026-06-26）**：`webui-bin/browser_mirror.rs` `/browser-ws`——每连接启 Edge headless→`Target.createTarget`+`attachToTarget`(flat)→screencast→二进制 JPEG 帧推 WS；`{type:navigate,url}` 导航；断开发 `Browser.close` 杀 Edge 子树防孤儿。前端 header「浏览器」按钮（`data-cap=browser`，capabilities 经 `BOTOBOT_CAP_BROWSER` env 在 browser 构建 true）→ `openBrowserMirror` 连 WS、`createImageBitmap` 绘 canvas、URL 回车导航。真机验证（`--features browser`+Edge）：Edge 启→帧渲染 canvas(756×488)→导航 example.com→关后无孤儿。**stage3 双向控制 ✅ 真机验证（2026-06-26）**：帧格式改 `[u32 metaLen][meta][JPEG]` 带 FrameMeta；后端 `Mirror::dispatch_input`（mouse/wheel/key→CDP `Input.dispatchMouseEvent`/`dispatchKeyEvent`）；前端 canvas 捕获鼠标/键盘/滚轮（tabIndex 可聚焦、悬停节流、cdpModifiers、按帧 meta 换算坐标）。验证：发 scroll→页面真滚动（帧像素 255→234 变）。**地址栏跟随 + 后退/前进/刷新 ✅（2026-06-26）**：后端订阅 `Page.frameNavigated` 回传当前 URL→前端地址栏跟随；`◀▶⟳` 按钮→`Page.reload`/`getNavigationHistory`+`navigateToHistoryEntry`（真机验证 com↔org 后退前进）。**C10 已是完整浏览器**（投屏+地址栏跟随+导航+后退/前进/刷新+鼠标/键盘/滚轮）。**仅剩**：接管/交还软锁（agent vs 人控制权仲裁，需 agent browser 工具与镜像共用同一 Edge——属 §4.6+C10 统一，agent 真用 browser 工具才验得到）。
- [x] **C11 ⭐ 4-Tab bot 属性面板 ✅ 全完成（2026-06-25）** — 点 active bot → 笼子（含可切 profile 下拉）/ 工具（彩色 tier 徽章+筛选框）/ subagent（explore/editor+职责）/ bot.md（**可编辑**，保存 PATCH 即 live 生效）四 Tab。后端 `get_bot_info`（`tool_brief`/`system_prompt_for_bot` 自省）+ `set_bot_system`（自定义 bot.md 覆盖）。preview 全验证（编辑 prompt 保存→后端 live 服务、重置回默认）。

**D 组 · 仅剩收尾杂项：**
- [ ] **D5 ⭐ 收尾杂项** — ① ✅（2026-06-25）WS 句柄绑定抽 `bindWsHandlers` 共用 + 删 `logsSubscribed`/`subscribeLogsNow` 死代码（服务端 SubscribeLogs 实为 no-op，实跑验证日志仍流入）；③ ✅ `log_snapshot_done` 缺到达的 6s 超时兜底（`logsBootTimer`）。**仅剩** ② `stream_reset` 旧 rAF 帧/日志截断 seq 去重等边界（低频，按痛点）。

**实施提示**：A 组不依赖后端、与 agent-core 正交，可随时并行插入。B 组按痛点（调 subagent→B7；调 steer→B6）。C 组跟对应路线（C9↔§4.5；C10↔§4.6）一起排期，不单独提前。

## 5.6 WebUI IM 化外壳：整体布局设计（列 booter ✅，余大布局重构待审美）

> **列 booter ✅（2026-06-25）**：会话列底 `#sessions-booter`（活动数 · 会话数）+ 对话列底 `#thread-booter`（上下文用量%/已用 token，复用 token live）；`renderBooters()` 在 setBusy/switchSession/usage 刷新，preview 验证。多列结构本就存在（nail|session|对话|canvas）。**七彩身份色 ✅（2026-06-25）**：`BOT_PALETTE`(7 色) + `botColor(idx)`，createBot 按序分配 `--bot-color`（默认 bot 也走），CSS nail 按钮左条/激活底/letter 着色。preview 验证。**IM 化群聊 ✅（2026-06-26，用户驱动）**：① 默认两 bot（coder `bot-default` + 通用 `bot-general`，§5.6 两起始联系人）；② **team 进 nail 栏 = 群聊**——活跃 team 渲成 nail「群」按钮带**组合图标**（≤4 成员色块 2x2 网格），点开进群聊视图（`#team-view` 替换 session transcript，抬头显总任务 + 👑 群主 + 成员）；③ **在群里聊**——发言→`POST /api/teams/:id/message`（贴 transcript + 触发 leader 编排，乐观回显 + 轮询刷新），点 bot 退回 1:1。建群后直接进群对话。preview 全验证。**务实第一版用 `activeTeamId` mode 标志**。**仍待 S1 proper**：`activeSubject={kind,id}` 统一状态层重构（去 `activeTeamId`/`activeBotId` 并存，派生自单一真源）——高回归风险，现 mode 标志够用、待真痛点再上。


> 纯前端 `webui-bin/webui/`（index.html / app.js / style.css），无后端改动。是否升 Design Doc 待定（D4）。

### 三层结构（自上而下）

```text
┌─ header（顶部固定栏 = Windows 任务栏类比，常驻）────────────────────────────┐
│  整个 bots.exe 系统状态 + 公共按钮：连接状态 · 日志 · 画布 · (未来)bot 名册 · 看板按钮 │
├──────┬──────────────┬────────────────────────────┬──────────────────────┤
│ nail │ session list │ 对话列(composer+transcript)  │ canvas 列            │
│ 钉栏  │ (booter:活动数)│ (booter:上下文用量%)        │ (booter:画布状态)     │
└──────┴──────────────┴────────────────────────────┴──────────────────────┘
```
- **header** = 系统级任务栏（常驻，非单会话级）；bot 最多 7 个，七彩身份色（一处定义、全局复用）。
- **多列主体**：nail 钉栏 | session list | 对话列（composer+transcript 合一）| canvas 画布列。
- **列底 booter**（除 nail 外每列下方显示该列状态）：session list→活动数；对话列→上下文用量%；canvas→画布状态。

### 待定决策
- **D1 ⭐ header 七彩 bot vs nail 钉栏 bot 关系**：倾向 **A 七彩 = bot 全局身份色系统**（颜色一处定义全局复用，header 七彩是状态灯/概览，导航仍归 nail）。否决 B（header 接管导航，改动大）/ C（两套列表，信息重复）。
- **D2** canvas 列 booter 显示什么。**D3** booter 是否可点（仅展示 vs 兼作入口）。**D4** 是否升 Design Doc（全局口径＝暂不采用·待议）。

### 架构与数据（倾向/已定，参前身 webui-next）
- **S1 状态层 = 方案1（平行 Map + 判别联合）**：`teams`/`bots` 各一 Map + 单一真相源 `activeSubject={kind,id}`，`activeBotId`/`activeSessionId` 逐步派生自它，**禁止**再加 `activeTeamId`。直接支撑 D1-A。否决方案2（全重构 YAGNI）/ 方案3（team 独立路径必漂移）。
- **S2 看板 = 全屏模态浮窗**（对齐前身 mission-pane，横向 3 泳道 Active/Done/Cancelled；header 加看板按钮）。与 canvas **不**互斥（修订先前「复用 canvas 右面板」判断）。与 §5.5 C9 是同一看板两处记账，升级时合并。
- **S3 新建 team = 轻量模态表单 ✅（2026-06-25）**：看板头「+ 新建 team」→ 模态（项目名/工作目录/成员多选
  (来自当前 bots+profile emoji)/leader∈成员/任务）→ POST projects→teams→conduct→刷看板。校验+错误回显+
  Esc/遮罩关闭，复用 bot 市场模态外壳。preview 实跑验证（建 team 出现在 /api/teams）。见 now.md。
- **S4 mock 契约 = 镜像后端**（直接长成 `GET /api/teams` 的 Switchboard 快照形状，接真数据只换源不改渲染）。
- **S5 归属/时序 = 独立工作项（暂名 `webui-im-shell`）+ 后置**（已拍板）：设计现在做，实现排在 team 后端落地之后。
- **S6 零依赖 vanilla，不引 React**（同 §5.5）。

**交叉引用**：§4.5（team 后端=数据上游）· §5.5 C9（看板蓝本）· §5.5 A 组（与本节正交可并行）。

## 5.7 Bot 市场 + 通用默认 bot（2026-06-25 立项，用户推动）

> **来源**：用户「现在程序默认是编程 bot，但默认应该是一个通用 bot；可以像 skill 市场一样有一个 bot 市场，
> 点 nail 栏加号后弹市场选 bot，默认有一个编程 bot；点添加 bot 时需指定这个 bot 的工作目录」。
> **定性**：把 §0 根定位（botobot = 通用框架，coder bot 是第一个 bot）**兑现到 UX**——
> 默认身份从「编程」退回「通用」，编程 bot 降为市场里的一个选项。

**现状（要改的起点）**：
- 唯一 profile = `webui-bin::profile::CoderBotProfile`（role prompt / tool preset / policy preset / workspace 规则）。
- nail `+` 当前只「选/输目录 → 注册 backend bot entry」，**不选模板、恒为 coder**（now.md nail/§Web UI 段、`POST /api/bots`）。
- 默认 bot = 第一个 nail-btn（`botobot`），指向启动 workdir。

**两个独立改动（可分阶段）**：
```text
改动 A（轻·后端为主）：默认 bot 通用化
  · 抽 `GeneralBotProfile`（通用 role prompt + 全工具但无 coder 专属 SOP 注入），
    默认 bot 用它；CoderBotProfile 保留为模板之一。
  · 决策点见下 Q1/Q2。

改动 B（重·前后端成对）：bot 市场
  · nail `+` → 不再直接弹目录输入，先弹「bot 市场」模态（类比 skill 市场分发层 §1.6）。
  · 市场列出 bot 模板（v1 内置：通用 / 编程；未来可扩 researcher/operator…）。
  · 选模板 → 再指定工作目录 → POST 注册（profile_id + workdir）。
```

**数据流（改动 B）**：
```text
点 nail [+]
   ↓
GET /api/bot-templates          ← 新端点：返回内置模板清单 [{id,name,desc,default_tools…}]
   ↓
市场模态（卡片列表，选一个模板）
   ↓
指定工作目录（复用现「选/输服务端可访问目录」控件）
   ↓
POST /api/bots {template_id, workdir, name?}   ← 扩展现端点：加 template_id
   ↓
后端按 template_id 装配对应 profile → 注册 bot entry → 落 bots.json（需加 profile/template 字段）
   ↓
nail 新增按钮（七彩身份色），切到该 bot
```

**待拍板决策（含推荐答案）**：
- **Q1 通用 profile 与 coder 的差异边界？** 推荐：**通用 = 全工具可用但不注入 coder SOP 纪律**
  （§1.7 superpowers 那套 brainstorm/TDD/plan 注入只在 coder 模板挂）。即差异在「身份 prompt + SOP 注入」，
  工具集与 exec policy 默认相同（都沙箱模型）。否决「通用 = 砍工具」（会变成残废助手）。
- **Q2 默认 bot 是否仍叫 `botobot`？** 推荐：**保留 `botobot` 名 + 通用 profile**（最小改动，避免动 bots.json 迁移）；
  名字是身份、profile 是能力，二者解耦。
- **Q3 模板从哪来（v1）？** 推荐：**编译期内置**（`webui-bin` 注册表，类比内置 skill `include_dir!`），
  v1 就两条（通用 / 编程）。市场「分发」语义（拉远端模板包）后置到 §1.6 server 线，与 skill 市场同机件。
- **Q4 bots.json 需存什么？** 推荐：bot entry 加 `template_id`（或 `profile`）字段，重启按它重建对应 profile；
  现 entry 仅 role/home（now.md），需扩 schema + 一次性迁移（旧 entry 缺字段 → 回退 coder，保兼容）。
- **Q5 加号交互改动幅度？** 推荐：**保留现「选/输目录」控件作为市场模态的第二步**，只在它前面插一层模板选择；
  不重写目录选择逻辑。
- **Q6 工作目录是否每个 bot 必填？** 推荐：**必填**（与现 `POST /api/bots` 一致，bot=workdir 隔离视图，
  now.md「bot/workdir 后端隔离」）；通用 bot 也需要一个 workdir 才能落 `.bot/` 与跑工具。

**施工纪律**：跨 crate（webui-bin profile + bot-api 注册 + bots.json schema）+ 改数据流 + 前端市场 UI
→ §工作流铁律「大改动先画图」：动手前补**依赖图 + profile 装配数据流 + bots.json 迁移状态机**。
属用户/审美 + 后端结构双驱动，**先跟用户把 Q1–Q6 走一遍**再开工，不自主 blast（§0 不预付）。

**交叉引用**：§0（根定位=本节的理论依据）· §1.6（skill 市场=分发层类比，远端模板包共用其机件）·
§5.6（nail 栏布局/七彩身份色=本节 UI 落点）。

## 6. 完成项收敛规则

- 完成一个条目后，从本文件**删除**（不留 ✅ 存档块），事实摘要写入 `now.md`。
- 大段历史、验收细节、旧计划不要长期留在 `todo.md`；需要追溯时看 git log。
