// Live smoke for the shell's download ledger and the panel's 下载 menu.
//
// Two things can rot independently: the shell's `add_DownloadStarting` wiring
// (then `state.downloads` stays empty and the user has no idea where a file
// went) and the injected menu plumbing (then the ledger grows but nothing on
// screen reads it). This probe drives one real download from a local fixture
// and checks both ends, plus the guards on the open/reveal hand-off.
// Usage: node probe-downloads.mjs [cdpPort] [dashboardUrlMatch] [fixturePort]
import http from "node:http";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { execFileSync } from "node:child_process";

const CDP_PORT = process.argv[2] ?? "9334";
const DASHBOARD_MATCH = process.argv[3] ?? "chat/main";
const FIXTURE_PORT = Number(process.argv[4] ?? 19211);

const FILE_NAME = "starship-dl-probe.bin";
const FILE_BYTES = 1_600_000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const INDEX = `<!doctype html>
<html><head><meta charset="utf-8"><title>Download Fixture</title></head>
<body style="margin:0;font-family:system-ui;font-size:16px">
  <h1 id="headline">Download Fixture</h1>
  <a id="dl" href="/dl" style="display:block;padding:20px;font-size:18px">download me</a>
</body></html>`;

const server = http.createServer(async (request, response) => {
  if ((request.url ?? "").startsWith("/dl")) {
    response.writeHead(200, {
      "Content-Type": "application/octet-stream",
      "Content-Disposition": `attachment; filename="${FILE_NAME}"`,
      "Content-Length": String(FILE_BYTES),
      "Cache-Control": "no-store",
    });
    // Hand the bytes over slowly enough that the shell's progress throttle has
    // something to coalesce: an instant body would prove the completion path
    // but never the in-progress one.
    const chunk = Buffer.alloc(100_000, 7);
    for (let sent = 0; sent < FILE_BYTES; sent += chunk.length) {
      if (response.writableEnded || response.destroyed) {
        return;
      }
      response.write(chunk.subarray(0, Math.min(chunk.length, FILE_BYTES - sent)));
      await sleep(60);
    }
    response.end();
    return;
  }
  response.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
  });
  response.end(INDEX);
});
await new Promise((resolve) => server.listen(FIXTURE_PORT, "127.0.0.1", resolve));
const indexUrl = `http://127.0.0.1:${FIXTURE_PORT}/index.html`;
const downloadUrl = `http://127.0.0.1:${FIXTURE_PORT}/dl`;

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
      reject(new Error(`${method} timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
}
socket.addEventListener("message", (event) => {
  const payload = JSON.parse(event.data);
  if (payload.id === undefined) {
    return;
  }
  const entry = pending.get(payload.id);
  if (!entry) {
    return;
  }
  pending.delete(payload.id);
  clearTimeout(entry.timer);
  if (payload.error) {
    entry.reject(new Error(JSON.stringify(payload.error)));
  } else {
    entry.resolve(payload.result);
  }
});
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", reject, { once: true });
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

// The visible pane owns the panel the user is looking at; a document-wide query
// could pick a cached pane's copy and drive a hidden WebView.
const PANEL_STATE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  const controller = panel ? panel.browserPanelController : null;
  if (!controller) return { mounted: false };
  return {
    mounted: true,
    activeTargetId: controller.activeTargetId ?? null,
    tabs: (controller.tabs ?? []).map((tab) => ({ id: tab.id, url: tab.url ?? null })),
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
// A freshly started shell can have the panel mounted with an empty tab strip.
// Ask the bridge itself for a tab instead of assuming a previous session left
// one behind.
if (panel.mounted && !panel.activeTargetId) {
  await evaluate(
    `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"open",url:${JSON.stringify(
      indexUrl,
    )}})`,
  );
  for (let attempt = 0; attempt < 20 && !panel.activeTargetId; attempt += 1) {
    await sleep(300);
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

const stateDownloads = async () =>
  evaluate(`(() => {
    const state = window.__OPENCLAW_NATIVE_BROWSER__;
    const list = state ? state.downloads : undefined;
    return Object.prototype.toString.call(list) === "[object Array]" ? list : null;
  })()`);

const request = (message) =>
  evaluate(
    `window.webkit.messageHandlers.openclawBrowser.postMessage(${JSON.stringify(message)})`,
    30000,
  );

const navigate = (url) =>
  evaluate(
    `window.webkit.messageHandlers.openclawBrowser.postMessage({type:"navigate",tabId:${JSON.stringify(
      tabId,
    )},url:${JSON.stringify(url)}})`,
  );

async function waitForTitle(expected, budgetMs = 12000) {
  const deadline = Date.now() + budgetMs;
  while (Date.now() < deadline) {
    await sleep(400);
    try {
      const title = await evaluate(
        `window.openclawBrowserDispatch("Runtime.evaluate",{expression:"document.title",returnByValue:true},${JSON.stringify(
          tabId,
        )})`,
      );
      if (title?.result?.result?.value === expected) {
        return true;
      }
    } catch (error) {
      // a transient CDP failure is not a verdict; keep polling
    }
  }
  return false;
}

// ------------------------------------------------------------------- ledger
check(
  "the shell publishes a downloads array in its browser state",
  (await stateDownloads()) !== null,
);

// 落盘目录只有壳层知道：用户在资源管理器里把「下载」挪到别的盘以后，
// `%USERPROFILE%\Downloads` 那个目录通常还在，但文件根本不落那儿。这条问句回
// 的 `directory` 就是壳层记账时用的目录。
const downloadsReply = await request({ type: "downloads" });
const downloadDirectory =
  typeof downloadsReply?.directory === "string" && downloadsReply.directory.length > 0
    ? downloadsReply.directory
    : path.join(os.homedir(), "Downloads");
const profileFallback = path.join(os.homedir(), "Downloads");

// 再用一份独立算出来的「系统记录的下载文件夹」交叉验证壳层。注意不能读隔壁的
// `Shell Folders`：那一份在本机就是过期的 `C:\Users\36042\Downloads`，真正有权
// 威的是 `User Shell Folders` 里 `FOLDERID_Downloads` 那条。PowerShell 把结果编
// 成 base64 交出来，免得中文路径死在控制台代码页上。
const knownDownloadsFolder = (() => {
  if (process.platform !== "win32") {
    return null;
  }
  const key =
    "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders";
  const id = "{374DE290-123F-4565-9164-39C4925E467B}";
  const read = `[Environment]::ExpandEnvironmentVariables((Get-ItemProperty -Path '${key}' -Name '${id}').'${id}')`;
  try {
    const encoded = execFileSync(
      "powershell.exe",
      [
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        `[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(${read}))`,
      ],
      { encoding: "utf8" },
    ).trim();
    return Buffer.from(encoded, "base64").toString("utf8").trim() || null;
  } catch {
    return null;
  }
})();

check(
  "the shell resolves the same download folder Windows records",
  knownDownloadsFolder === null ||
    path.resolve(downloadDirectory).toLowerCase() ===
      path.resolve(knownDownloadsFolder).toLowerCase(),
  {
    shell: downloadDirectory,
    windows: knownDownloadsFolder,
    profileFallback,
  },
);

// 两处目录都是这条探针允许碰的地方：壳层报的那个（文件真正落的地方）和
// `%USERPROFILE%\Downloads`（旧的错答案，早先的版本在那儿留过文件）。
const sweepDirectories = [
  ...new Set([downloadDirectory, profileFallback].map((directory) => path.resolve(directory))),
];

// 上一次跑剩下的 `starship-dl-probe.bin` 不是无害的：WebView2 会把这一次存成
// `starship-dl-probe (1).bin`，下面每一条「文件名对不对」的断言就都在问另一个文
// 件。所以下载开始前先扫一遍，跑完再扫一遍。
function sweepProbeFiles() {
  let removed = 0;
  for (const directory of sweepDirectories) {
    try {
      for (const name of fs.readdirSync(directory)) {
        if (!name.startsWith("starship-dl-probe") || !name.endsWith(".bin")) {
          continue;
        }
        const target = path.join(directory, name);
        if (path.dirname(path.resolve(target)) !== path.resolve(directory)) {
          continue;
        }
        fs.rmSync(target, { force: true });
        removed += 1;
      }
    } catch (error) {
      console.log(`NOTE  could not sweep ${directory}: ${error.message}`);
    }
  }
  return removed;
}

const stale = sweepProbeFiles();
if (stale > 0) {
  console.log(`NOTE  removed ${stale} stale probe file(s) before the download`);
}

check("navigate accepted", (await navigate(indexUrl))?.ok === true);
check("fixture index loaded", await waitForTitle("Download Fixture"), indexUrl);

// A download can only be attributed to a tab the shell owns, so clear anything
// a previous run left behind before judging this one.
await request({ type: "downloads", clear: true });

check("download navigate accepted", (await navigate(downloadUrl))?.ok === true);

let entry = null;
const appearedBy = Date.now() + 20000;
while (Date.now() < appearedBy && !entry) {
  await sleep(400);
  const list = await stateDownloads();
  entry = (list ?? []).find((item) => String(item?.filename ?? "").includes(FILE_NAME)) ?? null;
}
check("the shell recorded the download in its ledger", Boolean(entry), entry);

if (entry) {
  const landed = path.resolve(String(entry.path ?? ""));
  check(
    "the ledger points at a file the shell wrote inside the download folder",
    path.isAbsolute(String(entry.path ?? "")) &&
      path.dirname(landed).toLowerCase() === path.resolve(downloadDirectory).toLowerCase() &&
      String(entry.filename ?? "").includes(FILE_NAME) &&
      fs.existsSync(landed),
    {
      path: entry.path,
      filename: entry.filename,
      expected: downloadDirectory,
      exists: fs.existsSync(landed),
    },
  );
  check(
    "the ledger attributes the download to the tab that asked for it",
    entry.tabId === tabId,
    { ledgerTab: entry.tabId, activeTab: tabId },
  );

  const finishedBy = Date.now() + 30000;
  while (Date.now() < finishedBy && entry.state === "in_progress") {
    await sleep(500);
    const list = await stateDownloads();
    entry =
      (list ?? []).find((item) => String(item?.filename ?? "").includes(FILE_NAME)) ?? entry;
  }
  check("the download reaches the completed state", entry.state === "completed", entry.state);
  check(
    "the byte count matches the fixture body",
    Number(entry.total) === FILE_BYTES && Number(entry.received) === FILE_BYTES,
    { total: entry.total, received: entry.received, expected: FILE_BYTES },
  );

  // 这条才是这次的回归点：`open_download_path` 会先 canonicalize 一遍下载目录
  // 再做包含性校验，目录算错的时候，列表里刚下完的那个文件点开会被当成越权路
  // 径拒掉。`reveal` 会在资源管理器里选中该文件——探针唯一的窗口副作用。
  const revealed = await request({ type: "downloads", reveal: entry.path });
  check(
    "the file the shell just wrote can be revealed from the panel",
    revealed?.action?.ok === true,
    revealed,
  );
}

// --------------------------------------------------------------------- menu
// The ledger is only worth having if the panel reads it: drive the real entry
// point (⋮ → 下载) instead of calling the menu's internals.
const OPEN_MENU = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  if (!panel || !panel.shadowRoot) return { err: "no-panel" };
  const toggle = panel.shadowRoot.querySelector(".starship-extras__toggle");
  if (!toggle) return { err: "no-extras-toggle" };
  toggle.click();
  const menu = panel.shadowRoot.querySelector(".starship-extras__menu");
  if (!menu) return { err: "no-extras-menu" };
  const items = Array.from(menu.querySelectorAll("button"));
  const labels = items.map((item) => (item.textContent || "").trim());
  const target = items.find((item) => {
    const text = item.textContent || "";
    return text.includes("\\u4e0b\\u8f7d") && !text.includes("\\u6587\\u4ef6\\u5939");
  });
  if (!target) return { err: "no-download-entry", labels };
  target.click();
  return { ok: true };
})()`;
const MENU_STATE = `(() => {
  const pane = document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible");
  const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
  if (!panel || !panel.shadowRoot) return { mounted: false };
  const menu = panel.shadowRoot.querySelector(".starship-dl__menu");
  if (!menu) return { mounted: false };
  const rect = menu.getBoundingClientRect();
  return {
    mounted: true,
    hidden: Boolean(menu.hidden),
    rect: [rect.x, rect.y, rect.width, rect.height],
    rows: Array.from(menu.querySelectorAll(".starship-dl__row")).map((row) =>
      (row.textContent || "").trim(),
    ),
    overlays: Boolean(menu.getAttribute("data-starship-overlay")),
  };
})()`;

const opened = await evaluate(OPEN_MENU);
check("the ⋮ menu carries a 下载 entry", opened?.ok === true, opened);
await sleep(400);
let menu = await evaluate(MENU_STATE);
check("the 下载 entry reveals the download menu", menu.mounted && !menu.hidden, menu);
check(
  "the download menu lists the file that just finished",
  menu.rows.some((row) => row.includes(FILE_NAME)),
  menu.rows,
);
check(
  "the download menu is flagged as an overlay so the shell steps the page aside",
  Boolean(menu.overlays),
  menu,
);
check(
  "the download menu lands on screen, not off the edge of the window",
  Array.isArray(menu.rect) &&
    menu.rect[2] > 200 &&
    menu.rect[3] > 40 &&
    menu.rect[1] >= 0,
  menu.rect,
);

// ------------------------------------------------------------ open / reveal
// The panel handle is untrusted input: it must not become a shell-execute hook
// for arbitrary paths on disk.
const outside = await request({
  type: "downloads",
  openFile: "C:\\\\Windows\\\\System32\\\\calc.exe",
});
check(
  "opening a file outside the download directory is refused",
  outside?.action?.ok === false,
  outside,
);
const missing = await request({ type: "downloads", reveal: path.join(downloadDirectory, "starship-dl-missing.bin") });
check(
  "revealing a file that does not exist is refused",
  missing?.action?.ok === false,
  missing,
);

// -------------------------------------------------------------- clear sweep
const cleared = await request({ type: "downloads", clear: true });
const afterClear = (await stateDownloads()) ?? ["unreadable"];
check("clearing the ledger is accepted", cleared?.ok === true, cleared);
check("clearing the ledger empties the list", afterClear.length === 0, afterClear);

// ------------------------------------------------------------------ cleanup
// Only the file this probe just asked for, and only inside the two directories
// named above: the sweep never walks anywhere else.
const removed = sweepProbeFiles();
console.log(`      (swept ${removed} probe file(s) from ${sweepDirectories.join(" + ")})`);

console.log(failures === 0 ? "ALL PASS" : `FAILURES: ${failures}`);
socket.close();
server.close();
process.exit(failures === 0 ? 0 : 1);
