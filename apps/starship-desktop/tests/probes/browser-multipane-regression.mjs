// Multi-pane alignment regression: the dashboard caches every visited chat pane
// and each cached pane keeps its own browser panel pointing at the same tab, so
// a cached pane can ask for the native view at its own - stale - geometry. The
// invariant under test is continuous: at every quiet moment the geometry the
// native view actually received must equal the stage rect of the pane the user
// is looking at, and no cached pane may move it.
// Usage: node browser-multipane-regression.mjs [cdpPort] [urlMatch] [altSessionHref] [idleMs] [thirdSessionHref]
import { existsSync, readFileSync } from "node:fs";
import { lastPresentation, rectsClose as rectsCloseShared } from "./shell-geometry.mjs";

const CDP_PORT = process.argv[2] ?? "9222";
const URL_MATCH = process.argv[3] ?? "chat/main";
const ALT_HREF = process.argv[4] ?? "/chat/main/3f3e355c";
const IDLE_MS = Number(process.argv[5] ?? 12000);
const THIRD_HREF = process.argv[6] ?? "/chat/main/7f392817";
const LOG_PATH =
    process.env.STARSHIP_NATIVE_BROWSER_LOG ??
    "C:\\Users\\36042\\AppData\\Local\\ai.starship.client\\native-browser.log";

const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
const target = list
    .filter((entry) => entry.type === "page")
    .find((entry) => (entry.url ?? "").includes(URL_MATCH));
if (!target) {
    console.error(`no page target matching "${URL_MATCH}"`);
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

const logLines = () =>
    existsSync(LOG_PATH) ? readFileSync(LOG_PATH, "utf8").split("\n") : [];

// The child WebView2 exposes no readable bounds through CDP, so the geometry the
// shell logs as it applies it is the only end-to-end record of what is on screen.
// Resolve the newest apply batch in the whole log rather than the last single
// line: the invariant under test is "what the native view shows right now", which
// is global state. A delta since the run started would report "nothing" whenever
// the panel legitimately stayed put (the shell only logs geometry changes), and a
// trailing `shell hide` belongs to a different tab.
const lastApplied = () => lastPresentation(logLines());

const SAMPLE = `(() => {
    const panes = [...document.querySelectorAll("openclaw-chat-pane")].map((pane, index) => {
        const panel = pane.querySelector("openclaw-browser-panel");
        const stage = panel && panel.shadowRoot ? panel.shadowRoot.querySelector(".bp-stage") : null;
        const rect = stage ? stage.getBoundingClientRect() : null;
        return {
            index,
            visible: pane.classList.contains("chat-pane-cache__pane--visible"),
            active: pane.classList.contains("chat-pane-cache__pane--active"),
            presented: panel ? panel.presented : null,
            stage: rect ? [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)] : null,
        };
    });
    const visible = panes.find((pane) => pane.visible && pane.active) ?? panes.find((pane) => pane.visible) ?? null;
    return {
        paneCount: panes.length,
        visibleStage: visible ? visible.stage : null,
        visibleIndex: visible ? visible.index : null,
        cachedStages: panes.filter((pane) => !pane.visible).map((pane) => pane.stage),
        panelMounted: Boolean(document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible openclaw-browser-panel")),
    };
})()`;

const rectsClose = rectsCloseShared;

const VISIBLE_PANE = `document.querySelector("openclaw-chat-pane.chat-pane-cache__pane--visible")`;
const CLICK_TOGGLE = `(() => {
    const pane = ${VISIBLE_PANE};
    const button = pane ? pane.querySelector(".chat-browser-panel-toggle") : null;
    if (!button) return { err: "no-toggle-button" };
    button.click();
    return { ok: true };
})()`;

const clickSession = (href) => `(() => {
    const wanted = ${JSON.stringify(href)};
    const link = [...document.querySelectorAll("a[href]")].find((node) =>
        (node.getAttribute("href") ?? "").startsWith(wanted),
    );
    if (!link) return { err: "no-link" };
    link.click();
    return { ok: true, href: link.getAttribute("href") };
})()`;

const failures = [];
const note = (message) => {
    failures.push(message);
    console.log(`FAIL ${message}`);
};

// A DOM change during a pane switch is legitimate: the panel is mid-transition.
// Only a steady-state mismatch is a defect, so give every detected DOM change a
// grace window before asserting the invariant.
const SETTLE_GRACE_MS = 1200;
const TICK_MS = 250;

async function watch(durationMs, label) {
    const started = Date.now();
    let lastStage = null;
    let lastStageChangeAt = started;
    let worstLag = 0;
    let samples = 0;
    while (Date.now() - started < durationMs) {
        const sample = await evaluate(SAMPLE);
        const stage = sample.visibleStage;
        const applied = lastApplied();
        samples += 1;
        if (JSON.stringify(stage) !== JSON.stringify(lastStage)) {
            lastStage = stage;
            lastStageChangeAt = Date.now();
        }
        const quietFor = Date.now() - lastStageChangeAt;
        if (sample.panelMounted && stage) {
            if (!applied || !rectsClose(applied.rect, stage)) {
                worstLag = Math.max(worstLag, quietFor);
                if (quietFor > SETTLE_GRACE_MS) {
                    note(
                        `${label}: native view at ${JSON.stringify(applied?.rect ?? null)} but the ` +
                            `visible pane shows ${JSON.stringify(stage)} (quiet for ${quietFor}ms)`,
                    );
                }
            }
        } else if (applied?.kind !== "hide" && quietFor > SETTLE_GRACE_MS) {
            note(
                `${label}: the visible pane has no open panel, so the native view should be hidden, ` +
                    `but it is ${JSON.stringify(applied?.rect ?? applied?.kind ?? null)}`,
            );
        }
        await sleep(TICK_MS);
    }
    return { samples, worstLag };
}

// Phase 1: make sure the panel is open on the visible pane.
const initial = await evaluate(SAMPLE);
if (!initial.panelMounted) {
    await evaluate(CLICK_TOGGLE);
    await sleep(2500);
}

console.log(`logLines=${logLines().length} bytes=${existsSync(LOG_PATH) ? readFileSync(LOG_PATH, "utf8").length : 0}`);
const phaseResults = {};

// Every session keeps its own pane, and each pane remembers whether its browser
// panel was open. Switching therefore has two legitimate outcomes: the native
// view follows the new pane, or it is torn down because that pane's panel is
// closed. Leaking the previous pane's geometry is the defect.
async function assertPaneContract(label) {
    const sample = await evaluate(SAMPLE);
    const applied = lastApplied();
    const detail = {
        label,
        applied: applied?.rect ?? applied?.kind ?? null,
        visibleStage: sample.visibleStage,
        cachedStages: sample.cachedStages,
        panelMounted: sample.panelMounted,
        paneCount: sample.paneCount,
    };
    if (sample.panelMounted && sample.visibleStage) {
        if (!rectsClose(applied?.rect, sample.visibleStage)) {
            note(
                `${label}: native view at ${JSON.stringify(applied?.rect ?? applied?.kind ?? null)} but the ` +
                    `visible pane shows ${JSON.stringify(sample.visibleStage)}`,
            );
        } else {
            const stolen = (sample.cachedStages ?? []).some(
                (stage) => stage && !rectsClose(stage, sample.visibleStage) && rectsClose(applied?.rect, stage),
            );
            if (stolen) {
                note(`${label}: the native view followed a cached pane instead of the visible one`);
            }
            const cachedDiffers = (sample.cachedStages ?? []).some(
                (stage) => stage && !rectsClose(stage, sample.visibleStage),
            );
            if (cachedDiffers) {
                console.log(
                    `${label}: cached panes report a different rect, so this run really did exercise the guard`,
                );
            }
        }
    } else if (applied?.kind !== "hide") {
        note(
            `${label}: the visible pane has no open panel, so the native view should be hidden, ` +
                `but it is ${JSON.stringify(applied?.rect ?? applied?.kind ?? null)}`,
        );
    }
    console.log(
        `${label}: applied=${JSON.stringify(detail.applied)} visible=${JSON.stringify(sample.visibleStage)} ` +
            `cached=${JSON.stringify(sample.cachedStages)} mounted=${sample.panelMounted} panes=${sample.paneCount}`,
    );
    return { ...detail, sample };
}

// Phase 2: switch sessions. The new pane usually opens with its panel closed,
// which must tear the native view down rather than leave it over the old pane.
const clickedAlt = await evaluate(clickSession(ALT_HREF));
console.log("switch", JSON.stringify(clickedAlt));
await sleep(2500);
phaseResults.afterSwitch = await assertPaneContract("after switch");

// Phase 3: open the panel on the newly visible pane, then idle. A cached pane
// that re-presents on its own would drift the view off this pane.
if (!phaseResults.afterSwitch.sample.panelMounted) {
    await evaluate(CLICK_TOGGLE);
    await sleep(2500);
    phaseResults.openedOnNewPane = await assertPaneContract("opened on new pane");
}
phaseResults.idle = await watch(IDLE_MS, "idle");

// Phase 4: switch to a third session, then back. Coming back must restore
// alignment to the cached pane that owns the open panel.
const clickedThird = await evaluate(clickSession(THIRD_HREF));
console.log("switchThird", JSON.stringify(clickedThird));
await sleep(2500);
phaseResults.third = await assertPaneContract("third session");
const clickedBack = await evaluate(clickSession(ALT_HREF));
console.log("switchBack", JSON.stringify(clickedBack));
await sleep(2500);
phaseResults.back = await assertPaneContract("back to second pane");
phaseResults.backWatch = await watch(4000, "settled on second pane");

// Phase 5: close and confirm the native view is torn down.
await evaluate(CLICK_TOGGLE);
await sleep(2500);
const closedApplied = lastApplied();
const closedSample = await evaluate(SAMPLE);
phaseResults.closed = { sample: closedSample, applied: closedApplied };
console.log(
    `after close: applied=${JSON.stringify(closedApplied)} mounted=${closedSample.panelMounted}`,
);
if (closedApplied?.kind !== "hide") {
    note(`closing the panel left the native view at ${JSON.stringify(closedApplied)}`);
}

console.log(
    JSON.stringify({ failures, phaseResults }, null, 2),
);
socket.close();
process.exit(failures.length === 0 ? 0 : 1);
