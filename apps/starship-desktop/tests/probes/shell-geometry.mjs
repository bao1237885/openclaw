// Shared reader for the shell's native-view geometry log.
//
// `apply_presentations` in src-tauri/src/native_browser.rs writes one batch per
// geometry change, in tab order: the presented tab is logged as
// `shell apply tab=<id> rect=(x,y,w,h)` and every other tab in the same batch is
// logged as `shell hide tab=<id>`. Reading the log by scanning backwards for the
// first `apply|hide` line therefore reports the wrong tab whenever the presented
// tab is not last in the tab list, and reports `hide` for an open panel. The only
// correct reading is "the apply in the newest batch"; a batch without any apply
// is a teardown, i.e. nothing is presented.

export function parseGeometryLine(line) {
    const apply = /shell apply tab=(\S+) rect=\((-?\d+),(-?\d+),(-?\d+),(-?\d+)\)/.exec(line);
    if (apply) {
        return {
            kind: "apply",
            tab: apply[1],
            rect: [Number(apply[2]), Number(apply[3]), Number(apply[4]), Number(apply[5])],
        };
    }
    const hide = /shell hide tab=(\S+)/.exec(line);
    if (hide) {
        return { kind: "hide", tab: hide[1], rect: null };
    }
    return null;
}

// Returns the geometry the native view is actually showing right now:
// `{ kind: "apply", tab, rect }`, `{ kind: "hide", tab, rect: null }`, or null
// when the slice holds no geometry event at all.
export function lastPresentation(lines) {
    const parsed = lines.map(parseGeometryLine);
    let end = parsed.length - 1;
    while (end >= 0 && !parsed[end]) {
        end -= 1;
    }
    if (end < 0) {
        return null;
    }
    let start = end;
    while (start - 1 >= 0 && parsed[start - 1]) {
        start -= 1;
    }
    const batch = parsed.slice(start, end + 1);
    return batch.find((entry) => entry.kind === "apply") ?? batch[batch.length - 1];
}

export const rectsClose = (a, b, tolerance = 3) =>
    Boolean(a) && Boolean(b) && a.every((value, index) => Math.abs(value - b[index]) <= tolerance);

export const unixNow = () => Date.now() / 1000;

// The shell trims its own log, so a line index taken at the start of a run can
// end up past the end of the file (empty slice) even though the run produced
// events. Every line carries a unix-seconds prefix, so select by time instead.
export function eventsSince(lines, sinceSeconds) {
    return lines.filter((line) => {
        const match = /^(\d{9,}(?:\.\d+)?)\s/.exec(line);
        return match ? Number(match[1]) >= sinceSeconds : false;
    });
}
