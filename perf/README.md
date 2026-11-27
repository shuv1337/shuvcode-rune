# perf: the launch bench

`launch.bench.ts` measures the app's launch with [strobe](https://github.com/anomalyco/strobe) from the process's spawn. The app profile it launches is `opencode-rune` in `strobe.json` at the repository's root.

## Run it

```sh
# Is build B's launch faster than build A's? One series: a case and a target.
strobe bench perf/launch.bench.ts --base exe:<A>/opencode-desktop.exe --head exe:<B>/opencode-desktop.exe --filter warm --target small

# Every case once on one build, no verdict (does it start, do the milestones arrive).
strobe bench perf/launch.bench.ts --head exe:<B>/opencode-desktop.exe --check

# Where the time goes: a recording with stacks, then its hotspots and threads.
strobe record --bench perf/launch.bench.ts#warm --head exe:<B>/opencode-desktop.exe --target small
strobe query <rec> hotspots --thread main
strobe query <rec> threads --timeline
strobe open <rec>
```

`--why always` on a `bench` adds an attribution block (stacks) to each series.

| Case | The launch |
|---|---|
| `first` | A fresh profile, never run before (target `none`). |
| `first-desktop` | A fresh profile on a machine where OpenCode Desktop (the official app) left its database (target `none`): the first launch takes Desktop's windows, tabs and drafts. `desktop-profile.ts` makes the database (`OPENCODE_LAUNCH_DESKTOP_MB`, 64 by default). |
| `first-desktop-running` | The same while Desktop runs: its database has a WAL with frames, so the database and the WAL are read into memory (the WAL's committed frames applied) instead of the few pages a closed Desktop's file gives. |
| `first-new-build` | A profile an earlier run made, the exe a new file, none of the exe's own caches: the launch after an update. `OPENCODE_LAUNCH_PROFILE_BY=<older exe>` makes the profile with another build. |
| `cold` | The exe's pages not in memory, GPU shader cache cold. |
| `warm` | A later launch, everything cached. |

| Target | The profile and its server |
|---|---|
| `none` | No profile. |
| `small` | 5 projects, 12 sessions, a 10-message session open. |
| `big` | 40 projects, 4,800 sessions, a 10,000-message session open. |

The primary is `launch.first_frame` (the app's `frame.first.presented`). The other milestones are the app's own launch marks, printed by `--launch-marks` with each mark's time to the microsecond: `launch.main`, `launch.window`, `launch.input_ready`, `launch.server`, `launch.content_live`. A launch ends at `content_live`. A build from before 24 Nov 2026 prints no `main.entered` or `window.created`: set `OPENCODE_LAUNCH_EARLY=0` for it.

## What it needs

- **An exe per variant.** The bench never builds. Use CI's artifact (`build-opencode-rune-pr-windows-x64` of a pull request's run, the signed zip of a release) or a profiling build.
- **A packaged Windows build is two files:** `opencode-desktop.exe` (thin) loads the app from `opencode-desktop.dll` beside it. Keep them together: copy the folder, not the exe. `exe:<folder>/opencode-desktop.exe` is still the target; the bench makes both files cold or warm. The PDB with the app's names is `opencode_desktop_app.pdb` (beside the library).
- **A build with symbols for stacks.** CI's artifacts carry no PDB or dSYM, so their stacks do not resolve. A profiling build is `cargo build --profile perf -p opencode-desktop` (the `perf` profile in `Cargo.toml`: optimised as `release-quick`, 16 codegen units and no cross-crate LTO, with line tables kept), which leaves `opencode-desktop.pdb` beside the exe on Windows. Read strobe's `! Coverage` line and its two resolved shares before trusting a hotspot table, and say which build a recording used.
- **A fixture server.** The app is started with `--server`; the bench does not start OpenCode's own. Either servers already running (`OPENCODE_LAUNCH_SERVER_SMALL`, `OPENCODE_LAUNCH_SERVER_BIG`: their URLs) or `OPENCODE_LAUNCH_FIXTURE=<script>`, started as `bun <script> --port <p> --projects <n> --sessions <n> --messages <n>` once per size (ports `OPENCODE_LAUNCH_PORT_SMALL` 5871 and `OPENCODE_LAUNCH_PORT_BIG` 5872). The fixture is not in this repository yet.
- **`cold` and `first-new-build` replace the variant's exe file** with a new file of the same bytes at the same path before each launch (an unbuffered copy): point them at a copy of a build, not at the only one.
- **A scanner's check of a new exe, paid before the timed launch.** A file named `staged` beside a variant's exe makes `cold` and `first-new-build` create a suspended process from the new file and end it before the timed launch, as the updater does to an exe it has staged and the setup to the one it installed. Two copies of one build, one with the file, compare the launch after an update with and without that step; in a folder a virus scanner reads it is most of the launch (dev-478 on Windows, where Defender scans: first frame 497 ms without, 45.7 ms with, `exp_b9fa23`).
- **`warm` and `first` start with the exe's pages in memory.** Before each of their launches the bench touches every page of the exe's image (Windows), as a launch a moment earlier leaves them. Without it a machine short of memory drops the pages between launches and a "warm" launch reads them again before `main` (`main` at 85-1,873 ms instead of 20 in 5 of 24 launches; with it 2 of 24). `OPENCODE_LAUNCH_WARM_EXE=0` turns it off.
- **A window nobody can see gives no `launch.painted`.** On a hidden desktop strobe reads no frame of the window (`NoPresentSource`); the first-frame clock is the app's own `frame.first.presented` mark either way.
- Profiles are made once per exe and size by two untimed launches of that exe and kept in `OPENCODE_LAUNCH_SCRATCH` (the system's temporary folder by default).

## content.bench.ts: from the window to content

A later launch that restores the session the window had open, against **OpenCode's own server** holding a real-size history (not the fixture): the app is given no `--server` and finds the server from `<state>/opencode/service.json`, as an installed copy does.

```sh
OPENCODE_BENCH_SERVER=http://127.0.0.1:<port> OPENCODE_BENCH_PASSWORD=<password> OPENCODE_BENCH_SESSION=<session id> \
  strobe bench perf/content.bench.ts --base exe:<A>/opencode-desktop.exe --head exe:<B>/opencode-desktop.exe --filter live
```

| Case | Ends at |
|---|---|
| `live` | `content.live`: the server answered and the open session's rows show. |
| `session` | `session.read.received`: the open session's newest page came from the server. |
| `settled` | The window's last change (its pixels: needs a desktop a person could see). |

Milestones: `launch.first_frame`, `launch.input_ready`, `launch.rows` (the first frame with the session's messages: the saved copy's), `launch.server`, `launch.content_live`, `launch.session_read`. No case ends at a row of the timeline: the saved copy puts the session's last turns in the first frame, so strobe finds them in its first read of the window and refuses to time them.

- The server is yours to start: any `opencode serve` whose port is not 4096 (the app's default address, which a launch falls back to when the registered server is away: the bench refuses to run while something listens there).
- `OPENCODE_BENCH_ENGINE=<folder>`: an installed browser engine (`<state>/co.anomaly.opencode/browser-engine/<version>` of a profile that has one), linked into each launch's home. Without it every launch starts the engine's download once its content is up.
- Layout shifts: start the app with `--launch-trace <file> --launch-trace-shifts` and read `shifts` in its line (frames in which an element on screen moved, how many moved, and the first of them by name). It walks every frame's elements, so it is off in a timed launch.
## The pinned strobe, beside another install

`perf/package.json` pins strobe (`@hona/strobe` 0.44.1). `bun install` in `perf/`, then `bunx strobe …` there runs that version whatever the machine has installed.

On a machine whose installed strobe is older and must stay as it is (its daemon and deep helper belong to other work), give the pinned one a state folder of its own: it keeps its recordings and experiments under `%LOCALAPPDATA%\strobe`, so start it with `LOCALAPPDATA` pointing at a scratch folder. It then leaves the installed state folder, daemon and helper alone (`strobe status` names them as another build; only `strobe up` would replace them, and that asks for admin rights).

```powershell
cd perf; bun install
$env:LOCALAPPDATA = 'C:\tmp\strobe-0441'    # this shell only
bunx strobe bench launch.bench.ts --base exe:<A>\opencode-desktop.exe --head exe:<B>\opencode-desktop.exe --filter warm --target small
```

What 0.44.1 does with this bench on Windows (24 Nov 2026, `exp_e20f7c`, `rec_7ecb5f`, the installed strobe 0.28.0):

- **A comparison runs about twice the pairs in the same time:** 31 pairs of the warm case in 124 s (a block is one launch, about 1.4 s), with looks at 8, 12, 18, 26 and 40 pairs. The verdict needs as many pairs as before: a 4 % change on a machine with other work was still `unclear` at 31.
- **Milestones are still timed from the spawn call.** There is no clock from `main` or from the process's creation, so a late `CreateProcess` is inside every `launch.*` value. Stages counted from `main` come from the app's own marks: `strobe run --base "bun <tail-loop.ts> <A> --n 1 --metric-lines" --head "… <B> …" --primary metric.frame_from_main_us` (the tool is not in this repository yet).
- **`strobe record --bench launch.bench.ts#warm` keeps no native samples** while the installed deep helper is another build's (`0 samples`: strobe's own sampler takes one sample per 2 ms of a thread's CPU and a launch's threads end first). Stacks of a launch still come from a process created suspended with `strobe record pid:<n> --probes deep` attached before it is resumed.
- Values print in µs (`--unit`), and `strobe run --plan` estimates the pairs and time of a command comparison without running it.

## Not covered yet

The browser pane restored, an update pending. A window restored with its side panel open, two saved windows and a launch from a folder a virus scanner reads are measured by hand (the app's own launch trace under `strobe run`), not by a case here.

## macOS, started as the Dock starts it

strobe spawns its target as a child process. The Dock and Finder start an app through Launch Services, which costs about 20,000 to 55,000 µs more on an M4 MacBook Air (launchd starts the process, and AppKit waits for the open-application Apple event). `perf/macos-open.ts` is one such launch (`open -n -W -a <.app>`) for `strobe run`: it prints the app's own launch marks as `strobe:metric` lines, in µs from the kernel's start time of the process.

```sh
strobe run --base "bun perf/macos-open.ts /path/Old.app" --head "bun perf/macos-open.ts /path/New.app" \
  --primary metric.first_frame --metrics metric.input_ready,metric.content_live,metric.main,metric.resumed,metric.window
```

- Profiles `small` (the default), `first` and `cold` (a copy of the bundle at a new path); a fourth argument `child` starts the bundle's exe directly.
- The fixture server must already answer at `OPENCODE_LAUNCH_SERVER_SMALL`: a run that started one and left it behind is refused by strobe.
- The bundle must be signed (ad hoc is enough). A file added inside a signed bundle makes every later start from that path hang before `main`.
- For stacks, record the child launch (`strobe record --bench perf/launch.bench.ts#warm --head exe:<.app>/Contents/MacOS/opencode-desktop`) with the dSYM beside the bundle. macOS samples are untimed snapshots: rows are counts, not ms, and the sampler inflates `dlopen`.
## Linux, on a desktop session

`perf/linux-launch.ts` is the launch on a Linux desktop in one command, for a machine that has no strobe set up. It starts the app on the session it is run from (the real compositor and GPU), in a home of its own under the temporary folder, and prints each stage in µs from the spawn call: the app's own marks (`main.entered`, `window.create`, `window.native`, `gpu.adapter`, `gpu.device`, `renderer.ready`, `window.created`, `frame.first.presented`, `input.ready`).

```sh
bun perf/linux-launch.ts /usr/bin/opencode-rune        # or the path of an unpacked build; a second argument is the launches per case (5)
```

| Case | The launch |
|---|---|
| `first` | A fresh home: the first launch ever (that home's GPU shader cache and font cache cold). |
| `cold` | A later launch with the exe's pages dropped from the page cache first (`posix_fadvise`, no root): the first launch after a boot or an update. |
| `warm` | A later launch, everything in memory. |

- With no OpenCode server at `OPENCODE_LAUNCH_SERVER_SMALL` the app has nothing to list: `content.live` is left out and each launch ends by the app's own fallback, about 12 s after it starts (the whole run takes about 4 minutes).
- A published build's first launch of the home downloads the browser engine in the background (350 MB) and keeps it for the later ones; a build made without the release's engine pin fetches none. `OPENCODE_LAUNCH_SCRATCH` moves the home.
- A launch that goes wrong is listed under the table: how long the process ran, how it ended, and the app's last word on stderr. The app writes its trace as the last step of its quit, so a launch whose window came up and whose quit then stalled is counted by the marks it printed and listed as "the quit wrote no trace"; one that printed no mark drew nothing and is not counted.
- The app's clock on Linux starts at `/proc/self/stat`'s start time, which has 10 ms steps; the script counts from its own spawn call and `main.entered`'s wall-clock time instead.
- `launch.bench.ts` on Linux: `cold` and `first-new-build` drop the new file's pages (`linux-pages.ts`), `warm` and `first` read the exe into the page cache. Not run under strobe on Linux yet (24 Nov 2026): no Linux machine with a display; the published build starts under headless weston, headless sway and Xvfb, where the GPU is llvmpipe and nothing after `gpu.begin` says anything about a real GPU.