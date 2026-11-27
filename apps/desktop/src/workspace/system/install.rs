//! `opencode-desktop --uninstall`: the installed app removes itself, as Apps & features asks
//! (`win_install` describes the install, `apps/installer` lays it out). The app is its own
//! uninstaller, as it is its own update helper, so an update never leaves a stale one behind.
//!
//! ```text
//! opencode-desktop.exe --uninstall [--quiet] [--delete-data] [--plan] [--log <file>]
//! %TEMP%\opencode-rune-uninstall-<pid>.exe --uninstall-cleanup --wait <pid> --root <install>
//!     [--caches] [--delete-data] [--log <file>]
//! ```
//!
//! Without `--quiet` it asks first, with "Also delete my settings, tabs, drafts and caches" unticked:
//! like OpenCode Desktop's uninstaller it keeps them unless asked. It closes the copies running
//! from the install, removes the shortcuts, the uninstall entry, the URL schemes and the
//! notification identity, then copies itself to the temp folder and starts the copy, which
//! waits for this process to exit and deletes the install folder, the updater's folders beside
//! it, and with `--delete-data` the app's caches, logs, settings and browser profile,
//! then itself. Only the folder the uninstall entry names is ever deleted, never the browser
//! engine every app with a browser pane shares, and while another build that keeps its settings
//! in the same folders is installed (another release channel; a pull request's build keeps its
//! own, `rune-opencode.pr-<N>`) they stay. `--plan` prints what would go and does nothing.
//!
//! What it deletes is checked first: the install folder must hold the app and be none of the
//! user's own folders; the updater's folders beside it are allow-listed by name; a data folder
//! must lie in the folder Windows names for it, with no junction or link on the way; and the data
//! stays while any copy of the app holds it (an unpacked copy runs outside the install folder).
// The confirmation is a Win32 task dialog; each block says what it relies on.
#![allow(unsafe_code)]

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
/// How long a running copy has to close on its own before it's ended.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// Uninstalls and exits, when this run is the uninstaller.
pub fn run_if_asked() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--uninstall") => uninstall(&Args::parse(&args[1..])),
        Some("--uninstall-cleanup") => cleanup(&Args::parse(&args[1..])),
        _ => return,
    };
    std::process::exit(code);
}

#[derive(Default)]
struct Args {
    quiet: bool,
    plan: bool,
    caches: bool,
    delete_data: bool,
    wait: u32,
    root: PathBuf,
    log: Option<PathBuf>,
}

impl Args {
    fn parse(args: &[String]) -> Args {
        let mut a = Args::default();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--quiet" => a.quiet = true,
                "--plan" => a.plan = true,
                "--caches" => a.caches = true,
                "--delete-data" => a.delete_data = true,
                "--wait" => a.wait = it.next().and_then(|p| p.parse().ok()).unwrap_or(0),
                "--root" => a.root = it.next().map(PathBuf::from).unwrap_or_default(),
                "--log" => a.log = it.next().map(PathBuf::from),
                _ => {}
            }
        }
        a
    }

    fn log(&self, line: &str) {
        eprintln!("{line}");
        let Some(path) = &self.log else { return };
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            writeln!(f, "[uninstall pid {}] {line}", std::process::id()).ok();
        }
    }
}

fn uninstall(a: &Args) -> i32 {
    let p = crate::launch::product();
    let Some(root) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) else { return 1 };
    let registered = win_install::installed_root(&p);
    if !registered.as_deref().is_some_and(|r| same(r, &root)) {
        a.log(&format!("{} isn't where {} is installed ({registered:?}); nothing removed", root.display(), p.name()));
        if !a.quiet {
            dialog::error(&p, &format!("This copy of {} wasn't installed by its setup, so there's nothing to uninstall here.", p.name()));
        }
        return 1;
    }
    // The uninstall entry is the user's to write: what it names must still be an install folder.
    if let Err(why) = check_root(&root) {
        a.log(&format!("{} can't be uninstalled from ({why}); nothing removed", root.display()));
        if !a.quiet {
            dialog::error(&p, &format!("{} can't be uninstalled from {}: {why}.", p.name(), root.display()));
        }
        return 1;
    }
    // Builds that keep their settings in this build's folders (another release channel; a pull
    // request's build keeps its own) keep them while another is installed.
    let ours = crate::dirs::app().map(|d| d.id().to_owned()).unwrap_or_default();
    let others: Vec<String> = win_install::other_installs(&p).into_iter().filter(|id| folder_of(id) == ours).collect();
    let delete_data = if a.quiet || a.plan {
        a.delete_data && others.is_empty()
    } else {
        match dialog::confirm(&p, others.is_empty()) {
            Some(delete_data) => delete_data,
            None => return 2,
        }
    };
    if !others.is_empty() {
        a.log(&format!("keeps the settings and caches, which {} also uses", others.join(", ")));
    }
    // `--plan`: what the cleanup would delete, one path a line, and nothing done.
    if a.plan {
        let plan = plan(&root, delete_data, delete_data);
        for path in plan.delete.iter().chain(&plan.files) {
            println!("delete {}", path.display());
        }
        for why in &plan.kept {
            println!("keep {why}");
        }
        return 0;
    }
    let exe = root.join(win_install::EXE);
    if delete_data && let Err(why) = data_free(&exe) {
        a.log(&format!("{why}; nothing removed"));
        if !a.quiet {
            dialog::error(&p, &format!("{why}. Close it and try again, or uninstall without deleting your settings."));
        }
        return 1;
    }
    a.log(&format!("uninstalling {} {} from {}{}", p.name(), p.version(), root.display(), if delete_data { ", with its data" } else { "" }));
    if !win_install::close_running(&root, CLOSE_GRACE) {
        a.log("a running copy didn't close; nothing removed");
        if !a.quiet {
            dialog::error(&p, &format!("Close {} and try again.", p.name()));
        }
        return 1;
    }
    let started = rune::time::Instant::now();
    for line in win_install::unregister(&p, &root) {
        a.log(&line);
    }
    a.log(&format!("unregistered in {} µs", started.elapsed().as_micros()));
    // The caches and logs are every copy's, an unpacked one's too, which no uninstall entry
    // names: they go only with the user's data, as OpenCode Desktop's uninstaller keeps its app
    // data unless asked (`deleteAppDataOnUninstall: false`).
    match start_cleanup(&root, delete_data, delete_data, a.log.as_deref()) {
        Ok(pid) => a.log(&format!("cleanup (pid {pid}) removes the files once this process exits")),
        Err(e) => {
            a.log(&format!("couldn't start the cleanup: {e}"));
            return 1;
        }
    }
    0
}

/// Copies this executable to the temp folder and starts it to delete the install, which a
/// running program can't do to its own folder.
fn start_cleanup(root: &Path, caches: bool, delete_data: bool, log: Option<&Path>) -> std::io::Result<u32> {
    let temp = std::env::temp_dir();
    let copy = temp.join(format!("opencode-rune-uninstall-{}.exe", std::process::id()));
    std::fs::copy(std::env::current_exe()?, &copy)?;
    // A packaged build's exe loads the app from the library that carries its own name
    // (`apps/desktop-thin`): the copy gets one too. A hard link where it can be one: the library
    // is the app's whole size, and a link of a loaded file can be removed with the install.
    if let Some(library) = library() {
        let beside = copy.with_extension("dll");
        std::fs::remove_file(&beside).ok();
        std::fs::hard_link(&library, &beside).or_else(|_| std::fs::copy(&library, &beside).map(drop))?;
    }
    let mut cmd = Command::new(&copy);
    cmd.arg("--uninstall-cleanup").arg("--wait").arg(std::process::id().to_string()).arg("--root").arg(root);
    if caches {
        cmd.arg("--caches");
    }
    if delete_data {
        cmd.arg("--delete-data");
    }
    if let Some(log) = log {
        cmd.arg("--log").arg(log);
    }
    Ok(cmd.current_dir(&temp).creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW).spawn()?.id())
}

fn cleanup(a: &Args) -> i32 {
    // Anyone can start this with any `--root`: it must be an install folder of the app.
    if let Err(why) = check_root(&a.root) {
        a.log(&format!("{} isn't an install folder ({why}); nothing removed", a.root.display()));
        return 2;
    }
    wait_for_exit(a.wait);
    let plan = plan(&a.root, a.caches, a.delete_data);
    for why in &plan.kept {
        a.log(&format!("kept the app's data: {why}"));
    }
    let mut ok = true;
    for dir in &plan.delete {
        if !dir.exists() {
            continue;
        }
        let gone = win_install::remove_dir(dir);
        a.log(&format!("{} {}", if gone { "removed" } else { "couldn't remove" }, dir.display()));
        ok &= gone;
    }
    for file in &plan.files {
        std::fs::remove_file(file).ok();
    }
    a.log(if ok { "uninstalled" } else { "uninstalled; some files stay until they're free" });
    delete_self();
    i32::from(!ok)
}

/// What an uninstall deletes: folders, each removed whole, and single files.
#[derive(Default)]
struct Plan {
    delete: Vec<PathBuf>,
    files: Vec<PathBuf>,
    /// Why data the flags asked for stays.
    kept: Vec<String>,
}

/// The install folder, the updater's folders beside it and this install's browser socket; with
/// `caches` the app's caches and logs, with `delete_data` its settings and this install's browser
/// profile, unless a running copy holds them or a folder isn't where Windows says it is.
fn plan(root: &Path, caches: bool, delete_data: bool) -> Plan {
    let exe = root.join(win_install::EXE);
    let mut plan = Plan { delete: std::iter::once(root.to_owned()).chain(updater_folders(root)).collect(), ..Plan::default() };
    // The browser engine's folder is shared by every app with a browser pane: only this
    // install's socket, and with its data its browser profile, are its own.
    if let Some(engine) = crate::dirs::engine() {
        plan.files.push(engine.cache().join(browser_pane::installer::socket_name_for(&exe, 0)));
    }
    if caches || delete_data {
        match data_free(&exe).and_then(|()| data_targets(caches, delete_data, &exe)) {
            Ok(targets) => plan.delete.extend(targets),
            Err(why) => plan.kept.push(why),
        }
    }
    plan
}

/// The data folders the flags ask for, each checked: inside the folder Windows names for it
/// (`SHGetKnownFolderPath`, so no environment variable steers a delete; a `RUNE_DIRS_ROOT` run's
/// own root instead), with no junction or link on the way down to it. A folder inside another
/// that goes too goes with it (`%LOCALAPPDATA%\rune-opencode` holds `Cache` and `Logs`).
fn data_targets(caches: bool, delete_data: bool, exe: &Path) -> Result<Vec<PathBuf>, String> {
    let app = crate::dirs::app().ok_or("the app's folders aren't known")?;
    let rooted = app.is_rooted();
    // A scratch run's root is the one the environment sets; the user's folders are the shell's.
    #[allow(clippy::disallowed_methods)]
    let root = rooted.then(|| std::env::var_os("RUNE_DIRS_ROOT").map(PathBuf::from)).flatten();
    let (local, roaming) = match &root {
        Some(root) => (root.clone(), root.clone()),
        None => (win_install::local_app_data().ok_or("no local app data folder")?, win_install::roaming_app_data().ok_or("no roaming app data folder")?),
    };
    let mut targets: Vec<(PathBuf, &Path)> = Vec::new();
    if caches {
        targets.extend([app.state(), app.cache(), app.logs()].map(|t| (t.to_owned(), local.as_path())));
    }
    if delete_data {
        targets.extend([app.data(), app.config()].map(|t| (t.to_owned(), roaming.as_path())));
        if let Some(engine) = crate::dirs::engine() {
            targets.push((browser_pane::profile::profile_dir_for(engine.state(), exe), local.as_path()));
        }
    }
    for (target, base) in &targets {
        check_tree(target, base)?;
    }
    let all: Vec<PathBuf> = targets.into_iter().map(|(t, _)| t).collect();
    let mut outer: Vec<PathBuf> = all.iter().filter(|t| !all.iter().any(|o| !same(o, t) && inside(t, o).is_some())).cloned().collect();
    outer.dedup_by(|a, b| same(a, b));
    Ok(outer)
}

/// `target` lies inside `base`, and nothing between them, `target` included, is a junction or a
/// link: Windows follows one in the middle of a path, so a delete would land outside.
fn check_tree(target: &Path, base: &Path) -> Result<(), String> {
    let rest = inside(target, base).filter(|r| r.components().next().is_some()).ok_or_else(|| format!("{} isn't inside {}", target.display(), base.display()))?;
    let mut at = base.to_owned();
    for part in rest.components() {
        at.push(part);
        if is_link(&at) {
            return Err(format!("{} is a junction or link", at.display()));
        }
    }
    Ok(())
}

/// What follows `base` in `path`, compared without regard to case; none when it isn't inside.
fn inside(path: &Path, base: &Path) -> Option<PathBuf> {
    let lower = |p: &Path| p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    let (p, b) = (lower(path), lower(base));
    (p.len() >= b.len() && p[..b.len()] == b[..]).then(|| path.components().skip(b.len()).collect())
}

/// Refuses while a copy of the app holds this build's profile (its lease, `opencode.sqlite-boot`,
/// `opencode_storage`'s) or this install's browser profile (the engine's `engine.lock`): the
/// files nobody holds would go and the held ones stay, leaving half a profile under a copy that
/// still writes it. `close_running` closes only the copies inside the install folder.
fn data_free(exe: &Path) -> Result<(), String> {
    let held = |file: PathBuf| std::fs::File::open(&file).is_ok_and(|f| f.try_lock().is_err());
    let name = crate::launch::product().name().to_owned();
    if crate::dirs::data().is_some_and(|d| held(d.join("opencode.sqlite-boot"))) {
        return Err(format!("a running copy of {name} uses its settings"));
    }
    if crate::dirs::engine().is_some_and(|e| held(browser_pane::profile::profile_dir_for(e.state(), exe).join("engine.lock"))) {
        return Err(format!("{name}'s browser is still running"));
    }
    Ok(())
}

/// The data folder an installed build keeps (`dirs::app`), from its id: a pull request's build,
/// stable and beta their own, dev (and a local build) `rune-opencode`; every build
/// `rune-opencode` under this run's `RUNE_DIRS_ROOT`.
fn folder_of(id: &str) -> String {
    if crate::dirs::rooted() {
        return "shuvcode-rune".to_owned();
    }
    match id.strip_prefix("dev.shuv.shuvcode-rune.") {
        Some(own @ ("stable" | "beta")) => format!("shuvcode-rune.{own}"),
        Some(own) => own.strip_prefix("pr.").map_or_else(|| "shuvcode-rune".to_owned(), |n| format!("shuvcode-rune.pr-{n}")),
        None => "shuvcode-rune".to_owned(),
    }
}

/// Refuses `root` unless it can be an install folder: absolute and below a drive's top, not one
/// of the user's own folders (nor holding one), not a junction or link, and holding the app.
fn check_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute() || root.file_name().is_none() || root.parent().and_then(Path::parent).is_none() {
        return Err("a drive or a folder at a drive's top".into());
    }
    if win_install::protected_folders().iter().any(|k| inside(k, root).is_some()) {
        return Err("it is, or holds, one of the user's own folders".into());
    }
    if is_link(root) {
        return Err("it is a junction or link".into());
    }
    if !root.join(win_install::EXE).is_file() {
        return Err(format!("it has no {}", win_install::EXE));
    }
    Ok(())
}

/// A junction, a symbolic link or another reparse point.
fn is_link(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// The updater's and the setup's folders beside the install (`updater::windows::beside`,
/// `apps/installer`): `.<name>.old-*`, `.staging-*`, `.ready-*`, `.setup-*` and `.staged`, and no
/// other sibling.
fn updater_folders(root: &Path) -> Vec<PathBuf> {
    let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else { return Vec::new() };
    let Some(parent) = root.parent() else { return Vec::new() };
    let ours = |file: &str| {
        file.strip_prefix(&format!(".{name}.")).is_some_and(|rest| rest == "staged" || ["old-", "staging-", "ready-", "setup-"].iter().any(|p| rest.starts_with(p)))
    };
    std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| ours(&e.file_name().to_string_lossy()) && !is_link(&e.path()))
        .map(|e| e.path())
        .collect()
}

/// The temp copy deletes itself, and its library, once it has exited. The paths reach `cmd.exe`
/// through the environment, read with delayed expansion after the line is parsed, so no
/// character in the temp folder's name can run a command.
fn delete_self() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(system) = system_folder() else { return };
    Command::new(system.join("cmd.exe"))
        .args(["/d", "/v:on", "/c"])
        .raw_arg("ping -n 3 127.0.0.1 >nul & del /f /q \"!OPENCODE_UNINSTALL_SELF!\" & if defined OPENCODE_UNINSTALL_LIBRARY del /f /q \"!OPENCODE_UNINSTALL_LIBRARY!\"")
        .env("OPENCODE_UNINSTALL_SELF", &exe)
        .envs(library().map(|library| ("OPENCODE_UNINSTALL_LIBRARY", library)))
        .current_dir(&system)
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .ok();
}

/// The app's library, when a thin exe loaded the app from one (a packaged build:
/// `apps/desktop-thin`): the file of the image this code is in, unless that is the exe.
pub fn library() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
    let mut name = [0u16; 1024];
    // SAFETY: the image's base is its module handle; the buffer is `name` with its length.
    let len = unsafe { GetModuleFileNameW(crate::launch::image_pages::base() as _, name.as_mut_ptr(), name.len() as u32) } as usize;
    if len == 0 || len >= name.len() {
        return None;
    }
    let path = PathBuf::from(std::ffi::OsString::from_wide(&name[..len]));
    (std::env::current_exe().ok()? != path).then_some(path)
}

/// `System32`, where Windows says it is.
fn system_folder() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    let mut buf = [0u16; 260];
    // SAFETY: writes at most `buf.len()` characters into `buf` and returns how many.
    let n = unsafe { windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
}

fn wait_for_exit(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    if pid == 0 {
        return;
    }
    // SAFETY: a wait-only handle, closed once; a process already gone answers null.
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !handle.is_null() {
            WaitForSingleObject(handle, 30_000);
            CloseHandle(handle);
        }
    }
}

fn same(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

mod dialog {
    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Controls::{
        TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDCBF_CANCEL_BUTTON, TDCBF_CLOSE_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW,
        TD_ERROR_ICON, TaskDialogIndirect,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::IDCANCEL;
    use windows_sys::core::BOOL;

    use win_install::Product;

    const UNINSTALL: i32 = 100;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Asks before uninstalling: Some(whether to delete the user's data), or None to keep it all.
    /// `own_data`: no other build of the app shares the settings, so they may go too.
    pub fn confirm(p: &Product, own_data: bool) -> Option<bool> {
        let title = wide(&format!("Uninstall {}", p.name()));
        let instruction = wide(&format!("Uninstall {}?", p.name()));
        let content = wide(if own_data {
            "This removes the app and its Start menu and desktop shortcuts. Your OpenCode sessions stay on the server."
        } else {
            "This removes the app and its Start menu and desktop shortcuts. Its settings stay for the other OpenCode (Rune) installed, and your OpenCode sessions stay on the server."
        });
        let verification = wide("Also delete my settings, tabs, drafts and caches");
        let label = wide("Uninstall");
        let buttons = [TASKDIALOG_BUTTON { nButtonID: UNINSTALL, pszButtonText: label.as_ptr() }];
        // SAFETY: the module handle of this executable, whose icon resource 1 the dialog shows.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        // SAFETY: a zeroed config is the documented default for every field set below.
        let mut config: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
        config.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
        config.hInstance = instance;
        config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW;
        config.dwCommonButtons = TDCBF_CANCEL_BUTTON;
        config.pszWindowTitle = title.as_ptr();
        // MAKEINTRESOURCE(1): the exe's icon.
        config.Anonymous1.pszMainIcon = std::ptr::without_provenance(1);
        config.pszMainInstruction = instruction.as_ptr();
        config.pszContent = content.as_ptr();
        config.cButtons = buttons.len() as u32;
        config.pButtons = buttons.as_ptr();
        config.nDefaultButton = UNINSTALL;
        config.pszVerificationText = if own_data { verification.as_ptr() } else { std::ptr::null() };
        let (mut button, mut checked): (i32, BOOL) = (IDCANCEL, 0);
        // SAFETY: every string and the button array outlive the modal call, which writes the two out-values.
        let hr = unsafe { TaskDialogIndirect(&config, &mut button, std::ptr::null_mut(), &mut checked) };
        (hr == S_OK && button == UNINSTALL).then_some(checked != 0)
    }

    pub fn error(p: &Product, message: &str) {
        let title = wide(&format!("Uninstall {}", p.name()));
        let instruction = wide(&format!("Couldn't uninstall {}", p.name()));
        let content = wide(message);
        // SAFETY: as in `confirm`.
        let mut config: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
        config.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
        config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION;
        config.dwCommonButtons = TDCBF_CLOSE_BUTTON;
        config.pszWindowTitle = title.as_ptr();
        config.Anonymous1.pszMainIcon = TD_ERROR_ICON;
        config.pszMainInstruction = instruction.as_ptr();
        config.pszContent = content.as_ptr();
        // SAFETY: the strings outlive the modal call.
        unsafe { TaskDialogIndirect(&config, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    }
}
