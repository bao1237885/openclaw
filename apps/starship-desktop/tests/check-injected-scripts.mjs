// Syntax-checks every JavaScript block the shell injects into the dashboard or a
// tab, straight out of the Rust source that carries it.
//
// The injected layer is one big script: a single stray character - an unescaped
// backtick inside the CSS template literal is the one that actually shipped -
// turns the whole payload into a syntax error. The browser then swallows it and
// the panel quietly falls back to stock OpenClaw: no address bar, no tab strip,
// no download ledger. Nothing in the Rust build catches that, because to Rust it
// is just a string, so this check stands in for the JS parser.
// Usage: node check-injected-scripts.mjs [path-to-native_browser.rs]
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const source = path.resolve(
  process.argv[2] ?? path.join(here, "..", "src-tauri", "src", "native_browser.rs"),
);
const text = fs.readFileSync(source, "utf8");
const lines = text.split("\n");

const BLOCK = /(?:const|static|let)\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?::\s*&str)?\s*=\s*r#"/g;

const lineOf = (index) => text.slice(0, index).split("\n").length;

let failures = 0;
let checked = 0;
for (const match of text.matchAll(BLOCK)) {
  const name = match[1];
  const bodyStart = match.index + match[0].length;
  const bodyEnd = text.indexOf('"#', bodyStart);
  if (bodyEnd < 0) {
    console.log(`FAIL  ${name}: raw string is never closed`);
    failures += 1;
    continue;
  }
  const body = text.slice(bodyStart, bodyEnd);
  const startLine = lineOf(match.index);
  checked += 1;
  try {
    // Parsing only: these scripts talk to the page and the shell, so they are
    // never executed here.
    new vm.Script(body, { filename: `${name}@native_browser.rs:${startLine}` });
    console.log(
      `PASS  ${name} parses (${body.split("\n").length} lines from native_browser.rs:${startLine})`,
    );
  } catch (error) {
    failures += 1;
    // Translate the offset inside the block back to a line in the Rust file.
    const inner = /:(\d+)\n/.exec(String(error.stack ?? ""));
    const offset = inner ? Number(inner[1]) - 1 : null;
    const where = offset === null ? null : startLine + offset;
    console.log(
      `FAIL  ${name} does not parse: ${error.message}` +
        (where ? `  -> native_browser.rs:${where}` : ""),
    );
    if (where) {
      console.log(`      ${(lines[where - 1] ?? "").trim()}`);
    }
  }
}

console.log(failures === 0 ? `ALL PASS (${checked} blocks)` : `FAILURES: ${failures}`);
process.exit(failures === 0 ? 0 : 1);
