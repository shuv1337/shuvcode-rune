# Profile-guided optimisation

`<target>.profdata` is what LLVM optimises the release build with: how often each function and
branch of the app ran in a training run of the app as it ships. `<target>.txt` says what it was
trained on (commit, Rune rev, rustc, processes, functions that ran).

- **What uses it:** `apps/desktop/scripts/release.ps1` (and through it `apps/installer/scripts/package.ps1`,
  so the release workflow) adds `-Cprofile-use` for the target's file when there is one. A plain
  `cargo build --release` doesn't: time the app with a build from `release.ps1`.
- **What it's worth (Windows x64, fat LTO, measured per frame on the same scripted typing, hover,
  scroll and palette):** 14–22 % less CPU per frame than the same build without it. The exe grows
  (hot paths inlined), and functions that never ran in training are optimised for size.
- **What the training covers:** an agent turn streaming (markdown, code, tools, a subagent, a
  question), typing in short and long drafts, hovering and switching and dragging tabs, the command
  palette, scrolling and panning, side panels, menus, popovers, the sidebar, resizing, Home,
  Settings, a German run, and a launch and quit in every part; and launches by themselves, each to
  its content against a server: the first launch of a fresh profile, later launches of a small and of
  a big profile (4,800 sessions, a 10,000-message session open), the launch after an update, and a
  launch from a new copy of the exe. Office previews and the terminal
  aren't in it yet, so they build as cold code.
- **It goes stale fast:** a profile 58 commits old (a day, with Rune bumps) gave 4-9 % less CPU per
  frame than a build without one; retrained on the same code, 23-30 %. From 20 app commits on (or
  when the training commit isn't in the clone), `release.ps1` refuses to build with it unless told
  `-StaleProfileOk` (or `OPENCODE_RUNE_STALE_PROFILE_OK=1`, which reaches it through
  `package.ps1`); in the release workflow (`OPENCODE_RUNE_BUILD_CHANNEL` set) it only warns.
- **The instrumented build needs no big machine of your own:** it is whole-program (about 16 GB of
  memory for the one LTO module), and PR CI builds it. Open a draft pull request from a branch named
  `pgo-train/<anything>`: `release.ps1` then builds the Windows x64 exe with `-Cprofile-generate`
  instead of with the profile, and the run's `build-opencode-rune-pr-windows-x64` artifact is the
  training exe (same runner, toolchain and release profile as the build that ships, so its symbol
  names are the shipped ones). Run the training against it, merge the `.profraw` files with the
  toolchain's `llvm-profdata merge --sparse`, commit the two files from a branch of `dev`, and close
  the draft. Never install that build.
- **A profile names functions by symbol, and cargo hashes the release profile (LTO, codegen units,
  debug info), the Rust version and each dependency's source revision into every symbol:** a build
  with another `lto` or `debug` setting, or with another Rune pin, gets no counts from it. Extra
  flags must go through `RUSTFLAGS`, which is not in that hash (`release.ps1` does: the profile
  itself, and the line tables of a `symbols` build).
- **After a Rune pin bump, move the counts instead of training again:**
  `node scripts/pgo-rekey.mjs --counts <this profile> --names <an instrumented exe of the new code>`
  `--out <this profile> --stamp <target>.txt --commit <sha> --rune <rev>`. It rewrites the crate hash
  inside each symbol name, reads the new build's functions from the exe file (no run), and keeps a
  function's counts when the new build has it with the same control-flow hash. On the 23 Nov bump
  (81 crate hashes) 16,287 of 16,292 functions kept theirs. Train again when its report's
  `code_changed` grows.
- **Refresh it** before a release, when the app or Rune changed since `<target>.txt`'s commit (`release.ps1`
  prints how many app commits there have been), or after a Rust toolchain bump (the file format
  follows LLVM's): `pwsh apps/desktop/scripts/pgo.ps1 -Train "<training command>"`, then commit both
  files. A stale profile is never wrong: a function whose code changed since loses its counts and
  builds as it would without a profile.
- **Apple silicon** has a profile of its own, `aarch64-apple-darwin.profdata`: see below. Intel macOS,
  Windows ARM64 and Linux do not use a profile.

## The Apple silicon profile

`build-app.sh` builds the `release` profile (fat LTO: every pull request's build and the stable
channel) with `aarch64-apple-darwin.profdata`. The dev channel builds `release-thin`, whose symbols
no profile of a fat build names, so it uses none. `OPENCODE_RUNE_PGO_USE=0` (or a pull request from a
branch with "no-profile" in its name) builds the release without it, to time what it is worth.

- **What it is worth (an M4 Air, CI builds of one commit, 24 Nov):** against the profile of 14 Nov, dead by then, the frames of a streaming turn cost 5 % less
  CPU (270 -> 255 ms, strobe `exp_52cd93`) and a scrolled session's 3 % less (`exp_599ca2`); the
  everyday launch is the same (125.8 -> 124.7 ms, `exp_ee155c`), with 5 % less CPU in the app's
  first 1.2 s; the exe is 105.4 -> 98.0 MiB. Against the dev channel's kind of build (thin LTO,
  no profile) the everyday launch is 134.2 -> 129.5 ms (`exp_3a1676`), the frames cost 7 % less
  (`exp_1f661c`) and the exe is 119.8 -> 98.0 MiB.
- **It dies in two ways, and both happened:** a Rune pin bump or a dependency bump renames almost
  every function (the profile of 14 Nov named 741 of today's functions ten days later), and the code
  moves (re-keyed to today's names, that profile would still have kept only 3,071 of its 8,969
  functions; one pin bump alone left 2,681 of 16,407). A dead profile is not harmless: it grows the
  exe (105.4 MiB against 97.2 without one) and speeds nothing (`exp_f68148`, `exp_5bd528`).
- **After a pin bump, re-key it with the Windows one.** Name the names-draft
  `pgo-train/order-train-<sha>`: that one pull request builds both training exes (`release.ps1` looks
  for `pgo-train/` at the start, `build-app.sh` for "order-train" anywhere). Take
  `Contents/MacOS/opencode-desktop` out of the `build-opencode-rune-pr-macos-arm64` artifact's zip and
  run `node scripts/pgo-rekey.mjs --counts apps/desktop/pgo/aarch64-apple-darwin.profdata --names <that exe>`
  `--out apps/desktop/pgo/aarch64-apple-darwin.profdata --stamp apps/desktop/pgo/aarch64-apple-darwin.txt`
  `--commit <sha> --rune <rev> --min-kept 90`. The tool reads the names and control-flow hashes from
  the Mach-O file (`__llvm_prf_names`, `__llvm_prf_data`): no Mac and no run are needed.
- **Train it again** when the re-key's report says `code_changed` or `no_such_function` grew (or
  weekly): on a Mac, with the training app from the same kind of draft unzipped,
  `apps/desktop/scripts/pgo-train-macos.sh <the .app> [<server URL>]` from a checkout of the draft's
  commit. About four minutes, no build: five launches with a window (the first launch of a new
  profile, two later ones, two that replay `pgo-train-window.json`: scrolling, typing, undo, the model
  picker) and fourteen headless scenes in English and German (Home, a session typed in and scrolled,
  the demo's streaming turn, the queue, Settings, tab badges). It writes the profile, its stamp and
  `startup-order.txt`; commit the three from a branch of `dev` and close the draft. Office previews,
  the terminal and the browser pane are not in the training, so they build as cold code.
## The link order (`startup-order.txt`)

`startup-order.txt` lists the functions a launch runs, in the order it first runs them. The macOS
link puts them first in the exe and together (`apps/desktop/scripts/link-order.sh`, which
`build-app.sh` hands to Cargo as the linker), so a launch whose exe is not in memory reads those
pages and few others. Without it the same functions lie spread over the whole of the code: on
`dev-478` 9.5 MiB of launch code touched 2,569 pages of 16 KB (40 MiB).

- **Linux x86_64 reads the same list.** `.cargo/config.toml` names `apps/desktop/scripts/link-order-linux.sh`
  as the linker of the x86_64 Linux target, so a plain `cargo build` orders the exe (that link goes
  through lld, which takes `--symbol-ordering-file`; a link that fails with the order is made
  without it and says so). The aarch64 Linux link goes through the system linker, which takes no
  such order: that target is linked as cargo links it, without the script. The list is trained
  on macOS: the Linux build has about two thirds of its functions (8,011 of 11,765 on `dev-483`),
  which without the order lie on 4,600 pages spread over all 87 MiB of the code. A re-key of the
  list at a pin bump covers the three platforms.- **Names carry no crate hashes.** A hash changes with every Rune pin and every dependency bump;
  the list stores `Cs<hash>_` as `C` and back references as `B_`, and `link-order.sh` takes the same
  out of the names in the objects it links before it looks them up. So a pin bump does not empty
  the list. The link says how much of it this build has (`link order: N of M listed functions are
  in this build`, in the build log and in `<exe>.link-order.txt`).
- **It only names instrumented code:** Rust crates built from source. std, C and C++ (Luau, SQLite,
  zstd, the terminal) are not in it.
- **Make it again** when that share drops, or with a profile refresh (`pgo-train-macos.sh` above does all of this and the profile in one run):
  1. Open a draft pull request from a branch with `order-train` in its name (or build with
     `OPENCODE_RUNE_PGO_TRAIN=1`). `build-app.sh` then builds Apple silicon with
     `-Cprofile-generate -Cllvm-args=-pgo-temporal-instrumentation`; the run's
     `build-opencode-rune-pr-macos-arm64` artifact is the training exe. Never install that build.
  2. Launch it a few times to its content, each with `LLVM_PROFILE_FILE=<dir>/launch-%p.profraw`,
     and quit it (the profile is written at exit).
  3. `apps/desktop/scripts/startup-order.sh <dir>/launch-*.profraw`, with the `llvm-profdata` of the
     toolchain that built the exe (`rustup component add llvm-tools`; Xcode's cannot read the file).
     Commit `startup-order.txt` from a branch of `dev` and close the draft.
- `OPENCODE_RUNE_LINK_ORDER=0` links without it. A failure to make the order never fails a link.
- The same raw profiles, merged with `--sparse`, are a fresh `aarch64-apple-darwin.profdata`.
