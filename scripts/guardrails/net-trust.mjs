// Checks that every HTTPS request and `wss://` socket trusts the roots `crates/net-trust` gives it (the
// compiled-in list and the operating system's store), so a network that inspects TLS with a company root
// in the system store works for every path, as it does in Desktop. Flags, outside the allowed files: a ureq agent not
// started from `net_trust::agent_config()` (`Agent::config_builder()`, `Agent::new_with_defaults`,
// ureq's default agent `ureq::get(…)` and the like), a hand-made `RootCerts`, and a WebSocket opened
// with the library's own roots (`tungstenite::connect(…)`, `tungstenite::client_tls(…)`, or
// `client_tls_with_config(…)` without `net_trust::rustls_client_config()` on the same line).
// String literals and comments are not code and are skipped.
// Runs on Bun or Node >= 18. Usage: node scripts/guardrails/net-trust.mjs
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { gitLsFiles } from "./rust-scan.mjs";

const root = join(fileURLToPath(new URL("../..", import.meta.url)));

// The constructor itself; the browser shim (a page's requests are the browser's); and a build
// script's download at build time, not a request of the app.
const ALLOWED = ["crates/net-trust/", "crates/ureq-web/", "crates/terminal/build_support.rs"];
const RULES = [
  { re: /\bAgent::(config_builder|new_with_defaults|new_with_config)\s*\(/, use: "net_trust::agent_config()" },
  { re: /\bureq::(get|post|put|patch|delete|head|request|run)\s*\(/, use: "an agent from net_trust::agent_config()" },
  { re: /\bRootCerts::/, use: "net_trust::agent_config() (or net_trust::tls_config())" },
  { re: /\btungstenite::(connect|client_tls)\s*\(/, use: "client_tls_with_config(…, net_trust::rustls_client_config().map(tungstenite::Connector::Rustls))" },
  { re: /\bclient_tls_with_config\s*\((?![^\n]*net_trust::rustls_client_config)/, use: "net_trust::rustls_client_config() as its connector" },
];

// The line's code only: string and char literals emptied, then a `//` comment cut off.
function code(line) {
  return line
    .replace(/r(#*)"[\s\S]*?"\1/g, '""')
    .replace(/"(\\.|[^"\\])*"/g, '""')
    .replace(/'(\\.|[^'\\])'/g, "''")
    .replace(/\/\/.*$/, "");
}

const files = gitLsFiles(root, "apps/*.rs", "crates/*.rs", "features/*.rs");
let found = 0;

for (const f of files) {
  if (ALLOWED.some((p) => f.startsWith(p))) continue;
  const full = join(root, f);
  if (!existsSync(full)) continue;
  const lines = readFileSync(full, "utf8").split(/\r?\n/);
  let block = false;
  for (let i = 0; i < lines.length; i++) {
    let line = lines[i];
    if (block) {
      const end = line.indexOf("*/");
      if (end < 0) continue;
      line = line.slice(end + 2);
      block = false;
    }
    let c = code(line);
    const open = c.indexOf("/*");
    if (open >= 0) {
      const close = c.indexOf("*/", open + 2);
      if (close < 0) block = true;
      c = c.slice(0, open) + (close < 0 ? "" : c.slice(close + 2));
    }
    const rule = RULES.find((r) => r.re.test(c));
    if (!rule) continue;
    console.error(`  ERROR  ${f}:${i + 1}  use ${rule.use}, so the system store's roots are trusted too: ${line.trim().replace(/\s+/g, " ")}`);
    found++;
  }
}

console.log(`net trust: errors: ${found}`);
if (found > 0) process.exit(1);
