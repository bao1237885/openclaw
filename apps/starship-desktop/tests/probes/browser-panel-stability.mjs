// 10-round browser panel open/close stability test over CDP.
// Usage: node browser-panel-stability.mjs [port] [urlMatch] [rounds] [logPath]
//
// The other suites in this folder take a fixture-server port as their 4th
// positional argument. Accepting one here too is not useful, but silently
// treating it as a log path is actively harmful: the probe then reads no log at
// all and reports "0/6, every round timed out" for a perfectly healthy shell.
// Ignore a bare numeric 4th argument and use the real log, and refuse to run at
// all when the log cannot be read.
import { readFileSync, existsSync, statSync } from "node:fs";
import { eventsSince, lastPresentation, unixNow } from "./shell-geometry.mjs";

const port = process.argv[2] ?? "9222";
const urlMatch = process.argv[3] ?? "chat/main";
const rounds = Number(process.argv[4] ?? 10);
const DEFAULT_LOG =
    "C:\\Users\\36042\\AppData\\Local\\ai.starship.client\\native-browser.log";
const logArg = process.argv[5];
if (logArg !== undefined && /^\d+$/.test(logArg)) {
    console.error(
        `ignoring numeric argument "${logArg}" (a fixture port, not a log path); ` +
            `using ${DEFAULT_LOG}`,
    );
}
const logPath = logArg !== undefined && !/^\d+$/.test(logArg) ? logArg : DEFAULT_LOG;

// A probe that cannot read the shell log can only report false failures: every
// geometry wait times out and every round reports `applied=null`. Fail loudly
// instead of burning two minutes and blaming the shell.
if (!existsSync(logPath)) {
    console.error(`shell log not found: ${logPath}`);
    process.exit(2);
}
const logSizeAtStart = statSync(logPath).size;
if (logSizeAtStart === 0) {
    console.error(`shell log is empty: ${logPath}`);
    process.exit(2);
}

const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const target = list
    .filter((entry) => entry.type === "page")
    .find((entry) => (entry.url ?? "").includes(urlMatch));
if (!target) {
    console.error(
        `no page target matching "${urlMatch}"; available: ` +
            list.map((entry) => entry.url).join(", "),
    );
    process.exit(1);
}

const socket = new WebSocket(target.webSocketDebuggerUrl);
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

function fallbackCounts() {
    if (!existsSync(logPath)) {
        return { present: 0, cleared: 0 };
    }
    const text = readFileSync(logPath, "utf8");
    return {
        present: (text.match(/shell fallback present/g) ?? []).length,
        cleared: (text.match(/shell fallback cleared/g) ?? []).length,
    };
}

function logLines() {
    if (!existsSync(logPath)) {
        return [];
    }
    return readFileSync(logPath, "utf8").split("\n");
}

// `shell apply` / `shell hide` are written whenever the native view receives
// new geometry, so they are the ground truth for "what the user actually sees".
// `lastPresentation` resolves "the apply in the newest batch"; see
// shell-geometry.mjs for why a backwards scan for a single line is wrong.
const lastGeometry = lastPresentation;

// Never anchor a round to a line index: the shell trims its own log, so an index
// can slide past EOF mid-round and the slice would come back empty. Anchor to
// wall-clock instead - every log line starts with unix seconds.
const geometrySince = (sinceSeconds) => lastGeometry(eventsSince(logLines(), sinceSeconds));

function rectsClose(a, b, tolerance = 3) {
    if (!a || !b) {
        return false;
    }
    return a.every((value, index) => Math.abs(value - b[index]) <= tolerance);
}

const results = [];
const before = fallbackCounts();

// The dashboard caches every visited chat pane, and each cached pane keeps its
// own toolbar button. Always drive the pane the user is looking at.
const VISIBLE_PANE = `document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible")`;

const PANEL_STATE = `(() => {
        const pane = ${VISIBLE_PANE};
        const paneCount = document.querySelectorAll("openclaw-chat-pane").length;
        const panel = pane ? pane.querySelector("openclaw-browser-panel") : null;
        if (!panel) return { hasPanel: false, hasPane: Boolean(pane), paneCount };
        const stage = panel.shadowRoot?.querySelector(".bp-stage");
        const rect = stage ? stage.getBoundingClientRect() : null;
        return {
            hasPanel: true,
            paneCount,
            activeTarget: panel.browserPanelController?.activeTargetId ?? null,
            presented: panel.presented ?? null,
            suppressed: panel.suppressed ?? null,
            available: panel.available ?? null,
            dockOpen: panel.dockLayout?.open ?? null,
            rect: rect
                ? [
                      Math.round(rect.x),
                      Math.round(rect.y),
                      Math.round(rect.width),
                      Math.round(rect.height),
                  ]
                : null,
        };
    })()`;

// The official dashboard has two ways to show the browser: the layout slot
// (`presented=true`) and the panel's own dock (`dockLayout.open`). Both end up
// as a mounted `openclaw-browser-panel` with a real stage rect, and that is what
// the user sees, so the round asserts on the mounted stage plus the geometry the
// native view received instead of on one of the two internal flags.
const hasStage = (state) =>
    Boolean(state?.rect) && state.rect[2] > 100 && state.rect[3] > 100;
const isClosed = (state) => Boolean(state) && (state.hasPanel === false || !hasStage(state));
const isOpen = (state) => Boolean(state) && state.hasPanel === true && hasStage(state);

const CLICK_TOGGLE = `(() => {
    const pane = ${VISIBLE_PANE};
    const button = pane ? pane.querySelector(".chat-browser-panel-toggle") : null;
    if (!button) return { err: "no-toggle-button" };
    button.click();
    return { ok: true };
})()`;

// The panel animates in, so a single fixed sleep is both slower than needed and
// weaker than a real settle assertion. Poll until the state matches, and record
// how long that took: a panel that only reaches full size after seconds is a
// user-visible defect even when it eventually succeeds.
const OPEN_TIMEOUT_MS = 8000;
const CLOSE_TIMEOUT_MS = 6000;
const POLL_MS = 150;

// The dashboard mounts and unmounts the panel synchronously, so the DOM flips
// long before the child WebView2 moves. Wait on the shell log instead: it is the
// only readable record of the geometry the native view actually received.
async function waitForGeometry(match, timeoutMs, since) {
    const started = Date.now();
    let geometry = geometrySince(since);
    while (!match(geometry) && Date.now() - started < timeoutMs) {
        await sleep(POLL_MS);
        geometry = geometrySince(since);
    }
    return { ok: match(geometry), geometry, settleMs: Date.now() - started };
}

// `shell apply` / `shell hide` are deduplicated on the shell side, so they only
// fire when the geometry actually changes. Every round therefore has to own a
// fresh open and a fresh close; a round that reuses the previous round's open
// would assert on nothing. Start from a guaranteed-closed panel so round 0 gets
// the same treatment as the rest.
// A single closed sample is not a baseline: the stage shrinks through the
// animation, so the DOM can report "closed" for a frame and then finish
// opening. Require several consecutive closed samples before trusting it.
async function stableClosed(timeoutMs) {
    const started = Date.now();
    let consecutive = 0;
    let state = await evaluate(PANEL_STATE);
    while (Date.now() - started < timeoutMs) {
        consecutive = isClosed(state) ? consecutive + 1 : 0;
        if (consecutive >= 3) {
            return { ok: true, state, settleMs: Date.now() - started };
        }
        await sleep(POLL_MS);
        state = await evaluate(PANEL_STATE);
    }
    return { ok: false, state, settleMs: Date.now() - started };
}

async function ensureClosed() {
    const state = await evaluate(PANEL_STATE);
    if (!isClosed(state)) {
        await evaluate(CLICK_TOGGLE);
    }
    const baseline = await stableClosed(CLOSE_TIMEOUT_MS);
    if (!baseline.ok) {
        throw new Error(
            "cannot establish a closed baseline: " + JSON.stringify(baseline.state),
        );
    }
    return baseline;
}

await ensureClosed();

for (let round = 0; round < rounds; round += 1) {
    const entry = { round };
    // A round only proves anything if it starts from a settled closed panel,
    // even when something else toggled the panel between rounds.
    await ensureClosed();
    // Each round owns a fresh open/close pair, so the log window starts here.
    const logSince = unixNow();

    // Open via the real toolbar toggle (the user path).
    entry.openClick = await evaluate(CLICK_TOGGLE);
    const openGeometry = await waitForGeometry(
        (geometry) =>
            geometry?.kind === "apply" && geometry.rect[2] > 100 && geometry.rect[3] > 100,
        OPEN_TIMEOUT_MS,
        logSince,
    );
    entry.openGeometry = openGeometry.geometry;
    entry.openSettleMs = openGeometry.settleMs;
    entry.afterOpen = await evaluate(PANEL_STATE);

    // Close via the same toggle.
    entry.closeClick = await evaluate(CLICK_TOGGLE);
    const closeGeometry = await waitForGeometry(
        (geometry) => geometry?.kind === "hide",
        CLOSE_TIMEOUT_MS,
        logSince,
    );
    entry.closeGeometry = closeGeometry.geometry;
    entry.closeSettleMs = closeGeometry.settleMs;
    entry.afterClose = await evaluate(PANEL_STATE);

    entry.fallback = fallbackCounts();
    entry.ok =
        openGeometry.ok &&
        isOpen(entry.afterOpen) &&
        Boolean(entry.afterOpen?.activeTarget) &&
        closeGeometry.ok &&
        isClosed(entry.afterClose) &&
        // The native view has to receive the same geometry the panel shows,
        // and closing has to actually tear the native view down.
        rectsClose(entry.openGeometry.rect, entry.afterOpen?.rect);
    results.push(entry);
    console.log(
        `round ${round}: ok=${entry.ok} opened=${openGeometry.ok} closed=${closeGeometry.ok} ` +
            `activeTarget=${entry.afterOpen?.activeTarget ?? "null"} ` +
            `rect=${JSON.stringify(entry.afterOpen?.rect ?? null)} ` +
            `applied=${JSON.stringify(entry.openGeometry?.rect ?? null)} ` +
            `closeEvent=${entry.closeGeometry?.kind ?? "none"} ` +
            `settle=${entry.openSettleMs}/${entry.closeSettleMs}ms ` +
            `panes=${entry.afterOpen?.paneCount ?? "?"}`,
    );
}

const after = fallbackCounts();
const passed = results.filter((entry) => entry.ok).length;
console.log(
    JSON.stringify(
        {
            rounds,
            passed,
            failed: results.filter((entry) => !entry.ok).map((entry) => entry.round),
            fallbackDuringTest: {
                present: after.present - before.present,
                cleared: after.cleared - before.cleared,
            },
            details: results,
        },
        null,
        2,
    ),
);
socket.close();
process.exit(passed === rounds ? 0 : 1);
