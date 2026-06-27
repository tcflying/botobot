<div align="center">

# 🤖 botobot

**一个会自我唤醒、有长期记忆、断流自愈的纯 Rust AI Agent 框架**

*从神经网络到 shell 解释器,整条栈零 C 依赖 · 单二进制交付 · 协议全开放*

`candle` 本地推理 ・ `OpenAI 兼容` 任意后端 ・ `MCP / WebSocket / REST / SDK` 四路接入

</div>

---

```text
botobot = 通用 agent harness(harness 第一,场景第二)
  └─ coder bot      第一个、当前唯一的实装角色
        文件理解 · 抗漂移编辑 · 沙箱执行 · IDE 语义(LSP/DAP) · 多会话 · 长期记忆
     （researcher / operator / 多 bot 协作 = 已铺好接缝,按真实需求再落,绝不预付抽象）
```

> botobot 不是又一个 LangChain 包装层。它是一套**自己实现了算法与协议**的 Rust harness ——
> 借鉴 Codex / oh-my-pi 的思路,但不 fork 它们的身体。下面这些能力,大多数 agent 框架要么没有,要么只是 TODO。

---

## 💬 会发微信,就会用 botobot —— 零学习曲线

底层有多硬核,上手就有多简单。botobot 的交互**完全是一个聊天软件的样子**,没有命令行黑话、不用背 prompt 模板、不用懂什么是 agent —— **只要你用过微信 / QQ / Telegram,你已经会用它了**:

| 你在 IM 里的习惯 | 在 botobot 里就是 |
|------|------|
| 打开聊天框,打字发消息 | 打开网页,对着 bot 说人话,它就开干 |
| 发图片 | 直接拖图进去,多模态自动识别 |
| 联系人列表 | bot 列表 —— 每个 bot 是一个"人",有自己的人格和工作目录 |
| 拉群、@某人干活 | 建 Team、leader 自动拆活分给成员并行干、干完汇总回群 |
| 消息记录 | 会话自动落盘,关了再开接着聊;它还**记得你是谁、你的偏好** |
| 发完才想起说错 → 撤回补一句 | turn 跑到一半随时**插话改向**(steer),不用打断重来 |
| 群里设个提醒 | 让它"过 10 分钟 / 每天早上叫我",到点它**主动来找你** |
| 点一下「同意」 | 它要执行有风险的操作时,弹卡片让你点 **仅这次 / 本会话 / 永久 / 拒绝** —— 像授权一个 App |

**一句话:危险的判断它替你扛(沙箱式执行策略),复杂的编排它替你想(leader 自动拆活),你只管像聊天一样提需求。**

> 想更省事还有 `+` 号开 bot —— 先选模板(通用助手 ✨ / 编程 bot ⌨)再选目录,**点几下就有一个新助手上岗**,跟在 IM 里加好友一样自然。

---

## ✨ 这套 Agent 凭什么不一样

### 🧠 1. 一个真正像「记忆」的记忆系统(不是 RAG 套壳)

大多数 agent 的「记忆」= 把对话塞进向量库再 cosine 召回。botobot 的记忆系统借鉴认知科学,做了一整套机制:

- **召回是图,不是列表** —— `recall_graph` 返回 `{事实, 节点, 边}`:命中条目的关系(`from -rel-> to`)被解析成图,端点补全后让 **LLM 自己决定**要不要顺着 `read(memory://节点)` 深挖,而非 harness 预取全图。
- **1-hop 实体扩散召回(SAG)** —— 命中一条记忆后,自动捞出**共享实体**的其它 episode 作为"相关上下文"。纯余弦召回会漏掉的多事实关联,被实体链接救回(eval 实测:某查询召回从 1/3 → 3/3)。
- **置信度随时间衰减 + 用即回升** —— 旧记忆分数按半衰期(默认 30 天)指数衰减、足够旧就淡出;但**一旦被再次命中,时间戳刷新到当下,从此刻重新衰减**。常用的记得牢,不用的淡忘 —— 和人脑一个机理。
- **软取代,不硬删** —— 「我叫张三」→「我叫李四」会把旧事实标记 `superseded` 淡出召回,但**留在磁盘可审计**(旧记忆淡出而非抹除)。
- **钉住事实(pin)** —— 身份/偏好类事实逐字常驻、绕过淘汰,开口前就在上下文里。
- **巩固 pass(consolidation)** —— 够旧的一批 episode 被 LLM 合成一条紧凑 gist 手记,原文软取代留盘 —— 类比睡眠时的记忆固化。
- **Trail 导航捷径(「外脑」)** —— 把「请求意图 → 资源指针」学成低可信捷径。存的是**指针**(`skill://X#B`)不是答案,对不上就回退正常的渐进披露,爆炸半径极小,所以敢「低可信存、放心用」。还带一道相似度闸:够近才敢省导航。
- **派生式稳定 ID** —— entries 不可变,故条目 id 由 `hash(bank+content+ts)` **派生**而非持久化,零格式变更即获得稳定引用,支撑 `supersede-by-id` / `update-by-id` 精确修订。
- **事务式结构化写入** —— `memory_ops` 工具接受一批 `Create/Link/Supersede/Update` 操作,**先全量校验、任一非法整批拒绝**,再逐条落盘。
- **自带 eval 套件** —— `memory_eval.rs` 用真 bge-small-zh + 手编语料量 **recall@5** 与**「相似≠相关」陷阱规避率**,设基线门防退化(当前 recall@5=0.867、陷阱规避 100%)。改打分先过 eval,纪律落进代码。

> 模型升级也不失忆:每条记忆带 `model_id`,换嵌入模型时自动重嵌旧条目,召回只比同模型向量。

### ⏰ 2. 会自我唤醒的「心跳晶振」

agent 通常是**被动**的 —— 你不发消息它就睡死。botobot 有一颗进程级常驻的**晶振**(`spawn_heartbeat`,`tokio::interval` + Skip 防漂移),每个 tick 派发给一组 `TickHandler`:

- **cron handler** 是第一个真 handler:bot 可以调 `schedule_task("过 N 秒/每 N 秒再唤醒我")`,到点由心跳 `tokio::spawn` 一个新 turn 主动找你。
- 「主动发起 turn」这件事被抽象成驱动源 —— 未来的 ping-sweep、world 外部刺激都退化成它的一个 handler。

这是「定时任务」「自主 agent」「轮询外部世界」三件事的**统一底座**。

### 🔄 3. 断流自愈的流式推理

本地 LLM endpoint 会卡、会断、会吐畸形 SSE。botobot 把这些都当一等公民处理:

- **停滞流检测** —— `idle_timeout_stream` 包裹 SSE,单次等事件超时即产出可重试的 `LlmError::Idle`,把「静默卡死」转成「干净错误」。
- **流中途重放(mid-stream re-infer)** —— 长 turn 末尾断流不再丢整个 turn:发 `StreamReset` 事件(前端幂等清空本 run 已渲染的半截答案)后**重新推理**。
- **退避尊重 Retry-After** —— full-jitter 指数退避(用 nanos 当廉价随机源,不引 `rand`),429/503 的 `Retry-After` 头被尊重。
- **SSE 方言容错** —— 单条畸形 `data:` JSON 跳过 + warn 而非杀整流;自动识别 Anthropic/vLLM/Qwen 的 `input_tokens`/`thinking` 等字段别名。
- **优雅取消贯穿全栈** —— `CancellationToken` 串起 LLM 流 / 工具执行 / 子 agent / 后台任务;前端一键停止,`kill_on_drop` 当场杀进程,不必等长工具跑完。

### 💾 4. 进程崩溃也不丢 turn

借鉴 Codex 的 rollout 思路:一个 turn 内每条 finalized message 经 **history-delta 通道**(按 push 点逐条上抛,**天然免疫压缩造成的索引漂移**)增量写入 `turn-scratch.jsonl`。

- 干净收尾 → 并入 `messages.jsonl` 后清 scratch
- 取消/关停 → 丢弃 + 清 scratch
- **进程崩溃 → scratch 残留,下次加载先 `recover_scratch` 把残留并回再加载**

与正常提交/压缩路径完全解耦,零回归。

### 🗜️ 5. 无损的上下文压缩

窗口感知(soft 0.75W / hard 0.9W / tail 保护 0.4W),LLM 摘要优先,失败兜底 `prune → shake → window-drop` —— 关键是**三层折叠不销毁原文**:

- 原文先 spill 到 `artifact://id`,台面只留占位 `[~N tok | head:'…' | artifact://aN]`,`read(artifact://aN)` 无损取回
- **后台异步预摘要** —— 越过 soft 阈值就 `spawn` 预摘要,不阻塞当前 turn;越 hard 时 await 套用
- **工作集结转** —— 摘要头跨压缩累积记录「读过/改过哪些文件」,丢了原文也记得动过什么
- **BodyAfterPrefix 触发会计** —— 只比较「系统前缀之后的增长量」,既防抖动又保 provider 的前缀 KV 缓存

### 🛡️ 6. 沙箱式执行策略(杀伤力 × 越界,两维分级)

coder bot 默认的 `RuleTableExecPolicy` 不是白名单,而是按命令**杀伤力**和**路径越界**分级:

| 判定 | 触发条件 |
|------|---------|
| 🔴 **Deny** | `rm -rf` / `dd` / `mkfs` / fork bomb / 管道进 sh / `find -delete` —— **与路径无关,workdir 内也拒** |
| 🟡 **Prompt** | 触及 workdir 外(绝对路径 / `..` 逃逸 / `~`·`$HOME` 引用 / 不可分析的 `$()`)、`find -exec` 跑任意子命令 |
| 🟢 **Allow** | 仅在 workdir 内的相对路径、非破坏命令 —— **不再逐条问** |

配合四档审批(仅这次 / 本会话 / 永久 / 拒绝),永久决议持久化到 `.bot/approvals.json`;`forbidden` 段扫描覆盖管道/串联任意一段,basename 归一让 `/bin/rm -rf` 也拦得住。预留 `Sandbox` trait,未来换 OS 级隔离只改一处。

### 🦀 7. 纯 Rust 端到端 —— 真的没有 C 依赖

从神经网络到 shell,整条栈都是纯 Rust,换来**单二进制 + 跨平台行为一致 + 没有底层库链接的折磨**:

| 能力 | 选型 | 替代了什么 |
|------|------|-----------|
| 神经网络推理 / 文本嵌入 | `candle`(BERT, bge-small-zh) | onnxruntime / libtorch |
| 向量近邻检索 ANN | `instant-distance`(纯 Rust HNSW) | faiss / hnswlib |
| PDF 解读(分类+OCR+直出 MD) | `pdf-inspector` | poppler |
| Shell 解释器(可选) | `brush-core` | 系统 bash |
| 正则 | `fancy-regex` | oniguruma |

模型权重用 `include_bytes!` 编进二进制(+~46MB),**运行时零下载**。

### 🧩 8. 多 Agent 团队(把「卡死」教训固化成测试)

- **`ParticipantTracker`** —— `Done` / `Failed` / `Cancelled` **三类终结事件都递减**,把「只等 TurnDone 会卡死」的踩坑固化为单测
- **`TaskPlanner`** —— leader LLM 把任务拆成 per-member 子任务,`join_all` **并行**派发,未分配的兜底原任务
- **leader 自动汇总** —— 编排后 leader 把「完成 X · 失败 Y · 取消 Z」贴回 transcript
- 一行 `POST /api/teams/:id/conduct` 触发全流程

### 🎭 9. 正交派生的多人格 / 多模型

无需多 agent 重构,三个正交的派生方法叠着用:

```rust
agent.with_system(prompt)   // 只换角色 prompt(编程 SOP ↔ 通用助手)
     .with_workdir(path)    // 只换工作目录视图(多 bot 隔离)
     .with_llm(other_model) // 只换底层模型(真异构多模型)
```

切 profile **下一轮即生效**(每轮按 `bot.profile` 现取,无需重建/重启);设了 `BOTOBOT_GENERAL_MODEL` 才派生独立端点,默认共用、向后兼容零变化。

---

## 🧰 能力总览

| 维度 | 能力 |
|------|------|
| **推理** | OpenAI 兼容 · SSE 流式 · function calling · 多模态图像 · `<think>` 推理分流 · token 预算 · 413 自动去图重试 |
| **记忆** | 召回图 · 实体扩散 · 置信度衰减+回升 · pin · 软取代 · 巩固 · Trail 捷径 · 事务写入 · eval 套件 |
| **文件/编辑** | 统一 `read(url)` · hashline 抗漂移编辑 · `apply_patch` · `edit_by_hashline` · `rename_file`(LSP 感知) · 写后自动诊断 |
| **检索** | ignore-aware `search`/`find` · book 语义索引 · PageIndex 式推理检索 · `book_search` |
| **IDE 语义** | LSP(diagnostics/references/rename) · DAP(断点/单步/变量/数据断点) |
| **执行** | 沙箱式 shell · 后台长任务(`shell_background`/`job_status`) · 可选纯 Rust brush 内核(持久会话/Windows 路径翻译) |
| **浏览器** | Chrome CDP 调度 · 浏览/截图/点击/填充 · 页面投屏 |
| **文档** | PDF 解读 · Skill 程序性知识 · Book 权威资料 · officecli |
| **多 Agent** | 递归子 agent(editor/reader 专役) · Team 编排 · TaskPlanner 并行 · durable subsession 落盘 |
| **集成** | MCP server · cron 定时(心跳驱动) · Skill 市场 · bot 市场 |
| **可靠性** | 流恢复 · 崩溃恢复(turn-scratch) · 无损压缩 · 优雅取消 · HTTP 超时兜底 |

---

## 🏗️ 工作区结构

平铺 + 前缀分层,用 `base-*` / `model-*` / `agent-*` / `team-*` / `bot-*` / `webui-*` 表达层级。三个 slot 恒定:**infer · act · observe**。

| Crate | 职责 |
|-------|------|
| `base-types` | 共享契约层:`Llm`/`Tool`/`Observe`/`History`/`Policy`/`Sandbox` 等 trait,无实现 |
| `model-embed` | 本地中文嵌入(candle BERT + tokenizers),build.rs 内嵌权重,纯 Rust |
| `agent-infer` | LLM 推理:OpenAI 兼容 · SSE 流式 · tool-call 累积 · 重试 · 流恢复 |
| `agent-act` | 动作库:工具注册表 + 文件/shell/搜索/LSP/DAP/browser/PDF/记忆/后台 等叶子工具 |
| `agent-observe` | 观察:逐条回喂 / 长输出先摘要 |
| `agent-loop` | 驱动核心:reason → [compact] → act → observe 心跳循环 · exec policy · session 生命周期 |
| `team-core` | 多 bot 协作:Switchboard · ParticipantTracker · TaskPlanner · TeamOrchestrator |
| `bot-api` | 对外协议:Hub · WebSocket 传输 · 心跳晶振 · cron · 事件序列化 |
| `bot-sdk` | 进程内 Rust SDK:会话包装 · 事件收集 |
| `bot-mcp` | MCP 适配器:stdio JSON-RPC,暴露 `botobot` / `botobot-reply` 工具 |
| `webui-bin` | 工作台:`bots`(本地全权客户端) / `server`(远端只读站) + WebUI + 市场 |

---

## 🚀 快速开始

> 需要 Rust 1.85+(edition 2024)。

```bash
git clone https://github.com/tcflying/botobot.git
cd botobot

# 配置 LLM endpoint —— 任意 OpenAI 兼容后端(本地 endpoint 可不设 key)
export OPENAI_API_KEY=your-key
# export BOTOBOT_MODEL=...

# 构建本地完整工作台
cargo build -p webui-bin --features full --release

# 按需叠加重栈(默认构建零成本)
cargo build -p webui-bin --features full,browser,pdf,brush --release
```

### 常用环境变量

| 变量 | 作用 |
|------|------|
| `OPENAI_API_KEY` | LLM 鉴权(本地 endpoint 可空) |
| `BOTOBOT_MODEL` / `BOTOBOT_GENERAL_MODEL` | 编程 / 通用 bot 的模型(后者设了才派生独立端点) |
| `BOTOBOT_CONTEXT` | 真实模型窗口 tokens(默认 32768) |
| `BOTOBOT_TOKEN_BUDGET` | 单 turn token 预算 |
| `BOTOBOT_SHELL=brush` | 启用纯 Rust shell 内核(需 `brush` feature) |
| `BOTOBOT_STREAM_IDLE` / `BOTOBOT_RETRY` | 流停滞超时秒数 / 瞬时错误重试次数 |
| `BOTOBOT_MEMORY_DECAY` / `_HALF_LIFE_SECS` | 记忆置信度衰减开关 / 半衰期 |

优先级:env > `config.toml` > 内置默认。运行期落盘统一在 `.bot/`(sessions / memory / artifacts / skills / books / bin)。

---

## 🔌 对外接口

**WebSocket(`/ws`)** —— 双向事件流:
```jsonc
// → 服务器
{"type":"user_message","text":"...","images":[...],"force_recall":true}
{"type":"steer","text":"..."}            // turn 中途注入改向
{"type":"approval","approval_id":"...","decision":"session"}
{"type":"cancel"}
// ← 服务器:AgentEvent 序列
//   start · token · reasoning · tool_start · tool_end · usage · diagnostics
//   approval_request · stream_reset · done · error(子 agent 事件带 parent_id)
```

**REST** —— `/api/bots` · `/api/sessions/:id` · `/api/teams/:id/conduct` · `/api/cron` · `/api/skills` · `/api/skill-install` · `/api/resources?url=...` 等。

**MCP** —— `bots mcp` 启动 stdio JSON-RPC server,把 coder bot 作为可被 Claude 桌面版 / IDE 插件调用的能力,暴露 `botobot`(新对话)与 `botobot-reply`(继续线程)。

**Rust SDK** —— `BotSdk::open_session()` / `user_message()` / `submit(Submission)`,事件经 `broadcast::Receiver` 订阅。

---

## 🧭 设计哲学

> botobot 不是「调了几个 prompt 的 LLM 包装层」。它的架构同时落在三套思想坐标上 ——
> 卡尼曼的**《思考,快与慢》**给了它**双系统的心智**,布莱恩·阿瑟的**《技术的本质》**给了它**组合进化的骨架**,
> 而一条朴素的工程铁律 ——「**硬的沉淀,软的上浮,最软的是数据**」—— 给了它**按变化速率自我分层的地质结构**。
> 这三者不是事后贴上的标签,而是写进每一个 crate 边界、每一次 commit 取舍里的东西。

### Ⅰ. 思考,快与慢 —— agent 本就该是双系统

卡尼曼说人脑有两套系统:**System 1** 快、自动、联想式;**System 2** 慢、刻意、推理式。botobot 的内核 **不是把 LLM 当成唯一的大脑,而是把这两套系统都实现了出来**:

| | System 1(快·反射) | System 2(慢·推理) |
|---|---|---|
| **心智** | 心跳晶振主动发起 turn、记忆联想召回(cosine + 实体扩散)、Trail 捷径直奔资源、工具反射调用 | `reason → act → observe` 心跳循环、thinking 模式审议、对召回图「要不要再深挖」的主动决策、压缩摘要 |
| **特征** | 不耗 token、低延迟、可能出错就回退 | 耗 token、刻意、可审计、可纠错 |
| **回退** | Trail 捷径设了**相似度闸**:够近才敢省导航,不够近**自动降级到 System 2** 的渐进披露 | —— |

这就是为什么记忆召回**不是 harness 预取全图**,而是 System 1 给出联想线索、再**交给 System 2(LLM)决定是否顺着 `read(memory://节点)` 深挖** —— 快系统负责直觉,慢系统负责判断,和人脑同构。

### Ⅱ. 技术的本质 —— 组合进化,而非从零发明

阿瑟说:**一切新技术,都是已有技术的重新组合**;技术是递归的(每个组件本身又是一个技术),靠「捕获现象、驯化为可用」而演化。botobot 把这条原理当方法论:

- **「器官不 fork 身体」** —— 从 Codex / oh-my-pi **抄算法与协议**(rollout 崩溃恢复、execpolicy 沙箱、PageIndex 检索、artifact 外置……),但**自己用 Rust 重新实现、装进自己的器官**,绝不继承整个 crate 堆。这正是阿瑟说的「组合」而非「发明」。
- **递归组件** —— 11 个 crate 是可递归拆解的技术单元;`AgentTool` 让一个 agent 成为另一个 agent 的工具(技术嵌套技术),子 agent 共享 sink/cancel/budget 却各持独立历史。
- **恒定的「域」** —— `infer · act · observe` 三个 slot 是 botobot 的**核心域**,十八个月不变;一切新能力都是在这个域上的**重新组合**(新工具、新 profile、新 handler),而非新增 slot。心跳晶振更是把「定时/自主/轮询世界」三件事**捕获**成同一个 `TickHandler` 现象。

### Ⅲ. 硬的沉淀,软的上浮,最软的是数据 —— 按变化速率分层

这是 botobot 最硬的一条架构信仰:**一个系统应该按「各部分变化的速率」自然分层** —— 变得慢的往下沉淀成地基,变得快的往上浮成表皮,变得最快的干脆不写进代码、只当数据流动。

```text
  最软 · 每次交互都在变   ┌─────────────────────────────────────────┐
   ↑  数据(不是代码)     │  .bot/  记忆 · skill · book · artifact · 会话  │  ← 落盘、可重播种、删了能重建
   │                     ├─────────────────────────────────────────┤
   │  软 · 随场景调       │  prompt · profile · policy preset · 角色配置  │  ← with_system/with_llm 一行派生,下一轮即生效
   │                     ├─────────────────────────────────────────┤
   ↓  硬 · 极少动         │  base-types 契约 · 心跳内核 · 三 slot · 协议   │  ← 编译期保证,trait 锁死,改一处全栈受益
  最硬 · 沉淀成地基       └─────────────────────────────────────────┘
```

- **硬的沉淀** —— `Llm`/`Tool`/`Observe`/`Policy`/`Sandbox` 是纯 trait 契约,沉到 `base-types`,用**编译期**把不变量焊死;Driver 只有单一规范实现,**不为「将来可能」预付抽象**。它们一年动不了几次,所以配得上「地基」。
- **软的上浮** —— 人格、模型、策略这些**随场景而变**的东西,被 `with_system` / `with_llm` / `with_workdir` 三个**正交派生**方法托到表层,切换**下一轮即生效**,无需重建、无需重启。软的就该轻、该浮、该可热换。
- **最软是数据** —— 记忆、skill、book、artifact 变得最快,于是它们**根本不是代码,而是 `.bot/` 里的数据**:可落盘、可审计、可从仓库基线**重播种**、删掉能重建。记忆甚至自带衰减与巩固 —— 让最软的那层**像地质沉积一样自己新陈代谢**,而不靠人去 GC。

> 一句话:**botobot 把「变化速率」当成第一性的分层维度**。硬的用 Rust 类型沉到底,软的用派生浮到顶,最软的化成数据在 `.bot/` 里流动。这让它既稳(地基不晃)又活(表皮可换)。

### 落到地面的工程铁律

抽象之外,这套哲学每天靠几条朴素纪律兜底:

1. **框架第一,coding 第一用户** —— 通用 harness,coder bot 是第一套角色配置,不预付未触发的场景。
2. **只做必要的抽象** —— `Policy`/`Sandbox`/`ToolLookup` 必需时才上 trait,留接缝、不留半成品。
3. **观察驱动取代预付** —— 不预付未触发的能力,等真实痛点出现再重构。
4. **eval 先行** —— 改记忆打分先过 eval 套件,基线门写进纪律,防止「优化」变退化。

---

## 📄 许可

MIT
