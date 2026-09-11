// Probe: an unclaimed site dialog must be released by the shell's timeout net.
//
// Why this exists separately from the driver regression: that suite proves the
// *driven* path (`act dialog`). This one proves the *nobody is driving* path,
// which is what protects a user who just clicks a button on a site that pops
// `confirm()`: the tab has to keep working on its own, and it must not be
// released as "yes" (that would be answering on the user's behalf).
import http from "node:http";

const CDP_PORT = process.argv[2] ?? "9334";
const DASHBOARD_MATCH = process.argv[3] ?? "chat/main";
const FIXTURE_PORT = Number(process.argv[4] ?? 19099);
const WAIT_MS = Number(process.argv[5] ?? 14000);

const FIXTURE = `<!doctype html><title>Starship Dialog Timeout Fixture</title>
<button id="ask" style="width:240px;height:60px">ask</button>
<div id="pad" style="height:400px"></div>
<script>
  window.__events = [];
  document.getElementById("ask").addEventListener("click", () => {
    window.__events.push("ask-click");
    window.__events.push("answer:" + confirm("starship-timeout"));
    window.__events.push("ask-after");
  });
</script>`;

const server = http.createServer((request, response) => {
  response.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
  });
  response.end(FIXTURE);
});
await new Promise((resolve) => server.listen(FIXTURE_PORT, "127.0.0.1", resolve));
const fixtureUrl = `http://127.0.0.1:${FIXTURE_PORT}/dialog-timeout.html`;

const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
const dashboard = list
  .filter((entry) => entry.type === "page")
  .find((entry) => (entry.url ?? "").includes(DASHBOARD_MATCH));
if (!dashboard) {
  console.error(
    `no dashboard target matching "${DASHBOARD_MATCH}"; available: ` +
      list.map((entry) => entry.url).join(", "),
  );
  server.close();
  process.exit(1);
}

const socket = new WebSocket(dashboard.webSocketDebuggerUrl);
const pending = new Map();
let nextId = 0;
function send(method, params) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params }));
  });
}
socket.addEventListener("message", (event) => {
  const message = JSON.parse(event.data);
  if (!message.id || !pending.has(message.id)) {
    return;
  }
  const entry = pending.get(message.id);
  pending.delete(message.id);
  if (message.error) {
    entry.reject(new Error(JSON.stringify(message.error)));
  } else {
    entry.resolve(message.result);
  }
});
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", reject, { once: true });
});

async function evaluate(expression) {
  const result = await send("Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
    userGesture: true,
  });
  if (result.exceptionDetails) {
    throw new Error(JSON.stringify(result.exceptionDetails));
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
    `${ok ? "PASS" : "FAIL"}  ${name}${ok || detail === undefined ? "" : `  -> ${JSON.stringify(detail)}`}`,
  );
  return ok;
}

const VISIBLE_PANEL_STATE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  const stage = panel?.shadowRoot?.querySelector(".bp-stage");
  const rect = stage ? stage.getBoundingClientRect() : null;
  return {
    mounted: Boolean(panel),
    rect: rect ? [Math.round(rect.width), Math.round(rect.height)] : null,
    target: panel?.browserPanelController?.activeTargetId ?? null,
  };
})()`;
const CLICK_VISIBLE_TOGGLE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const button = pane ? pane.querySelector(".chat-browser-panel-toggle") : null;
  if (!button) return { err: "no-toggle-button" };
  button.click();
  return { ok: true };
})()`;

let panelState = await evaluate(VISIBLE_PANEL_STATE);
if (!panelState.mounted) {
  await evaluate(CLICK_VISIBLE_TOGGLE);
  for (let attempt = 0; attempt < 20 && !panelState.mounted; attempt += 1) {
    await sleep(250);
    panelState = await evaluate(VISIBLE_PANEL_STATE);
  }
}
for (let attempt = 0; attempt < 20 && !panelState.rect; attempt += 1) {
  await sleep(250);
  panelState = await evaluate(VISIBLE_PANEL_STATE);
}
const open = panelState.mounted && Boolean(panelState.rect);
check("the visible pane presents a browser panel", open, panelState);
if (!open) {
  socket.close();
  server.close();
  process.exit(1);
}

const tabId = panelState.target;
check("panel exposes an active native tab", typeof tabId === "string" && tabId.length > 0, tabId);
if (typeof tabId !== "string" || !tabId) {
  socket.close();
  server.close();
  process.exit(1);
}

const childEval = (expression) =>
  evaluate(
    `window.openclawBrowserDispatch("Runtime.evaluate",{expression:${JSON.stringify(
      expression,
    )},returnByValue:true},${JSON.stringify(tabId)})`,
  );

const navReply = await evaluate(
  `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"navigate",tabId:${JSON.stringify(
    tabId,
  )},url:${JSON.stringify(fixtureUrl)}})`,
);
check("navigate accepted", navReply?.ok === true, navReply);

let ready = false;
for (let attempt = 0; attempt < 25 && !ready; attempt += 1) {
  await sleep(400);
  const title = (await childEval("document.title"))?.result?.result?.value;
  ready = title === "Starship Dialog Timeout Fixture";
}
check("fixture page loaded in the native tab", ready);

await childEval("window.__events = []");
const elements = await evaluate(`window.openclawBrowserElements(${JSON.stringify(tabId)})`);
const button = elements?.elements?.find((entry) => entry.tag === "button");
check("element scan returns the fixture button", Boolean(button?.ref), elements?.count);
// 点击必须走原生输入，而且**不能等它的回执**：弹窗一开，页面就停了，
// 壳层那次 `Input.dispatchMouseEvent` 也拿不到完成回调 —— 这本身就是
// 「被弹窗挡住」的证据。等回执的写法只会测出 CDP 超时。
await evaluate(
  `void window.openclawBrowserAct({action:"click",tabId:${JSON.stringify(
    tabId,
  )},elementRef:${JSON.stringify(button?.ref)}})`,
);

// 这里刻意什么也不做：既不驱动 `dialog`，也不关标签，看壳层能不能自己收场。
await sleep(WAIT_MS);

let resumed = false;
let events = [];
for (let attempt = 0; attempt < 15 && !resumed; attempt += 1) {
  const raw = await childEval("JSON.stringify(window.__events)");
  events = JSON.parse(raw?.result?.result?.value ?? "[]");
  resumed = events.includes("ask-after");
  if (!resumed) {
    await sleep(400);
  }
}
check("the click reached the page before the dialog opened", events.includes("ask-click"), events);
check("the shell releases the dialog without being asked", resumed, events);
check("the page kept running after the timeout", events.includes("ask-after"), events);
check("the timeout did not answer the dialog on the user's behalf", events.includes("answer:false"), events);

socket.close();
server.close();
console.log(failures === 0 ? "\nDIALOG TIMEOUT: ALL PASS" : `\nDIALOG TIMEOUT: ${failures} FAIL`);
process.exit(failures === 0 ? 0 : 1);
