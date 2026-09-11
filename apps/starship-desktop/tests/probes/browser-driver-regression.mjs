// Starship embedded-browser driver regression over CDP.
// Usage: node browser-driver-regression.mjs [cdpPort] [dashboardUrlMatch] [fixturePort]
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const CDP_PORT = process.argv[2] ?? "9222";
const DASHBOARD_MATCH = process.argv[3] ?? "chat/main";
const FIXTURE_PORT = Number(process.argv[4] ?? 18999);
const UPLOAD_FILE = path.join(os.tmpdir(), "starship-upload-fixture.txt");

const FIXTURE_HTML = `<!doctype html>
<html><head><meta charset="utf-8"><title>Starship Driver Fixture</title></head>
<body style="margin:0;font-family:system-ui">
  <h1 id="headline">Starship Driver Fixture</h1>
  <button id="btn" aria-label="press me" style="padding:24px;font-size:18px">Press</button>
  <input id="inp" aria-label="text field" style="display:block;margin:16px 0;padding:12px;width:300px">
  <div id="slider" aria-label="slider" style="width:320px;height:40px;background:#ddd;margin:12px 0">slide</div>
  <div id="dndsrc" draggable="true" aria-label="drag source" style="width:140px;height:36px;background:#cde;margin:8px 0">drag me</div>
  <div id="dnddst" aria-label="drop target" style="width:180px;height:64px;background:#efc;border:2px dashed #333">drop here</div>
  <input id="file" type="file" multiple aria-label="file input" style="display:block;margin:12px 0">
  <button id="dlg" aria-label="dialog">Dialog</button>
  <div id="log" aria-label="event log"></div>
  <div id="scroller" style="height:120px;width:300px;overflow:auto;border:1px solid #333">
    <div style="height:2400px">scroll content</div>
  </div>
  <script>
    window.__events = [];
    const btn = document.getElementById("btn");
    const inp = document.getElementById("inp");
    const log = document.getElementById("log");
    btn.addEventListener("click", () => {
      window.__events.push("click");
      log.textContent = (log.textContent || "") + "C";
    });
    // 合成事件通道的判据：同一个处理器里只认「不是可信事件」的那一半，
    // 这样可信通道和退化通道各自留下自己的痕迹，谁也盖不住谁。
    btn.addEventListener("click", (event) => {
      if (!event.isTrusted) { window.__events.push("synthclick"); }
    });
    inp.addEventListener("input", () => window.__events.push("input:" + inp.value));
    inp.addEventListener("keydown", (event) => window.__events.push("key:" + event.key));
    window.addEventListener("scroll", () =>
      window.__events.push("windowscroll:" + Math.round(window.scrollY)));
    document.getElementById("scroller").addEventListener("scroll", () =>
      window.__events.push("scroller"));
    const slider = document.getElementById("slider");
    let dragging = false;
    slider.addEventListener("mousedown", () => { dragging = true; window.__events.push("slider:down"); });
    slider.addEventListener("mousemove", () => { if (dragging) { window.__events.push("slider:move"); } });
    window.addEventListener("mouseup", () => {
      if (dragging) { dragging = false; window.__events.push("slider:up"); }
    });
    const src = document.getElementById("dndsrc");
    const dst = document.getElementById("dnddst");
    src.addEventListener("dragstart", (event) => {
      window.__events.push("dragstart");
      try { event.dataTransfer.setData("text/plain", "starship"); } catch (error) {}
    });
    dst.addEventListener("dragover", (event) => { event.preventDefault(); window.__events.push("dragover"); });
    dst.addEventListener("drop", (event) => {
      event.preventDefault();
      window.__events.push("drop:" + (event.dataTransfer.getData("text/plain") || ""));
    });
    document.getElementById("file").addEventListener("change", (event) =>
      window.__events.push("file:" + event.target.files.length));
    document.getElementById("dlg").addEventListener("click", () => {
      window.__events.push("dialog");
      alert("starship-dialog");
    });
  </script>
</body></html>`;

const server = http.createServer((request, response) => {
  response.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
  });
  response.end(FIXTURE_HTML);
});
await new Promise((resolve) => server.listen(FIXTURE_PORT, "127.0.0.1", resolve));
const fixtureUrl = `http://127.0.0.1:${FIXTURE_PORT}/fixture.html`;

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

// The driver acts through the native view, so the panel has to be presented on
// the pane the user is actually looking at. Every visited session keeps a cached
// pane, and a cached pane can still hold a mounted panel with its own active tab:
// a document-wide query would pick that tab and then drive a hidden WebView,
// where input dispatch and screen capture silently do nothing.
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
const visibleOpen = panelState.mounted && Boolean(panelState.rect);
check("the visible pane presents a browser panel", visibleOpen, panelState);
if (!visibleOpen) {
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

const navReply = await evaluate(
  `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"navigate",tabId:${JSON.stringify(
    tabId,
  )},url:${JSON.stringify(fixtureUrl)}})`,
);
check("navigate accepted", navReply?.ok === true, navReply);

let ready = false;
for (let attempt = 0; attempt < 25 && !ready; attempt += 1) {
  await sleep(400);
  const state = await evaluate(
    `window.openclawBrowserDispatch("Runtime.evaluate",{expression:"document.title",returnByValue:true},${JSON.stringify(
      tabId,
    )})`,
  );
  const title = state?.result?.result?.value;
  ready = title === "Starship Driver Fixture";
}
check("fixture page loaded in the native tab", ready);

const elements = await evaluate(
  `window.openclawBrowserElements(${JSON.stringify(tabId)})`,
);
const button = elements?.elements?.find((entry) => entry.tag === "button");
const input = elements?.elements?.find((entry) => entry.tag === "input");
check("element scan returns the fixture controls", Boolean(button && input), elements?.count);
check("element scan assigns stable refs", /^sr-/.test(button?.ref ?? ""), button?.ref);

if (button) {
  const clickByRef = await evaluate(
    `window.openclawBrowserAct({action:"click",tabId:${JSON.stringify(
      tabId,
    )},elementRef:${JSON.stringify(button.ref)}})`,
  );
  check(
    "act click by elementRef confirms",
    clickByRef?.ok === true && clickByRef?.effect === "confirmed",
    clickByRef,
  );
  await sleep(300);
  const events = await evaluate(
    `window.openclawBrowserDispatch("Runtime.evaluate",{expression:"JSON.stringify(window.__events)",returnByValue:true},${JSON.stringify(
      tabId,
    )})`,
  );
  const parsed = JSON.parse(events?.result?.result?.value ?? "[]");
  check("click reached the page handler", parsed.includes("click"), parsed);

  const centreX = Math.round(button.rect.x + button.rect.width / 2);
  const centreY = Math.round(button.rect.y + button.rect.height / 2);
  const clickByPoint = await evaluate(
    `window.openclawBrowserAct({action:"click",tabId:${JSON.stringify(
      tabId,
    )},x:${centreX},y:${centreY}})`,
  );
  check(
    "act click by coordinates confirms",
    clickByPoint?.ok === true && clickByPoint?.effect === "confirmed",
    clickByPoint,
  );
}

if (input) {
  const typed = await evaluate(
    `window.openclawBrowserAct({action:"type",tabId:${JSON.stringify(
      tabId,
    )},elementRef:${JSON.stringify(input.ref)},text:"starship"})`,
  );
  check(
    "act type confirms",
    typed?.ok === true && typed?.effect === "confirmed",
    typed,
  );
  await sleep(300);
  const typedValue = await evaluate(
    `window.openclawBrowserDispatch("Runtime.evaluate",{expression:"document.getElementById('inp').value",returnByValue:true},${JSON.stringify(
      tabId,
    )})`,
  );
  check(
    "typed text reached the page",
    typedValue?.result?.result?.value === "starship",
    typedValue?.result?.result?.value,
  );

  const press = await evaluate(
    `window.openclawBrowserAct({action:"key",tabId:${JSON.stringify(tabId)},key:"Enter"})`,
  );
  check("act key confirms", press?.ok === true && press?.effect === "confirmed", press);
  await sleep(300);
  const afterKey = await evaluate(
    `window.openclawBrowserDispatch("Runtime.evaluate",{expression:"JSON.stringify(window.__events)",returnByValue:true},${JSON.stringify(
      tabId,
    )})`,
  );
  const keyEvents = JSON.parse(afterKey?.result?.result?.value ?? "[]");
  check("keydown reached the page", keyEvents.includes("key:Enter"), keyEvents);
}

const scrolled = await evaluate(
  `window.openclawBrowserAct({action:"scroll",tabId:${JSON.stringify(
    tabId,
  )},x:180,y:220,deltaY:260})`,
);
check("act scroll confirms", scrolled?.ok === true && scrolled?.effect === "confirmed", scrolled);

const inspected = await evaluate(
  `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"inspect",tabId:${JSON.stringify(
    tabId,
  )},x:${button ? Math.round(button.rect.x + button.rect.width / 2) : 40},y:${
    button ? Math.round(button.rect.y + button.rect.height / 2) : 90
  }})`,
);
check("inspect returns a node", inspected?.ok === true && inspected?.node?.tag === "button", inspected);

const snapshot = await evaluate(
  `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"snapshot",tabId:${JSON.stringify(
    tabId,
  )}})`,
);
check(
  "snapshot returns a PNG",
  snapshot?.ok === true &&
    typeof snapshot.dataUrl === "string" &&
    snapshot.dataUrl.startsWith("data:image/png;base64,") &&
    snapshot.cssWidth > 0,
  { ok: snapshot?.ok, width: snapshot?.cssWidth, height: snapshot?.cssHeight },
);

const allowedDispatch = await evaluate(
  `window.openclawBrowserDispatch("DOM.getDocument",{},${JSON.stringify(tabId)})`,
);
check(
  "dispatch allows whitelisted DOM methods",
  allowedDispatch?.ok === true && Boolean(allowedDispatch?.result),
  { ok: allowedDispatch?.ok, error: allowedDispatch?.error },
);

for (const method of ["Browser.close", "Target.activateTarget", "Storage.getCookies"]) {
  const blocked = await evaluate(
    `window.openclawBrowserDispatch(${JSON.stringify(method)},{},${JSON.stringify(tabId)})`,
  );
  check(`dispatch blocks ${method}`, blocked?.ok === false, blocked);
}

const oversized = await evaluate(
  `window.openclawBrowserAct({action:"type",tabId:${JSON.stringify(
    tabId,
  )},text:"x".repeat(20001)})`,
);
check("oversized text rejected", oversized?.ok === false, oversized);

// ---------------------------------------------------------------------------
// 2.0.17 原生级操作：drag / dom_event / upload / 观测序号 / 契约错误码 / dialog
// ---------------------------------------------------------------------------
fs.writeFileSync(UPLOAD_FILE, "starship upload fixture\n", "utf8");

async function childEval(expression) {
  const reply = await evaluate(
    `window.openclawBrowserDispatch("Runtime.evaluate",{expression:${JSON.stringify(
      expression,
    )},returnByValue:true},${JSON.stringify(tabId)})`,
  );
  if (reply?.ok !== true) {
    throw new Error(`child eval failed: ${JSON.stringify(reply)}`);
  }
  return reply.result?.result?.value;
}
async function childEvents() {
  return JSON.parse((await childEval("JSON.stringify(window.__events)")) ?? "[]");
}
async function act(payload) {
  return evaluate(`window.openclawBrowserAct(${JSON.stringify({ tabId, ...payload })})`);
}

const rects = await childEval(
  `(() => {
     const centre = (id) => {
       const box = document.getElementById(id).getBoundingClientRect();
       return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
     };
     return { slider: centre("slider"), src: centre("dndsrc"), dst: centre("dnddst") };
   })()`,
);

if (rects?.slider) {
  await childEval("window.__events = []");
  const dragged = await act({
    action: "drag",
    from: rects.slider,
    to: { x: rects.slider.x + 120, y: rects.slider.y },
    steps: 8,
  });
  check(
    "act drag (pointer route) confirms",
    dragged?.ok === true && dragged?.effect === "confirmed",
    dragged,
  );
  await sleep(300);
  const dragEvents = await childEvents();
  check(
    "pointer drag delivered mousedown/move/mouseup",
    dragEvents.includes("slider:down") &&
      dragEvents.filter((entry) => entry === "slider:move").length >= 2 &&
      dragEvents.includes("slider:up"),
    dragEvents,
  );
}

if (rects?.src && rects?.dst) {
  await childEval("window.__events = []");
  const dropped = await act({
    action: "drag",
    from: rects.src,
    to: rects.dst,
    steps: 6,
    dataItems: [{ mimeType: "text/plain", data: "starship" }],
  });
  check(
    "act drag with dataItems confirms",
    dropped?.ok === true && dropped?.detail?.dragData === true,
    dropped,
  );
  await sleep(400);
  const dndEvents = await childEvents();
  check("HTML5 dragstart reached the source", dndEvents.includes("dragstart"), dndEvents);
  check(
    "HTML5 drop reached the target",
    dndEvents.some((entry) => entry.startsWith("drop")),
    dndEvents,
  );
}

if (button) {
  await childEval("window.__events = []");
  const synthClick = await act({
    action: "click",
    elementRef: button.ref,
    inputRoute: "dom_event",
  });
  check(
    "act click via dom_event confirms",
    synthClick?.ok === true && synthClick?.detail?.inputRoute === "dom_event",
    synthClick,
  );
  await sleep(250);
  check("synthetic click reached the page handler", (await childEvents()).includes("synthclick"));
}

if (input) {
  const synthType = await act({
    action: "type",
    elementRef: input.ref,
    text: "starship-synth",
    inputRoute: "dom_event",
  });
  check(
    "act type via dom_event confirms",
    synthType?.ok === true && synthType?.detail?.inputRoute === "dom_event",
    synthType,
  );
  await sleep(250);
  check(
    "synthetic input landed in the field",
    (await childEval("document.getElementById('inp').value")) === "starship-synth",
  );

  await childEval("window.__events = []");
  const synthKey = await act({ action: "key", key: "Enter", inputRoute: "dom_event" });
  check("act key via dom_event confirms", synthKey?.ok === true, synthKey);
  await sleep(200);
  check("synthetic keydown reached the page", (await childEvents()).includes("key:Enter"));
}

const fileRef = await childEval("document.getElementById('file').getAttribute('data-starship-ref')");
check("file input carries a driver reference", /^sr-/.test(fileRef ?? ""), fileRef);
if (fileRef) {
  await childEval("window.__events = []");
  const uploaded = await act({ action: "upload", elementRef: fileRef, files: [UPLOAD_FILE] });
  check("act upload confirms", uploaded?.ok === true && uploaded?.detail?.files === 1, uploaded);
  await sleep(250);
  check(
    "uploaded file reached the page input",
    (await childEval("document.getElementById('file').files.length")) === 1 &&
      (await childEval("document.getElementById('file').files[0].name")) ===
        path.basename(UPLOAD_FILE),
  );
  const relative = await act({ action: "upload", elementRef: fileRef, files: ["relative.txt"] });
  check("relative upload path is refused", relative?.ok === false, relative);
}

const observed = await evaluate(
  `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"snapshot",tabId:${JSON.stringify(
    tabId,
  )}})`,
);
check("snapshot returns an observation id", /^obs-\d+$/.test(observed?.observationId ?? ""), observed?.observationId);
if (observed?.observationId) {
  const staleId = observed.observationId === "obs-999" ? "obs-998" : "obs-999";
  const stale = await act({ action: "click", x: 4, y: 4, observationId: staleId });
  check(
    "stale observation is refused with the official code",
    stale?.ok === false && stale?.code === "COMPUTER_STALE_OBSERVATION",
    stale,
  );
  const fresh = await act({
    action: "click",
    x: 4,
    y: 4,
    observationId: observed.observationId,
  });
  check(
    "current observation is accepted",
    fresh?.ok === true && fresh?.effect === "confirmed",
    fresh,
  );
}

const mismatch = await act({ action: "teleport", x: 1, y: 1 });
check(
  "unknown action is refused with the official code",
  mismatch?.ok === false && mismatch?.code === "COMPUTER_CONTRACT_MISMATCH",
  mismatch,
);

if (process.env.STARSHIP_SKIP_DIALOG !== "1") {
  await childEval("window.__events = []");
  // 触发 alert 时不能等它的回执：弹窗开着的时候那次 evaluate 本来就回不来。
  await evaluate(
    `void window.openclawBrowserDispatch("Runtime.evaluate",{expression:"document.getElementById('dlg').click()",returnByValue:true},${JSON.stringify(
      tabId,
    )}); "fired"`,
  );
  await sleep(1500);
  const dismissed = await act({ action: "dialog", mode: "dismiss" });
  check(
    "act dialog dismiss confirms",
    dismissed?.ok === true && dismissed?.detail?.dialog === "dismiss",
    dismissed,
  );
  await sleep(400);
  check(
    "panel is responsive after the dialog was handled",
    (await childEval("document.title")) === "Starship Driver Fixture",
  );
}

socket.close();
server.close();
console.log(failures === 0 ? "\nDRIVER REGRESSION: ALL PASS" : `\nDRIVER REGRESSION: ${failures} FAIL`);
process.exit(failures === 0 ? 0 : 1);
