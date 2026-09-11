// Live smoke for the two browser symptoms the user kept hitting:
//   1. a normal link must navigate inside the tab the user is looking at
//      (no surprise second tab),
//   2. a `target="_blank"` link must land in the panel's own tab strip
//      (never a detached OS window), and
//   3. a site dialog (confirm) must not leave the tab dead.
// Usage: node probe-link-routing.mjs [cdpPort] [dashboardUrlMatch] [fixturePort]
import http from "node:http";

const CDP_PORT = process.argv[2] ?? "9334";
const DASHBOARD_MATCH = process.argv[3] ?? "chat/main";
const FIXTURE_PORT = Number(process.argv[4] ?? 19199);

const PAGE = (title, body) => `<!doctype html>
<html><head><meta charset="utf-8"><title>${title}</title></head>
<body style="margin:0;font-family:system-ui;font-size:16px">${body}</body></html>`;

const INDEX = PAGE(
  "Link Routing Fixture",
  `<h1 id="headline">Link Routing Fixture</h1>
   <a id="same" href="/next.html" style="display:block;padding:20px;font-size:18px">same tab</a>
   <a id="blank" href="/next.html" target="_blank" style="display:block;padding:20px;font-size:18px">new tab</a>
   <button id="confirm" style="padding:20px;font-size:18px">ask me</button>
   <div id="log"></div>
   <script>
     window.__events = [];
     document.getElementById("confirm").addEventListener("click", () => {
       window.__events.push("confirm");
       const answer = confirm("starship-confirm");
       window.__events.push("answered:" + answer);
     });
      // With ?auto=1 the page raises the dialog on its own. The driver can then
      // watch the panel surface it and resolve it, which a driver-triggered
      // click cannot do: the click act waits on the page it just froze.
      if (location.search.indexOf("auto") !== -1) {
        setTimeout(() => {
          window.__events.push("auto");
          const answer = confirm("starship-confirm");
          window.__events.push("answered:" + answer);
        }, 1200);
      }
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

// The user is looking at one pane; a document-wide query could pick a cached
// pane's own panel and then drive a hidden WebView.
const PANEL_STATE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  const controller = panel ? panel.browserPanelController : null;
  if (!controller) return { mounted: false };
  const tabs = (controller.tabs ?? []).map((tab) => ({
    id: tab.id,
    url: tab.url ?? null,
    dialog: tab.dialog ?? null,
  }));
  return { mounted: true, activeTargetId: controller.activeTargetId ?? null, tabs };
})()`;
// Focus + panel state, captured only when an assertion is about to fail: the
// shell window losing the foreground is the one condition that makes a
// `confirmed` click a no-op, and it leaves no trace in the panel state alone.
const FOCUS_DIAG = `(() => {
  const base = ${PANEL_STATE};
  return {
    shellFocused: document.hasFocus(),
    visibility: document.visibilityState,
    ...base,
  };
})()`;
const TOGGLE_VISIBLE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const button = pane ? pane.querySelector(".chat-browser-panel-toggle") : null;
  if (!button) return { err: "no-toggle-button" };
  button.click();
  return { ok: true };
})()`;

let panel = await evaluate(PANEL_STATE);
if (!panel.mounted) {
  await evaluate(TOGGLE_VISIBLE);
  for (let attempt = 0; attempt < 20 && !panel.mounted; attempt += 1) {
    await sleep(250);
    panel = await evaluate(PANEL_STATE);
  }
}
const tabId = panel.activeTargetId;
check("the visible pane presents a browser panel with an active tab", Boolean(tabId), panel);
if (!tabId) {
  socket.close();
  server.close();
  process.exit(1);
}

const dispatch = (method, params) =>
  evaluate(
    `window.openclawBrowserDispatch(${JSON.stringify(method)},${JSON.stringify(
      params,
    )},${JSON.stringify(tabId)})`,
  );
const titleOf = async () => (await dispatch("Runtime.evaluate", {
  expression: "document.title",
  returnByValue: true,
})).result?.result?.value;

async function navigate(url) {
  const reply = await evaluate(
    `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"navigate",tabId:${JSON.stringify(
      tabId,
    )},url:${JSON.stringify(url)}})`,
  );
  return reply;
}

async function waitForTitle(expected, budgetMs = 12000) {
  const deadline = Date.now() + budgetMs;
  while (Date.now() < deadline) {
    await sleep(400);
    try {
      if ((await titleOf()) === expected) {
        return true;
      }
    } catch (error) {
      // a transient CDP failure is not a verdict; keep polling
    }
  }
  return false;
}

async function act(params) {
  return evaluate(
    `window.openclawBrowserAct(${JSON.stringify({ tabId, ...params })})`,
    60000,
  );
}

// 关掉面板里所有 URL 命中 `fragment` 的标签（绝不动测试自己那个驱动标签）。
// 恢复会话会把上一次跑留下的标签带回来，用例开始前得先把它们清干净。
async function closeTabsMatching(fragment) {
  const state = await evaluate(PANEL_STATE);
  for (const tab of state.tabs) {
    if (tab.id === tabId || !(tab.url ?? "").includes(fragment)) {
      continue;
    }
    await evaluate(
      `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"close",tabId:${JSON.stringify(
        tab.id,
      )}})`,
    );
    await sleep(250);
  }
}

// 让官方面板切到某个标签。`selectTab` 就是面板自己点标签时走的那条路，这里只是
// 让测试能制造「面板停在别的标签上」这个前提。
async function selectPanelTab(id) {
  await evaluate(`(() => {
    const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
    const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
    const controller = panel ? panel.browserPanelController : null;
    if (!controller) return { err: "no-controller" };
    controller.selectTab(${JSON.stringify(id)});
    return { ok: true };
  })()`);
  for (let attempt = 0; attempt < 20; attempt += 1) {
    await sleep(250);
    if ((await evaluate(PANEL_STATE)).activeTargetId === id) {
      return true;
    }
  }
  return false;
}

// The scan reports the accessible name, not a CSS selector: `ref` is the handle
// the driver takes, and a name is what a user would read off the page anyway.
async function refFor(name) {
  const elements = await evaluate(`window.openclawBrowserElements(${JSON.stringify(tabId)})`);
  return elements?.elements?.find((entry) => entry.name === name)?.ref ?? null;
}

// ---------------------------------------------------------------- same tab
check("navigate accepted", (await navigate(indexUrl))?.ok === true);
check("fixture index loaded", await waitForTitle("Link Routing Fixture"));

const tabsBefore = (await evaluate(PANEL_STATE)).tabs.length;
const sameRef = await refFor("same tab");
check("the element scan offers a handle for the in-page link", Boolean(sameRef), sameRef);
const sameClick = await act({ action: "click", elementRef: sameRef });
check("click on an in-page link confirms", sameClick?.ok === true, sameClick);
// A click that comes back `confirmed` without navigating is either a real
// routing regression or the known "client window is not foreground" case, where
// WebView2's trusted input stack drops the synthesized mouse event and the
// shell still answers `confirmed`. Record both facts at the moment of failure so
// the next reader can tell them apart without re-running by hand.
const sameNavigated = await waitForTitle("Next Page");
if (!sameNavigated) {
  const diag = await evaluate(FOCUS_DIAG).catch((error) => ({ probeError: String(error) }));
  check("the in-page link navigated inside the same tab", false, {
    hint: "shellFocused:false means WebView2 dropped the trusted click - focus the client window and re-run before calling this a shell regression",
    ...diag,
    click: sameClick,
  });
} else {
  check("the in-page link navigated inside the same tab", true);
}
let after = await evaluate(PANEL_STATE);
check(
  "an in-page link did not spawn a second tab",
  after.tabs.length === tabsBefore,
  { before: tabsBefore, after: after.tabs.length },
);
check("the active tab is still the one the user is looking at", after.activeTargetId === tabId);

// ------------------------------------------------------------ target=_blank
// The tab link policy is deliberate: a plain left click on `target="_blank"`
// stays in the tab the user is reading, because that is the symptom the user
// reported ("click a headline and it jumps to another tab"). The paths that
// genuinely need a new tab keep one: modifier clicks, middle click, and
// `window.open` (login / OAuth flows).
check("navigate back accepted", (await navigate(indexUrl))?.ok === true);
check("fixture index reloaded", await waitForTitle("Link Routing Fixture"));
const topLevelBefore = (await listTargets()).filter((entry) =>
  (entry.url ?? "").includes(":18791/"),
).length;

const blankRef = await refFor("new tab");
check("the element scan offers a handle for the target=_blank link", Boolean(blankRef), blankRef);
const blankClick = await act({ action: "click", elementRef: blankRef });
check("click on a target=_blank link confirms", blankClick?.ok === true, blankClick);
check("a plain click on target=_blank stays in the same tab", await waitForTitle("Next Page"));
after = await evaluate(PANEL_STATE);
check(
  "a plain click on target=_blank did not open another tab",
  after.tabs.length === tabsBefore,
  { before: tabsBefore, after: after.tabs.length, tabs: after.tabs.map((tab) => tab.url) },
);

// `window.open` is the path that must still produce a panel tab.
check("navigate accepted for the popup case", (await navigate(indexUrl))?.ok === true);
check("fixture index reloaded before the popup", await waitForTitle("Link Routing Fixture"));
// 恢复会话可能已经留着一条 `next.html`（上一次跑留下的存档标签）。壳层对同一个
// 地址是有意去重的，所以先把它清掉，让这条用例落在确定状态上。
await closeTabsMatching("next.html");
after = await evaluate(PANEL_STATE);
const tabsSettled = after.tabs.length;
const popup = await dispatch("Runtime.evaluate", {
  expression: `window.open(${JSON.stringify(nextUrl)}, "_blank") ? "opened" : "blocked"`,
  userGesture: true,
  returnByValue: true,
});
check(
  "the page called window.open",
  popup?.result?.result?.value === "opened" || popup?.result?.result?.value === "blocked",
  popup,
);
let nextTabs = [];
for (let attempt = 0; attempt < 15 && nextTabs.length === 0; attempt += 1) {
  await sleep(400);
  after = await evaluate(PANEL_STATE);
  nextTabs = after.tabs.filter((tab) => (tab.url ?? "").includes("next.html"));
}
check(
  "window.open put the target in exactly one panel tab",
  nextTabs.length === 1 && after.tabs.length === tabsSettled + 1,
  { before: tabsSettled, after: after.tabs.length, tabs: after.tabs.map((tab) => tab.url) },
);
check(
  "the panel presents the tab window.open produced",
  nextTabs.some((tab) => tab.id === after.activeTargetId),
  { activeTargetId: after.activeTargetId, tabs: after.tabs.map((tab) => tab.url) },
);

// 同一个地址再来一次：壳层不再复制标签，而是把已经开着的那个请到前台。先让面板
// 停在原来的标签上，否则「请到前台」是恒真的、测不出任何东西。
await selectPanelTab(tabId);
const repeat = await dispatch("Runtime.evaluate", {
  expression: `window.open(${JSON.stringify(nextUrl)}, "_blank") ? "opened" : "blocked"`,
  userGesture: true,
  returnByValue: true,
});
check(
  "the page called window.open a second time",
  repeat?.result?.result?.value === "opened" || repeat?.result?.result?.value === "blocked",
  repeat,
);
let reused = false;
for (let attempt = 0; attempt < 15 && !reused; attempt += 1) {
  await sleep(400);
  after = await evaluate(PANEL_STATE);
  const again = after.tabs.filter((tab) => (tab.url ?? "").includes("next.html"));
  reused = again.length === 1 && after.activeTargetId === again[0].id;
}
check("re-opening the same address reused the one tab it already had", reused, {
  activeTargetId: after.activeTargetId,
  tabs: after.tabs.map((tab) => tab.url),
});
check(
  "re-opening the same address brought that tab back to the front",
  reused && after.tabs.filter((tab) => (tab.url ?? "").includes("next.html")).length === 1,
  { activeTargetId: after.activeTargetId, tabs: after.tabs.map((tab) => tab.url) },
);

const topLevelAfter = (await listTargets()).filter((entry) =>
  (entry.url ?? "").includes(":18791/"),
).length;
check(
  "no detached OS window appeared",
  topLevelAfter === topLevelBefore,
  { before: topLevelBefore, after: topLevelAfter },
);
check(
  "no stray about:blank window was left behind",
  !(await listTargets()).some((entry) => (entry.url ?? "") === "about:blank"),
);
const openerTitle = await titleOf().catch(() => "evaluate failed");
check(
  "the opener tab is still drivable after the popup",
  openerTitle === "Link Routing Fixture" || openerTitle === "Next Page",
  openerTitle,
);

// ---------------------------------------------------------------- dialogs
// A driver-triggered click that opens a dialog is the hardest case: the page
// freezes the moment the dialog opens, so the act that caused it cannot report
// until the dialog is resolved. What matters to the user is that it *comes
// back* instead of leaving the tab dead.
const autoUrl = `${indexUrl}?auto=1`;
check("navigate accepted again", (await navigate(indexUrl))?.ok === true);
check("fixture index reloaded for the dialog case", await waitForTitle("Link Routing Fixture"));

const confirmRef = await refFor("ask me");
check("the element scan offers a handle for the dialog button", Boolean(confirmRef), confirmRef);
const clickStarted = Date.now();
const confirmClick = await act({ action: "click", elementRef: confirmRef }).catch((error) => ({
  thrown: String(error),
}));
const clickElapsedMs = Date.now() - clickStarted;
console.log(
  `info  the dialog-raising click came back after ${clickElapsedMs}ms ` +
    `-> ${JSON.stringify(confirmClick)}`,
);
check(
  "a click that opens a dialog reports either success or the blocked code",
  confirmClick?.ok === true || confirmClick?.code === "COMPUTER_DIALOG_BLOCKED",
  confirmClick,
);
check(
  "the dialog did not wedge the act longer than the auto-dismiss budget",
  clickElapsedMs < 20000,
  { clickElapsedMs },
);

// The dialog is surfaced and resolvable while it is still open: the page raises
// it on its own, so nothing is waiting on the panel to come back first.
check("navigate to the self-raising page", (await navigate(autoUrl))?.ok === true);
check("self-raising page loaded", await waitForTitle("Link Routing Fixture"));

let dialogSeen = null;
const dialogDeadline = Date.now() + 9000;
while (!dialogSeen && Date.now() < dialogDeadline) {
  const state = await evaluate(PANEL_STATE);
  dialogSeen = state.tabs.find((tab) => tab.dialog)?.dialog ?? null;
  if (!dialogSeen) {
    await sleep(200);
  }
}
check("the site dialog is surfaced on the tab", dialogSeen?.kind === "confirm", dialogSeen);
check(
  "the dialog carries what the site asked",
  dialogSeen?.message === "starship-confirm",
  dialogSeen,
);

// 页面正停在弹窗里时，驱动的 pointer 动作必然卡在 CDP 那一侧。这是「点一下把
// 页面挡在弹窗里」的常规时序：回执必须是官方的 `COMPUTER_DIALOG_BLOCKED` 加上
// 弹窗内容，绝不能是一条裸的 `CDP ... failed` —— 后者只会让上层傻傻重试同一个
// 动作。这条用例是确定性的：弹窗在动作之前就已经开着。
const blockedStart = Date.now();
const blockedClick = await act({ action: "click", x: 40, y: 80 });
const blockedElapsedMs = Date.now() - blockedStart;
console.log(
  `info  the click against a page waiting on a dialog came back after ` +
    `${blockedElapsedMs}ms -> ${JSON.stringify(blockedClick)}`,
);
check(
  "a click against a page stuck in a dialog reports the official blocked code",
  blockedClick?.ok === false &&
    blockedClick?.code === "COMPUTER_DIALOG_BLOCKED" &&
    blockedClick?.dialog?.message === "starship-confirm",
  blockedClick,
);

const dismissed = await act({ action: "dialog", mode: "dismiss" });
check("act dialog dismiss confirms", dismissed?.ok === true, dismissed);
// The page lives in the child tab, so its state has to be read through the
// dispatch bridge; the dashboard's own `window` is a different world.
const readEvents = async () =>
  (
    await dispatch("Runtime.evaluate", {
      expression: "JSON.stringify(window.__events || [])",
      returnByValue: true,
    })
  )?.result?.result?.value ?? null;
let events = JSON.parse((await readEvents().catch(() => null)) ?? "[]");
for (let attempt = 0; attempt < 15; attempt += 1) {
  if (Array.isArray(events) && events.includes("answered:false")) {
    break;
  }
  await sleep(300);
  events = JSON.parse((await readEvents().catch(() => null)) ?? "[]");
}
check(
  "the dialog was answered as cancel, not as accept",
  Array.isArray(events) && events.includes("answered:false"),
  events,
);
const responsive = await dispatch("Runtime.evaluate", {
  expression: "document.getElementById('headline').textContent",
  returnByValue: true,
});
check(
  "the tab is drivable after the dialog",
  responsive?.result?.result?.value === "Link Routing Fixture",
  responsive,
);
check(
  "the dialog cleared from the panel state",
  !(await evaluate(PANEL_STATE)).tabs.some((tab) => tab.dialog),
  await evaluate(PANEL_STATE),
);

socket.close();
server.close();
console.log(failures === 0 ? "\nLINK ROUTING: ALL PASS" : `\nLINK ROUTING: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
