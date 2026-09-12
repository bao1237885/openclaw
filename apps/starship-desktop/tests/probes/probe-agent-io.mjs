// 星舰壳层「智能体原生交互」回归：历史直达、select、悬停首帧兜底、动作可视化、
// 人机共存。
//
// 五组断言各自对着一条真实故障或一份刚接上的契约：
//   1. 地址栏历史（下拉行 + 回车 + 真实鼠标点击）在**没有活动标签**时必须开出一个
//      标签 —— 用户报的「点了历史、按了回车，浏览器不出来」就是这个；
//   2. `select`：value / label（大小写不敏感）/ index 三条成功路径与四条失败路径，
//      回执里的 `inputRoute: "dom_event"` 与 `detail.*` 必须能对上；
//   3. `hover` 的首帧兜底：回执带 `rewarmed`（布尔），页面 `:hover` 必须真的落上；
//   4. 可视化层：虚拟光标 / 点击涟漪 / 滚动指示画在页面里，且这一层**不能**被算成
//      遮挡 —— 算成遮挡就意味着壳层让位（原生视图换成截图替身），那正是白屏与
//      抖动的老路径。最后一组用一个带 `data-starship-visual="1"` 的假浮层来证伪。
//   5. 人机共存：用户和智能体**同时**用一个页面，只有撞上同一块地方才让路。
//      这一组要证伪的是「人手优先 = 全局静音」那种写法 —— 用户一边看页面、智能体
//      就一边停着，等于没人干活。所以每一条「让路」旁边都配了一条「放行」：
//      同一点让路、别处照走，观察类永不被挡，写字挡的是**那个框**。
//
// 这套会把面板里的标签全部关光：只有「零活动标签」才谈得上复现第 1 组，
// 所以只在开发实例上跑（见 README 第 2 条）。
//
// Usage: node probe-agent-io.mjs [cdpPort] [dashboardUrlMatch] [fixturePort]
import http from "node:http";

const CDP_PORT = process.argv[2] ?? "9334";
const DASHBOARD_MATCH = process.argv[3] ?? "chat/main";
const FIXTURE_PORT = Number(process.argv[4] ?? 18997);

// 夹具页要同时满足三件事：悬停有可验的颜色变化、`<select>` 有稳定的 value/label、
// 有一个能真正滚起来的容器。`aria-label` 是给元素扫描用的 —— 扫描的 `name`
// 优先取 `aria-label`，探针才拿得到 ref。
const PAGE = (title, body) => `<!doctype html>
<html><head><meta charset="utf-8"><title>${title}</title>
<style>
  body { margin:0; font:16px system-ui; }
  #hoverbtn { background: rgb(200,200,200); padding:18px; font-size:16px; }
  #hoverbtn:hover { background: rgb(10,20,30); }
  #scroller { height:200px; overflow:auto; border:2px solid #888; }
  #scroller .tall { height:1400px; }
</style></head>
<body>${body}</body></html>`;

const INDEX = PAGE(
  "Agent IO Fixture",
  `<h1 id="headline">Agent IO Fixture</h1>
   <button id="hoverbtn" aria-label="hover target">hover target</button>
   <select id="picker" aria-label="picker">
     <option value="red">Red</option>
     <option value="green">Green</option>
     <option value="blue">Blue</option>
   </select>
    <button id="plainbtn" aria-label="plain button">plain button</button>
    <input id="note" aria-label="note field" value="">
    <input id="other" aria-label="other field" value="">
    <div id="scroller"><div class="tall">scroll me</div></div>
   <script>
     window.__events = [];
     document.getElementById('hoverbtn').addEventListener('mouseover', function () {
       window.__events.push('mouseover:hoverbtn');
     });
     document.getElementById('hoverbtn').addEventListener('mouseenter', function () {
       window.__events.push('mouseenter:hoverbtn');
     });
     document.getElementById('picker').addEventListener('change', function (event) {
       window.__events.push('change:' + event.target.value);
     });
     document.getElementById('plainbtn').addEventListener('click', function () {
       window.__events.push('click:plain');
     });
   </script>`,
);
const NEXT = PAGE("Next Page", `<h1 id="headline">Next Page</h1>`);

const server = http.createServer((request, response) => {
  const body = request.url?.startsWith("/next") ? NEXT : INDEX;
  response.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
  });
  response.end(body);
});
await new Promise((resolve) => server.listen(FIXTURE_PORT, "127.0.0.1", resolve));
const indexUrl = `http://127.0.0.1:${FIXTURE_PORT}/index.html`;
const nextUrl = `http://127.0.0.1:${FIXTURE_PORT}/next.html`;
console.log(`fixture: ${indexUrl} / ${nextUrl}`);

const listTargets = async () =>
  (await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json()).filter(
    (entry) => entry.type === "page",
  );

const targets = await listTargets();
const dashboard = targets.find((entry) => (entry.url ?? "").includes(DASHBOARD_MATCH));
if (!dashboard) {
  console.error(
    `no dashboard matching "${DASHBOARD_MATCH}"; available: ` +
      targets.map((entry) => entry.url).join(", "),
  );
  server.close();
  process.exit(1);
}

const socket = new WebSocket(dashboard.webSocketDebuggerUrl);
const pending = new Map();
let nextId = 0;
function send(method, params, timeoutMs = 45000) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} timed out`));
    }, timeoutMs);
    pending.set(id, { resolve, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
}
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", reject, { once: true });
});
socket.addEventListener("message", (event) => {
  const payload = JSON.parse(event.data);
  if (!payload.id || !pending.has(payload.id)) {
    return;
  }
  const entry = pending.get(payload.id);
  pending.delete(payload.id);
  clearTimeout(entry.timer);
  if (payload.error) {
    entry.reject(new Error(JSON.stringify(payload.error)));
  } else {
    entry.resolve(payload.result);
  }
});

async function evaluate(expression, timeoutMs = 45000) {
  const result = await send(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true, userGesture: true },
    timeoutMs,
  );
  if (result.exceptionDetails) {
    throw new Error(JSON.stringify(result.exceptionDetails.exception ?? result.exceptionDetails));
  }
  return result.result?.value;
}
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let failures = 0;
function check(name, condition, detail) {
  const ok = Boolean(condition);
  if (!ok) {
    failures += 1;
  }
  console.log(
    `${ok ? "PASS" : "FAIL"}  ${name}${
      ok || detail === undefined ? "" : `  -> ${JSON.stringify(detail)}`
    }`,
  );
  return ok;
}

// 轮询到第一个真值为止；超时返回 null。断言只认最终态，中间的抖动留给轮询吸收。
async function until(probe, budgetMs, stepMs = 250) {
  const deadline = Date.now() + budgetMs;
  for (;;) {
    let value = null;
    try {
      value = await probe();
    } catch (error) {
      value = null;
    }
    if (value) {
      return value;
    }
    if (Date.now() >= deadline) {
      return null;
    }
    await sleep(stepMs);
  }
}

// ---------------------------------------------------------------- 面板口径

// 用户看的是某一个窗格；文档级查询会捞到缓存窗格里那块隐藏面板，驱动的就是另一个 WebView。
const PANEL_PRELUDE = `const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  const root = panel && panel.shadowRoot ? panel.shadowRoot : null;`;

const PANEL_STATE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  const controller = panel ? panel.browserPanelController : null;
  if (!controller) return { mounted: false };
  const tabs = (controller.tabs ?? []).map((tab) => ({ id: tab.id, url: tab.url ?? null }));
  return { mounted: true, activeTargetId: controller.activeTargetId ?? null, tabs };
})()`;
const TOGGLE_VISIBLE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const button = pane ? pane.querySelector(".chat-browser-panel-toggle") : null;
  if (!button) return { err: "no-toggle-button" };
  button.click();
  return { ok: true };
})()`;

// 下拉与替身都长在面板的 shadow root 里：document.querySelector 穿不过那层边界。
const ADDRESS_PROBE = `(() => {
  ${PANEL_PRELUDE}
  if (!root) return { err: "no-panel-root" };
  const input = root.querySelector(".bp-toolbar .bp-url");
  const menu = root.querySelector(".starship-addr__menu");
  const rows = menu ? Array.prototype.slice.call(menu.querySelectorAll(".starship-addr__row")) : [];
  const active = menu ? menu.querySelector('.starship-addr__row[data-active="1"]') : null;
  return {
    hasInput: Boolean(input),
    menuHidden: !menu || Boolean(menu.hidden),
    rows: rows.map((row) => ({
      url: row.__starshipUrl || null,
      label: String(row.textContent || "").replace(/\\s+/g, " ").trim().slice(0, 60),
    })),
    activeUrl: active ? active.__starshipUrl || null : null,
    standins: root.querySelectorAll("img.starship-standin").length,
  };
})()`;

const ADDRESS_FOCUS = `(() => {
  ${PANEL_PRELUDE}
  const input = root ? root.querySelector(".bp-toolbar .bp-url") : null;
  if (!input) return { ok: false, error: "no-address-input" };
  input.focus();
  input.dispatchEvent(new FocusEvent("focusin", { bubbles: true, composed: true }));
  return { ok: true };
})()`;

// 面板在工具栏上用捕获阶段听 `input`：只有从输入框冒上去的事件才带得到 value。
const addressType = (query) => `(() => {
  ${PANEL_PRELUDE}
  const input = root ? root.querySelector(".bp-toolbar .bp-url") : null;
  if (!input) return { ok: false, error: "no-address-input" };
  input.value = ${JSON.stringify(query)};
  input.dispatchEvent(new InputEvent("input", {
    bubbles: true, composed: true, inputType: "insertText", data: ${JSON.stringify(query)},
  }));
  return { ok: true };
})()`;

const addressKey = (key) => `(() => {
  ${PANEL_PRELUDE}
  const input = root ? root.querySelector(".bp-toolbar .bp-url") : null;
  if (!input) return { ok: false, error: "no-address-input" };
  const event = new KeyboardEvent("keydown", {
    key: ${JSON.stringify(key)}, bubbles: true, cancelable: true, composed: true,
  });
  input.dispatchEvent(event);
  return { ok: true, defaultPrevented: event.defaultPrevented };
})()`;

// 真实鼠标点那一行的坐标：下拉在 shadow root 里，坐标仍然是顶层视口的 CSS 像素，
// 所以 CDP 的 Input.* 能直接落在它上面。
const addressRowRect = (url) => `(() => {
  ${PANEL_PRELUDE}
  const menu = root ? root.querySelector(".starship-addr__menu") : null;
  if (!menu) return { err: "no-menu" };
  const rows = Array.prototype.slice.call(menu.querySelectorAll(".starship-addr__row"));
  const row = rows.find((item) => item.__starshipUrl === ${JSON.stringify(url)});
  if (!row) return { err: "row-not-found" };
  const rect = row.getBoundingClientRect();
  return { ok: true, x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
})()`;

const STANDIN_COUNT = `(() => {
  ${PANEL_PRELUDE}
  return root ? root.querySelectorAll("img.starship-standin").length : -1;
})()`;

// 假浮层：带上可视化层的标记，铺满视口。白名单生效时它必须被当作装饰忽略。
const DECOY_ON = `(() => {
  if (document.getElementById("__starshipDecoy")) return { ok: true, existed: true };
  const decoy = document.createElement("div");
  decoy.id = "__starshipDecoy";
  decoy.setAttribute("data-starship-visual", "1");
  decoy.setAttribute("aria-hidden", "true");
  decoy.style.cssText =
    "position:fixed;left:0;top:0;width:100%;height:100%;" +
    "pointer-events:none;z-index:2147483645;background:transparent;";
  (document.documentElement || document.body).appendChild(decoy);
  return { ok: true };
})()`;
const DECOY_OFF = `(() => {
  const decoy = document.getElementById("__starshipDecoy");
  if (decoy && decoy.parentNode) decoy.parentNode.removeChild(decoy);
  return { ok: true };
})()`;

const SHELL_STATE = `window.__OPENCLAW_NATIVE_BROWSER__ ?? null`;

let panel = await evaluate(PANEL_STATE);
if (!panel.mounted) {
  await evaluate(TOGGLE_VISIBLE);
  for (let attempt = 0; attempt < 20 && !panel.mounted; attempt += 1) {
    await sleep(250);
    panel = await evaluate(PANEL_STATE);
  }
}
check("看得到的那块窗格里挂着浏览器面板", panel.mounted === true, panel);

const shellState = () => evaluate(SHELL_STATE);
const post = (message) =>
  evaluate(
    `window.webkit.messageHandlers.openclawBrowser.postMessage(${JSON.stringify(message)})`,
    60000,
  );
const dispatch = (method, params, tabId) =>
  evaluate(
    `window.openclawBrowserDispatch(${JSON.stringify(method)},${JSON.stringify(
      params,
    )},${JSON.stringify(tabId)})`,
    60000,
  );
// 页面里的求值走 dispatch：动作与观测都落在面板正在显示的那个子 WebView 上。
// 回执是两层套：壳层给 `{ok, method, result}`，里面那层才是 CDP 的
// `{result: {type, value}}` —— 少剥一层就会把整个 CDP 结果对象当成页面答案。
const tabEval = async (tabId, expression) => {
  const reply = await dispatch(
    "Runtime.evaluate",
    { expression, returnByValue: true, awaitPromise: true },
    tabId,
  );
  if (reply?.ok === false) {
    throw new Error(`dispatch failed: ${JSON.stringify(reply)}`);
  }
  return reply?.result?.result?.value;
};
const tabEvents = async (tabId) => (await tabEval(tabId, "window.__events || []")) ?? [];

async function waitForTabTitle(tabId, expected, budgetMs = 15000) {
  return Boolean(
    await until(
      async () => (await tabEval(tabId, "document.title")) === expected,
      budgetMs,
    ),
  );
}

async function openTab(url) {
  const reply = await post({ type: "open", url });
  return reply?.ok === true ? reply.tabId : null;
}

// 第 1 组的前提是「一条标签都不剩」，所以这里清的是全部标签，不是只清夹具那些。
async function closeEveryTab() {
  for (let round = 0; round < 6; round += 1) {
    const state = await shellState();
    const tabs = state?.tabs ?? [];
    if (!tabs.length) {
      return { tabs: [] };
    }
    for (const tab of tabs) {
      await post({ type: "close", tabId: tab.id });
      await sleep(200);
    }
  }
  return await shellState();
}

async function act(params) {
  return evaluate(
    `window.openclawBrowserAct(${JSON.stringify({ tabId, ...params })})`,
    60000,
  );
}

// 可信输入三段：`mouseMoved` 先建立悬停，再按下、抬起。合成 click 骗不过面板的
// `pointerdown` 那一层；只有真鼠标才证明「用户点得到」。
async function trustedClick(x, y) {
  await send("Input.dispatchMouseEvent", {
    type: "mouseMoved",
    x,
    y,
    button: "none",
    clickCount: 0,
  });
  await send("Input.dispatchMouseEvent", {
    type: "mousePressed",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 1,
  });
  await send("Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 0,
  });
}

// ------------------------------------------------------------ 人机共存的取证

// 人手那一下长什么样：`TAB_INIT_SCRIPT` 末尾的探针就是这么往壳层报的 —— 落点、
// 目标元素、是不是在写字。这里从页面里发同一份报文，好处是**可控**：真鼠标那一
// 版（第 5h 条）走的是同一条通道，只是触发者从脚本换成 WebView2 自己派发的可信
// 事件，两者在壳层眼里没有区别。
const humanSay = (target, fields) =>
  tabEval(
    target,
    `window.chrome.webview.postMessage(JSON.stringify(${JSON.stringify({
      __starshipTab: true,
      ...fields,
    })})); true`,
  );

// 报文落壳是异步的（页面 → 桥 → 壳层 worker），所以不能拿 `sleep` 赌时间。
// 让路回执本身就是最好的探针：账不在时这一下照常执行，账在时返回
// `COMPUTER_HUMAN_INPUT`。轮询到它为止，「这一笔到底记没记上」就不再是猜的。
async function actUntilBlocked(params, budgetMs = 4000, stepMs = 150) {
  return until(async () => {
    const reply = await act(params);
    return reply?.code === "COMPUTER_HUMAN_INPUT" ? reply : null;
  }, budgetMs, stepMs);
}

const heldBack = (reply) => reply?.ok === false && reply?.code === "COMPUTER_HUMAN_INPUT";

// 另一个 CDP 目标的极简客户端。只为一件事：往标签自己的 WebView 里灌**可信
// 输入** —— 走壳层的 `dispatch` 会被盖章成智能体，那就验不到人手这条路了。
function connectTarget(url) {
  return new Promise((resolve, reject) => {
    const targetSocket = new WebSocket(url);
    const targetPending = new Map();
    let targetId = 0;
    targetSocket.addEventListener("message", (event) => {
      const payload = JSON.parse(event.data);
      const entry = targetPending.get(payload.id);
      if (!entry) return;
      targetPending.delete(payload.id);
      if (payload.error) entry.reject(new Error(JSON.stringify(payload.error)));
      else entry.resolve(payload.result);
    });
    const targetSend = (method, params) =>
      new Promise((res, rej) => {
        const id = ++targetId;
        targetPending.set(id, { resolve: res, reject: rej });
        targetSocket.send(JSON.stringify({ id, method, params }));
      });
    targetSocket.addEventListener(
      "open",
      () => resolve({ send: targetSend, socket: targetSocket }),
      { once: true },
    );
    targetSocket.addEventListener("error", reject, { once: true });
  });
}

// ------------------------------------------------- 1. 历史：回车 / 真实点击

// 先制造历史：开一页、再走到下一页，两条 URL 都会进壳层的历史账。
await closeEveryTab();
const seedTab = await openTab(indexUrl);
check("夹具首页在面板里打开了", Boolean(seedTab), seedTab);
check("夹具首页载入完成", seedTab ? await waitForTabTitle(seedTab, "Agent IO Fixture") : false);
if (seedTab) {
  await post({ type: "navigate", tabId: seedTab, url: nextUrl });
  check("夹具第二页载入完成", await waitForTabTitle(seedTab, "Next Page", 12000));
}

const emptied = await closeEveryTab();
check("标签全关光了（第 1 组的前提：没有活动标签）", (emptied?.tabs ?? []).length === 0, emptied);

async function openHistoryMenu(query) {
  await evaluate(addressType(query));
  const filled = await until(async () => {
    const probe = await evaluate(ADDRESS_PROBE);
    return probe?.rows?.length ? probe : null;
  }, 8000);
  return filled;
}

async function focusAddress() {
  const focus = await evaluate(ADDRESS_FOCUS);
  if (!focus?.ok) {
    check("地址栏还在（没有活动标签也要能用）", false, focus);
  }
  return focus;
}

// ---- 1a. 键盘：ArrowDown 选中、Enter 直达

const logMark = (await evaluate("(window.__starshipAddrLog || []).length")) ?? 0;
await focusAddress();
const menu = await openHistoryMenu(String(FIXTURE_PORT));
check("输入端口号后下拉列出了历史行", Boolean(menu?.rows?.length), menu);
const indexRow = menu?.rows?.find((row) => (row.url ?? "").includes("index.html")) ?? null;
check("历史里找得到夹具首页", Boolean(indexRow), menu?.rows);

const arrow = await evaluate(addressKey("ArrowDown"));
check("ArrowDown 被地址栏接管", arrow?.defaultPrevented === true, arrow);
const activeProbe = await evaluate(ADDRESS_PROBE);
const enterUrl = activeProbe?.activeUrl ?? null;
check("ArrowDown 把一行标成活动行", Boolean(enterUrl), activeProbe);

const enter = await evaluate(addressKey("Enter"));
check("Enter 被地址栏接管", enter?.defaultPrevented === true, enter);
const openedByEnter = await until(async () => {
  const state = await shellState();
  const tabs = state?.tabs ?? [];
  return tabs.length ? tabs : null;
}, 8000);
check("回车之后壳层开出了一个标签", Boolean(openedByEnter), openedByEnter);
check(
  "开出的是历史里选中的那一条",
  Boolean(openedByEnter?.some((tab) => (tab.url ?? "") === enterUrl)),
  { enterUrl, opened: openedByEnter },
);
const logTail = (await evaluate(`(window.__starshipAddrLog || []).slice(${logMark})`)) ?? [];
check(
  "走的是「无活动标签 → 壳层 open」那条路",
  logTail.some((line) => String(line).includes("(no active tab)")),
  logTail,
);
const adopted = await until(async () => {
  const state = await evaluate(PANEL_STATE);
  return state?.activeTargetId ? state : null;
}, 6000);
check("面板把新标签接管成当前标签", Boolean(adopted?.activeTargetId), adopted);

// ---- 1b. 鼠标：真实点击历史行

const emptiedAgain = await closeEveryTab();
check("点之前又清空了标签", (emptiedAgain?.tabs ?? []).length === 0, emptiedAgain);
await focusAddress();
const menu2 = await openHistoryMenu(String(FIXTURE_PORT));
const clickRow = menu2?.rows?.find((row) => (row.url ?? "").includes("next.html")) ?? null;
check("历史里找得到夹具第二页", Boolean(clickRow), menu2?.rows);

let clickTrace = null;
if (clickRow) {
  const rect = await evaluate(addressRowRect(clickRow.url));
  check("拿到了历史行的屏幕坐标", rect?.ok === true, rect);
  if (rect?.ok) {
    await trustedClick(rect.x, rect.y);
    const afterClick = await until(async () => {
      const probe = await evaluate(ADDRESS_PROBE);
      const state = await shellState();
      const tabs = state?.tabs ?? [];
      return tabs.length ? { probe, tabs } : null;
    }, 8000);
    clickTrace = afterClick;
    check("真实鼠标点历史行也开出了标签", Boolean(afterClick?.tabs?.length), afterClick);
    check(
      "点中的就是那一条（下拉随之收起）",
      afterClick?.probe?.menuHidden === true,
      afterClick?.probe,
    );
  }
}

// ---------------------------------------------------------------- 2. select

await closeEveryTab();
const tabId = await openTab(indexUrl);
check("动作工作标签就绪", Boolean(tabId), tabId);
check("动作工作标签载入完成", tabId ? await waitForTabTitle(tabId, "Agent IO Fixture") : false);
if (!tabId) {
  console.error("没有可用标签，后面的动作断言无法进行");
  socket.close();
  server.close();
  process.exit(1);
}

const scanned = await evaluate(`window.openclawBrowserElements(${JSON.stringify(tabId)})`);
const refOf = (name) => scanned?.elements?.find((entry) => entry.name === name)?.ref ?? null;
const pickerRef = refOf("picker");
const plainRef = refOf("plain button");
const hoverRef = refOf("hover target");
check("元素扫描给了 picker 的句柄", Boolean(pickerRef), scanned?.elements?.map((e) => e.name));
check("元素扫描给了普通按钮的句柄", Boolean(plainRef), plainRef);
check("元素扫描给了悬停目标的句柄", Boolean(hoverRef), hoverRef);

// `select` 的回执是 `{inputRoute, detail}` 两层：外层说走的哪条通道，内层是页面答案。
const selectInner = (reply) => reply?.detail?.detail ?? null;

const byValue = await act({ action: "select", elementRef: pickerRef, value: "green" });
check("select value 路径动作确认", byValue?.ok === true && byValue?.effect === "confirmed", byValue);
check("select 回执写 dom_event 通道", byValue?.detail?.inputRoute === "dom_event", byValue?.detail);
const green = selectInner(byValue) ?? {};
check(
  "select value=green 命中第 1 项（index/selected/value 对得上）",
  green.ok === true && green.value === "green" && green.index === 1 && green.selected === 1,
  green,
);
check(
  "select 回执带 label 与三项 options",
  green.label === "Green" &&
    Array.isArray(green.options) &&
    green.options.length === 3 &&
    green.options[1]?.selected === true,
  green,
);
check(
  "页面收到了 change:green",
  (await tabEvents(tabId)).includes("change:green"),
  await tabEvents(tabId),
);
check(
  "页面里的选中值真的改了",
  (await tabEval(tabId, "document.getElementById('picker').value")) === "green",
);

const byLabel = await act({ action: "select", elementRef: pickerRef, label: "BLUE" });
const blue = selectInner(byLabel) ?? {};
check(
  "select label 大小写不敏感（BLUE → blue/index 2）",
  blue.ok === true && blue.value === "blue" && blue.index === 2,
  blue,
);

const byIndex = await act({ action: "select", elementRef: pickerRef, index: 0 });
const red = selectInner(byIndex) ?? {};
check("select index=0 → red", red.ok === true && red.value === "red" && red.index === 0, red);
check(
  "页面收到了三次 change（red/green/blue）",
  (await tabEvents(tabId)).filter((line) => String(line).startsWith("change:")).length === 3,
  await tabEvents(tabId),
);

const both = await act({ action: "select", elementRef: pickerRef, value: "red", index: 0 });
check(
  "同时给 value+index 被拒（说不清要哪一个就不猜）",
  both?.ok === false && String(both?.error ?? "").includes("exactly one"),
  both,
);
const none = await act({ action: "select", elementRef: pickerRef });
check(
  "什么都不给被拒",
  none?.ok === false && String(none?.error ?? "").includes("one of value, label, or index"),
  none,
);
const outOfRange = await act({ action: "select", elementRef: pickerRef, index: 9 });
check(
  "index 越界被拒并报出 9/3",
  outOfRange?.ok === false && String(outOfRange?.error ?? "").includes("index-out-of-range:9/3"),
  outOfRange,
);
const notSelect = await act({ action: "select", elementRef: plainRef, value: "red" });
check(
  "select 落在非 <select> 上被拒",
  notSelect?.ok === false && String(notSelect?.error ?? "").includes("not-a-select"),
  notSelect,
);

// ---------------------------------------------------------------- 3. hover

const hoverChain = async () =>
  (await tabEval(
    tabId,
    `Array.prototype.slice.call(document.querySelectorAll(":hover")).map(function (node) { return node.id || node.tagName; }).join(">")`,
  )) ?? "";

const plainRect = scanned?.elements?.find((entry) => entry.name === "plain button")?.rect ?? null;
let rewarmTrace = null;
if (plainRect) {
  await act({
    action: "hover",
    x: Math.round(plainRect.x + plainRect.width / 2),
    y: Math.round(plainRect.y + plainRect.height / 2),
  });
  check("先离开悬停目标（悬停状态跟着走）", (await hoverChain()).includes("plainbtn"));
}

const hoverByRef = await act({ action: "hover", elementRef: hoverRef });
rewarmTrace = hoverByRef?.detail ?? null;
check(
  "hover 按 elementRef 确认",
  hoverByRef?.ok === true && hoverByRef?.effect === "confirmed",
  hoverByRef,
);
check(
  "hover 回执带首帧兜底标记 rewarmed",
  typeof rewarmTrace?.rewarmed === "boolean",
  rewarmTrace,
);
check("悬停落到了目标上（页面 :hover 链里有 hoverbtn）", (await hoverChain()).includes("hoverbtn"));
check(
  "页面 :hover 的样式真的生效（背景变深）",
  Boolean(
    await until(
      async () =>
        (await tabEval(
          tabId,
          `getComputedStyle(document.getElementById("hoverbtn")).backgroundColor`,
        )) === "rgb(10, 20, 30)",
      4000,
    ),
  ),
);
check(
  "页面收到了悬停事件",
  (await tabEvents(tabId)).some((line) => String(line).includes(":hoverbtn")),
  await tabEvents(tabId),
);

const hoverRect = scanned?.elements?.find((entry) => entry.name === "hover target")?.rect ?? null;
if (hoverRect) {
  const byPoint = await act({
    action: "hover",
    x: Math.round(hoverRect.x + hoverRect.width / 2),
    y: Math.round(hoverRect.y + hoverRect.height / 2),
  });
  check(
    "hover 按坐标确认（同一条 mouseMoved 通道）",
    byPoint?.ok === true && typeof byPoint?.detail?.rewarmed === "boolean",
    byPoint,
  );
}

// ------------------------------------------------------------- 4. 可视化层

// 4a. 点击 → 页面里长出光标与涟漪，且不抢命中测试
const clickReply = await act({ action: "click", elementRef: plainRef });
check("点击动作确认", clickReply?.ok === true && clickReply?.effect === "confirmed", clickReply);

const visualProbe = `(() => {
  const layer = document.getElementById("__starshipVisualLayer");
  if (!layer) return { layer: false };
  const boxes = Array.prototype.slice.call(layer.children);
  const cursor = boxes.find(function (node) {
    return String(node.style.transform || "").indexOf("translate3d") >= 0 &&
      String(node.style.transform || "").indexOf("rotate") < 0;
  });
  const rect = document.getElementById("plainbtn").getBoundingClientRect();
  const hit = document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2);
  return {
    layer: true,
    marker: layer.getAttribute("data-starship-visual"),
    pointerEvents: getComputedStyle(layer).pointerEvents,
    boxes: boxes.length,
    parentIsHtml: layer.parentNode === document.documentElement,
    cursorTransform: cursor ? cursor.style.transform : null,
    cursorOpacity: cursor ? cursor.style.opacity : null,
    hit: hit ? hit.id || hit.tagName : null,
  };
})()`;

const visual = await until(async () => {
  const probe = await tabEval(tabId, visualProbe);
  return probe?.layer && probe?.cursorTransform ? probe : null;
}, 4000);
check("页面里出现了可视化层", visual?.layer === true, visual);
check("层带着白名单标记 data-starship-visual", visual?.marker === "1", visual);
check("整层 pointer-events:none（不参与命中测试）", visual?.pointerEvents === "none", visual);
check(
  "虚拟光标被摆到了落点上",
  typeof visual?.cursorTransform === "string" && visual.cursorTransform.includes("translate3d"),
  visual,
);
check("点哪儿还是哪儿（叠加层没吃掉点击）", visual?.hit === "plainbtn", visual?.hit);

// 4b. 滚动 → 层里出现方向指示
const scrollerRect = await tabEval(
  tabId,
  `(() => { const r = document.getElementById("scroller").getBoundingClientRect();
     return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`,
);
const scrollReply = await act({
  action: "scroll",
  x: Math.round(scrollerRect.x),
  y: Math.round(scrollerRect.y),
  deltaY: 600,
});
check("滚动动作确认", scrollReply?.ok === true, scrollReply);
const pill = await until(async () => {
  const text = await tabEval(
    tabId,
    `(() => {
       const layer = document.getElementById("__starshipVisualLayer");
       if (!layer) return null;
       const pills = Array.prototype.slice.call(layer.children).filter(function (node) {
         return String(node.textContent || "").indexOf("px") > 0;
       });
       return pills.length ? pills[pills.length - 1].textContent : null;
     })()`,
  );
  return text ? String(text) : null;
}, 2000, 120);
check("滚动在页面里画出了方向指示", Boolean(pill && pill.includes("↓") && pill.includes("600")), pill);

// 4c. 装饰层不算遮挡：带标记的假浮层铺满视口，壳层也不该让位
const standinIdle = await until(async () => {
  const count = await evaluate(STANDIN_COUNT);
  return count === 0 ? { count } : null;
}, 5000);
check("没有浮层时没有替身图（基线）", standinIdle?.count === 0, standinIdle);
await evaluate(DECOY_ON);
await sleep(2500);
const decoyCount = await evaluate(STANDIN_COUNT);
check("带可视化标记的浮层不被算成遮挡（不让位、不白屏）", decoyCount === 0, {
  standins: decoyCount,
});
await evaluate(DECOY_OFF);
await sleep(600);

// 4d. 真浮层照旧要触发表层级让位（下拉压住原生视图是既定行为）
await focusAddress();
const stdinUp = await until(async () => {
  const count = await evaluate(STANDIN_COUNT);
  return count > 0 ? { count } : null;
}, 8000);
check("真浮层（地址下拉）仍然触发替身让位", Boolean(stdinUp?.count), stdinUp);
await evaluate(addressKey("Escape"));
const stdInDown = await until(async () => {
  const count = await evaluate(STANDIN_COUNT);
  return count === 0 ? { count } : null;
}, 8000);
check("浮层收起后替身撤掉", stdInDown?.count === 0, stdInDown);

// 4e. 闲下来之后光标自己淡出（这一层不该在页面上留痕）
const idle = await until(async () => {
  const probe = await tabEval(tabId, visualProbe);
  return probe?.cursorOpacity === "0" ? probe : null;
}, 9000, 400);
check("动作停一会儿之后光标淡出", idle?.cursorOpacity === "0", idle);

// ------------------------------------------------------------- 5. 人机共存

// 用户和智能体**同时**用一个页面。上一版的写法是「用户一动，智能体全停」——
// 那等于用户一边看着页面，智能体就一边歇着。这一组要证的是新规则：只撞上同一
// 块地方才让路。所以每一条「让路」旁边都配了一条「放行」。
//
// 先让场面静下来：前面的动作留的盖章尾巴（`HUMAN_ARM_SLACK_MS`）还没过，
// 那段时间里页面报的人手账会被当智能体自己的吃掉。
await sleep(1500);

const noteRef = refOf("note field");
const otherRef = refOf("other field");
check("元素扫描给了两个输入框的句柄", Boolean(noteRef && otherRef), {
  note: noteRef,
  other: otherRef,
});

const viewport = JSON.parse(
  (await tabEval(tabId, "JSON.stringify({ w: innerWidth, h: innerHeight })")) ?? "{}",
);
const plainCenter = {
  x: Math.round((plainRect?.x ?? 40) + (plainRect?.width ?? 80) / 2),
  y: Math.round((plainRect?.y ?? 40) + (plainRect?.height ?? 30) / 2),
};
const noteRect =
  scanned?.elements?.find((entry) => entry.name === "note field")?.rect ?? plainCenter;
// A：用户碰过的地方。B：离 A 三百来像素，撞不上。P / Q：单给智能体自己用的两个点。
const A = plainCenter;
const B = { x: Math.round(A.x + 300), y: A.y };
const P = { x: 24, y: 24 };
const Q = { x: Math.round((viewport.w ?? 800) / 2), y: Math.round((viewport.h ?? 600) - 30) };
check(
  "两个落点确实离得够远（超过 48px 的让路半径）",
  Math.hypot(B.x - A.x, B.y - A.y) > 48 && B.x < (viewport.w ?? 800) - 5,
  { A, B, viewport },
);

// ---- 5a. 人手那一笔带没带上现场

await humanSay(tabId, { kind: "pointerdown", x: A.x, y: A.y, ref: plainRef, typing: false });
const sameSpot = await actUntilBlocked({ action: "click", x: A.x, y: A.y });
check("落点同处时让路（说明人手那一笔带上了现场）", heldBack(sameSpot), sameSpot);
check("让路的理由是「指针撞车」", sameSpot?.conflict === "pointer", sameSpot);
check(
  "回执给的是「重试时间」，不是一句失败",
  Number(sameSpot?.retryAfterMs) > 0 && Number(sameSpot?.retryAfterMs) <= 1200,
  sameSpot?.retryAfterMs,
);

// ---- 5b. 共存：同一笔账还在，别处照走

await humanSay(tabId, { kind: "pointerdown", x: A.x, y: A.y, ref: plainRef, typing: false });
const elsewhere = await act({ action: "click", x: B.x, y: B.y });
check(
  "同一笔人手账还在时，别处照常执行（共存，不是静音）",
  elsewhere?.ok === true && elsewhere?.effect === "confirmed",
  elsewhere,
);
const stillHeld = await act({ action: "click", x: A.x, y: A.y });
check("紧接着原地仍然让路（上一条不是「账已经过期」蒙过去的）", heldBack(stillHeld), stillHeld);

// ---- 5c. 让路有窗口，不是封锁

await sleep(1400);
const afterQuiet = await act({ action: "click", x: A.x, y: A.y });
check("安静一会儿之后，同一点恢复放行", afterQuiet?.ok === true, afterQuiet);

// ---- 5d. 观察类根本不进闸

const observedAt = Date.now();
await humanSay(tabId, { kind: "pointerdown", x: A.x, y: A.y, ref: plainRef, typing: false });
const snapshot = await act({ action: "snapshot" });
check("用户正在页面上操作时，快照照常给（观察类不进闸）", snapshot?.ok === true, snapshot);
const peek = await dispatch(
  "Runtime.evaluate",
  { expression: "document.title", returnByValue: true },
  tabId,
);
check("用户正在页面上操作时，页面求值照常给", peek?.ok === true, peek);
check(
  "那两下确实落在人手账的窗口里",
  Date.now() - observedAt < 1200,
  Date.now() - observedAt,
);
const heldDuringPeek = await act({ action: "click", x: A.x, y: A.y });
check("（同一时刻原地让路仍在，所以上面两条不是在没人时过的）", heldBack(heldDuringPeek), heldDuringPeek);

// ---- 5e. 键盘：挡的是「那个框」，而且写字档更长

const notePoint = {
  x: Math.round(noteRect.x + noteRect.width / 2),
  y: Math.round(noteRect.y + noteRect.height / 2),
};
await humanSay(tabId, { kind: "keydown", typing: true, ref: noteRef, x: notePoint.x, y: notePoint.y });
const otherField = await actUntilBlocked({ action: "type", elementRef: otherRef, text: "x" }, 4000, 120);
check(
  "用户在写字时，往别的框打字也让路（抢焦点就是抢字）",
  heldBack(otherField) && otherField?.conflict === "keyboard",
  otherField,
);
check(
  "这一档用的是短的静默窗口，不是写字的长窗口",
  Number(otherField?.retryAfterMs) <= 1200,
  otherField?.retryAfterMs,
);
await sleep(1400);
const shorterWindowOver = await act({ action: "type", elementRef: otherRef, text: "after" });
check("那一档短的窗口过去之后就能打进去", shorterWindowOver?.ok === true, shorterWindowOver);

await humanSay(tabId, { kind: "keydown", typing: true, ref: noteRef, x: notePoint.x, y: notePoint.y });
const sameField = await actUntilBlocked({ action: "type", elementRef: noteRef, text: "hi" }, 4000, 120);
check(
  "往用户正在写的那个框里打字：让路，而且窗口更长",
  heldBack(sameField) && sameField?.conflict === "keyboard" && Number(sameField?.retryAfterMs) > 2000,
  sameField,
);

// ---- 5f. 滚动恒让路（同一个视口，两次滚动互相顶掉）

await humanSay(tabId, { kind: "wheel", x: A.x, y: A.y, typing: false });
const scrollHeld = await actUntilBlocked({ action: "scroll", x: A.x, y: A.y, deltaY: 400 }, 4000, 120);
check(
  "用户刚滚过，智能体的滚动让路",
  heldBack(scrollHeld) && scrollHeld?.conflict === "scroll",
  scrollHeld,
);

// ---- 5g. 反向：智能体自己的输入不能记成人手

// 壳层每次灌输入前后都会在页面里盖一个时间戳（`window.__starshipAgentInputUntil`）。
// 没有这个章，CDP 派出来的事件也是 `isTrusted === true`，智能体会把自己当用户，
// 然后被自己锁在门外。
await sleep(1400);
const agentClick = await act({ action: "click", x: P.x, y: P.y });
check("壳层驱动动作确认（下一步用它来验盖章）", agentClick?.ok === true, agentClick);
const gate = Number(await tabEval(tabId, "window.__starshipAgentInputUntil || 0"));
const pageNow = Number(await tabEval(tabId, "Date.now()"));
check("壳层给自己盖了章（窗口盖在页面当前时间之后）", gate - pageNow > 200, { gate, pageNow });
const agentAgain = await act({ action: "click", x: P.x, y: P.y });
check(
  "智能体自己的输入没被记成人手（原地连做两下都放行）",
  agentAgain?.ok === true,
  agentAgain,
);

// ---- 5h. 真机可信输入走的也是同一条账

const fixtureTargets = await listTargets();
const fixtureTarget = fixtureTargets.find((entry) =>
  (entry.url ?? "").includes(`127.0.0.1:${FIXTURE_PORT}`),
);
check(
  "找得到夹具标签自己的 CDP 目标（可信输入的唯一入口）",
  Boolean(fixtureTarget),
  fixtureTargets.map((entry) => entry.url),
);
let liveHeld = null;
if (fixtureTarget) {
  // 先等盖章窗口过掉：标签自己派发的可信事件，只有没被盖章时才算人手的。
  await sleep(1200);
  const tabTargetConnection = await connectTarget(fixtureTarget.webSocketDebuggerUrl);
  for (const [type, buttons] of [
    ["mouseMoved", 0],
    ["mousePressed", 1],
    ["mouseReleased", 0],
  ]) {
    await tabTargetConnection.send("Input.dispatchMouseEvent", {
      type,
      x: Q.x,
      y: Q.y,
      button: type === "mouseMoved" ? "none" : "left",
      clickCount: type === "mouseMoved" ? 0 : 1,
      buttons,
    });
  }
  tabTargetConnection.socket.close();
  await sleep(300);
  liveHeld = await actUntilBlocked({ action: "click", x: Q.x, y: Q.y }, 2500, 150);
}
check("真的可信鼠标（标签自己的 CDP 目标）也会被记成人手", heldBack(liveHeld), liveHeld);

// ------------------------------------------------------------------- 收尾

await closeEveryTab();
console.log(
  failures
    ? `\n${failures} 条断言未通过`
    : `\nALL PASS（历史直达 / select / hover / 可视化 / 人机共存）${clickTrace ? "" : "（点击路径未跑）"}`,
);
socket.close();
server.close();
process.exit(failures ? 1 : 0);
