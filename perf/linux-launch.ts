// The launch of OpenCode (Rune) on a Linux desktop, in one command, for pasting back:
//
//   bun perf/linux-launch.ts [path to opencode-rune] [launches per case, 5]
//
// It starts the app on the session it is run from (Wayland or X11, the real compositor and GPU), in a
// home of its own under the temporary folder (nothing of the person's profile is read or written),
// and prints each stage in microseconds from the spawn call, the median of the launches:
//   first   a fresh home: the first launch ever (GPU shader cache and font cache of that home cold)
//   cold    a later launch with the exe's pages dropped from memory first: the first launch after a
//           boot or an update reads the exe from the disk
//   warm    a later launch, everything in memory
// The stages are the app's own launch marks (`--launch-trace`). With no OpenCode server at
// OPENCODE_LAUNCH_SERVER_SMALL the app has nothing to list, so `content.live` is left out and each
// launch ends by the app's own fallback about 12 s after it starts (the whole run: about 4 minutes).
// A published build's first launch of a home downloads the browser engine in the background (350 MB)
// and keeps it for the later ones (a build made without the release's engine pin fetches none);
// OPENCODE_LAUNCH_SCRATCH moves the home.
//
// A launch that goes wrong is listed under the table with what happened to it. The app writes its
// trace as the last step of its quit, so "no trace" is not "no window": a launch whose window came up
// (its marks are on stdout) and whose quit then stalled or was cut is counted by those marks and
// listed as such; one that printed no mark drew nothing and is not counted. Each line has how long
// the process ran, how it ended and the app's last word on stderr (Rune says where a quit is stuck).
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs"
import { cpus, release, tmpdir } from "node:os"
import { join } from "node:path"

import { evict, warm } from "./linux-pages"

if (process.platform !== "linux") throw new Error("perf/linux-launch.ts runs on Linux")
const exe = process.argv[2] ?? Bun.which("opencode-rune") ?? ""
if (!exe || !existsSync(exe)) throw new Error("usage: bun perf/linux-launch.ts <path to opencode-rune> [launches per case]")
const N = Number(process.argv[3] ?? 5)
const SERVER = process.env.OPENCODE_LAUNCH_SERVER_SMALL ?? "http://127.0.0.1:5871"
const SCRATCH = process.env.OPENCODE_LAUNCH_SCRATCH ?? join(tmpdir(), "opencode-rune-launch-bench", "linux")
const home = join(SCRATCH, "home")
const engine = join(home, "state", "co.anomaly.opencode", "browser-engine")
// One copy a scratch folder: a second one deletes the first one's profile under its running app, and
// its launches find that app alive, hand over to it and end at once.
const lock = join(SCRATCH, "running.pid")
const other = existsSync(lock) ? Number(readFileSync(lock, "utf8")) : 0
if (other && existsSync(`/proc/${other}`)) throw new Error(`another copy of this bench (pid ${other}) runs on ${SCRATCH}: give each its own OPENCODE_LAUNCH_SCRATCH`)
mkdirSync(SCRATCH, { recursive: true })
writeFileSync(lock, String(process.pid))
const served = await fetch(`${SERVER}/api/info`, { signal: AbortSignal.timeout(1000) }).then((r) => r.ok).catch(() => false)

/** The launch's own home (every folder the app reads or writes) on the session this was started from. */
function seal() {
  for (const d of ["home/Documents", "tmp", "xdg/config", "xdg/state", "xdg/data", "xdg/cache"]) mkdirSync(join(home, d), { recursive: true })
  const session = ["WAYLAND_DISPLAY", "DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION", "LANG"]
  return {
    ...Object.fromEntries(session.flatMap((k) => (process.env[k] ? [[k, process.env[k]]] : []))),
    PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
    RUNE_DIRS_ROOT: home, OPENCODE_RUNE_HOME: home, HOME: join(home, "home"), TMPDIR: join(home, "tmp"),
    XDG_CONFIG_HOME: join(home, "xdg", "config"), XDG_STATE_HOME: join(home, "xdg", "state"), XDG_DATA_HOME: join(home, "xdg", "data"), XDG_CACHE_HOME: join(home, "xdg", "cache"),
    OPENCODE_SERVER_PASSWORD: "parity-harness", OPENCODE_RUNE_OTEL: "0",
  } as Record<string, string>
}

type Launch = { marks: Record<string, number>; notes: Record<string, string>; renderer?: string }

/** One launch: its marks in microseconds from the spawn call, and what went wrong with it, if anything. */
async function launch(): Promise<{ launch?: Launch; lost?: string }> {
  const trace = join(home, "launch-trace.jsonl")
  rmSync(trace, { force: true })
  const spawned = (performance.timeOrigin + performance.now()) * 1000
  const p = Bun.spawn([exe, "--launch-marks", "--launch-trace", trace, "--launch-trace-quit", "300", "--server", served ? SERVER : "http://127.0.0.1:9"], { env: seal(), stdout: "pipe", stderr: "pipe", stdin: "ignore" })
  const cut = { done: false }
  const timer = setTimeout(() => {
    cut.done = true
    p.kill()
  }, 45_000)
  const [out, err] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text()])
  const code = await p.exited
  clearTimeout(timer)
  const ran = Math.round(performance.timeOrigin + performance.now() - spawned / 1000)
  const said = err.trim().split("\n").filter((l) => /^rune: |panicked/.test(l)).at(-1) ?? err.trim().split("\n").at(-1) ?? ""
  const ended = `${cut.done ? "ended by this script after" : `exit ${code} after`} ${ran} ms${said ? `; the app said: ${said.slice(0, 200)}` : ""}`
  // Each mark's own wall-clock time, to the microsecond. The app prints them once its first frame is up.
  const echoed = Object.fromEntries([...out.matchAll(/^launch-mark (\S+) \d+ at=(\d+\.\d+)$/gm)].map((m) => [m[1]!, Math.round(Number(m[2]) * 1000 - spawned)]))
  if (echoed["main.entered"] === undefined) return { lost: `no first frame: the app printed no mark (${ended})` }
  const line = existsSync(trace) ? JSON.parse(readFileSync(trace, "utf8").trim().split("\n").at(-1)!) : undefined
  if (!line) return { launch: { marks: echoed, notes: {} }, lost: `the window came up, the quit wrote no trace: counted by the marks on stdout (${ended})` }
  // The trace's marks count from `main.entered`.
  const from = echoed["main.entered"] - line.marks_ns["main.entered"] / 1000
  return { launch: { marks: Object.fromEntries(Object.entries(line.marks_ns as Record<string, number>).map(([k, ns]) => [k, Math.round(from + ns / 1000)])), notes: line.notes ?? {}, renderer: line.renderer } }
}

const STAGES = ["main.entered", "loop.built", "window.create", "window.native", "gpu.adapter", "gpu.device", "renderer.ready", "window.created", "frame.first.build", "frame.first.encoded", "frame.first.presented", "input.ready", ...(served ? ["server.answered", "content.live"] : [])]
const median = (v: number[]) => [...v].sort((a, b) => a - b)[Math.floor(v.length / 2)]!

async function run(name: string, before: () => void) {
  const launches: Launch[] = []
  const lost: string[] = []
  for (let i = 0; i < N; i++) {
    before()
    const one = await launch()
    if (one.launch) launches.push(one.launch)
    if (one.lost) lost.push(`${name} ${i + 1}/${N}: ${one.lost}`)
    process.stderr.write(`${name} ${i + 1}/${N}${one.launch ? ` first frame ${one.launch.marks["frame.first.presented"]} us` : ""}${one.lost ? ` ${one.lost}` : ""}\n`)
  }
  const stages = Object.fromEntries(STAGES.flatMap((s) => {
    const v = launches.flatMap((l) => (l.marks[s] === undefined ? [] : [l.marks[s]!]))
    return v.length ? [[s, { median: median(v), min: Math.min(...v), max: Math.max(...v) }]] : []
  }))
  return { case: name, launches: launches.length, lost, stages, notes: launches.at(-1)?.notes ?? {}, renderer: launches.at(-1)?.renderer }
}

/** A fresh home that keeps an installed browser engine, so each first launch does not download it again. */
function fresh() {
  const kept = join(SCRATCH, "engine-kept")
  if (existsSync(engine)) renameSync(engine, kept)
  rmSync(home, { recursive: true, force: true })
  if (existsSync(kept)) {
    mkdirSync(join(engine, ".."), { recursive: true })
    renameSync(kept, engine)
  }
}

const results = [await run("first", fresh)]
// The profile a later launch starts from: two untimed launches.
await launch()
await launch()
results.push(await run("cold", () => evict(exe)))
results.push(await run("warm", () => warm(exe)))

const read = (path: string) => (existsSync(path) ? readFileSync(path, "utf8") : "")
const machine = {
  app: Bun.spawnSync([exe, "--version"]).stdout.toString().trim(),
  exe_bytes: statSync(exe).size,
  kernel: release(),
  distro: /PRETTY_NAME="?([^"\n]*)/.exec(read("/etc/os-release"))?.[1],
  cpu: `${cpus()[0]?.model} x${cpus().length}`,
  session: process.env.XDG_SESSION_TYPE,
  desktop: process.env.XDG_CURRENT_DESKTOP,
  wayland: Boolean(process.env.WAYLAND_DISPLAY),
  server: served,
}
console.log(`\n${machine.app} on ${machine.distro}, ${machine.kernel}, ${machine.cpu}; ${machine.session ?? "?"} session, ${machine.desktop ?? "?"}; GPU ${results.at(-1)?.notes.adapter ?? "?"}`)
console.log(`microseconds from the spawn call, median (min-max) of the launches counted: ${results.map((r) => `${r.case} ${r.launches} of ${N}`).join(", ")}\n`)
console.log(["stage".padEnd(24), ...results.map((r) => r.case.padStart(30))].join(""))
for (const s of STAGES) console.log([s.padEnd(24), ...results.map((r) => (r.stages[s] ? `${r.stages[s].median} (${r.stages[s].min}-${r.stages[s].max})` : "-").padStart(30))].join(""))
const lost = results.flatMap((r) => r.lost)
if (lost.length) console.log(`\n${lost.length} launch(es) went wrong:\n${lost.join("\n")}`)
console.log(`\nlinux-launch ${JSON.stringify({ machine, results })}`)
rmSync(lock, { force: true })
