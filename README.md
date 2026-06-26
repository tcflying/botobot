# botobot

> 一个**纯 Rust** 的通用 AI Agent 框架（harness）—— 端到端无 C 依赖、单二进制交付、协议开放、特性可选装。第一个、也是当前唯一的实装角色是 **coder bot**:文件理解 / 检索 / 安全执行 / 可审计编辑 / IDE 语义 / 多会话。

```text
botobot = 通用 agent harness
  └─ coder bot      第一个、当前唯一的 bot
        文件理解 / 检索 / 安全执行 / 可审计编辑 / IDE 语义 / 多 session
     （researcher / operator / 多 bot 协作 = 后续，按真实需求再落，不预付抽象）
```

---

## 为什么是 botobot —— 核心优势

### 1. 纯 Rust,端到端无 C 依赖
从神经网络推理到 shell 解释器,整条栈都是纯 Rust,没有 oniguruma / onnxruntime / poppler 这类 C 库要链接:

| 能力 | 选型 | 守的约束 |
|------|------|---------|
| 文本嵌入 / 神经网络推理 | `candle`(BERT, bge-small-zh-v1.5) | 无 C |
| 向量近邻检索 ANN | `instant-distance`(纯 Rust HNSW) | 无 C |
| PDF 解读 | `pdf-inspector` | 无 C |
| Shell 解释器(可选) | `brush-core` | 无 C |
| 正则 | `fancy-regex` | 非 oniguruma |

**结果**:单二进制交付、跨平台行为一致、没有底层库链接的折磨。

### 2. Feature 分层,零预付成本
默认构建轻量,重栈全部 opt-in:

- `full` = 本地完整工作台(chat + embed + browser + team + mcp)
- `chat` = 远端纯对话(无 ML / 团队栈)
- 细粒度开关:`browser` / `pdf` / `hnsw` / `brush` / `officecli` 各自独立
- 嵌入式模型权重用 `include_bytes!` 编进二进制,**运行时零下载**

### 3. 无厂商锁定 —— OpenAI 兼容 + 开放协议
- LLM 后端任选:**任何 OpenAI-compatible endpoint**(vLLM / unsloth / llama.cpp / 本地 Qwen·Llama),默认指向本地 Qwen
- 三个可替换契约 trait:`Llm` / `Tool` / `Observe`,各自独立换实现
- 对外协议全是标准件:**WebSocket** + **REST** + **MCP** + **Rust SDK**,无私有协议
- SSE 解析方言容错:自动识别 Anthropic / vLLM / Qwen 的 usage 字段与 `reasoning`/`thinking` 别名

### 4. 健壮的流式推理
- Token 级 SSE 流式输出,工具调用增量累积(`Accumulator`)
- `<think>...</think>` 推理内容跨分片自动分流
- **流恢复**:停滞流检测(idle timeout)、中途断流自动重放(mid-stream re-infer)、瞬时错误退避重试(full jitter + 尊重 `Retry-After`)
- 优雅取消:`CancellationToken` 贯穿 LLM 流 / 工具执行 / 后台任务,前端一键停止当场杀进程

### 5. 分层精细的执行安全
coder bot 默认走**沙箱式 exec policy**(`RuleTableExecPolicy`),按命令杀伤力 + 路径越界分级:

- 🔴 **破坏性命令**(`rm -rf` / `dd` / `mkfs` / fork bomb / 管道进 sh)→ **Deny**,与路径无关
- 🟡 **触及 workdir 外**(绝对路径 / `..` 逃逸 / `~`·`$HOME` 引用 / 不可分析的 `$()`)→ **Prompt**
- 🟢 **仅在 workdir 内**(相对路径、非破坏)→ **Allow**,不再逐条问

配套四档审批(仅这次 / 本会话 / 永久 / 拒绝),永久决议持久化到 `.bot/approvals.json`;预留 `Sandbox` trait 接缝,未来可换 OS 级隔离。

### 6. 可回溯的上下文压缩
默认开启、窗口感知(soft 0.75W / hard 0.9W / tail 保护 0.4W)的压缩,**LLM 摘要优先**,失败兜底 `prune → shake → window-drop`:

- 三层折叠**无损**:原文先 spill 到 `artifact://`,台面留占位,`read(artifact://id)` 可取回
- 后台异步预摘要(越 soft 即 spawn,不阻塞当前 turn)
- 工作集结转:摘要头跨压缩累积记录读过/改过哪些文件

---

## 能力总览

| 维度 | 能力 |
|------|------|
| **推理** | OpenAI 兼容、SSE 流式、function calling、多模态图像、推理内容分流、token 预算 |
| **记忆** | 语义向量召回(可选 HNSW)、跨会话持久化、pin 钉住事实、episode 自动记录、可信度分档 |
| **文件 / 编辑** | 统一 `read(url)`、hashline 抗漂移编辑、`apply_patch`、`edit_by_hashline`、`rename_file`(LSP 感知) |
| **检索** | ignore-aware 的 `search` / `find`,结构化输出,路径不逃逸 workdir |
| **IDE 语义** | LSP(diagnostics / references / rename)、DAP 调试(断点 / 单步 / 变量)、写后自动诊断 |
| **执行** | 沙箱式 shell、后台长任务(`shell_background` / `job_status`)、可选纯 Rust brush 内核 |
| **浏览器** | Chrome CDP 调度、浏览 / 截图 / 点击 / 填充、页面投屏(feature gated) |
| **文档** | PDF 解读(分类 + OCR + 直出 Markdown)、Skill 程序性知识、Book 权威资料 |
| **多 Agent** | `AgentTool` 递归子 agent(editor / reader 专役)、Team 编排、TaskPlanner 并行 |
| **集成** | MCP server(暴露给 Claude 桌面版等)、cron 定时任务、Skill 市场 |
| **Web UI** | 实时事件流、Bot 市场、多 profile、四档执行审批 |

---

## 工作区结构

平铺 + 前缀分层,用 `base-*` / `agent-*` / `bot-*` / `webui-*` 前缀表达层级:

| Crate | 职责 |
|-------|------|
| `base-types` | 共享契约层:`Llm` / `Tool` / `Observe` / `History` / `Policy` 等 trait,无实现 |
| `model-embed` | 本地中文文本嵌入(candle BERT + tokenizers),纯 Rust 自包含 |
| `agent-infer` | LLM 推理实现:OpenAI 兼容、SSE 流式、tool-call 累积、重试 / 流恢复 |
| `agent-act` | 动作库:工具注册表 + 文件 / shell / 搜索 / LSP / DAP / browser / PDF / 记忆 等叶子工具 |
| `agent-observe` | 观察实现:逐条回喂 / 长输出先摘要 |
| `agent-loop` | 驱动核心:reason → [compact] → act → observe 心跳循环、exec policy、session 生命周期 |
| `team-core` | 多 bot 协作层:Switchboard、Team / Bot 注册表、TaskPlanner 编排 |
| `bot-api` | 对外协议层:Hub 生命周期、WebSocket 传输、事件序列化、cron / team 工具注入 |
| `bot-sdk` | 进程内 Rust SDK:会话包装、事件收集、便利接口 |
| `bot-mcp` | MCP 适配器:stdio JSON-RPC server,暴露 `botobot` / `botobot-reply` 工具 |
| `webui-bin` | 工作台与市场:`bots`(本地全权客户端) / `server`(远端只读站) |

---

## 快速开始

> 需要 Rust 1.85+(edition 2024)。

```bash
# 克隆
git clone https://github.com/tcflying/botobot.git
cd botobot

# 配置 LLM endpoint(任意 OpenAI 兼容后端)
export OPENAI_API_KEY=your-key            # 或本地 endpoint 无需 key
# export BOTOBOT_MODEL=...                # 可选,覆盖默认模型

# 构建本地完整工作台(bots)
cargo build -p webui-bin --features full --release

# 想要更多能力,叠加 feature:
cargo build -p webui-bin --features full,browser,pdf,brush --release
```

### 常用环境变量

| 变量 | 作用 |
|------|------|
| `OPENAI_API_KEY` | LLM 鉴权(本地 endpoint 可留空) |
| `BOTOBOT_MODEL` | 覆盖默认模型 |
| `BOTOBOT_CONTEXT` | 真实模型窗口 tokens(默认 32768) |
| `BOTOBOT_TOKEN_BUDGET` | 单 turn token 预算 |
| `BOTOBOT_SHELL=brush` | 启用纯 Rust shell 内核(需 `brush` feature) |
| `BOTOBOT_RETRY` | LLM 瞬时错误重试次数(默认 2,`0` 关) |

也可用 `config.toml` 配置 LLM 条目、上下文窗口等;优先级:env > `config.toml` > 内置默认。

---

## 对外接口

**WebSocket(`/ws`)** —— 浏览器/客户端 ↔ agent,双向事件流:
```jsonc
// → 服务器
{"type":"user_message","text":"...","images":[...]}
{"type":"steer","text":"..."}
{"type":"approval","approval_id":"...","decision":"session"}
{"type":"cancel"}
// ← 服务器:AgentEvent 序列(start / token / reasoning / tool_start / tool_end / done / error)
```

**REST** —— `/api/bots`、`/api/sessions/:id`、`/api/teams/:id/conduct`、`/api/skills`、`/api/skill-install`、`/api/resources?url=...` 等。

**MCP** —— stdio JSON-RPC server,暴露 `botobot`(新对话)与 `botobot-reply`(继续线程)工具给 Claude 桌面版 / IDE 插件。

**Rust SDK** —— `BotSdk::open_session()` / `user_message()` / `submit(Submission)`,事件经 `broadcast::Receiver` 订阅。

---

## 设计哲学

1. **框架第一,coding 第一用户** —— 通用 harness,coder bot 是第一套角色配置,不预付未触发的场景。
2. **抄算法+协议,不 fork 身体** —— 借鉴 Codex / oh-my-pi 的算法与协议,自己实现,不继承整个 crate 堆。
3. **纯 Rust 一致性** —— 消灭 C 依赖,换单二进制与跨平台一致。
4. **只做必要的抽象** —— Policy / Sandbox / ToolLookup 必需时才上 trait,Driver 单一规范实现不泛化。
5. **观察驱动取代预付** —— 不预付 world / multi-bot 等未触发能力,等真实痛点再重构。

---

## 许可

MIT
