// Single entry point for all cheap repository guardrails.
// Runs every check and exits non-zero if any check fails.
// Slow compile/lint checks stay separate:
//   - scripts/clippy.sh
//   - cargo check -p session-view --examples
//   - pwsh scripts/web.ps1 -Check
// Runs on Bun or Node >= 18. Usage: scripts/guardrails.sh (or pwsh scripts/guardrails.ps1)
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const root = join(here, "../..");

const checks = [
  { name: "arch-check", script: join(here, "arch-check.mjs") },
  { name: "file-size", script: join(here, "file-size.mjs") },
  { name: "folder-shape", script: join(here, "folder-shape.mjs") },
  { name: "dialog-size", script: join(here, "dialog-size.mjs") },
  { name: "tables-check", script: join(here, "tables-check.mjs") },
  { name: "plugin-props", script: join(here, "plugin-props.mjs") },
  { name: "external-open", script: join(here, "external-open.mjs") },
  { name: "net-trust", script: join(here, "net-trust.mjs") },
  { name: "engine-revision", script: join(here, "engine-revision.mjs") },
  { name: "cursor-allow", script: join(here, "cursor-allow.mjs") },
  { name: "fork-boundary", script: join(here, "fork-boundary.mjs") },
];

const started = Date.now();
const failed = [];

for (const check of checks) {
  console.log(`==> ${check.name}`);
  const t0 = Date.now();
  const res = spawnSync(process.execPath, [check.script], {
    cwd: root,
    stdio: "inherit",
  });
  const ms = Date.now() - t0;
  if (res.error) {
    console.error(`  ERROR  ${check.name}: failed to spawn (${res.error.message})`);
    failed.push(check.name);
  } else if (res.status !== 0) {
    console.error(`  FAIL   ${check.name} (exit ${res.status}, ${ms} ms)`);
    failed.push(check.name);
  }
}

const totalSec = ((Date.now() - started) / 1000).toFixed(1);
if (failed.length > 0) {
  console.error(`guardrails: ${checks.length - failed.length}/${checks.length} passed, failed: ${failed.join(", ")} (${totalSec}s)`);
  process.exit(1);
}

console.log(`guardrails: ok (${checks.length}/${checks.length} passed in ${totalSec}s)`);
