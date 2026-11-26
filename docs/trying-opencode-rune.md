# Trying OpenCode (Rune)

**OpenCode (Rune)** is the native desktop client for macOS, Windows, and Linux (`apps/desktop`) built on Rune. You can keep the `dev` build installed for daily work and install pull-request builds side by side whenever you want to test a change.

## 1. Install the `dev` build

1. Download the `dev` package for your platform (full target table in [docs/releasing.md](releasing.md)):
   - **macOS (Apple Silicon):** [`OpenCode (Rune).dmg`](https://anoma.ly/release/opencode/desktop-rune/dev/macos-arm64-dmg) (drag **OpenCode (Rune).app** into `/Applications`).
   - **macOS (Intel):** [`OpenCode (Rune).dmg`](https://anoma.ly/release/opencode/desktop-rune/dev/macos-x64-dmg).
   - **Windows (x64 / arm64):** [x64 setup](https://anoma.ly/release/opencode/desktop-rune/dev/windows-x64-setup) · [arm64 setup](https://anoma.ly/release/opencode/desktop-rune/dev/windows-arm64-setup).
   - **Linux (x64 / arm64):** [x64 AppImage](https://anoma.ly/release/opencode/desktop-rune/dev/linux-x64-appimage) · [x64 DEB](https://anoma.ly/release/opencode/desktop-rune/dev/linux-x64-deb) · [x64 RPM](https://anoma.ly/release/opencode/desktop-rune/dev/linux-x64-rpm) · [arm64 AppImage](https://anoma.ly/release/opencode/desktop-rune/dev/linux-arm64-appimage) · [arm64 DEB](https://anoma.ly/release/opencode/desktop-rune/dev/linux-arm64-deb) · [arm64 RPM](https://anoma.ly/release/opencode/desktop-rune/dev/linux-arm64-rpm) (set `chmod +x` on AppImage files before launching).
2. Make sure an OpenCode server is running, locally or on another host (e.g. `opencode serve --service`, or your own shuvcode server).
3. Open **OpenCode (Rune)**. It connects to `http://127.0.0.1:4096`, `~/.config/opencode/service.json`, `--server <url>` / `OPENCODE_SERVER_URL`, or a server you added, and it never installs or starts a server on its own. If nothing is running yet, start the server and click **Retry**.

On macOS and Windows, the `dev` app checks `https://anoma.ly/release` 10 seconds after launch and once an hour. When a new `dev` build is published (checked every six hours when app files changed, publishing all 14 targets), the app downloads and verifies the update in the background and switches to it on your next restart; Linux uses manual downloads. Opening the browser pane (`⌃⌘B` on macOS, `Ctrl+Alt+B` on Windows/Linux) downloads the Browser Engine sidecar from `https://anoma.ly/release` without GitHub sign-in (where published). The app also fetches it on its own once it has been idle for 30 seconds (throttled, and resumed if cut short), so the pane usually opens without a wait. Turn that off in **Settings › Download browser in the background**.

## 2. Try a pull request side by side

Every open pull request in `anomalyco/opencode-rune` that touches the desktop app posts a comment with links to a macOS Apple Silicon `.dmg` (`OpenCode-Rune-PR-<N>-macos-arm64.dmg`) and unsigned Windows setups (`OpenCode-Rune-PR-<N>-windows-x64-setup.exe`, `OpenCode-Rune-PR-<N>-windows-arm64-setup.exe`).

### macOS

1. Download the `.dmg` from the PR comment, drag **`OpenCode (Rune) · <branch>.app`** to `/Applications`, and run `xattr -cr "/Applications/OpenCode (Rune) · <branch>.app"` once before first open (PR builds are ad-hoc signed).
2. Look for the `#<N>` badge on its Dock icon and the branch name in its title. Each PR build has its own bundle ID (`anomaly.opencode-rune.pr.<N>`) and does not auto-update, so your `/Applications/OpenCode (Rune).app` from `dev` stays untouched.
3. Switch between **`OpenCode (Rune)`** and **`OpenCode (Rune) · <branch>`** with `⌘Tab` or the Dock. Both apps talk to the same local OpenCode server, so you can compare the same session in both builds.
4. Drag **`OpenCode (Rune) · <branch>.app`** to the Trash when you finish testing.

### Windows

1. Download the x64 or arm64 setup from the PR comment and run it (SmartScreen prompts once for unsigned PR builds: choose **More info**, then **Run anyway**).
2. It installs side by side as **`OpenCode (Rune) · <branch>`** with its own app identity and does not replace your `dev` install. Uninstall it from **Installed apps** when finished.

GitHub Actions deletes the hosted PR build automatically as soon as the PR merges or closes.

## 3. Send feedback with Scrawl (macOS)

Scrawl records your screen, voice, cursor clicks, and drawn marks into a bundle an agent or teammate can act on right away.

1. Install Scrawl once from [scrawl.kitlangton.com](https://scrawl.kitlangton.com) or with Homebrew:
   ```sh
   brew install --cask kitlangton/tap/scrawl
   ```
2. Focus the **OpenCode (Rune)** window you are testing and press `⌥⇧S` to start recording (or hold `⌥⇧S` to pick a single window).
3. Show the issue, draw on the part that looks wrong, explain what you expected out loud, and press `⌥⇧S` again to finish.
4. From Scrawl's review window, copy the capture or export an `.mp4`, then paste it on the PR thread or attach it to a GitHub issue with the PR number (shown on the Dock icon badge).
