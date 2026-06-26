// botobot webui — 零依赖 vanilla JS + marked（已 vendor）。
// 消费新版 AgentEvent 协议（嵌套靠 parent_id）。
// 渲染管线：WS → 累加器 → rAF 节流 → marked.parse → 意图感知滚动。
//
// 关键招（抄自 unsloth_frontend 的 markdown-text.tsx + use-intent-aware-autoscroll）：
//   - rAF 单帧合并：LLM token 速率 >> 60fps，直接 setState 会卡。
//   - 未闭合 fence 保护：流式期间 ``` 是常态不是错误，必须补全才能 parse。
//   - monotonic startsWith：只有"新文本是旧文本前缀"才接受节流后的结果，否则直传。
//   - 流式 caret：bubble 末尾的光标动画，done 事件时移除。
//   - 意图感知滚动：wheel↑ 立即 detach，滑回 24px 内 reattach + 延长 follow 窗口。
"use strict";

const WS_URL = `ws://${location.host}/ws`;
// [启动诊断] app.js 何时开始执行（它在 body 末尾、排在 269KB tailwind.js 之后；
// 若这个数很大，说明 app.js 被前面的大脚本拖住了）。
console.log(`[app] app.js 开始执行 @ ${Math.round(performance.now())}ms`);
const $ = (id) => document.getElementById(id);
const statusPill = $("status");

// 量出 header 真实高度并写到 --header-h CSS 变量,让抽屉能精准地从 header
// 下方开始(top: var(--header-h) + height: calc(100vh - var(--header-h)))。
// resize / DPR 变化 / 字体加载完成后都要重新量,否则抽屉会盖住 header 或留缝。
function measureHeader() {
  const header = document.querySelector("header");
  if (!header) return;
  document.documentElement.style.setProperty("--header-h", header.offsetHeight + "px");
}
measureHeader();
if (document.fonts && document.fonts.ready) document.fonts.ready.then(measureHeader);
window.addEventListener("resize", measureHeader);
new ResizeObserver(measureHeader).observe(document.querySelector("header"));
const statusPillText = statusPill.querySelector(".status-pill-text");
const transcript = $("transcript");
const sessionList = $("session-list");
const newSessionBtn = $("new-session");
const input = $("input");
const form = $("composer");
const sendBtn = $("send");
const attachmentsEl = $("attachments");
const pendingSteersEl = $("pending-steers");
const dropzone = document.querySelector(".aui-composer-attachment-dropzone");
const addAttachmentBtn = $("add-attachment");
const fileInput = $("file-input");
const logsToggleBtn = $("logs-toggle");
const logsDrawer = $("logs");
const logsList = $("logs-list");
const logsClearBtn = $("logs-clear");
const logsClearInline = $("logs-clear-inline");
const logsMeta = $("logs-meta");
const logsEmpty = $("logs-empty");
const logsTruncated = $("logs-truncated");
const canvasToggleBtn = $("canvas-toggle");
const canvasCloseBtn = $("canvas-close");
const detailToggleBtn = $("detail-toggle");
const canvasPanel = $("canvas");
const canvasTitle = $("canvas-title");
const canvasBody = $("canvas-body");
const nailRail = $("nail");
const defaultBotBtn = $("default-bot");
const addBotBtn = $("add-bot");
const nailWorkdir = document.querySelector(".nail-workdir");
// §1.6 S5 技能市场面板
const marketToggleBtn = $("market-toggle");
const marketCloseBtn = $("market-close");
const marketRefreshBtn = $("market-refresh");
const marketPanel = $("market");
const marketSourceSel = $("market-source");
const marketSourceName = $("market-source-name");
const marketSourceUrl = $("market-source-url");
const marketAddSourceBtn = $("market-add-source");
const marketList = $("market-list");
const marketEmpty = $("market-empty");

// ── marked 配置 ──────────────────────────────────────────────
if (window.marked) {
  window.marked.setOptions({ breaks: true, gfm: true });
}

let ws = null;
let thinkEnabled = false; // Think pill 状态,下次发 user_message/steer 时携带
let searchEnabled = false; // Search pill 状态,本 turn 暴露 web_search 工具
let codeEnabled = false; // Code pill 状态,本 turn 暴露 shell/code/http 工具
let recallEnabled = true; // §1.8.3b 记忆 pill 默认开(仍可关):每 turn 按 query 检索记忆图 + skill/book 能力提示并增广
let logsOpen = true;           // 抽屉开/关状态
let logsBootMode = true;
let logsBootTimer = null;      // §5.5 D5：日志面板自动收起的超时兜底句柄
let canvasOpen = false;
let canvasRequestId = 0;
const DEFAULT_BOT_ID = "bot-default";
const bots = new Map(); // bot_id -> { id,name,workdir,button,sessionIds,activeSessionId }
const sessions = new Map(); // session_id -> { id,title,pane,item,runs,topRun,busy,userDetached }
const deletedSessionIds = new Set();
let activeBotId = DEFAULT_BOT_ID;
let activeSessionId = null;
// §5.6 群聊：nail 栏的 team（群）。activeTeamId 非空 = 处于群聊视图（隐藏 session transcript）。
// 注：这是 IM 化的务实第一版（mode 标志），S1 的 activeSubject 统一重构留后续。
const teams = new Map();       // team_id -> { team(snapshot), button }
let activeTeamId = null;
let botCounter = 1;
let sessionCounter = 1;
let attachments = []; // [{name, dataUrl}]

function setStatus(state, text) {
  // state: "connecting" | "on" | "off";text 可选,缺省走内置文案。
  const label = text ?? (
    state === "on" ? "已连接" :
    state === "off" ? "未连接" :
    "连接中"
  );
  statusPill.dataset.state = state;
  statusPillText.textContent = label;
}

// §5.5 D5：WS 句柄绑定去重——onclose/onerror/onmessage 三者两处完全一致，提取到此，
// onopen 因上下文不同（普通连接 vs bootstrap）由调用方传入。
function bindWsHandlers(socket, onOpen) {
  socket.onopen = onOpen;
  socket.onclose = () => {
    setStatus("off");
    for (const session of sessions.values()) setBusy(false, session);
    console.info("[ws] close");
    setTimeout(connect, 1500);
  };
  socket.onerror = () => {
    setStatus("off");
    console.warn("[ws] error");
  };
  socket.onmessage = (e) => handleWsData(e.data);
}

function connect() {
  setStatus("connecting");
  ws = new WebSocket(WS_URL);
  bindWsHandlers(ws, () => {
    setStatus("on");
    console.info(`[ws] open @ ${Math.round(performance.now())}ms`);
    subscribeKnownSessions();
  });
}

function handleWsData(data) {
  try {
    const ev = JSON.parse(data);
    if (ev.type === "log") {
      appendLog(ev);
      return;
    }
    if (ev.type === "log_snapshot_done") {
      if (logsBootMode) {
        logsBootMode = false;
        if (logsBootTimer) { clearTimeout(logsBootTimer); logsBootTimer = null; }
        setTimeout(() => setLogsOpen(false), 900);
      }
      return;
    }
    // A2（§5.5 详细度无损显隐）：debug 事件不再丢弃，渲染为 data-lv="debug" 行，
    // 默认 CSS 隐藏，切「详细」即显——零往返、零重渲染、不丢历史。
    if ((ev.level || "info") === "debug" || ev.type === "debug") {
      console.debug(`[ws:debug] ${ev.type}:${ev.label || ""}`, ev.data);
    } else {
      (ev.type === "error" ? console.error : console.info)(`[ws] ${ev.type}`, ev.run_id || "");
    }
    const eventSessionId = ev.session_id || activeSessionId;
    if (eventSessionId && deletedSessionIds.has(eventSessionId)) return;
    const session = ensureSession(eventSessionId);
    if (eventSessionId) session.serverKnown = true;
    handle(ev, session);
  } catch (_) {}
}

function attachBootWsHandlers(socket) {
  ws = socket;
  bindWsHandlers(ws, () => {
    setStatus("on");
    console.info(`[ws] open @ ${Math.round(performance.now())}ms`);
    subscribeKnownSessions();
  });
}

const connectWithoutBootstrap = connect;
connect = function connectWithBootstrap() {
  setStatus("connecting");
  const boot = window.__botobotWsBoot;
  if (boot && boot.ws && (
    boot.ws.readyState === WebSocket.CONNECTING ||
    boot.ws.readyState === WebSocket.OPEN
  )) {
    attachBootWsHandlers(boot.ws);
    if (ws.readyState === WebSocket.OPEN) {
      setStatus("on");
      subscribeKnownSessions();
    }
    const queued = boot.queue.splice(0);
    for (const data of queued) handleWsData(data);
    window.__botobotWsBoot = null;
    return;
  }
  connectWithoutBootstrap();
};

// ── 日志面板 ────────────────────────────────────────────────
// 单条日志上限:防止调试时(尤其 BOTOBOT_LOG=debug)日志洪泛把 DOM 卡死。
// 超出上限丢老的(性能优先,日志是辅助视图)。
const LOGS_MAX = 500;
const seenLogSeq = new Set();

function setLogsOpen(open) {
  logsOpen = open;
  document.body.classList.toggle("logs-open", open);
  logsToggleBtn.setAttribute("aria-expanded", String(open));
  logsDrawer.setAttribute("aria-hidden", String(!open));
  // 服务端对每个 WS 连接自动订阅日志（SubscribeLogs 实为 no-op），无需客户端再发订阅。
}

function setCanvasOpen(open) {
  canvasOpen = open;
  document.body.classList.toggle("canvas-open", open);
  if (!open) {
    // 关画布 → 退出投屏布局 + 断帧流（若在投屏）。
    document.body.classList.remove("browser-mode");
    if (typeof browserWs !== "undefined" && browserWs) { try { browserWs.close(); } catch (_) { /* 忽略 */ } browserWs = null; }
  }
  if (canvasToggleBtn) canvasToggleBtn.setAttribute("aria-expanded", String(open));
  if (canvasPanel) canvasPanel.setAttribute("aria-hidden", String(!open));
  if (typeof renderBooters === "function") renderBooters(); // §5.6 画布 booter
}

// ── §1.6 S5 技能市场 ─────────────────────────────────────────
// 空选 = 本地已装（GET /api/skills，可删）；选某远端源 = 拉 catalog（可装/更新）。
let marketOpen = false;

function setMarketOpen(open) {
  marketOpen = open;
  document.body.classList.toggle("market-open", open);
  if (marketToggleBtn) marketToggleBtn.setAttribute("aria-expanded", String(open));
  if (marketPanel) marketPanel.setAttribute("aria-hidden", String(!open));
  if (open) refreshMarket();
}

async function loadMarketSources() {
  try {
    const res = await fetch("/api/market/sources");
    if (!res.ok) return;
    const sources = await res.json();
    const cur = marketSourceSel.value;
    marketSourceSel.innerHTML = '<option value="">本地已装</option>';
    for (const s of sources) {
      const opt = document.createElement("option");
      opt.value = s.url;
      opt.textContent = `${s.name} (${s.url})`;
      marketSourceSel.appendChild(opt);
    }
    if (cur) marketSourceSel.value = cur;
  } catch (_) { /* 网络异常静默，列表保持 */ }
}

async function refreshMarket() {
  await loadMarketSources();
  const source = marketSourceSel.value;
  let entries = [];
  try {
    if (source) {
      const res = await fetch(`/api/market/catalog?source=${encodeURIComponent(source)}`);
      entries = res.ok ? await res.json() : [];
    } else {
      const res = await fetch("/api/skills");
      const local = res.ok ? await res.json() : [];
      // 本地视图：统一成 catalog 形状（installed=true，无 update）。
      entries = local.map((d) => ({ ...d, installed: true, update_available: false }));
    }
  } catch (_) { entries = []; }
  renderMarket(entries, source);
}

function renderMarket(entries, source) {
  marketList.innerHTML = "";
  const visible = entries.filter((e) => !e.hidden);
  marketEmpty.hidden = visible.length > 0;
  for (const e of visible) {
    const item = el("div", "market-item");
    item.setAttribute("role", "listitem");

    const top = el("div", "market-item-top");
    top.appendChild(el("span", "market-item-id", e.id));
    if (e.installed) top.appendChild(el("span", "market-badge market-badge--installed", "已装"));
    if (e.update_available) top.appendChild(el("span", "market-badge market-badge--update", "可更新"));
    item.appendChild(top);

    if (e.description) item.appendChild(el("div", "market-item-desc", e.description));

    const actions = el("div", "market-item-actions");
    if (source) {
      // 远端源视图：装 / 更新。
      const label = e.installed ? (e.update_available ? "更新" : "重装") : "安装";
      const installBtn = el("button", "md-btn md-btn--filled-tonal", label);
      installBtn.addEventListener("click", () => marketInstall(source, e.id, installBtn));
      actions.appendChild(installBtn);
    }
    if (e.installed) {
      const delBtn = el("button", "md-btn md-btn--panel", "删除");
      delBtn.addEventListener("click", () => marketDelete(e.id, delBtn));
      actions.appendChild(delBtn);
    }
    item.appendChild(actions);
    marketList.appendChild(item);
  }
}

async function marketInstall(source, id, btn) {
  if (btn) btn.disabled = true;
  try {
    const res = await fetch("/api/market/install", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ source, id }),
    });
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      alert(`安装失败：${err.error || res.status}`);
    }
  } catch (e) {
    alert(`安装失败：${e}`);
  } finally {
    refreshMarket();
  }
}

async function marketDelete(id, btn) {
  if (!confirm(`删除本地 skill「${id}」？(市场包/用户编辑彻底移除；影子内嵌则回退出厂)`)) return;
  if (btn) btn.disabled = true;
  try {
    await fetch(`/api/skills/${encodeURIComponent(id)}`, { method: "DELETE" });
  } catch (_) { /* 忽略，刷新反映真实状态 */ }
  refreshMarket();
}

async function marketAddSource() {
  const name = marketSourceName.value.trim();
  const url = marketSourceUrl.value.trim();
  if (!name || !url) { alert("源名和地址都要填"); return; }
  try {
    const res = await fetch("/api/market/sources", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name, url }),
    });
    if (res.ok) {
      marketSourceName.value = "";
      marketSourceUrl.value = "";
      await loadMarketSources();
      marketSourceSel.value = url;
      refreshMarket();
    }
  } catch (e) { alert(`加源失败：${e}`); }
}

const RESOURCE_URL_RE = /\b(?:artifact:\/\/[A-Za-z0-9._:-]+|blob:sha256:[a-fA-F0-9]{64})\b/g;

function isResourceContainerSkipped(node) {
  let el = node.parentElement;
  while (el) {
    if (["A", "BUTTON", "CODE", "PRE", "SCRIPT", "STYLE", "TEXTAREA"].includes(el.tagName)) return true;
    if (el.classList && el.classList.contains("canvas-resource-ref")) return true;
    el = el.parentElement;
  }
  return false;
}

function decorateResourceRefs(root) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const nodes = [];
  while (walker.nextNode()) {
    const node = walker.currentNode;
    if (!RESOURCE_URL_RE.test(node.nodeValue) || isResourceContainerSkipped(node)) {
      RESOURCE_URL_RE.lastIndex = 0;
      continue;
    }
    RESOURCE_URL_RE.lastIndex = 0;
    nodes.push(node);
  }
  for (const node of nodes) {
    const text = node.nodeValue;
    const fragment = document.createDocumentFragment();
    let last = 0;
    for (const match of text.matchAll(RESOURCE_URL_RE)) {
      const url = match[0];
      if (match.index > last) fragment.appendChild(document.createTextNode(text.slice(last, match.index)));
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "canvas-resource-ref";
      btn.textContent = url;
      btn.title = url;
      btn.addEventListener("click", () => openCanvasResource(url));
      fragment.appendChild(btn);
      last = match.index + url.length;
    }
    if (last < text.length) fragment.appendChild(document.createTextNode(text.slice(last)));
    node.parentNode.replaceChild(fragment, node);
  }
}

function canvasEmpty() {
  if (!canvasBody) return;
  if (canvasTitle) canvasTitle.textContent = "画布";
  canvasBody.innerHTML = `
    <div class="canvas-empty">
      <div class="canvas-empty-glyph" aria-hidden="true">
        <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
          <path d="M4 5h16"></path>
          <path d="M4 12h16"></path>
          <path d="M4 19h10"></path>
        </svg>
      </div>
      <div class="canvas-empty-title">暂无画布内容</div>
      <div class="canvas-empty-hint">尚未选择资源。</div>
    </div>`;
}

// 内容是否是可直接显示的图片（data:image 或图片 url）——决定画布渲 <img> 还是 <pre>。
function imageSrc(s) {
  if (!s || typeof s !== "string") return null;
  const t = s.trim();
  if (/^data:image\//.test(t)) return t;
  if (/^https?:\/\/\S+\.(png|jpe?g|gif|webp|svg)(\?\S*)?$/i.test(t)) return t;
  return null;
}

// ═══════════ §5.6 C10 浏览器投屏（canvas 镜像） ═══════════
// 专用 /browser-ws：服务端推二进制 JPEG 帧 → createImageBitmap 画到 <canvas>；
// 入站文本 {type:navigate,url} 控制导航。view-only（双向控制鼠标/键盘待 stage3）。
let browserWs = null;
function closeBrowserMirror() {
  if (browserWs) { try { browserWs.close(); } catch (_) { /* 忽略 */ } browserWs = null; }
  document.body.classList.remove("browser-mode"); // 退出投屏布局
}
function openBrowserMirror() {
  closeBrowserMirror();
  setCanvasOpen(true);
  document.body.classList.add("browser-mode"); // 投屏布局：隐会话栏、压对话栏、画布最大
  if (canvasTitle) canvasTitle.textContent = "浏览器投屏";
  if (!canvasBody) return;
  canvasBody.innerHTML = "";
  const bar = el("div", "bm-bar");
  const navBtn = (label, type, title) => {
    const b = el("button", "bm-nav", label); b.type = "button"; b.title = title;
    b.addEventListener("click", () => { if (browserWs && browserWs.readyState === 1) browserWs.send(JSON.stringify({ type })); });
    return b;
  };
  bar.appendChild(navBtn("◀", "back", "后退"));
  bar.appendChild(navBtn("▶", "forward", "前进"));
  bar.appendChild(navBtn("⟳", "reload", "刷新"));
  const url = el("input", "bm-url"); url.type = "text"; url.placeholder = "输入网址回车导航（默认 about:blank）…";
  bar.appendChild(url);
  const stage = el("div", "bm-stage");
  const cvs = document.createElement("canvas"); cvs.className = "bm-canvas";
  cvs.tabIndex = 0; // 可聚焦才能收键盘
  const status = el("div", "bm-status", "连接中…");
  stage.appendChild(cvs); stage.appendChild(status);
  canvasBody.append(bar, stage);
  // §5.6 只读/可控开关：默认可控（转发输入）；只读时纯看，防误点（看长页面/等加载时）。
  let controllable = true;
  const modeBtn = el("button", "bm-nav bm-mode", "🖱"); modeBtn.type = "button"; modeBtn.title = "可控（点击切只读）";
  modeBtn.addEventListener("click", () => {
    controllable = !controllable;
    modeBtn.textContent = controllable ? "🖱" : "👁";
    modeBtn.title = controllable ? "可控（点击切只读）" : "只读（点击切可控）";
    cvs.classList.toggle("bm-readonly", !controllable);
  });
  bar.appendChild(modeBtn);
  const ctx = cvs.getContext("2d");
  // §5.6 stage3 双向控制：最近一帧的设备尺寸（坐标换算）+ 按下的鼠标键位掩码。
  let meta = { deviceWidth: 0, deviceHeight: 0 };
  let downBtns = 0;
  const MOUSE_BTN = ["left", "middle", "right"];
  const send = (o) => { if (browserWs && browserWs.readyState === 1) browserWs.send(JSON.stringify(o)); };
  const sendInput = (o) => { if (controllable) send(o); }; // 只读模式不转发输入
  const cdpMods = (e) => (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
  // canvas 像素 → 页面 CSS 像素（归一化 × 设备宽高，自动消化下采样）。
  const pagePos = (e) => {
    const r = cvs.getBoundingClientRect();
    const nx = r.width ? (e.clientX - r.left) / r.width : 0;
    const ny = r.height ? (e.clientY - r.top) / r.height : 0;
    return { x: nx * (meta.deviceWidth || cvs.width), y: ny * (meta.deviceHeight || cvs.height) };
  };
  const sendNav = () => {
    const u = url.value.trim();
    if (u && browserWs && browserWs.readyState === 1) {
      send({ type: "navigate", url: /^https?:\/\//.test(u) ? u : "https://" + u });
      status.style.display = ""; status.textContent = "导航中…";
    }
  };
  url.addEventListener("keydown", (e) => { if (e.key === "Enter") sendNav(); });

  // ── 鼠标：按下/抬起/移动/滚轮/右键 ──
  cvs.addEventListener("mousedown", (e) => {
    e.preventDefault(); cvs.focus(); downBtns |= (1 << e.button);
    const p = pagePos(e);
    sendInput({ type: "mouse", kind: "mousePressed", x: p.x, y: p.y, button: MOUSE_BTN[e.button] || "left", buttons: downBtns, clickCount: e.detail || 1, modifiers: cdpMods(e) });
  });
  cvs.addEventListener("mouseup", (e) => {
    e.preventDefault(); downBtns &= ~(1 << e.button);
    const p = pagePos(e);
    sendInput({ type: "mouse", kind: "mouseReleased", x: p.x, y: p.y, button: MOUSE_BTN[e.button] || "left", buttons: downBtns, clickCount: e.detail || 1, modifiers: cdpMods(e) });
  });
  let lastMove = 0;
  cvs.addEventListener("mousemove", (e) => {
    const now = performance.now();
    if (downBtns === 0 && now - lastMove < 40) return; // 悬停节流 ~25fps
    lastMove = now;
    const p = pagePos(e);
    sendInput({ type: "mouse", kind: "mouseMoved", x: p.x, y: p.y, button: "none", buttons: downBtns, clickCount: 0, modifiers: cdpMods(e) });
  });
  cvs.addEventListener("contextmenu", (e) => e.preventDefault());
  cvs.addEventListener("wheel", (e) => {
    e.preventDefault();
    const p = pagePos(e);
    sendInput({ type: "wheel", x: p.x, y: p.y, dx: e.deltaX, dy: e.deltaY, modifiers: cdpMods(e) });
  }, { passive: false });
  // ── 键盘：聚焦 canvas 后捕获；可打印键带 text 触发字符输入 ──
  const keyEvt = (kind, e) => {
    const printable = e.key.length === 1 && !e.ctrlKey && !e.metaKey;
    sendInput({ type: "key", kind, text: (kind === "keyDown" && printable) ? e.key : "", code: e.code, key: e.key, vk: e.keyCode || 0, modifiers: cdpMods(e) });
  };
  cvs.addEventListener("keydown", (e) => { e.preventDefault(); keyEvt("keyDown", e); });
  cvs.addEventListener("keyup", (e) => { e.preventDefault(); keyEvt("keyUp", e); });

  const proto = location.protocol === "https:" ? "wss://" : "ws://";
  browserWs = new WebSocket(proto + location.host + "/browser-ws");
  browserWs.binaryType = "arraybuffer";
  browserWs.onopen = () => { status.textContent = "已连接 · 等待画面…（首帧需 Edge 启动）"; };
  browserWs.onclose = () => { status.textContent = "已断开"; };
  browserWs.onerror = () => { status.textContent = "连接出错（需 --features browser 构建 + Edge）"; };
  browserWs.onmessage = async (e) => {
    if (typeof e.data === "string") {
      try { const v = JSON.parse(e.data);
        if (v.type === "error") status.textContent = "浏览器启动失败：" + (v.message || "");
        else if (v.type === "ready") status.textContent = "浏览器就绪 · 等待首帧…";
        else if (v.type === "url" && document.activeElement !== url) url.value = v.url; // 地址栏反映当前页（不打断输入）
      } catch (_) { /* 忽略 */ }
      return;
    }
    try {
      // 帧格式 [u32_le metaLen][meta JSON][JPEG]：先取 metadata（坐标换算），再画 JPEG。
      const buf = e.data;
      const dv = new DataView(buf);
      const metaLen = dv.getUint32(0, true);
      meta = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 4, metaLen)));
      const blob = new Blob([new Uint8Array(buf, 4 + metaLen)], { type: "image/jpeg" });
      const bmp = await createImageBitmap(blob);
      if (cvs.width !== bmp.width || cvs.height !== bmp.height) { cvs.width = bmp.width; cvs.height = bmp.height; }
      ctx.drawImage(bmp, 0, 0);
      bmp.close();
      status.style.display = "none"; // 有画面后藏状态
    } catch (_) { /* 解码失败丢帧 */ }
  };
}

// §5.6 画布查看（参前身 datoobot openCanvas）：把任意文本/图片塞进右侧画布看全文。
function openCanvasContent(title, content) {
  if (typeof closeBrowserMirror === "function") closeBrowserMirror(); // 切到全文/图片时断镜像流
  setCanvasOpen(true);
  if (canvasTitle) canvasTitle.textContent = title || "画布";
  if (!canvasBody) return;
  canvasBody.innerHTML = "";
  const src = imageSrc(content);
  if (src) {
    const frame = el("div", "canvas-image-frame");
    const img = document.createElement("img");
    img.alt = title || "图片"; img.src = src;
    frame.appendChild(img); canvasBody.appendChild(frame);
  } else {
    const pre = el("pre", "canvas-text-preview");
    pre.textContent = content || "(空)";
    canvasBody.appendChild(pre);
  }
}

function canvasStatus(title, detail) {
  if (!canvasBody) return;
  if (canvasTitle) canvasTitle.textContent = title;
  canvasBody.innerHTML = "";
  const box = el("div", "canvas-status");
  box.appendChild(el("div", "canvas-status-title", title));
  if (detail) box.appendChild(el("div", "canvas-status-detail", detail));
  canvasBody.appendChild(box);
}

function renderCanvasResource(doc) {
  if (!canvasBody) return;
  if (canvasTitle) canvasTitle.textContent = "资源";
  canvasBody.innerHTML = "";

  const view = el("div", "canvas-resource-view");
  const meta = el("div", "canvas-resource-meta");
  const url = el("div", "canvas-resource-url", doc.url);
  url.title = doc.url;
  const type = el("div", "canvas-resource-type", doc.content_type || "text/plain");
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "canvas-copy-btn";
  copy.textContent = "复制";
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(doc.content || "");
      copy.textContent = "已复制";
      setTimeout(() => { copy.textContent = "复制"; }, 1200);
    } catch (_) {
      copy.textContent = "失败";
      setTimeout(() => { copy.textContent = "复制"; }, 1200);
    }
  });
  meta.append(url, type, copy);
  view.appendChild(meta);

  if ((doc.content_type || "").startsWith("image/")) {
    const frame = el("div", "canvas-image-frame");
    const img = document.createElement("img");
    img.alt = doc.url;
    img.src = `data:${doc.content_type};base64,${doc.content || ""}`;
    frame.appendChild(img);
    view.appendChild(frame);
  } else {
    const pre = el("pre", "canvas-text-preview");
    pre.textContent = doc.content || "";
    view.appendChild(pre);
  }

  canvasBody.appendChild(view);
}

async function openCanvasResource(url) {
  const requestId = ++canvasRequestId;
  setCanvasOpen(true);
  canvasStatus("加载中", url);
  try {
    const res = await fetch(`/api/resource?url=${encodeURIComponent(url)}`);
    const body = await res.json().catch(() => ({}));
    if (requestId !== canvasRequestId) return;
    if (!res.ok) {
      canvasStatus("读取失败", body.error || `${res.status} ${res.statusText}`);
      return;
    }
    renderCanvasResource(body);
  } catch (err) {
    if (requestId === canvasRequestId) canvasStatus("读取失败", String(err));
  }
}

function appendLog(ev) {
  if (ev.seq && seenLogSeq.has(ev.seq)) return;
  if (ev.seq) seenLogSeq.add(ev.seq);
  const row = document.createElement("div");
  row.className = `log ${ev.level || "info"}`;
  if (ev.seq) row.dataset.seq = String(ev.seq);
  const t = document.createElement("span"); t.className = "t"; t.textContent = ev.time || "";
  const lvl = document.createElement("span"); lvl.className = "lvl"; lvl.textContent = (ev.level || "info").slice(0, 4);
  const tgt = document.createElement("span"); tgt.className = "target"; tgt.textContent = ev.target || "";
  const msg = document.createElement("span"); msg.className = "msg"; msg.textContent = ev.message || "";
  row.append(t, lvl, tgt, msg);
  logsList.appendChild(row);
  // 截断(老的出队)。同时也意味着发生过截断 → 显示 banner。
  let truncated = false;
  while (logsList.childElementCount > LOGS_MAX) {
    const oldSeq = Number(logsList.firstElementChild?.dataset?.seq || 0);
    if (oldSeq) seenLogSeq.delete(oldSeq);
    logsList.removeChild(logsList.firstElementChild);
    truncated = true;
  }
  if (truncated) logsTruncated.hidden = false;
  refreshLogsChrome();
  // 自动滚到底(本期不做 detach 滚轮跟随,简单起见一直 pin)
  logsList.scrollTop = logsList.scrollHeight;
}

function refreshLogsChrome() {
  const n = logsList.childElementCount;
  // 计数
  logsMeta.textContent = n === 1 ? "1 条" : `${n} 条`;
  // Clear 按钮:0 条时禁用,>0 条时启用
  logsClearBtn.disabled = n === 0;
  // Empty state:0 条时显形,有日志时藏起来
  logsEmpty.hidden = n > 0;
}

function clearLogs() {
  logsList.innerHTML = "";
  seenLogSeq.clear();
  logsTruncated.hidden = true;
  refreshLogsChrome();
}

// A2（§5.5）：详细度无损显隐。info（默认，藏 debug 行）↔ debug（全显）。
// 纯改 #transcript 的 .lv-* 容器类，CSS 据各行 data-lv 即时显隐——零往返、零重渲染、不丢历史。
let detailLevel = (() => {
  try { return localStorage.getItem("botobot.detailLevel") || "info"; } catch (_) { return "info"; }
})();
function setDetailLevel(lv) {
  detailLevel = lv === "debug" ? "debug" : "info";
  transcript.classList.remove("lv-info", "lv-debug");
  transcript.classList.add("lv-" + detailLevel);
  if (detailToggleBtn) {
    detailToggleBtn.setAttribute("aria-pressed", String(detailLevel === "debug"));
    const lbl = detailToggleBtn.querySelector("span");
    if (lbl) lbl.textContent = detailLevel === "debug" ? "详细" : "简洁";
  }
  try { localStorage.setItem("botobot.detailLevel", detailLevel); } catch (_) {}
}
setDetailLevel(detailLevel);
if (detailToggleBtn) {
  detailToggleBtn.addEventListener("click", () =>
    setDetailLevel(detailLevel === "debug" ? "info" : "debug"));
}

// §5.5 A5：主题切换（跟随系统 → 深色 → 浅色 循环）+ 圆形揭开过渡。
// 跟随系统 = 删 data-theme（回退 prefers-color-scheme）；深/浅 = 手动覆盖。localStorage 持久化。
const THEME_CYCLE = ["system", "dark", "light"];
function applyTheme(mode) {
  const root = document.documentElement;
  if (mode === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", mode);
  const btn = document.getElementById("theme-toggle");
  const lbl = btn && btn.querySelector("span");
  if (lbl) lbl.textContent = mode === "system" ? "主题" : mode === "dark" ? "深色" : "浅色";
  try { localStorage.setItem("botobot.theme", mode); } catch (_) {}
}
function currentTheme() {
  try { return localStorage.getItem("botobot.theme") || "system"; } catch (_) { return "system"; }
}
applyTheme(currentTheme()); // 启动应用
{
  const themeBtn = document.getElementById("theme-toggle");
  if (themeBtn) {
    themeBtn.addEventListener("click", (e) => {
      const next = THEME_CYCLE[(THEME_CYCLE.indexOf(currentTheme()) + 1) % THEME_CYCLE.length];
      // 圆形揭开：从点击点放射。不支持 startViewTransition 时直接切（降级安全）。
      if (document.startViewTransition) {
        const x = e.clientX, y = e.clientY;
        const r = Math.hypot(Math.max(x, innerWidth - x), Math.max(y, innerHeight - y));
        const t = document.startViewTransition(() => applyTheme(next));
        t.ready.then(() => {
          document.documentElement.animate(
            { clipPath: [`circle(0px at ${x}px ${y}px)`, `circle(${r}px at ${x}px ${y}px)`] },
            { duration: 420, easing: "ease-in-out", pseudoElement: "::view-transition-new(root)" }
          );
        }).catch(() => {});
      } else {
        applyTheme(next);
      }
    });
  }
}

logsToggleBtn.addEventListener("click", () => setLogsOpen(!logsOpen));
if (canvasToggleBtn) canvasToggleBtn.addEventListener("click", () => setCanvasOpen(!canvasOpen));
if (canvasCloseBtn) canvasCloseBtn.addEventListener("click", () => setCanvasOpen(false));
// §5.5 C11：bot 属性面板（笼子 / 工具 / subagent tab 已实现；bot.md 编辑待写回端点）。
let botInfoOpen = false;
let lastBotInfo = null; // 取一次缓存，tab 间切换不重取。
function setBotInfoOpen(open) {
  botInfoOpen = open;
  const p = document.getElementById("botinfo");
  if (p) p.setAttribute("aria-hidden", String(!open));
}
async function openBotInfo(botId) {
  setBotInfoOpen(true);
  const body = document.getElementById("botinfo-body");
  if (body) body.textContent = "加载中…";
  lastBotInfo = null;
  try {
    const res = await fetch(`/api/bots/${encodeURIComponent(botId)}/info`);
    if (res.ok) lastBotInfo = await res.json();
  } catch (_) { /* 忽略 */ }
  // 默认回到「笼子」tab。
  document.querySelectorAll(".botinfo-tab").forEach((t) =>
    t.dataset.tab === "cage" ? t.setAttribute("data-active", "true") : t.removeAttribute("data-active"));
  renderBotInfoTab("cage");
}
function renderBotInfoTab(tab) {
  const body = document.getElementById("botinfo-body");
  if (!body) return;
  body.innerHTML = "";
  const info = lastBotInfo;
  if (!info) { body.textContent = "无法获取 bot 信息（后端不可达或未知 bot）。"; return; }
  if (tab === "cage") return renderBotInfoCage(info, body);
  if (tab === "tools") return renderBotInfoTools(info, body);
  if (tab === "sub") return renderBotInfoSubagents(info, body);
  if (tab === "md") return renderBotInfoMd(info, body);
}
let botTemplatesCache = null;
async function loadBotTemplates() {
  if (botTemplatesCache) return botTemplatesCache;
  try {
    const res = await fetch("/api/bot-templates");
    if (res.ok) botTemplatesCache = await res.json();
  } catch (_) { /* 旧后端无端点 */ }
  return botTemplatesCache || [];
}
function renderBotInfoCage(info, body) {
  const row = (k, v) => {
    const d = el("div", "botinfo-row");
    d.append(el("span", "botinfo-k", k), el("span", "botinfo-v", String(v)));
    return d;
  };
  body.append(row("名称", info.name));
  // Profile 行：可切换（§5.7，下一轮生效）。模板端点不可用时退化为只读文本。
  const pRow = el("div", "botinfo-row");
  pRow.append(el("span", "botinfo-k", "Profile"));
  const sel = el("select", "botinfo-profile-sel");
  pRow.append(sel);
  body.append(pRow);
  loadBotTemplates().then((tmpls) => {
    if (!tmpls.length) { sel.replaceWith(el("span", "botinfo-v", info.profile)); return; }
    for (const t of tmpls) {
      const opt = el("option", null, `${t.emoji || ""} ${t.name}`);
      opt.value = t.id;
      if (t.id === info.profile) opt.selected = true;
      sel.append(opt);
    }
    // 当前 profile 不在模板里（如旧自定义）→ 补一个保留项。
    if (!tmpls.some((t) => t.id === info.profile)) {
      const opt = el("option", null, info.profile);
      opt.value = info.profile; opt.selected = true;
      sel.append(opt);
    }
    sel.addEventListener("change", async () => {
      const next = sel.value;
      sel.disabled = true;
      try {
        const res = await fetch(`/api/bots/${encodeURIComponent(info.id)}`, {
          method: "PATCH", headers: { "content-type": "application/json" },
          body: JSON.stringify({ profile: next }),
        });
        if (res.ok) {
          info.profile = next;
          if (lastBotInfo) lastBotInfo.profile = next;
          // 同步更新该 bot 在前端的 profile + nail 角标。
          const b = bots.get(info.id);
          if (b) { b.profile = next; setNailProfileBadge(b); }
        } else {
          console.warn("[bot] profile change failed", res.status);
          sel.value = info.profile; // 回滚
        }
      } catch (e) {
        console.warn("[bot] profile change error", e);
        sel.value = info.profile;
      } finally {
        sel.disabled = false;
      }
    });
  });
  body.append(
    row("工作目录（笼子）", info.workdir),
    row("会话总数", info.session_count),
    row("对话会话", info.chat_count),
  );
}
function renderBotInfoTools(info, body) {
  const tools = (info.tools || []).filter((t) => !(info.subagents || []).includes(t.name));
  if (!tools.length) { body.append(el("div", "botinfo-empty", "无已注册工具。")); return; }
  const hint = el("div", "botinfo-section-hint", `${tools.length} 个工具（按危险度分级）`);
  body.append(hint);
  // 过滤框（37 个工具一长串，键入按名/tier 实时筛）。
  const filter = el("input", "tool-filter");
  filter.type = "search";
  filter.placeholder = "筛选工具…";
  body.append(filter);
  const list = el("div", "tool-list");
  body.append(list);
  const render = (q) => {
    list.innerHTML = "";
    const ql = q.trim().toLowerCase();
    const shown = tools.filter((t) => !ql || t.name.toLowerCase().includes(ql) || t.tier.includes(ql));
    for (const t of shown) {
      const item = el("div", "tool-item");
      item.append(el("span", "tool-name", t.name));
      item.append(el("span", `tool-tier tier-${t.tier}`, t.tier));
      list.append(item);
    }
    hint.textContent = ql
      ? `${shown.length}/${tools.length} 个工具（筛「${q.trim()}」）`
      : `${tools.length} 个工具（按危险度分级）`;
  };
  filter.addEventListener("input", () => render(filter.value));
  render("");
}
function renderBotInfoMd(info, body) {
  const sp = info.system_prompt || "";
  body.append(el("div", "botinfo-section-hint",
    `角色 bot.md（${sp.length} 字符；编辑保存后下一轮生效，技能/书每轮另注入）`));
  const ta = el("textarea", "botinfo-md-edit");
  ta.value = sp;
  ta.spellcheck = false;
  body.append(ta);
  const bar = el("div", "botinfo-md-bar");
  const status = el("span", "botinfo-md-status");
  const save = el("button", "teamcreate-submit botinfo-md-save", "保存");
  save.type = "button";
  const reset = el("button", "botinfo-md-reset", "重置为模板默认");
  reset.type = "button";
  const patchSystem = async (value, label) => {
    save.disabled = true; reset.disabled = true; status.textContent = "保存中…";
    try {
      const res = await fetch(`/api/bots/${encodeURIComponent(info.id)}`, {
        method: "PATCH", headers: { "content-type": "application/json" },
        body: JSON.stringify({ system: value }),
      });
      if (!res.ok) throw new Error(`${res.status}`);
      // 重新拉有效 prompt（清除时回 profile 默认）。
      const fresh = await (await fetch(`/api/bots/${encodeURIComponent(info.id)}/info`)).json();
      info.system_prompt = fresh.system_prompt || "";
      if (lastBotInfo) lastBotInfo.system_prompt = info.system_prompt;
      ta.value = info.system_prompt;
      status.textContent = `${label}✓`;
    } catch (e) {
      status.textContent = `失败 ${e.message || e}`;
    } finally {
      save.disabled = false; reset.disabled = false;
    }
  };
  save.addEventListener("click", () => patchSystem(ta.value, "已保存 "));
  reset.addEventListener("click", () => patchSystem("", "已重置 "));
  bar.append(save, reset, status);
  body.append(bar);
}
function renderBotInfoSubagents(info, body) {
  const subs = info.subagents || [];
  if (!subs.length) { body.append(el("div", "botinfo-empty", "无 subagent（未启用 explore/editor）。")); return; }
  body.append(el("div", "botinfo-section-hint", `${subs.length} 个 subagent（隔离上下文、只回蒸馏）`));
  const list = el("div", "tool-list");
  const DESC = { explore: "只读理解/检索（read/search/find/lsp）", editor: "读+改（apply_patch/edit_by_hashline/rename）" };
  for (const name of subs) {
    const item = el("div", "tool-item");
    item.append(el("span", "tool-name", name));
    item.append(el("span", "tool-sub-desc", DESC[name] || ""));
    list.append(item);
  }
  body.append(list);
}
{
  const closeBtn = document.getElementById("botinfo-close");
  if (closeBtn) closeBtn.addEventListener("click", () => setBotInfoOpen(false));
  // Esc 关闭属性面板（与 bot 市场模态一致的惯例）。
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && botInfoOpen) setBotInfoOpen(false);
  });
  document.querySelectorAll(".botinfo-tab").forEach((tab) => {
    tab.addEventListener("click", () => {
      document.querySelectorAll(".botinfo-tab").forEach((t) => t.removeAttribute("data-active"));
      tab.setAttribute("data-active", "true");
      renderBotInfoTab(tab.dataset.tab);
    });
  });
}

// §5.5 C9：团队看板（全屏模态，3 泳道 Active/Done/Cancelled，读 /api/teams 快照）。
let boardOpen = false;
const BOARD_LANES = [
  { key: "active", label: "进行中 Active" },
  { key: "done", label: "已完成 Done" },
  { key: "cancelled", label: "已取消 Cancelled" },
];
function setBoardOpen(open) {
  boardOpen = open;
  const b = document.getElementById("board");
  if (b) b.setAttribute("aria-hidden", String(!open));
  if (open) loadBoard();
}
async function loadBoard() {
  let snap = null;
  try {
    const res = await fetch("/api/teams");
    if (res.ok) snap = await res.json();
  } catch (_) { /* 忽略 */ }
  renderBoard(snap && snap.teams ? snap.teams : []);
}
function renderBoard(teams) {
  const lanes = document.getElementById("board-lanes");
  if (!lanes) return;
  lanes.innerHTML = "";
  for (const lane of BOARD_LANES) {
    const col = el("div", "board-lane");
    const inLane = teams.filter((t) => (t.status || "active") === lane.key);
    col.appendChild(el("div", "board-lane-title", `${lane.label} · ${inLane.length}`));
    if (inLane.length === 0) {
      col.appendChild(el("div", "board-empty", "（空）"));
    } else {
      for (const t of inLane) {
        const card = el("div", "board-card");
        card.appendChild(el("div", "board-card-task", (t.task && t.task.description) || t.id));
        const meta = el("div", "board-card-meta");
        const msgs = t.messages || [];
        meta.append(
          el("span", "board-chip", `leader: ${botName(t.leader)}`),
          el("span", "board-chip", `成员 ${(t.members || []).length}`),
          el("span", "board-chip", `💬 ${msgs.length}`),
        );
        card.appendChild(meta);
        // 点卡片 → 展开/收起 transcript（数据已在快照，无需再取）。
        const tr = el("div", "board-card-transcript");
        tr.style.display = "none";
        if (!msgs.length) {
          tr.appendChild(el("div", "board-empty", "（暂无消息——编排进行中或未产出）"));
        } else {
          for (const m of msgs) {
            const line = el("div", "board-msg");
            const who = m.author === "user" ? "user" : (m.author && m.author.bot ? botName(m.author.bot) : "system");
            line.append(el("span", "board-msg-who", who), el("span", "board-msg-text", m.content || ""));
            tr.appendChild(line);
          }
        }
        card.appendChild(tr);
        card.style.cursor = "pointer";
        card.addEventListener("click", () => {
          const open = tr.style.display !== "none";
          tr.style.display = open ? "none" : "block";
          card.dataset.expanded = String(!open);
        });
        col.appendChild(card);
      }
    }
    lanes.appendChild(col);
  }
}
{
  const bt = document.getElementById("board-toggle");
  const bc = document.getElementById("board-close");
  if (bt) bt.addEventListener("click", () => setBoardOpen(!boardOpen));
  if (bc) bc.addEventListener("click", () => setBoardOpen(false));
  const overlay = document.getElementById("board");
  if (overlay) overlay.addEventListener("click", (e) => { if (e.target === overlay) setBoardOpen(false); });
  const nt = document.getElementById("board-new-team");
  if (nt) nt.addEventListener("click", () => openTeamCreator());
  const ct = document.getElementById("cron-toggle");
  if (ct) ct.addEventListener("click", () => openCronPanel());
}

// §2.10：定时任务面板（查看/取消所有 cron 后台任务）。
async function openCronPanel() {
  const overlay = el("div", "botmarket-overlay");
  const modal = el("div", "botmarket-modal cron-modal");
  const head = el("div", "botmarket-head");
  head.append(el("h2", "botmarket-title", "定时任务"));
  const close = el("button", "botmarket-close", "✕");
  close.type = "button";
  close.addEventListener("click", () => overlay.remove());
  head.append(close);
  modal.append(head);
  const body = el("div", "cron-list");
  modal.append(body);
  const render = async () => {
    body.innerHTML = "";
    let jobs = [];
    try { const r = await fetch("/api/cron"); if (r.ok) jobs = await r.json(); }
    catch (_) { body.append(el("div", "botinfo-empty", "无法获取定时任务（后端不可达）。")); return; }
    if (!jobs.length) { body.append(el("div", "botinfo-empty", "当前没有定时任务。")); return; }
    body.append(el("div", "botinfo-section-hint", `${jobs.length} 个定时任务`));
    for (const j of jobs) {
      const row = el("div", "cron-item");
      const info = el("div", "cron-item-info");
      info.append(el("div", "cron-prompt", j.prompt || j.id));
      const meta = el("div", "cron-meta");
      meta.append(
        el("span", "board-chip", j.recurring ? "🔁 周期" : "⌛ 一次性"),
        el("span", "board-chip", `会话 ${j.session_id || "—"}`),
      );
      info.append(meta);
      row.append(info);
      const cancel = el("button", "cron-cancel", "取消");
      cancel.type = "button";
      cancel.addEventListener("click", async () => {
        cancel.disabled = true; cancel.textContent = "取消中…";
        try {
          const r = await fetch(`/api/cron/${encodeURIComponent(j.id)}`, { method: "DELETE" });
          if (!r.ok && r.status !== 404) throw new Error(`${r.status}`);
          await render();
          refreshCronIndicators();
        } catch (e) { cancel.disabled = false; cancel.textContent = "取消失败"; }
      });
      row.append(cancel);
      body.append(row);
    }
  };
  await render();
  overlay.append(modal);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) overlay.remove(); });
  const onKey = (e) => { if (e.key === "Escape") { overlay.remove(); document.removeEventListener("keydown", onKey); } };
  document.addEventListener("keydown", onKey);
  document.body.appendChild(overlay);
}

// §2.10：给有定时任务的 session 项打 ⏰ 标记（让用户知道「这个会话还有后台定时任务在排」）。
async function refreshCronIndicators() {
  let jobs = [];
  try { const r = await fetch("/api/cron"); if (r.ok) jobs = await r.json(); } catch (_) { return; }
  const withCron = new Set(jobs.map((j) => j.session_id).filter(Boolean));
  for (const session of sessions.values()) {
    const item = session.item;
    if (!item) continue;
    if (withCron.has(session.id)) item.dataset.cron = "true";
    else item.removeAttribute("data-cron");
  }
}

// §5.6 S3：新建 team 轻量模态（项目+成员+leader+任务）→ 建项目→开 team→编排。
function slugify(s) {
  return (s || "team").trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 32) || "team";
}
function openTeamCreator() {
  const overlay = el("div", "botmarket-overlay");
  const modal = el("div", "botmarket-modal teamcreate-modal");
  const head = el("div", "botmarket-head");
  head.append(el("h2", "botmarket-title", "新建 team"));
  const close = el("button", "botmarket-close", "✕");
  close.type = "button";
  close.addEventListener("click", () => overlay.remove());
  head.append(close);
  modal.append(head);

  const form = el("div", "teamcreate-form");
  const field = (label, ctrl) => { const f = el("label", "teamcreate-field"); f.append(el("span", "teamcreate-label", label), ctrl); return f; };
  const nameInput = el("input", "teamcreate-input"); nameInput.type = "text"; nameInput.placeholder = "如 重构登录模块";
  const rootInput = el("input", "teamcreate-input"); rootInput.type = "text";
  rootInput.value = (bots.get(activeBotId)?.workdir) || ".";
  const taskInput = el("textarea", "teamcreate-input teamcreate-task"); taskInput.placeholder = "leader 要拆解并交给成员的总任务…";

  // 成员多选（来自当前 bots）。
  const memberBox = el("div", "teamcreate-members");
  const leaderSel = el("select", "teamcreate-input");
  const refreshLeader = () => {
    const checked = [...memberBox.querySelectorAll("input:checked")].map((c) => c.value);
    leaderSel.innerHTML = "";
    for (const id of checked) {
      const opt = el("option", null, bots.get(id)?.name || id); opt.value = id; leaderSel.append(opt);
    }
  };
  for (const b of bots.values()) {
    const row = el("label", "teamcreate-member");
    const cb = el("input"); cb.type = "checkbox"; cb.value = b.id;
    cb.addEventListener("change", refreshLeader);
    row.append(cb, el("span", null, `${PROFILE_EMOJI[b.profile] || "🤖"} ${b.name}`));
    memberBox.append(row);
  }

  form.append(
    field("项目名", nameInput),
    field("工作目录", rootInput),
    field("成员（勾选 bot）", memberBox),
    field("Leader", leaderSel),
    field("任务", taskInput),
  );
  const err = el("div", "teamcreate-err");
  const submit = el("button", "teamcreate-submit", "创建并编排");
  submit.type = "button";
  submit.addEventListener("click", async () => {
    const name = nameInput.value.trim();
    const members = [...memberBox.querySelectorAll("input:checked")].map((c) => c.value);
    const leader = leaderSel.value;
    const task = taskInput.value.trim();
    err.textContent = "";
    if (!name) { err.textContent = "请填项目名"; return; }
    if (!members.length) { err.textContent = "至少勾选一个成员"; return; }
    if (!leader || !members.includes(leader)) { err.textContent = "leader 须是成员之一"; return; }
    if (!task) { err.textContent = "请填任务"; return; }
    submit.disabled = true; submit.textContent = "创建中…";
    try {
      const pid = `${slugify(name)}-${Math.random().toString(36).slice(2, 6)}`;
      const pr = await fetch("/api/projects", { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ id: pid, name, root_dir: rootInput.value.trim() || ".", default_bots: members }) });
      if (!pr.ok) throw new Error(`项目创建失败 ${pr.status}`);
      const tr = await fetch("/api/teams", { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ project_id: pid, members, leader, task }) });
      if (!tr.ok) throw new Error(`开 team 失败 ${tr.status}`);
      const { team_id } = await tr.json();
      // 触发编排（异步，后端 spawn）。
      await fetch(`/api/teams/${encodeURIComponent(team_id)}/conduct`, { method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ members, leader, task }) });
      overlay.remove();
      loadBoard(); // 刷新看板显示新 team
      await loadTeamsIntoNail(); // §5.6：新 team 进 nail 栏
      enterTeamView(team_id); // 直接进入新建群的对话
    } catch (e) {
      err.textContent = String(e.message || e);
      submit.disabled = false; submit.textContent = "创建并编排";
    }
  });
  modal.append(form, err, submit);
  overlay.append(modal);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) overlay.remove(); });
  const onKey = (e) => { if (e.key === "Escape") { overlay.remove(); document.removeEventListener("keydown", onKey); } };
  document.addEventListener("keydown", onKey);
  document.body.appendChild(overlay);
}

// ═══════════ §5.6 IM 化：nail 栏 team（群聊） ═══════════
// 拉 /api/teams，把活跃 team 渲染成 nail 栏「群」按钮（组合图标），点开进群聊视图。
async function loadTeamsIntoNail() {
  let snap = null;
  try { const r = await fetch("/api/teams"); if (r.ok) snap = await r.json(); } catch (_) { return; }
  const list = (snap && snap.teams ? snap.teams : []).filter((t) => (t.status || "active") === "active");
  const seen = new Set();
  for (const t of list) {
    seen.add(t.id);
    const existing = teams.get(t.id);
    if (existing) {
      existing.team = t;
      paintTeamButton(existing.button, t);
      if (activeTeamId === t.id) renderTeamView(t); // 实时刷新当前群
    } else {
      const button = makeTeamNailButton(t);
      teams.set(t.id, { team: t, button });
    }
  }
  // 移除已不在快照里的 team 按钮（已归档/取消）。
  for (const [id, entry] of [...teams.entries()]) {
    if (!seen.has(id)) { entry.button.remove(); teams.delete(id); if (activeTeamId === id) exitTeamView(); }
  }
}

// 组合图标：最多 4 个成员的色块 + 首字母拼成 2x2 网格（IM 群头像观感）。
function makeTeamNailButton(t) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "nail-btn nail-team-btn";
  btn.dataset.teamId = t.id;
  paintTeamButton(btn, t);
  btn.addEventListener("click", () => enterTeamView(t.id));
  if (addBotBtn && nailRail) nailRail.insertBefore(btn, addBotBtn);
  return btn;
}
function paintTeamButton(btn, t) {
  btn.setAttribute("aria-label", (t.task && t.task.description) || t.id);
  btn.title = `群：${(t.task && t.task.description) || t.id}\nleader: ${botName(t.leader)} · 成员 ${(t.members || []).length}`;
  let grid = btn.querySelector(".nail-team-grid");
  if (!grid) { grid = el("div", "nail-team-grid"); btn.appendChild(grid); }
  grid.innerHTML = "";
  const members = (t.members || []).slice(0, 4);
  for (const id of members) {
    const cell = el("span", "nail-team-cell", botInitial(botName(id)));
    cell.style.background = bots.get(id)?.color || "#5fae8f";
    grid.appendChild(cell);
  }
}

function enterTeamView(teamId) {
  const entry = teams.get(teamId);
  if (!entry) return;
  activeTeamId = teamId;
  // 取消所有 bot 激活态 + 高亮该 team 按钮。
  for (const b of bots.values()) b.button.dataset.active = "false";
  for (const [id, e] of teams) e.button.dataset.active = id === teamId ? "true" : "false";
  const tv = document.getElementById("team-view");
  if (transcript) transcript.hidden = true;
  if (tv) tv.hidden = false;
  renderTeamView(entry.team);
  // composer 提示语切换到「在群里发言」。
  if (input) input.placeholder = `在「${(entry.team.task && entry.team.task.description) || "群聊"}」里发言（leader 会编排成员）`;
  autoSize();
}
function exitTeamView() {
  if (activeTeamId === null) return;
  activeTeamId = null;
  for (const [, e] of teams) e.button.dataset.active = "false";
  const tv = document.getElementById("team-view");
  if (tv) tv.hidden = true;
  if (transcript) transcript.hidden = false;
  if (input) input.placeholder = "输入消息…";
}
function renderTeamView(t) {
  const tv = document.getElementById("team-view");
  if (!tv) return;
  tv.innerHTML = "";
  const head = el("div", "team-view-head");
  head.append(el("div", "team-view-title", (t.task && t.task.description) || t.id));
  const meta = el("div", "team-view-meta");
  meta.append(
    el("span", "board-chip", `👑 群主 ${botName(t.leader)}`),
    ...(t.members || []).map((m) => el("span", "board-chip", `${PROFILE_EMOJI[bots.get(m)?.profile] || "🤖"} ${botName(m)}`)),
  );
  head.append(meta);
  tv.append(head);
  const msgs = t.messages || [];
  if (!msgs.length) {
    tv.append(el("div", "team-view-empty", "群里还没有消息。在下方发言，群主会把任务拆给成员。"));
  } else {
    for (const m of msgs) {
      const who = m.author === "user" ? "user" : (m.author && m.author.bot ? botName(m.author.bot) : "system");
      const isUser = m.author === "user";
      const row = el("div", "team-msg" + (isUser ? " team-msg-user" : ""));
      row.append(el("div", "team-msg-who", isUser ? "你" : (m.author && m.author.bot === t.leader ? `👑 ${who}` : who)));
      row.append(el("div", "team-msg-text", m.content || ""));
      tv.append(row);
    }
  }
  tv.scrollTop = tv.scrollHeight;
}

// 用户在群聊里发言 → POST /api/teams/:id/message（贴 transcript + 触发 leader 编排），乐观回显 + 轮询刷新。
async function sendToTeam(text) {
  const entry = teams.get(activeTeamId);
  if (!entry) return;
  const tid = activeTeamId;
  // 乐观把用户发言加进当前视图。
  entry.team.messages = entry.team.messages || [];
  entry.team.messages.push({ seq: entry.team.messages.length, author: "user", content: text });
  renderTeamView(entry.team);
  try {
    await fetch(`/api/teams/${encodeURIComponent(tid)}/message`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ text }),
    });
  } catch (e) { console.warn("[team] send failed", e); }
  // 编排在后台跑，轮询几次刷新群消息。
  let n = 0;
  const poll = setInterval(async () => {
    if (activeTeamId !== tid || ++n > 8) { clearInterval(poll); return; }
    await loadTeamsIntoNail();
  }, 1500);
}

{
  const browserBtn = document.getElementById("browser-toggle");
  if (browserBtn) browserBtn.addEventListener("click", () => openBrowserMirror());
}
if (marketToggleBtn) marketToggleBtn.addEventListener("click", () => setMarketOpen(!marketOpen));
if (marketCloseBtn) marketCloseBtn.addEventListener("click", () => setMarketOpen(false));
if (marketRefreshBtn) marketRefreshBtn.addEventListener("click", refreshMarket);
if (marketAddSourceBtn) marketAddSourceBtn.addEventListener("click", marketAddSource);
if (marketSourceSel) marketSourceSel.addEventListener("change", refreshMarket);
if (addBotBtn) {
  addBotBtn.addEventListener("click", () => {
    openBotMarket().catch((err) => {
      if (err && err.name === "AbortError") return;
      console.warn("[bot] open market failed", err);
    });
  });
}
logsClearBtn.addEventListener("click", clearLogs);
if (logsClearInline) logsClearInline.addEventListener("click", clearLogs);
// 初始化一次(0 条日志)
setLogsOpen(true);
// §5.5 D5 超时兜底：boot 时打开日志面板，但若 log_snapshot_done 始终不来（后端无日志/异常），
// 也在 6s 后自动收起，避免面板永久占屏。snapshot 到达时会清掉此计时器（见 handleWsData）。
logsBootTimer = setTimeout(() => {
  if (logsBootMode) {
    logsBootMode = false;
    setLogsOpen(false);
  }
  logsBootTimer = null;
}, 6000);
setCanvasOpen(false);
canvasEmpty();
refreshLogsChrome();

function setBusy(b, session = activeSession()) {
  if (!session) return;
  session.busy = b;
  session.item.dataset.busy = b ? "true" : "false";
  if (session.id === activeSessionId) {
    autoSize();
    // busy 切换时刷新待引导队列：放行按钮在「放行引导」(忙)/「空闲)间切换
    if (typeof renderPendingSteers === "function") renderPendingSteers(session);
  }
  renderBooters(); // §5.6：活动数变化 → 刷新列底
}

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
}

// §2.7 token live：紧凑数字（1234→1.2k，1200000→1.2M）。
function fmtTokens(n) {
  n = Number(n) || 0;
  if (n < 1000) return String(n);
  if (n < 1_000_000) return (n / 1000).toFixed(n < 10_000 ? 1 : 0) + "k";
  return (n / 1_000_000).toFixed(1) + "M";
}

// §2.7 token live：把当前会话用量渲染到 composer 的 #token-usage（空 usage→清空，empty:hidden 自动收起）。
function renderTokenUsage(usage) {
  const elt = document.getElementById("token-usage");
  if (!elt) return;
  if (!usage || !usage.spent) {
    elt.textContent = "";
    return;
  }
  const spent = fmtTokens(usage.spent);
  elt.textContent = usage.budget ? `▣ ${spent} / ${fmtTokens(usage.budget)}` : `▣ ${spent}`;
}

// §5.6 列底 booter：会话列=当前 bot 活动会话数；对话列=活动会话上下文用量（有预算→%，否则 token）。
function renderBooters() {
  const sb = document.getElementById("sessions-booter");
  if (sb) {
    const busy = [...sessions.values()].filter((s) => s.botId === activeBotId && s.busy).length;
    const total = [...sessions.values()].filter((s) => s.botId === activeBotId).length;
    sb.textContent = total ? (busy ? `${busy} 活动 · ${total} 会话` : `${total} 会话`) : "";
  }
  const tb = document.getElementById("thread-booter");
  if (tb) {
    const u = activeSession()?.usage;
    if (u && u.spent) {
      tb.textContent = u.budget
        ? `上下文 ${Math.min(100, Math.round((u.spent / u.budget) * 100))}% · ${fmtTokens(u.spent)}/${fmtTokens(u.budget)}`
        : `已用 ${fmtTokens(u.spent)} tok`;
    } else {
      tb.textContent = "";
    }
  }
  const cb = document.getElementById("canvas-booter");
  if (cb) cb.textContent = typeof canvasOpen !== "undefined" && canvasOpen ? "画布: 打开" : "";
}

function makeBotId() {
  const rand = Math.random().toString(36).slice(2, 7);
  return `bot-${Date.now().toString(36)}-${rand}`;
}

function makeSessionId() {
  const rand = Math.random().toString(36).slice(2, 8);
  return `web-${Date.now().toString(36)}-${rand}`;
}

function botInitial(name) {
  return (name || "b").trim().slice(0, 1).toLowerCase() || "b";
}

// bot id → 显示名（前端 bots Map；未知则原样返回 id）。看板/transcript 渲染用。
function botName(id) {
  if (!id) return "—";
  return bots.get(id)?.name || id;
}

function activeBot() {
  return bots.get(activeBotId) || bots.get(DEFAULT_BOT_ID);
}

function activeSession() {
  return activeSessionId ? sessions.get(activeSessionId) : null;
}

function renderBotState() {
  for (const bot of bots.values()) {
    bot.button.dataset.active = bot.id === activeBotId ? "true" : "false";
  }
  const bot = activeBot();
  if (nailWorkdir && bot) {
    nailWorkdir.textContent = bot.workdir;
    nailWorkdir.title = bot.workdir;
  }
}

function refreshSessionVisibility() {
  for (const session of sessions.values()) {
    const visible = session.botId === activeBotId;
    session.item.dataset.botActive = visible ? "true" : "false";
    session.pane.dataset.botActive = visible ? "true" : "false";
  }
}

// §5.6 D1：bot 全局身份色系统——七彩（最多 7 bot），一处定义全局复用。按创建序分配。
const BOT_PALETTE = ["#5fae8f", "#6d9fd9", "#d99a5f", "#b97fd0", "#d97070", "#5fc2c2", "#c2c25f"];
function botColor(index) {
  return BOT_PALETTE[index % BOT_PALETTE.length];
}

// §5.7 profile→身份 emoji（与后端 BOT_TEMPLATES 一致）。未知 profile 不显角标。
const PROFILE_EMOJI = { coder: "⌨", general: "✨" };
// 在 nail 按钮右下角设/清 profile 角标 + 刷新 title（§5.7 多 profile 主 UI 可见）。
function setNailProfileBadge(bot) {
  if (!bot || !bot.button) return;
  let badge = bot.button.querySelector(".nail-profile");
  const emoji = PROFILE_EMOJI[bot.profile];
  if (!emoji) { if (badge) badge.remove(); }
  else {
    if (!badge) { badge = el("span", "nail-profile"); bot.button.appendChild(badge); }
    badge.textContent = emoji;
  }
  bot.button.title = bot.profile ? `${bot.workdir}\n[${bot.profile}]` : bot.workdir;
}

function createBot({ id = makeBotId(), name, workdir, profile = "coder", button = null, serverKnown = false } = {}) {
  const botName = (name || `bot ${botCounter++}`).trim();
  const botWorkdir = (workdir || botName).trim();
  const color = botColor(bots.size); // §5.6 身份色：按当前 bot 数定色
  let botButton = button;
  if (!botButton) {
    botButton = document.createElement("button");
    botButton.type = "button";
    botButton.className = "nail-btn nail-bot-btn";
    botButton.dataset.botId = id;
    botButton.appendChild(el("span", "nail-letter", botInitial(botName)));
    if (addBotBtn && nailRail) nailRail.insertBefore(botButton, addBotBtn);
  }
  botButton.style.setProperty("--bot-color", color); // §5.6 七彩身份色
  botButton.title = botWorkdir;
  botButton.setAttribute("aria-label", botName);
  // §5.5 C11：点未激活 bot = 切换；点已激活 bot = 打开属性面板。
  botButton.addEventListener("click", () => {
    if (id === activeBotId) openBotInfo(id);
    else switchBot(id);
  });
  const bot = {
    id,
    name: botName,
    workdir: botWorkdir,
    profile: profile || "coder",
    button: botButton,
    color, // §5.6 身份色（全局复用）
    sessionIds: new Set(),
    activeSessionId: null,
    serverKnown,
  };
  bots.set(id, bot);
  setNailProfileBadge(bot); // §5.7 profile 角标
  renderBotState();
  return bot;
}

function switchBot(id) {
  const bot = bots.get(id);
  if (!bot) return;
  exitTeamView(); // §5.6：点 bot 退出群聊视图，回 1:1
  activeBotId = id;
  renderBotState();
  refreshSessionVisibility();
  let target = bot.activeSessionId && sessions.has(bot.activeSessionId)
    ? bot.activeSessionId
    : bot.sessionIds.values().next().value;
  if (!target) target = ensureSession(makeSessionId(), null, id).id;
  switchSession(target);
}

async function createBackendBot(name, workdir, templateId) {
  const res = await fetch("/api/bots", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name, workdir, template_id: templateId || null }),
  });
  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(`create bot failed: ${res.status} ${detail}`);
  }
  const data = await res.json();
  return data.bot;
}

async function addBotFromDirectory(name, workdir = name, templateId) {
  const backendBot = await createBackendBot(name, workdir, templateId);
  const bot = createBot({
    id: backendBot.id,
    name: backendBot.name,
    workdir: backendBot.workdir,
    profile: backendBot.profile,
    serverKnown: true,
  });
  ensureSession(makeSessionId(), null, bot.id);
  switchBot(bot.id);
  input.focus();
  return bot;
}

// §5.7 bot 市场：nail [+] → 模板模态（卡片）→ 选模板 → 目录选择（沿用原流程）。
async function openBotMarket() {
  let templates = [];
  try {
    const res = await fetch("/api/bot-templates");
    if (res.ok) templates = await res.json();
  } catch (_) { /* 后端旧版无端点 → 降级直接走目录 */ }
  if (!templates.length) {
    // 兜底（旧后端无模板端点）：直接问本地路径，无浏览器上传选择器。
    const workdir = window.prompt("本地工作目录绝对路径", (bots.get(DEFAULT_BOT_ID)?.workdir) || ".");
    if (workdir && workdir.trim()) {
      const name = workdir.trim().replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "bot";
      addBotFromDirectory(name, workdir.trim()).catch((err) => console.warn("[bot] create failed", err));
    }
    return;
  }
  const overlay = el("div", "botmarket-overlay");
  const modal = el("div", "botmarket-modal");
  const head = el("div", "botmarket-head");
  head.append(el("h2", "botmarket-title", "添加 bot — 选模板"));
  const close = el("button", "botmarket-close", "✕");
  close.type = "button";
  close.addEventListener("click", () => overlay.remove());
  head.append(close);
  modal.append(head);
  const grid = el("div", "botmarket-grid");
  // 选模板 → 进第二步「本地工作目录」表单（bots.exe 本地运行，直接填服务端本地路径，无上传）。
  const gotoWorkdirStep = (template) => {
    grid.remove();
    head.querySelector(".botmarket-title").textContent = `添加 bot — ${template.emoji || ""} ${template.name}`;
    const form = el("div", "teamcreate-form");
    const field = (label, ctrl) => { const f = el("label", "teamcreate-field"); f.append(el("span", "teamcreate-label", label), ctrl); return f; };
    const nameInput = el("input", "teamcreate-input"); nameInput.type = "text"; nameInput.placeholder = "可留空（取目录名）";
    const dirInput = el("input", "teamcreate-input"); dirInput.type = "text";
    dirInput.value = (bots.get(DEFAULT_BOT_ID)?.workdir) || ".";
    dirInput.placeholder = "本地目录绝对路径，如 D:\\projects\\foo";
    const err = el("div", "teamcreate-err");
    const create = el("button", "teamcreate-submit", "创建 bot");
    create.type = "button";
    create.addEventListener("click", async () => {
      const workdir = dirInput.value.trim();
      if (!workdir) { err.textContent = "请填本地工作目录"; return; }
      const name = nameInput.value.trim() || workdir.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "bot";
      create.disabled = true; create.textContent = "创建中…"; err.textContent = "";
      try {
        await addBotFromDirectory(name, workdir, template.id);
        overlay.remove();
      } catch (e) {
        err.textContent = `创建失败：${(e.message || e)}（该目录在本机上须存在且可访问）`;
        create.disabled = false; create.textContent = "创建 bot";
      }
    });
    dirInput.addEventListener("keydown", (e) => { if (e.key === "Enter") create.click(); });
    form.append(field("名称", nameInput), field("本地工作目录", dirInput));
    modal.append(form, err, create);
    dirInput.focus();
    dirInput.select();
  };
  for (const t of templates) {
    const card = el("button", "botmarket-card");
    card.type = "button";
    card.append(el("span", "botmarket-emoji", t.emoji || "🤖"));
    card.append(el("span", "botmarket-name", t.name));
    card.append(el("span", "botmarket-desc", t.description || ""));
    card.addEventListener("click", () => gotoWorkdirStep(t));
    grid.append(card);
  }
  modal.append(grid);
  overlay.append(modal);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) overlay.remove(); });
  // Esc 关闭（无障碍/惯例）：监听一次，关闭即解绑。
  const onKey = (e) => {
    if (e.key === "Escape") { overlay.remove(); document.removeEventListener("keydown", onKey); }
  };
  document.addEventListener("keydown", onKey);
  document.body.appendChild(overlay);
}

async function ensureServerSession(session) {
  if (!session || session.serverKnown) return;
  const bot = bots.get(session.botId) || bots.get(DEFAULT_BOT_ID);
  if (!bot || bot.id === DEFAULT_BOT_ID) {
    session.serverKnown = true;
    return;
  }
  const res = await fetch(`/api/bots/${encodeURIComponent(bot.id)}/sessions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ session_id: session.id }),
  });
  if (!res.ok) {
    const detail = await res.text().catch(() => "");
    throw new Error(`create bot session failed: ${res.status} ${detail}`);
  }
  session.serverKnown = true;
}

function ensureSession(id, title, botId = activeBotId) {
  const sid = id || makeSessionId();
  if (sessions.has(sid)) return sessions.get(sid);
  const bot = bots.get(botId) || bots.get(DEFAULT_BOT_ID);
  const ownerBotId = bot ? bot.id : DEFAULT_BOT_ID;

  const pane = el("section", "session-pane");
  pane.dataset.sessionId = sid;
  pane.dataset.active = "false";
  pane.dataset.botActive = "false";
  // §5.6 空会话欢迎态：有内容（.run/.msg/.error）后由 CSS :has() 自动隐藏。
  {
    const who = bot ? bot.name : "botobot";
    const emoji = bot ? (PROFILE_EMOJI[bot.profile] || "") : "";
    const empty = el("div", "pane-empty");
    empty.append(
      el("div", "pane-empty-emoji", emoji || "·"),
      el("div", "pane-empty-title", `和 ${who} 开始对话`),
      el("div", "pane-empty-hint", "提问、让它读代码、跑工具、查记忆——直接在下方输入。"),
    );
    pane.appendChild(empty);
  }
  transcript.appendChild(pane);

  const item = el("div", "session-item");
  item.dataset.sessionId = sid;
  item.dataset.active = "false";
  item.dataset.botActive = "false";
  item.dataset.busy = "false";
  item.setAttribute("role", "listitem");
  const main = document.createElement("button");
  main.type = "button";
  main.className = "session-main";
  main.setAttribute("aria-label", "切换会话");
  const label = el("span", "session-title", title || `会话 ${sessionCounter++}`);
  const dot = el("span", "session-state-dot");
  const toolTag = el("span", "session-tool"); // §5.5 B7：忙时显示当前工具/阶段名
  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "session-delete-btn";
  remove.setAttribute("aria-label", "删除会话");
  remove.title = "删除会话";
  remove.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 6h18"></path><path d="M8 6V4h8v2"></path><path d="M19 6l-1 14H6L5 6"></path><path d="M10 11v5"></path><path d="M14 11v5"></path></svg>';
  main.append(label, toolTag, dot);
  item.append(main, remove);
  main.addEventListener("click", () => switchSession(sid));
  remove.addEventListener("click", (event) => {
    event.stopPropagation();
    deleteSession(sid);
  });
  sessionList.appendChild(item);

  const session = {
    id: sid,
    botId: ownerBotId,
    title: label.textContent,
    label,
    remove,
    pane,
    item,
    toolTag,
    runs: new Map(),
    topRun: null,
    busy: false,
    serverKnown: false,
    userDetached: false,
  };
  sessions.set(sid, session);
  if (bot) {
    bot.sessionIds.add(sid);
    if (!bot.activeSessionId) bot.activeSessionId = sid;
  }
  refreshSessionVisibility();
  if (!activeSessionId) switchSession(sid);
  return session;
}

function removeSessionLocal(id) {
  const session = sessions.get(id);
  if (!session) return;
  const wasActive = activeSessionId === id;
  const bot = bots.get(session.botId);
  if (bot) bot.sessionIds.delete(id);
  deletedSessionIds.add(id);
  session.pane.remove();
  session.item.remove();
  sessions.delete(id);

  if (wasActive) {
    activeSessionId = null;
    const nextId = bot && bot.sessionIds.values().next().value;
    if (bot) bot.activeSessionId = nextId || null;
    if (nextId) {
      switchSession(nextId);
    } else {
      const fresh = ensureSession(makeSessionId(), null, session.botId);
      switchSession(fresh.id);
    }
  }
  refreshSessionVisibility();
  autoSize();
}

async function deleteSession(id) {
  const session = sessions.get(id);
  if (!session || session.deleting) return;
  session.deleting = true;
  session.item.dataset.deleting = "true";
  session.remove.disabled = true;
  if (!session.serverKnown) {
    removeSessionLocal(id);
    return;
  }
  try {
    const res = await fetch(`/api/sessions/${encodeURIComponent(id)}`, { method: "DELETE" });
    if (!res.ok && res.status !== 404) throw new Error(`delete session failed: ${res.status}`);
    removeSessionLocal(id);
  } catch (err) {
    console.warn("[session] delete failed", err);
    session.deleting = false;
    session.item.dataset.deleting = "false";
    session.remove.disabled = false;
  }
}

function switchSession(id) {
  const target = sessions.get(id);
  if (!target) return;
  if (target.botId !== activeBotId) {
    activeBotId = target.botId;
    renderBotState();
  }
  const bot = bots.get(target.botId);
  if (bot) bot.activeSessionId = id;
  activeSessionId = id;
  // §2.8：持久化最近选中会话，刷新后回到上次所在会话（非空 chat/fork 才值得记）。
  try { localStorage.setItem("botobot.lastSession", id); } catch (_) {}
  for (const session of sessions.values()) {
    const visible = session.botId === activeBotId;
    const active = visible && session.id === id;
    session.item.dataset.botActive = visible ? "true" : "false";
    session.pane.dataset.botActive = visible ? "true" : "false";
    session.pane.dataset.active = active ? "true" : "false";
    session.item.dataset.active = active ? "true" : "false";
  }
  autoSize();
  renderTokenUsage(target.usage); // §2.7：显示该会话自己的 live token 用量（无则清空）
  renderBooters(); // §5.6：切会话刷新列底
  renderPendingSteers(target); // 显示该会话自己的待引导队列
  loadHistoryIfNeeded(target); // §2.8：首次切到持久化会话时回读历史
  requestAnimationFrame(() => {
    transcript.scrollTop = transcript.scrollHeight;
  });
}

function renameSession(session, text) {
  if (!session || session.titleLocked) return;
  const name = text.trim().replace(/\s+/g, " ").slice(0, 36);
  if (!name) return;
  session.title = name;
  session.label.textContent = name;
  session.titleLocked = true;
}

function subscribeKnownSessions() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  for (const session of sessions.values()) {
    ws.send(JSON.stringify({ type: "subscribe", session_id: session.id, bot_id: session.botId }));
  }
}

// ── 持久化回读（§2.8）：重启/刷新后从后端拉回 bot 列表 + 会话列表 + 历史 ──
function msgText(m) {
  if (!m) return "";
  if (typeof m.content === "string") return m.content;
  if (Array.isArray(m.content)) {
    return m.content.map((p) => (typeof p === "string" ? p : p && p.text ? p.text : "")).join("");
  }
  return "";
}

// 把持久化的历史消息渲染成气泡（user 气泡 + assistant markdown）。tool/system 略过保持可读。
// 用户气泡：文本 + 附件图片缩略图都包进同一个气泡（之前只显「[N 张图片]」文字，看不到图）。
function makeUserBubble(text, images, extraClass) {
  const bubble = el("div", "msg user" + (extraClass ? " " + extraClass : ""));
  const imgs = (images || []).filter(Boolean);
  if (text) bubble.appendChild(el("div", "msg-user-text", text));
  for (const src of imgs) {
    const img = document.createElement("img");
    img.className = "msg-user-img";
    img.src = src;
    img.alt = "附件图片";
    img.loading = "lazy";
    bubble.appendChild(img);
  }
  if (!text && !imgs.length) bubble.appendChild(el("div", "msg-user-text", "[消息]"));
  return bubble;
}
// 从历史消息 content 抽出图片 url（ImageUrl 部件序列化为 {image_url:{url}}）。
function imagesFromMessage(m) {
  if (!m || !Array.isArray(m.content)) return [];
  return m.content
    .map((p) => (p && p.image_url && p.image_url.url) || null)
    .filter(Boolean);
}

function renderHistory(session, messages) {
  for (const m of messages || []) {
    const text = msgText(m).trim();
    if (m.role === "user") {
      session.pane.appendChild(makeUserBubble(text, imagesFromMessage(m)));
    } else if (m.role === "assistant" && text) {
      const box = el("div", "run");
      const ans = el("div", "answer");
      ans.innerHTML = renderMarkdown(text);
      decorateMarkdown(ans);
      box.appendChild(ans);
      session.pane.appendChild(box);
    }
  }
}

async function loadHistoryIfNeeded(session) {
  if (!session || session.historyLoaded || session.historyLoading) return;
  session.historyLoading = true;
  try {
    const res = await fetch(`/api/sessions/${encodeURIComponent(session.id)}/history`);
    if (res.ok) {
      const data = await res.json();
      renderHistory(session, data.messages);
      session.historyLoaded = true;
      // 标题取首条 user 消息
      const firstUser = (data.messages || []).find((m) => m.role === "user");
      if (firstUser) renameSession(session, msgText(firstUser));
      if (session.id === activeSessionId) {
        transcript.scrollTop = transcript.scrollHeight;
      }
    }
  } catch (e) {
    console.warn("[history] load failed", e);
  } finally {
    session.historyLoading = false;
  }
}

async function restorePersisted() {
  // 1) 回读自定义 bot（default 已本地建）
  try {
    const res = await fetch("/api/bots");
    if (res.ok) {
      const data = await res.json();
      for (const b of data.bots || []) {
        // 默认 bot 已本地建——只同步其 profile（可能被切过）到角标。
        if (b.id === DEFAULT_BOT_ID) {
          const d = bots.get(DEFAULT_BOT_ID);
          if (d && b.profile) { d.profile = b.profile; setNailProfileBadge(d); }
          continue;
        }
        if (bots.has(b.id)) continue;
        createBot({ id: b.id, name: b.name, workdir: b.workdir, profile: b.profile, serverKnown: true });
      }
    }
  } catch (e) {
    console.warn("[boot] load bots failed", e);
  }
  // 2) 回读会话列表（只顶层 chat/fork 进侧边栏；subagent/team_member 不展示）
  try {
    const res = await fetch("/api/sessions");
    if (!res.ok) return;
    const data = await res.json();
    let restored = 0;
    for (const s of data.sessions || []) {
      if (s.kind !== "chat" && s.kind !== "fork") continue;
      if (!(s.message_count > 0)) continue; // 跳过空会话（懒持久化下一般不存在，防御）
      if (sessions.has(s.id)) continue;
      const botId = bots.has(s.bot_id) ? s.bot_id : DEFAULT_BOT_ID;
      const session = ensureSession(s.id, "(历史会话)", botId);
      session.serverKnown = true;
      session.historyLoaded = false;
      restored++;
    }
    if (restored > 0) {
      console.info(`[boot] 回读 ${restored} 个持久化会话`);
      // 当前激活会话若是回读来的，载入其历史
      const active = activeSession();
      if (active && !active.historyLoaded) loadHistoryIfNeeded(active);
    }
  } catch (e) {
    console.warn("[boot] load sessions failed", e);
  }
  // §5.5 D3：服务端按 session 订阅转发（非全量广播）。回读的历史会话须显式订阅，否则首屏恢复的
  // 会话在「首条消息/一次重连」前收不到实时事件（后台 team/委派会话尤其会看起来卡住）。
  // ws 未就绪时无害——连接 onopen 会再 subscribeKnownSessions 一次。
  subscribeKnownSessions();
  // §2.8：恢复最近选中会话（若仍存在且非当前）。放最后——确保回读的历史会话已建。
  try {
    const last = localStorage.getItem("botobot.lastSession");
    if (last && last !== activeSessionId && sessions.has(last)) {
      switchSession(last);
    }
  } catch (_) {}
  // §5.5 B8：能力协商——探测后端能力，按能力显隐 `[data-cap]` 元素（一前端多后端）。
  probeCapabilities();
  // §2.10：给有定时任务的会话打 ⏰ 标记。
  refreshCronIndicators();
  // §5.6：把活跃 team 渲进 nail 栏（群聊入口）。
  loadTeamsIntoNail();
}

// §5.5 B8：拉 /api/capabilities，对每个 `[data-cap="<key>"]` 元素：能力为 false 则隐藏。
// 探测失败（旧后端无此端点）→ 全部视为可用（不隐藏），向后兼容。
async function probeCapabilities() {
  let caps = null;
  try {
    const res = await fetch("/api/capabilities");
    if (res.ok) caps = await res.json();
  } catch (_) { /* 旧后端：保持全部可见 */ }
  if (!caps) return;
  document.querySelectorAll("[data-cap]").forEach((elt) => {
    const key = elt.getAttribute("data-cap");
    if (key && caps[key] === false) elt.style.display = "none";
  });
}

newSessionBtn.addEventListener("click", () => {
  const session = ensureSession(makeSessionId(), null, activeBotId);
  switchSession(session.id);
  input.focus();
});

function ensureRun(ev, session) {
  if (session.runs.has(ev.run_id)) return session.runs.get(ev.run_id);
  const box = el("div", "run");
  // 注意:details 容器**不在此处创建**。LLM 真正输出 reasoning 之前不显示
  // 折叠块(避免空 details 占位 + summary 假"思考中…")。第一个 reasoning
  // 事件到达时再补建(见 case "reasoning")。
  // A1（§5.5 活动行）：每个 run 一条原地更新的进度行。summary=spinner+当前阶段，
  // 折叠 body=逐阶段日志；done/error 时封顶为静态摘要、停转。默认 info 档可见，
  // 让「现在在干嘛」始终一眼可见，不必盯事件流。
  // §5.6「处理状态」块：assistant 回合的处理过程（思考 + 工具 + 阶段日志）全收进这个可折叠块，
  // 与下方干净的「回答」视觉分离。流式中展开看实时过程，收尾自动折叠成一行摘要。
  const activity = el("details", "activity");
  activity.open = true; // 流式中展开
  const actSum = el("summary", "activity-sum");
  actSum.appendChild(el("span", "activity-spin"));
  const actText = el("span", "activity-text", "处理中…");
  actSum.appendChild(actText);
  activity.appendChild(actSum);
  const actLog = el("ol", "activity-log");
  activity.appendChild(actLog);
  box.appendChild(activity);
  // answer 段不在此处预建：文本与工具/审批要按事件到达顺序交错，故 answer 段改为
  // 首个 token 到达时由 ensureAnswerSegment 懒建并追加到 box 末尾（见交错渲染）。
  const caret = el("span", "caret");
  const parent = ev.parent_id && session.runs.get(ev.parent_id);
  if (parent) {
    let kids = parent.el.querySelector(":scope > .children");
    if (!kids) { kids = el("div", "children"); parent.el.appendChild(kids); }
    kids.appendChild(box);
  } else {
    session.pane.appendChild(box);
  }
  const run = {
    el: box, acc: "", lastRendered: "", answer: null, answerSealed: false,
    // reasoning 系列字段首次 reasoning 事件时补建
    reasoning: null, reasoningBody: null, summaryText: null, chevron: null,
    reasoningAcc: "",
    caret, streaming: false, tools: new Map(), approvals: new Map(),
    activity, activityText: actText, activityLog: actLog,
    activityLast: "", activityFrozen: false, toolCount: 0, startedAt: Date.now(),
    sessionId: session.id, topLevel: !ev.parent_id, // §5.5 B7：顶层 run 把当前阶段映到会话项
  };
  session.runs.set(ev.run_id, run);
  return run;
}

// A1：把一个生命周期阶段写进活动行（原地更新 summary + 追加可折叠日志）。
// 去重：同一 label 连续到达只更新一次日志，避免 token/reasoning 流刷屏。
function activitySet(r, label) {
  if (!r || !r.activity || r.activityFrozen) return;
  r.activityText.textContent = label;
  r.activity.classList.add("spinning");
  if (label !== r.activityLast) {
    r.activityLast = label;
    r.activityLog.appendChild(el("li", null, label));
  }
  // §5.5 B7：顶层 run 的当前阶段同步到会话列表项（侧边栏一眼看「在干嘛」）。
  if (r.topLevel && r.sessionId) {
    const s = sessions.get(r.sessionId);
    if (s && s.toolTag) s.toolTag.textContent = label;
  }
}

// A1：封顶。停 spinner，summary 改静态摘要；保留折叠日志供回看。
function activityFreeze(r, label) {
  if (!r || !r.activity || r.activityFrozen) return;
  r.activityFrozen = true;
  r.activity.classList.remove("spinning");
  const secs = Math.max(0, Math.round((Date.now() - r.startedAt) / 1000));
  const parts = [label];
  if (r.hadReasoning) parts.push("已思考");
  if (r.toolCount > 0) parts.push(`${r.toolCount} 工具`);
  parts.push(`${secs}s`);
  r.activityText.textContent = parts.join(" · ");
  // §5.6：收尾自动折叠「处理状态」块——只留一行摘要 + 下方干净回答（除非有错保持展开看细节）。
  if (label !== "出错") r.activity.open = false;
  // §5.5 B7：顶层 run 收尾 → 清会话项的当前工具小字（busy 圆点另由 setBusy 控制）。
  if (r.topLevel && r.sessionId) {
    const s = sessions.get(r.sessionId);
    if (s && s.toolTag) s.toolTag.textContent = "";
  }
}

/// 第一次有 reasoning 时补建 details 容器(unsloth 风格 + 立即 open 展示)。
// §5.5 A4：reasoning 分段——镜像 answer 段。当前段未封口则复用；被工具/答案封口后
// 新一波 reasoning 另起新 details，**append（DOM 序 = 事件序）**，与答案/工具交错。
function ensureReasoningDOM(r) {
  if (r.reasoning && !r.reasoningSealed) return;
  const details = el("details", "reasoning");
  details.open = true;
  const summary = el("summary");
  // SVG 灯泡(MD3 风格, 不用 emoji 违反 taste-skill §7)
  const glyph = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  glyph.setAttribute("class", "reasoning-glyph");
  glyph.setAttribute("viewBox", "0 0 24 24");
  glyph.setAttribute("fill", "none");
  glyph.setAttribute("stroke", "currentColor");
  glyph.setAttribute("stroke-width", "1.75");
  glyph.setAttribute("stroke-linecap", "round");
  glyph.setAttribute("stroke-linejoin", "round");
  glyph.setAttribute("aria-hidden", "true");
  glyph.innerHTML = '<path d="M15 14c.2-1 .7-1.7 1.5-2.5 1-1 1.5-2.2 1.5-3.5A6 6 0 0 0 6 8c0 1 .2 2.2 1.5 3.5.7.7 1.3 1.5 1.5 2.5"/><path d="M9 18h6"/><path d="M10 22h4"/>';
  const summaryText = el("span", "reasoning-text", "思考中…");
  // chevron 也换成 SVG(右侧三角)
  const chevron = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  chevron.setAttribute("class", "chevron");
  chevron.setAttribute("viewBox", "0 0 24 24");
  chevron.setAttribute("fill", "none");
  chevron.setAttribute("stroke", "currentColor");
  chevron.setAttribute("stroke-width", "2");
  chevron.setAttribute("stroke-linecap", "round");
  chevron.setAttribute("stroke-linejoin", "round");
  chevron.setAttribute("width", "12");
  chevron.setAttribute("height", "12");
  chevron.setAttribute("aria-hidden", "true");
  chevron.innerHTML = '<polyline points="9 6 15 12 9 18"/>';
  summary.appendChild(glyph);
  summary.appendChild(summaryText);
  summary.appendChild(chevron);
  details.appendChild(summary);
  const body = el("div", "reasoning-body");
  details.appendChild(body);
  // §5.6：思考块进「处理状态」块（与工具、阶段日志同处）。
  r.activity.appendChild(details);
  r.hadReasoning = true;
  r.reasoning = details;
  r.reasoningBody = body;
  r.summaryText = summaryText;
  r.chevron = chevron;
  r.reasoningAcc = "";
  r.reasoningSealed = false;
  r._reasoningRenderer = null; // 新段用全新渲染器（指向新 body）
}

// ── markdown 渲染 ────────────────────────────────────────────
// 补齐未闭合的 ``` fence（流式期间 ``` 是常态，marked 解析会丢内容）。
// 同时保护未闭合的 ` inline code（避免被错配成粗体）。
function closeUnbalanced(md) {
  let fence = (md.match(/```/g) || []).length;
  let s = md;
  while (fence % 2 === 1) { s += "\n```"; fence += 1; }
  // inline code：奇数个反引号会乱配对，简单追加一个收尾
  const backticks = (s.match(/`/g) || []).length;
  if (backticks % 2 === 1) s += "`";
  return s;
}

// §5.5 D1 XSS 净化（安全命脉）：页面与能跑 shell/code 的 API 同源 → XSS≈宿主任意命令执行
//（注入脚本可自建 WS 发 user_message 带 code_execution）。bot 输出（尤其联网搜索/工具结果）
// 不可信。旧正则净化不可靠（单引号 javascript: 穿透、不拦 iframe srcdoc/object/embed/base）。
// 改用**浏览器真 HTML 解析器 + 危险元素/属性黑名单**（vanilla、可审计，强于正则）。
const DANGEROUS_TAGS = new Set([
  "SCRIPT", "IFRAME", "OBJECT", "EMBED", "BASE", "LINK", "META", "FORM",
  "INPUT", "BUTTON", "TEXTAREA", "SELECT", "OPTION", "STYLE", "SVG", "MATH",
  "FRAME", "FRAMESET", "APPLET", "NOSCRIPT", "TEMPLATE",
]);
const URL_ATTRS = new Set(["href", "src", "xlink:href", "data", "action", "formaction", "poster", "background"]);
function sanitize(html) {
  const doc = new DOMParser().parseFromString(String(html), "text/html");
  doc.body.querySelectorAll("*").forEach((node) => {
    if (DANGEROUS_TAGS.has(node.tagName)) {
      node.remove();
      return;
    }
    for (const attr of [...node.attributes]) {
      const name = attr.name.toLowerCase();
      // 事件处理器一律剥；srcdoc 永不允许（iframe 已删，防御性再剥）。
      if (name.startsWith("on") || name === "srcdoc") {
        node.removeAttribute(attr.name);
        continue;
      }
      if (URL_ATTRS.has(name)) {
        const v = attr.value.replace(/\s+/g, "").toLowerCase();
        const bad =
          v.startsWith("javascript:") ||
          v.startsWith("vbscript:") ||
          (v.startsWith("data:") && !v.startsWith("data:image/"));
        if (bad) node.removeAttribute(attr.name);
      }
    }
  });
  return doc.body.innerHTML;
}

const SYNTAX_KEYWORDS = new Set([
  "as", "async", "await", "break", "case", "catch", "class", "const", "continue",
  "crate", "def", "default", "do", "else", "enum", "export", "extends", "fn", "for",
  "from", "function", "if", "impl", "import", "in", "interface", "let", "loop",
  "match", "mod", "mut", "new", "package", "pub", "return", "self", "static",
  "struct", "super", "switch", "this", "throw", "trait", "try", "type", "use",
  "var", "while", "yield",
]);

const SYNTAX_LITERALS = new Set([
  "False", "None", "Some", "True", "false", "null", "undefined", "true",
]);

function escapeHtml(value) {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function languageOf(code) {
  const match = code.className.match(/(?:^|\s)language-([\w-]+)/);
  return match ? match[1].toLowerCase() : "";
}

function isWordStart(ch) {
  return /[A-Za-z_]/.test(ch);
}

function isWord(ch) {
  return /[A-Za-z0-9_]/.test(ch);
}

function syntaxSpan(kind, text) {
  return `<span class="syntax-${kind}">${escapeHtml(text)}</span>`;
}

const LATEX_SYMBOLS = new Map(Object.entries({
  alpha: "α", beta: "β", gamma: "γ", delta: "δ", epsilon: "ε", zeta: "ζ",
  eta: "η", theta: "θ", iota: "ι", kappa: "κ", lambda: "λ", mu: "μ",
  nu: "ν", xi: "ξ", pi: "π", rho: "ρ", sigma: "σ", tau: "τ",
  upsilon: "υ", phi: "φ", chi: "χ", psi: "ψ", omega: "ω",
  Gamma: "Γ", Delta: "Δ", Theta: "Θ", Lambda: "Λ", Xi: "Ξ", Pi: "Π",
  Sigma: "Σ", Phi: "Φ", Psi: "Ψ", Omega: "Ω",
  times: "×", cdot: "·", pm: "±", mp: "∓", div: "÷",
  le: "≤", leq: "≤", ge: "≥", geq: "≥", neq: "≠", approx: "≈",
  infty: "∞", partial: "∂", nabla: "∇", forall: "∀", exists: "∃",
  in: "∈", notin: "∉", subset: "⊂", subseteq: "⊆", cup: "∪", cap: "∩",
  to: "→", rightarrow: "→", leftarrow: "←", leftrightarrow: "↔",
  sum: "∑", prod: "∏", int: "∫", lim: "lim", log: "log", ln: "ln",
  sin: "sin", cos: "cos", tan: "tan", min: "min", max: "max",
}));

function readLatexGroup(src, start) {
  if (src[start] !== "{") return null;
  let depth = 1;
  let i = start + 1;
  while (i < src.length && depth > 0) {
    if (src[i] === "\\" && i + 1 < src.length) {
      i += 2;
      continue;
    }
    if (src[i] === "{") depth += 1;
    if (src[i] === "}") depth -= 1;
    i += 1;
  }
  if (depth !== 0) return null;
  return { value: src.slice(start + 1, i - 1), end: i };
}

function readLatexScript(src, start) {
  if (src[start] === "{") {
    const group = readLatexGroup(src, start);
    if (group) return group;
  }
  if (src[start] === "\\") {
    const command = src.slice(start + 1).match(/^[A-Za-z]+/);
    if (command) return { value: src.slice(start, start + command[0].length + 1), end: start + command[0].length + 1 };
  }
  return { value: src[start] || "", end: Math.min(src.length, start + 1) };
}

function renderLatex(src) {
  let out = "";
  let i = 0;
  while (i < src.length) {
    const ch = src[i];

    if (ch === "\\") {
      const name = src.slice(i + 1).match(/^[A-Za-z]+/);
      if (!name) {
        out += escapeHtml(src[i + 1] || "\\");
        i += src[i + 1] ? 2 : 1;
        continue;
      }
      const command = name[0];
      i += command.length + 1;
      if (command === "frac") {
        const numerator = readLatexGroup(src, i);
        const denominator = numerator ? readLatexGroup(src, numerator.end) : null;
        if (numerator && denominator) {
          out += `<span class="math-frac"><span>${renderLatex(numerator.value)}</span><span>${renderLatex(denominator.value)}</span></span>`;
          i = denominator.end;
          continue;
        }
      }
      if (command === "sqrt") {
        const body = readLatexGroup(src, i);
        if (body) {
          out += `<span class="math-sqrt">${renderLatex(body.value)}</span>`;
          i = body.end;
          continue;
        }
      }
      if (command === "left" || command === "right") {
        if (src[i]) {
          out += escapeHtml(src[i]);
          i += 1;
        }
        continue;
      }
      out += escapeHtml(LATEX_SYMBOLS.get(command) || command);
      continue;
    }

    if (ch === "^" || ch === "_") {
      const body = readLatexScript(src, i + 1);
      const tag = ch === "^" ? "sup" : "sub";
      out += `<${tag}>${renderLatex(body.value)}</${tag}>`;
      i = body.end;
      continue;
    }

    if (ch === "{") {
      const group = readLatexGroup(src, i);
      if (group) {
        out += renderLatex(group.value);
        i = group.end;
        continue;
      }
    }

    out += escapeHtml(ch);
    i += 1;
  }
  return out;
}

function findMathDelim(text, start, delim) {
  let i = start;
  while (i < text.length) {
    const hit = text.indexOf(delim, i);
    if (hit === -1) return -1;
    if (hit > 0 && text[hit - 1] === "\\") {
      i = hit + delim.length;
      continue;
    }
    return hit;
  }
  return -1;
}

function splitMathText(text) {
  const parts = [];
  let i = 0;
  while (i < text.length) {
    const displayStart = findMathDelim(text, i, "$$");
    const inlineStart = findMathDelim(text, i, "$");
    let start = -1;
    let display = false;
    if (displayStart !== -1 && (inlineStart === -1 || displayStart <= inlineStart)) {
      start = displayStart;
      display = true;
    } else if (inlineStart !== -1) {
      start = inlineStart;
    }
    if (start === -1) {
      parts.push({ text: text.slice(i) });
      break;
    }
    if (start > i) parts.push({ text: text.slice(i, start) });
    const delim = display ? "$$" : "$";
    const end = findMathDelim(text, start + delim.length, delim);
    if (end === -1) {
      parts.push({ text: text.slice(start) });
      break;
    }
    const latex = text.slice(start + delim.length, end).trim();
    if (latex) parts.push({ latex, display });
    i = end + delim.length;
  }
  return parts;
}

function shouldSkipMathNode(node) {
  let el = node.parentElement;
  while (el) {
    if (["CODE", "PRE", "SCRIPT", "STYLE", "TEXTAREA"].includes(el.tagName)) return true;
    if (el.classList && el.classList.contains("math")) return true;
    el = el.parentElement;
  }
  return false;
}

function decorateMath(root) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const nodes = [];
  while (walker.nextNode()) {
    const node = walker.currentNode;
    if (!node.nodeValue.includes("$") || shouldSkipMathNode(node)) continue;
    nodes.push(node);
  }
  for (const node of nodes) {
    const parts = splitMathText(node.nodeValue);
    if (!parts.some((part) => part.latex)) continue;
    const fragment = document.createDocumentFragment();
    for (const part of parts) {
      if (part.text) {
        fragment.appendChild(document.createTextNode(part.text.replace(/\\\$/g, "$")));
      } else {
        const span = document.createElement("span");
        span.className = part.display ? "math math-display" : "math math-inline";
        span.innerHTML = renderLatex(part.latex);
        fragment.appendChild(span);
      }
    }
    node.parentNode.replaceChild(fragment, node);
  }
}

function highlightCodeText(code, lang) {
  const hashCommentLangs = new Set(["bash", "fish", "py", "python", "rb", "ruby", "sh", "shell", "toml", "yaml", "yml"]);
  let html = "";
  let i = 0;
  while (i < code.length) {
    const ch = code[i];
    const next = code[i + 1] || "";

    if ((ch === "/" && next === "/") || (hashCommentLangs.has(lang) && ch === "#")) {
      const end = code.indexOf("\n", i);
      const j = end === -1 ? code.length : end;
      html += syntaxSpan("comment", code.slice(i, j));
      i = j;
      continue;
    }

    if (ch === "/" && next === "*") {
      const end = code.indexOf("*/", i + 2);
      const j = end === -1 ? code.length : end + 2;
      html += syntaxSpan("comment", code.slice(i, j));
      i = j;
      continue;
    }

    if (ch === "<" && code.slice(i, i + 4) === "<!--") {
      const end = code.indexOf("-->", i + 4);
      const j = end === -1 ? code.length : end + 3;
      html += syntaxSpan("comment", code.slice(i, j));
      i = j;
      continue;
    }

    if (ch === "\"" || ch === "'" || ch === "`") {
      const quote = ch;
      let j = i + 1;
      while (j < code.length) {
        if (code[j] === "\\") {
          j += 2;
          continue;
        }
        if (code[j] === quote) {
          j += 1;
          break;
        }
        j += 1;
      }
      html += syntaxSpan("string", code.slice(i, j));
      i = j;
      continue;
    }

    if (/\d/.test(ch)) {
      const match = code.slice(i).match(/^\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/);
      if (match) {
        html += syntaxSpan("number", match[0]);
        i += match[0].length;
        continue;
      }
    }

    if (isWordStart(ch)) {
      let j = i + 1;
      while (j < code.length && isWord(code[j])) j += 1;
      const word = code.slice(i, j);
      if (SYNTAX_KEYWORDS.has(word)) {
        html += syntaxSpan("keyword", word);
      } else if (SYNTAX_LITERALS.has(word)) {
        html += syntaxSpan("literal", word);
      } else {
        html += escapeHtml(word);
      }
      i = j;
      continue;
    }

    html += escapeHtml(ch);
    i += 1;
  }
  return html;
}

function decorateMarkdown(root) {
  decorateMath(root);
  root.querySelectorAll("pre > code").forEach((code) => {
    const lang = languageOf(code);
    if (lang) code.parentElement.dataset.lang = lang;
    code.innerHTML = highlightCodeText(code.textContent, lang);
  });
  decorateResourceRefs(root);
}

function renderMarkdown(md) {
  if (!window.marked) return md; // 解析器未就绪时退化到纯文本
  const safe = closeUnbalanced(md);
  return sanitize(window.marked.parse(safe));
}

// ── 意图感知滚动 ─────────────────────────────────────────────
const REATTACH_PX = 24;
// §5.5 A3：pre-start 占位 run 的临时 id；start 到达时重对账为真实 run_id。
const PENDING_RUN = "__pending__";

function pinIfFollowing(session = activeSession()) {
  // 跟随策略:只要用户没主动上滑(wheel↑),就一直跟到底。
  // 不设时间窗口,因为流式输出可能任意慢(LLM 思考几秒后才出第一个 token);
  // 短窗口会让流中段"丢"跟随,用户看到内容不滚。
  if (!session || session.id !== activeSessionId || session.userDetached) return;
  transcript.scrollTop = transcript.scrollHeight;
}

transcript.addEventListener("wheel", (e) => {
  if (e.deltaY < 0) {
    // 向上滚 = 脱离跟随
    const session = activeSession();
    if (session) session.userDetached = true;
  }
  // 向下滚不动手:已经在底部,自然保持跟随
}, { passive: true });

// §5.5 A6 触摸脱离（移动端）：手指下滑（内容上移=回看早先）即脱离跟随，与 wheel↑ 对称。
let _touchY = null;
transcript.addEventListener("touchstart", (e) => {
  _touchY = e.touches[0]?.clientY ?? null;
}, { passive: true });
transcript.addEventListener("touchmove", (e) => {
  if (_touchY == null) return;
  const y = e.touches[0]?.clientY ?? _touchY;
  if (y - _touchY > 6) {
    // 手指下移 → 视口上滚 → 脱离
    const session = activeSession();
    if (session) session.userDetached = true;
  }
  _touchY = y;
}, { passive: true });

transcript.addEventListener("scroll", () => {
  const dist = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight;
  if (dist <= REATTACH_PX) {
    // 滑到底部 → 重新跟随
    const session = activeSession();
    if (session) session.userDetached = false;
  }
}, { passive: true });

// ── rAF 节流的"答案"渲染器 ──────────────────────────────────
const answerRenderers = new WeakMap(); // run -> { pending, scheduled, render }

function ensureAnswerRenderer(r) {
  if (answerRenderers.has(r)) return answerRenderers.get(r);
  const state = { pending: "", scheduled: false, lastShown: "" };
  const render = () => {
    state.scheduled = false;
    if (!r.answer) return; // 段已被 stream_reset 清掉时,丢弃排队中的旧帧
    if (state.pending === state.lastShown) return;
    // 单调追加保护：pending 必须是 lastShown 的前缀才信任；否则直接展示新值（覆盖异常流）
    const txt = state.pending;
    if (txt.startsWith(state.lastShown)) {
      state.lastShown = txt;
    } else {
      state.lastShown = txt;
    }
    const html = renderMarkdown(txt);
    // §5.5 A6 滚动稳定器：detached（用户上翻阅读）时，重渲染（代码块高亮/重排）会改变
    // 上方内容高度 → 视口"跳动"。重设 innerHTML 前后量高度差，把 scrollTop 补偿回去，
    // 让用户正在看的内容锚定不动（仅 detached 时；following 时本就贴底无需锚定）。
    const detached = activeSession()?.userDetached;
    const prevH = detached ? transcript.scrollHeight : 0;
    const prevTop = detached ? transcript.scrollTop : 0;
    r.answer.innerHTML = html;
    decorateMarkdown(r.answer);
    // 只在流式进行中追加 caret：done 事件后 r.streaming=false，
    // 即使还有一帧 rAF 排队也不会再 append 闪烁元素。
    if (r.streaming) {
      r.answer.appendChild(r.caret);
    } else if (r.caret.parentNode) {
      r.caret.parentNode.removeChild(r.caret);
    }
    if (detached) {
      const delta = transcript.scrollHeight - prevH;
      if (delta !== 0) transcript.scrollTop = prevTop + delta; // 锚定：抵消上方高度变化
    } else {
      pinIfFollowing();
    }
  };
  const schedule = () => {
    if (state.scheduled) return;
    state.scheduled = true;
    requestAnimationFrame(render);
  };
  const fn = (newText) => { state.pending = newText; schedule(); };
  answerRenderers.set(r, { state, render, schedule, push: fn });
  return answerRenderers.get(r);
}

function ensureReasoningRenderer(r) {
  if (r._reasoningRenderer) return r._reasoningRenderer;
  const state = { pending: "", scheduled: false, lastShown: "" };
  const render = () => {
    state.scheduled = false;
    if (state.pending === state.lastShown) return;
    state.lastShown = state.pending;
    const html = renderMarkdown(state.pending);
    r.reasoningBody.innerHTML = html;
    decorateMarkdown(r.reasoningBody);
    // reasoning 折叠时不需要 pin（外层 transcript 在管）
  };
  const schedule = () => {
    if (state.scheduled) return;
    state.scheduled = true;
    requestAnimationFrame(render);
  };
  const fn = (newText) => { state.pending = newText; schedule(); };
  r._reasoningRenderer = { state, render, schedule, push: fn };
  return r._reasoningRenderer;
}

// ── 交错渲染 ─────────────────────────────────────────────────
// 文本与工具/审批按事件到达顺序交错：每段连续 token 渲染为一个独立 .answer 段；
// 工具/审批/错误到达时 sealAnswer 封口当前段(移除光标、停 streaming),下一个 token
// 由 ensureAnswerSegment 另起一个新 .answer 追加在该卡片之后。这样多步工具调用
// (说一句→调一次→再说一句)的叙事顺序与事件流一致,而非把全部文字堆在工具之上。
function ensureAnswerSegment(r) {
  if (r.answer && !r.answerSealed) return r.answer;
  const seg = el("div", "answer streaming");
  r.el.appendChild(seg);
  r.answer = seg;
  r.acc = "";
  r.answerSealed = false;
  answerRenderers.delete(r); // 新段用全新的累加器/渲染器,避免沿用上一段文本
  return seg;
}

function sealAnswer(r) {
  if (r.answer) {
    r.answer.classList.remove("streaming");
    if (r.caret.parentNode) r.caret.parentNode.removeChild(r.caret);
  }
  r.answerSealed = true;
}

// §5.5 A4：封口当前 reasoning 段（折叠 + 收尾文字）；下一波 reasoning 另起新段。
function sealReasoning(r) {
  if (r.reasoning && !r.reasoningSealed) {
    r.reasoning.open = false;
    if (r.summaryText) r.summaryText.textContent = "思考过程";
  }
  r.reasoningSealed = true;
}

// ── 事件处理 ─────────────────────────────────────────────────
function handle(ev, session = activeSession()) {
  if (!session) return;
  if (ev.type === "pong") return;

  switch (ev.type) {
    case "start": {
      // §5.5 A3 重对账：把 send() 预建的 pending 占位 run 收编为真实 run_id（避免重复活动行）。
      if (!ev.parent_id && session.runs.has(PENDING_RUN) && !session.runs.has(ev.run_id)) {
        const p = session.runs.get(PENDING_RUN);
        session.runs.delete(PENDING_RUN);
        session.runs.set(ev.run_id, p);
        if (session.topRun === PENDING_RUN) session.topRun = ev.run_id;
      }
      const r = ensureRun(ev, session);
      r.streaming = true;
      activitySet(r, "调用模型…");
      if (!ev.parent_id) {
        session.topRun = ev.run_id;
        session.userDetached = false;
      }
      break;
    }
    case "token": {
      const r = ensureRun(ev, session);
      sealReasoning(r); // §5.5 A4：答案开始 → 封口当前思考段（下一波思考另起新段）
      ensureAnswerSegment(r); // 若上一段被工具封口,这里另起新段(追加在工具之后)
      r.acc += ev.text;
      activitySet(r, "生成回答…");
      ensureAnswerRenderer(r).push(r.acc);
      break;
    }
    case "reasoning": {
      const r = ensureRun(ev, session);
      r.reasoningAcc += ev.text;
      ensureReasoningDOM(r);          // 首次 reasoning 事件才建 details
      r.reasoning.open = true;          // 流式期间保持打开
      r.summaryText.textContent = "思考中…";
      activitySet(r, "思考中…");
      ensureReasoningRenderer(r).push(r.reasoningAcc);
      break;
    }
    case "tool_start": {
      const r = ensureRun(ev, session);
      sealAnswer(r); // 封口当前文本段,工具卡片插在其后;后续 token 另起新段
      sealReasoning(r); // §5.5 A4：工具开始 → 封口当前思考段
      const t = el("div", "tool");
      t.appendChild(el("span", "name", ev.name));
      t.appendChild(document.createTextNode(`(${JSON.stringify(ev.args)})`));
      t.appendChild(el("span", "mark", " …"));
      r.tools.set(ev.call_id, t);
      r.activity.appendChild(t); // §5.6：工具卡进「处理状态」块
      r.toolCount += 1;
      activitySet(r, `执行 ${ev.name}…`);
      break;
    }
    case "tool_end": {
      const r = ensureRun(ev, session);
      const t = r.tools.get(ev.call_id);
      if (t) {
        t.classList.add(ev.ok ? "ok" : "err");
        t.querySelector(".mark").textContent = ev.ok ? " ✓" : " ✗";
        const fullText = typeof ev.result === "string" ? ev.result : JSON.stringify(ev.result, null, 2);
        const result = el("span", "result", fullText);
        decorateResourceRefs(result);
        t.appendChild(result);
        // §5.6 画布查看：结果较长或是图片 → 给个「⤢ 在画布查看」按钮，点开右侧画布看全文（参前身 datoobot）。
        const img = imageSrc(fullText);
        if (img || (fullText && fullText.length > 200)) {
          const pin = el("button", "tool-canvas-btn", img ? "⤢ 在画布查看图片" : "⤢ 在画布查看全文");
          pin.type = "button";
          const title = ev.name || "工具结果";
          pin.addEventListener("click", () => openCanvasContent(title, fullText));
          t.appendChild(pin);
        }
      }
      break;
    }
    case "diagnostics": {
      const r = ensureRun(ev, session);
      sealAnswer(r);
      sealReasoning(r); // §5.5 A4
      const t = el("div", `tool diagnostics ${ev.ok ? "ok" : "err"}`);
      t.appendChild(el("span", "name", "diagnostics"));
      t.appendChild(el("span", "mark", ev.ok ? " ✓" : " ✗"));
      const result = el("span", "result", ev.summary || "");
      t.appendChild(result);
      r.activity.appendChild(t); // §5.6：诊断卡进「处理状态」块
      break;
    }
    case "stream_reset": {
      // §2.6：流中途失败重放前清空本 run 已 emit 的部分答案/推理，避免重放重复输出。
      const r = session.runs.get(ev.run_id);
      if (r) {
        activitySet(r, "重试中…");
        // 交错渲染下,已 emit 的可能是多个 .answer 段 + 工具卡片 + 思考段;全部清掉再重放。
        // §5.5 D5：reasoning 与 answer **对称**全清——原先只清 .answer/.tool DOM、且 reasoningSealed
        // 不复位，重放时 ensureReasoningDOM 见 reasoningSealed 仍 true 会另建第二个思考块（重复）。
        r.el.querySelectorAll(":scope > .answer").forEach((n) => n.remove());
        // §5.6：工具/思考已移进「处理状态」块，从那里清。
        if (r.activity) r.activity.querySelectorAll(":scope > .tool, :scope > .reasoning").forEach((n) => n.remove());
        r.tools.clear();
        r.toolCount = 0;
        r.acc = "";
        r.answer = null;
        r.answerSealed = false;
        answerRenderers.delete(r);
        r.reasoning = null;
        r.reasoningSealed = false;
        r.reasoningAcc = "";
        r.reasoningBody = null;
        r._reasoningRenderer = null;
      }
      break;
    }
    case "debug": {
      // A2：调试细节行（llm payload / 完整 tool result 等）。data-lv="debug"，
      // 默认隐藏，详细度切到「详细」时由 CSS 即时显示。
      const r = ensureRun(ev, session);
      const row = el("div", "dbg");
      row.dataset.lv = "debug";
      const det = el("details", "dbg-det");
      const sum = el("summary", "dbg-sum", ev.label || "debug");
      det.appendChild(sum);
      const body = typeof ev.data === "string" ? ev.data : JSON.stringify(ev.data, null, 2);
      det.appendChild(el("pre", "dbg-body", body));
      row.appendChild(det);
      r.el.appendChild(row);
      break;
    }
    case "approval_request": {
      const r = ensureRun(ev, session);
      sealAnswer(r);
      const card = el("div", "approval");
      card.dataset.approvalId = ev.approval_id;
      card.dataset.state = "pending";
      card.appendChild(el("div", "approval-title", `${ev.name} 需要确认`));
      card.appendChild(el("div", "approval-reason", ev.reason || "需要审批"));
      const args = el("pre", "approval-args", JSON.stringify(ev.args ?? {}, null, 2));
      card.appendChild(args);
      const actions = el("div", "approval-actions");
      // §2.11 四档：仅这次 / 本会话 / 永久 / 拒绝。decision 字段表达四档；approved 布尔向后兼容。
      const specs = [
        ["allow", "允许", "once"],
        ["allow", "本会话", "session"],
        ["allow", "永久", "always"],
        ["deny", "拒绝", "deny"],
      ];
      const btns = specs.map(([kind, label]) => el("button", `approval-btn ${kind}`, label));
      const respond = (decision) => {
        if (!ws || ws.readyState !== WebSocket.OPEN || card.dataset.state !== "pending") return;
        const approved = decision !== "deny";
        card.dataset.state = approved ? "approved" : "denied";
        btns.forEach((b) => {
          b.disabled = true;
        });
        ws.send(JSON.stringify({
          type: "approval",
          session_id: session.id,
          approval_id: ev.approval_id,
          approved,
          decision,
        }));
      };
      btns.forEach((b, i) => b.addEventListener("click", () => respond(specs[i][2])));
      actions.append(...btns);
      card.appendChild(actions);
      r.approvals.set(ev.approval_id, card);
      r.el.appendChild(card);
      break;
    }
    case "approval_resolved": {
      const r = ensureRun(ev, session);
      const card = r.approvals.get(ev.approval_id);
      if (card) {
        card.dataset.state = ev.approved ? "approved" : "denied";
        const title = card.querySelector(".approval-title");
        if (title) title.textContent = ev.approved ? "已允许" : "已拒绝";
        card.querySelectorAll("button").forEach((btn) => { btn.disabled = true; });
      }
      break;
    }
    case "usage": {
      // §2.7 token live：累计已花 token（+ 可选预算），仅顶层 run 计入会话用量，活动会话即时渲染。
      if (!ev.run_id || ev.run_id === session.topRun || !session.topRun) {
        session.usage = { spent: ev.spent || 0, budget: ev.budget ?? null };
        if (session.id === activeSessionId) { renderTokenUsage(session.usage); renderBooters(); }
      }
      break;
    }
    case "done": {
      const r = session.runs.get(ev.run_id);
      if (r) {
        r.streaming = false;
        activityFreeze(r, "完成");
        if (r.answer) r.answer.classList.remove("streaming"); // 可能整轮无文本(只有工具)
        if (r.caret && r.caret.parentNode) r.caret.parentNode.removeChild(r.caret);
        // §5.5 A4：收口当前思考段（已封口的旧段保持折叠）。
        sealReasoning(r);
      }
      if (ev.run_id === session.topRun) { setBusy(false, session); session.topRun = null; }
      pinIfFollowing(session);
      break;
    }
    case "error": {
      if (ev.run_id) {
        const r = ensureRun(ev, session);
        sealAnswer(r);
        activityFreeze(r, "出错");
        r.el.appendChild(el("div", "error", "✗ " + ev.message));
      } else {
        session.pane.appendChild(el("div", "error", "✗ " + ev.message));
      }
      if (ev.run_id === session.topRun) { setBusy(false, session); session.topRun = null; }
      break;
    }
    case "turn_complete": {
      if (session.topRun) activityFreeze(session.runs.get(session.topRun), "完成");
      setBusy(false, session);
      session.topRun = null;
      pinIfFollowing(session);
      break;
    }
    case "cancel_complete":
    case "shutdown_complete": {
      // 取消/关闭完成：复位 busy，停止按钮变回发送（即使 Error 事件未送达也兜底）。
      if (session.topRun) activityFreeze(session.runs.get(session.topRun), "已取消");
      setBusy(false, session);
      session.topRun = null;
      break;
    }
  }
}

function turnOpts() {
  return {
    ...(thinkEnabled ? { thinking: true } : {}),
    ...(searchEnabled ? { web_search: true } : {}),
    ...(codeEnabled ? { code_execution: true } : {}),
    ...(recallEnabled ? { force_recall: true } : {}),
  };
}

// 待引导队列(§5.5 B6·方案A)：忙时输入暂存于 composer 上方,点「放行」才作为 steer 发出;
// 未放行可逐条 × 删除 = 真取消(没发后端)。空闲时「放行」= 合并起一条新 user_message。
function renderPendingSteers(session = activeSession()) {
  if (!pendingSteersEl) return;
  pendingSteersEl.innerHTML = "";
  const pend = (session && session.pendingSteers) || [];
  if (!pend.length) return;
  pend.forEach((p, i) => {
    const chip = el("div", "pending-steer-chip");
    chip.appendChild(el("span", "pending-steer-text", p.text || `[${(p.images || []).length} 张图片]`));
    const rm = el("button", "pending-steer-remove", "×");
    rm.type = "button";
    rm.setAttribute("aria-label", "移除待引导");
    rm.addEventListener("click", () => {
      session.pendingSteers.splice(i, 1);
      renderPendingSteers(session);
    });
    chip.appendChild(rm);
    pendingSteersEl.appendChild(chip);
  });
  const flush = el("button", "pending-steer-flush", session.busy ? "放行引导" : "发送");
  flush.type = "button";
  flush.addEventListener("click", () => flushPendingSteers(session));
  pendingSteersEl.appendChild(flush);
}

function flushPendingSteers(session = activeSession()) {
  const pend = (session && session.pendingSteers) || [];
  if (!pend.length || !ws || ws.readyState !== WebSocket.OPEN) return;
  if (session.busy) {
    // 忙：逐条作为 steer 注入运行中的 turn
    for (const p of pend) {
      ws.send(JSON.stringify({
        type: "steer", session_id: session.id, bot_id: session.botId,
        text: p.text, images: p.images, ...p.opts,
      }));
      session.pane.appendChild(makeUserBubble(p.text, p.images, "steer"));
    }
    session.serverKnown = true;
    session.pendingSteers = [];
    renderPendingSteers(session);
    session.userDetached = false;
    transcript.scrollTop = transcript.scrollHeight;
  } else {
    // 空闲：合并为一条 user_message 起新 turn
    const text = pend.map((p) => p.text).filter(Boolean).join("\n");
    const images = pend.flatMap((p) => p.images || []);
    const opts = {
      ...(pend.some((p) => p.opts.thinking) ? { thinking: true } : {}),
      ...(pend.some((p) => p.opts.web_search) ? { web_search: true } : {}),
      ...(pend.some((p) => p.opts.code_execution) ? { code_execution: true } : {}),
      ...(pend.some((p) => p.opts.force_recall) ? { force_recall: true } : {}),
    };
    session.pendingSteers = [];
    renderPendingSteers(session);
    ensureServerSession(session).then(() => {
      renameSession(session, text || "图片消息");
      session.pane.appendChild(makeUserBubble(text, images));
      setBusy(true, session);
      session.runs.clear(); session.topRun = null;
      session.serverKnown = true;
      ws.send(JSON.stringify({
        type: "user_message", session_id: session.id, bot_id: session.botId, text, images, ...opts,
      }));
      session.userDetached = false;
      transcript.scrollTop = transcript.scrollHeight;
    });
  }
}

async function send() {
  // §5.6：处于群聊视图 → 发言给 team（leader 编排），不走 1:1 会话路径。
  if (activeTeamId) {
    const t = input.value.trim();
    if (!t) return;
    input.value = "";
    autoSize();
    sendToTeam(t);
    return;
  }
  const session = activeSession() || ensureSession(makeSessionId());
  const text = input.value.trim();
  const images = attachments.map((a) => a.dataUrl);
  if ((!text && images.length === 0) || !ws || ws.readyState !== WebSocket.OPEN) return;

  // 忙时不立即发——进待引导队列(本地暂存,可取消)。点「放行」才发。
  if (session.busy) {
    session.pendingSteers = session.pendingSteers || [];
    session.pendingSteers.push({ text, images, opts: turnOpts() });
    input.value = "";
    attachments = [];
    renderAttachments();
    autoSize();
    renderPendingSteers(session);
    return;
  }

  // A3（§5.5 乐观回显）：气泡 + busy + 清空 composer 在 await 之前同步完成——
  // 自定义 bot 首条消息也 0 延迟反馈（text/images 已存入局部变量，提前清空安全）。
  renameSession(session, text || (images.length ? "图片消息" : ""));
  session.pane.appendChild(makeUserBubble(text, images));
  input.value = "";
  attachments = [];
  renderAttachments();
  autoSize(); // 清空后收起 + send 置灰
  setBusy(true, session);
  session.runs.clear(); session.topRun = null;
  session.userDetached = false;
  // §5.5 A3 余项：pre-start 活动行占位——发出即建一个 pending run，活动行立刻显示「调用模型…」，
  // 不等 start 事件（自定义 bot 首条 await + 网络期间也有即时反馈）。start 到达时重对账收编。
  const pending = ensureRun({ run_id: PENDING_RUN, session_id: session.id }, session);
  pending.streaming = true;
  session.topRun = PENDING_RUN;
  activitySet(pending, "调用模型…");
  transcript.scrollTop = transcript.scrollHeight;

  // 仅自定义 bot 首条会真正 await（默认 bot 立即返回）。失败则回滚 busy，
  // 已回显的用户气泡保留 + 追加错误行，不再发后端。
  try {
    await ensureServerSession(session);
  } catch (e) {
    setBusy(false, session);
    // A3：发送失败 → 移除 pre-start 占位行（没有真实 run 会到来）。
    const p = session.runs.get(PENDING_RUN);
    if (p) { p.el.remove(); session.runs.delete(PENDING_RUN); }
    session.topRun = null;
    session.pane.appendChild(el("div", "error", "✗ " + (e && e.message ? e.message : "发送失败")));
    return;
  }
  session.serverKnown = true;
  ws.send(JSON.stringify({
    type: "user_message", session_id: session.id, bot_id: session.botId, text, images, ...turnOpts(),
  }));
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  // busy 时按钮是「停止」：点击取消运行中的 turn（而非发送/排队）。
  const session = activeSession();
  if (session && session.busy) {
    cancelTurn(session);
    return;
  }
  send().catch((err) => {
    console.warn("[send] failed", err);
    if (session) session.pane.appendChild(el("div", "error", "✗ " + err.message));
  });
});
input.addEventListener("keydown", (e) => {
  // §5.5 D2：中文 IME（拼音/五笔）确认候选词的回车 isComposing=true，绝不能当发送
  //（兜底 keyCode===229 = 旧浏览器 composing 标志）。否则全中文界面高频「半成品消息被发出」。
  if (e.key === "Enter" && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
    e.preventDefault();
    send().catch((err) => {
      console.warn("[send] failed", err);
      const session = activeSession();
      if (session) session.pane.appendChild(el("div", "error", "✗ " + err.message));
    });
  }
});

// ── Composer UX（unsloth 风格） ──────────────────────────────

// Pill toggle:点击切 data-active。三个 pill 都是 per-turn 开关:
// Think -> LlmOpts.thinking; Search/Code -> 本 turn 暴露对应工具。
document.querySelectorAll(".composer-pill-btn").forEach((btn) => {
  btn.addEventListener("click", () => {
    const active = btn.getAttribute("data-active") === "true";
    const next = !active;
    btn.setAttribute("data-active", next ? "true" : "false");
    btn.setAttribute("aria-pressed", next ? "true" : "false");
    if (btn.dataset.pill === "think") thinkEnabled = next;
    if (btn.dataset.pill === "search") searchEnabled = next;
    if (btn.dataset.pill === "code") codeEnabled = next;
    if (btn.dataset.pill === "recall") recallEnabled = next;
  });
});

// Textarea autosize(永远双行:composer 上行是 textarea,下行是 actions)
// 不再切 data-expanded;只控制 textarea 自身高度 + send 按钮 enabled。
const MAX_ROWS = 10;
function autoSize() {
  input.style.height = "auto";
  const lineHeight = parseFloat(getComputedStyle(input).lineHeight) || 24;
  const maxHeight = lineHeight * MAX_ROWS;
  const h = Math.min(maxHeight, input.scrollHeight);
  input.style.height = h + "px";
  const hasContent = input.value.trim().length > 0 || attachments.length > 0;
  const canSend = hasContent && ws && ws.readyState === WebSocket.OPEN;
  // busy 时按钮变「停止」：可点（即使无输入），点击发 cancel 取消运行中的 turn。
  const busy = !!(activeSession() && activeSession().busy);
  if (busy) {
    sendBtn.dataset.mode = "stop";
    sendBtn.disabled = false;
    sendBtn.setAttribute("aria-label", "停止");
  } else {
    sendBtn.dataset.mode = "send";
    sendBtn.disabled = !canSend;
    sendBtn.setAttribute("aria-label", "发送消息");
  }
}

// 发送 cancel：触发后端优雅取消运行中的 turn（后端工具/推理中途都会被打断）。
function cancelTurn(session = activeSession()) {
  if (!session || !ws || ws.readyState !== WebSocket.OPEN) return;
  ws.send(JSON.stringify({ type: "cancel", session_id: session.id }));
}
input.addEventListener("input", autoSize);
input.addEventListener("compositionend", autoSize);
autoSize();

// ── 附件(目前仅图片,data-URL) ───────────────────────────────

function renderAttachments() {
  attachmentsEl.innerHTML = "";
  attachments.forEach((a, i) => {
    const chip = document.createElement("div");
    chip.className = "attachment-chip";
    const img = document.createElement("img");
    img.src = a.dataUrl;
    img.alt = a.name;
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = a.name;
    const rm = document.createElement("button");
    rm.type = "button";
    rm.className = "remove";
    rm.setAttribute("aria-label", "Remove attachment");
    rm.innerHTML = '<svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"></path><path d="m6 6 12 12"></path></svg>';
    rm.addEventListener("click", () => {
      attachments.splice(i, 1);
      renderAttachments();
      autoSize();
    });
    chip.appendChild(img);
    chip.appendChild(name);
    chip.appendChild(rm);
    attachmentsEl.appendChild(chip);
  });
  // empty:hidden 切换通过 :empty 选择器(已写在 Tailwind class)
  autoSize();
}

addAttachmentBtn.addEventListener("click", () => fileInput.click());

fileInput.addEventListener("change", () => {
  let skipped = 0;
  Array.from(fileInput.files || []).forEach((f) => {
    if (!f.type.startsWith("image/")) { skipped++; return; } // 非图片（如 PDF）暂不支持上传
    const reader = new FileReader();
    reader.onload = () => {
      attachments.push({ name: f.name, dataUrl: String(reader.result) });
      renderAttachments();
    };
    reader.readAsDataURL(f);
  });
  if (skipped) flashComposerNotice(`暂只支持图片附件，已跳过 ${skipped} 个非图片文件（如需让 bot 读 PDF，把文件放进工作目录后让它用 pdf_read 读取）`);
  fileInput.value = "";
});

// 在 composer 附件区上方短暂提示（3.5s 自动消失）。非阻塞，不打断输入。
function flashComposerNotice(text) {
  if (!attachmentsEl) return;
  let n = attachmentsEl.querySelector(".composer-notice");
  if (!n) { n = el("div", "composer-notice"); attachmentsEl.appendChild(n); }
  n.textContent = text;
  clearTimeout(n._t);
  n._t = setTimeout(() => n.remove(), 3500);
}

// 拖拽:highlight dropzone(实际 drop 不在本任务范围,占位用)
let dragDepth = 0;
const hasFiles = (e) => Array.from(e.dataTransfer?.types ?? []).includes("Files");
dropzone.addEventListener("dragenter", (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  dragDepth += 1;
  dropzone.setAttribute("data-dragging", "true");
});
dropzone.addEventListener("dragover", (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
});
dropzone.addEventListener("dragleave", () => {
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) dropzone.setAttribute("data-dragging", "false");
});
dropzone.addEventListener("drop", (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  dragDepth = 0;
  dropzone.setAttribute("data-dragging", "false");
  // 本期不接 drop-to-attach,只清 highlight;用户用 + 按钮走同样流程
});

createBot({
  id: DEFAULT_BOT_ID,
  name: "botobot",
  workdir: nailWorkdir ? nailWorkdir.textContent : "botobot",
  button: defaultBotBtn,
  serverKnown: true,
});
switchBot(DEFAULT_BOT_ID);
connect();
// §2.8 持久化回读：重启/刷新后拉回 bot + 会话列表 + 历史，让侧边栏不再刷新即空。
restorePersisted();

// ── 记忆语义嵌入器加载指示 ───────────────────────────────────
// 启动时 candle bge 在后台线程加载(数秒),期间召回降级关键词、不阻塞使用。轮询
// /api/status,加载中显示带 spinner 的徽章,就绪后淡出移除;失败则静默移除(关键词召回够用)。
function initEmbedderStatusPill() {
  const header = document.querySelector("header");
  if (!header) return;
  let pill = null;
  const ensurePill = () => {
    if (pill) return pill;
    pill = el("span", "mem-pill");
    pill.setAttribute("aria-live", "polite");
    const spin = el("span", "mem-pill-spin");
    spin.setAttribute("aria-hidden", "true");
    pill.append(spin, el("span", "mem-pill-text", "记忆语义加载中…"));
    if (statusPill && statusPill.parentNode === header) {
      statusPill.insertAdjacentElement("afterend", pill);
    } else {
      header.insertBefore(pill, header.firstChild);
    }
    return pill;
  };
  const removePill = (delay = 0) => {
    if (!pill) return;
    const p = pill;
    pill = null;
    setTimeout(() => p.remove(), delay);
  };
  let stopped = false;
  let misses = 0;
  const tick = async () => {
    if (stopped) return;
    try {
      const res = await fetch("/api/status");
      const data = await res.json();
      misses = 0;
      if (data.embedder === "loading") {
        ensurePill();
        setTimeout(tick, 1200);
      } else if (data.embedder === "ready") {
        if (pill) {
          pill.dataset.state = "ready"; // 触发 CSS 淡出
          const t = pill.querySelector(".mem-pill-text");
          if (t) t.textContent = "记忆语义就绪";
          removePill(1600);
        }
        stopped = true;
      } else {
        removePill(0); // failed:保持关键词召回,不打扰
        stopped = true;
      }
    } catch (_) {
      // 端点不可用(老后端/启动初期):重试几次后放弃,不显示
      if (++misses >= 3) { removePill(0); stopped = true; return; }
      setTimeout(tick, 1200);
    }
  };
  tick();
}
initEmbedderStatusPill();
