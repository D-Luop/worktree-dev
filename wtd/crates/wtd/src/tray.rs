//! `wtd tray`: notification-area icon. Clicking it opens the flyout (see flyout.rs): daemon controls,
//! usage, and the sessions (open each in VS Code / Zed / a terminal). Double-click opens VS Code.
//!
//! A separate process from the daemon on purpose: if the daemon owned the icon, stopping it would
//! remove the only button that can start it again. Holds one daemon subscription (on its own thread)
//! to keep the icon and tooltip in sync; when the daemon is down it retries the pipe every 2s.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wtd_core::model::{Account, Worktree};
use wtd_core::protocol::{method, Push, ServerLine};
use wtd_core::status::Status;

use crate::{client::Client, paths, win};

const WM_TRAY: u32 = WM_APP + 1;
const WM_STATE: u32 = WM_APP + 2;
const WM_PIN_HINT: u32 = WM_APP + 3;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WorkTreeDev";
const WTD_KEY: &str = r"Software\WorkTreeDev";
/// volatile: Windows drops it at sign-out, so what it records lasts for this sign-in only
const SESSION_KEY: &str = r"Software\WorkTreeDev\Session";
const LOGON_OFF: &str = "TrayLogonOff";
const PROMOTED: &str = "TrayPromoted";
const HINT_SHOWN: &str = "TrayPinHintShown";
/// Windows 11's per-icon taskbar settings, one subkey per (exe path, uID), written by Explorer
const NOTIFY_SETTINGS: &str = r"Control Panel\NotifyIconSettings";
const CLASS: &str = "wtd-tray";

#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) struct View {
    pub running: bool,
    pub sessions: u32,
    pub need_you: u32,
    /// sessions the daemon hosts (stopping it ends them)
    pub hosted: u32,
}

struct Tray {
    hwnd: HWND,
    view: View,
    icons: [HICON; 3], // stopped, running, needs-you
    taskbar_created: u32,
}
unsafe impl Send for Tray {}

static TRAY: Mutex<Option<Tray>> = Mutex::new(None);
/// the daemon's worktrees and account usage, kept by the watcher thread for the flyout
pub(crate) static FLEET: Mutex<Vec<Worktree>> = Mutex::new(Vec::new());
pub(crate) static ACCOUNTS: Mutex<Vec<Account>> = Mutex::new(Vec::new());
/// the daemon's update status (`wtd update status`), for the ⚙ menu
pub(crate) static UPDATE: Mutex<Option<serde_json::Value>> = Mutex::new(None);
/// the staged version a "ready to update" balloon was last shown for
static UPDATE_BALLOON: Mutex<String> = Mutex::new(String::new());

pub(crate) fn view() -> View {
    TRAY.lock().unwrap().as_ref().map(|t| t.view).unwrap_or_default()
}

pub(crate) fn hwnd() -> HWND {
    TRAY.lock().unwrap().as_ref().map(|t| t.hwnd).unwrap_or(std::ptr::null_mut())
}

/// `wtd tray [--spawn | --ensure | --open | --quit | --logon on|off|refresh]`
pub fn main(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        // start detached (from install / a terminal) so it doesn't die with the caller
        Some("--spawn") => {
            let mut cmd = tray_command();
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            win::spawn_detached(&mut cmd)?;
            return Ok(0);
        }
        Some("--ensure") => {
            ensure();
            return Ok(0);
        }
        // close a running tray (e.g. before replacing wtd.exe)
        Some("--quit") => {
            quit();
            return Ok(0);
        }
        // open the running tray's flyout, as a click on its icon would
        Some("--open") => unsafe {
            let hwnd = FindWindowW(win::wide(CLASS).as_ptr(), std::ptr::null());
            if !hwnd.is_null() {
                PostMessageW(hwnd, WM_TRAY, 0, WM_LBUTTONUP as isize);
            }
            return Ok(0);
        },
        Some("--logon") => {
            match args.get(1).map(String::as_str) {
                Some("off") => set_start_at_logon(false),
                Some("refresh") => refresh_start_at_logon(),
                _ => set_start_at_logon(true),
            }
            return Ok(0);
        }
        Some(a) => bail!("unknown option '{a}' (wtd tray [--spawn | --ensure | --open | --quit | --logon on|off|refresh])"),
        None => {}
    }
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // one tray per user session
        CreateMutexW(std::ptr::null(), 0, win::wide("Local\\wtd-tray").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(0);
        }
        set_session_state(SessionState::Running);
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = win::wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            bail!("RegisterClassW failed");
        }
        // hidden top-level window (not message-only, so it receives the TaskbarCreated broadcast)
        let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null());
        if hwnd.is_null() {
            bail!("CreateWindowExW failed");
        }
        let icons = [make_icon(Kind::Stopped), make_icon(Kind::Running), make_icon(Kind::NeedsYou)];
        let taskbar_created = RegisterWindowMessageW(win::wide("TaskbarCreated").as_ptr());
        *TRAY.lock().unwrap() = Some(Tray { hwnd, view: View::default(), icons, taskbar_created });
        notify_icon(NIM_ADD);

        let hwnd_val = hwnd as isize;
        std::thread::spawn(move || watch_daemon(hwnd_val));
        // a sign-in / tray start is a safe point for an update that was waiting on live sessions
        std::thread::spawn(crate::updater::apply_at_tray_start);
        std::thread::spawn(move || pin_to_taskbar(hwnd_val));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        set_session_state(SessionState::Quit);
        notify_icon(NIM_DELETE);
    }
    Ok(0)
}

/// Background: mirror the daemon's state into the window as WM_STATE messages.
fn watch_daemon(hwnd: isize) {
    let post = |v: View| unsafe {
        let packed = ((v.sessions.min(0xFFFF) as isize) << 16) | v.need_you.min(0xFFFF) as isize;
        PostMessageW(hwnd as HWND, WM_STATE, (v.running as usize) | ((v.hosted.min(0xFFFF) as usize) << 1), packed);
    };
    loop {
        if let Ok(Some(mut c)) = Client::connect() {
            if c.hello("tray").is_ok() {
                // before subscribing: a request on a subscribed connection drops the pushes it skips
                set_update(c.request(method::UPDATE_STATUS, serde_json::json!({})).ok());
            }
            if c.request(method::SUBSCRIBE, serde_json::json!({})).is_ok() {
                let mut wts: std::collections::BTreeMap<String, Worktree> = Default::default();
                let mut approvals: u32 = 0; // agent messages waiting for the user's approval
                let summarize = |wts: &std::collections::BTreeMap<String, Worktree>, approvals: u32| View {
                    running: true,
                    sessions: wts.values().filter(|w| w.live).count() as u32,
                    need_you: wts.values().filter(|w| w.status == Status::Input).count() as u32 + approvals,
                    hosted: wts.values().filter(|w| w.hosted).count() as u32,
                };
                loop {
                    match c.read() {
                        Ok(Some(ServerLine::Push(p))) => {
                            match p {
                                Push::Snapshot { snapshot, .. } => {
                                    *ACCOUNTS.lock().unwrap() = snapshot.accounts;
                                    approvals = snapshot.messages.iter().filter(|m| m.state == wtd_core::model::MessageState::Pending).count() as u32;
                                    wts = snapshot.worktrees.into_iter().map(|w| (w.id.clone(), w)).collect();
                                }
                                Push::Messages { messages, .. } => {
                                    approvals = messages.iter().filter(|m| m.state == wtd_core::model::MessageState::Pending).count() as u32;
                                }
                                Push::Upsert { worktree, .. } => { wts.insert(worktree.id.clone(), worktree); }
                                Push::Remove { id, .. } => { wts.remove(&id); }
                                Push::Accounts { accounts, .. } => { *ACCOUNTS.lock().unwrap() = accounts; }
                                Push::Update { update } => {
                                    set_update(Some(update));
                                    continue;
                                }
                                Push::Shutdown => break,
                                _ => continue,
                            }
                            *FLEET.lock().unwrap() = wts.values().cloned().collect();
                            post(summarize(&wts, approvals));
                        }
                        Ok(Some(_)) => continue,
                        _ => break,
                    }
                }
            }
        }
        FLEET.lock().unwrap().clear();
        ACCOUNTS.lock().unwrap().clear();
        set_update(None);
        post(View::default());
        std::thread::sleep(Duration::from_secs(2));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU => crate::flyout::toggle(),
                WM_LBUTTONDBLCLK => open_vscode(),
                _ => {}
            }
            0
        }
        WM_STATE => {
            let v = View { running: wp & 1 != 0, hosted: ((wp >> 1) & 0xFFFF) as u32, sessions: ((lp >> 16) & 0xFFFF) as u32, need_you: (lp & 0xFFFF) as u32 };
            let changed = {
                let mut t = TRAY.lock().unwrap();
                let t = t.as_mut().unwrap();
                let c = t.view != v;
                t.view = v;
                c
            };
            if changed {
                notify_icon(NIM_MODIFY);
            }
            crate::flyout::refresh();
            0
        }
        WM_PIN_HINT => {
            show_pin_hint();
            0
        }
        WM_CLOSE => {
            // record the quit before the window goes, so the watchdog never sees "running" with no window
            set_session_state(SessionState::Quit);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => {
            let tc = TRAY.lock().unwrap().as_ref().map(|t| t.taskbar_created).unwrap_or(0);
            if tc != 0 && msg == tc {
                notify_icon(NIM_ADD); // Explorer restarted: our icon is gone, add it back
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

fn tooltip(v: View) -> String {
    if !v.running {
        return "WorkTreeDev — stopped".into();
    }
    let mut s = format!("WorkTreeDev — running · {} session{}", v.sessions, if v.sessions == 1 { "" } else { "s" });
    if v.need_you > 0 {
        s.push_str(&format!(" · {} need{} you", v.need_you, if v.need_you == 1 { "s" } else { "" }));
    }
    s
}

unsafe fn notify_icon(op: u32) {
    let t = TRAY.lock().unwrap();
    let Some(t) = t.as_ref() else { return };
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = t.hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = match (t.view.running, t.view.need_you > 0) {
        (false, _) => t.icons[0],
        (true, false) => t.icons[1],
        (true, true) => t.icons[2],
    };
    let tip: Vec<u16> = tooltip(t.view).encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    Shell_NotifyIconW(op, &nid);
}

/// New icons land in the ^ overflow. Once, on first run, show ours on the taskbar instead
/// (`IsPromoted = 1`) unless the user already chose. Explorer writes the entry shortly after
/// `NIM_ADD`; if it never appears (no such setting on this Windows), hint how to pin it by hand.
fn pin_to_taskbar(hwnd: isize) {
    if win::reg::exists(WTD_KEY, PROMOTED) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    for _ in 0..15 {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(key) = notify_settings_entry(&exe.to_string_lossy()) {
            if should_promote(win::reg::get_dword(&key, "IsPromoted")) {
                win::reg::set_dword(&key, "IsPromoted", 1);
            }
            win::reg::set_dword(WTD_KEY, PROMOTED, 1);
            return;
        }
    }
    if !win::reg::exists(WTD_KEY, HINT_SHOWN) {
        unsafe { PostMessageW(hwnd as HWND, WM_PIN_HINT, 0, 0) };
    }
}

/// Absent = the user hasn't chosen; 0 = they moved it to the overflow, which we leave alone.
fn should_promote(is_promoted: Option<u32>) -> bool {
    is_promoted.is_none()
}

fn notify_settings_entry(exe: &str) -> Option<String> {
    win::reg::subkeys(NOTIFY_SETTINGS).into_iter().map(|k| format!(r"{NOTIFY_SETTINGS}\{k}")).find(|key| {
        win::reg::get_dword(key, "UID") == Some(1) && win::reg::get_sz(key, "ExecutablePath").is_some_and(|p| same_exe(&p, exe))
    })
}

/// Explorer stores paths under known folders as `{folder-guid}\rest`: match those by the rest.
fn same_exe(stored: &str, exe: &str) -> bool {
    let (stored, exe) = (stored.to_lowercase(), exe.to_lowercase());
    match stored.strip_prefix('{').and_then(|s| s.split_once(r"}\")) {
        Some((_, rest)) => exe.ends_with(&format!(r"\{rest}")),
        None => stored == exe,
    }
}

fn show_pin_hint() {
    if balloon("Keep WorkTreeDev on the taskbar", "Drag this icon out of the ^ overflow onto the taskbar, or turn it on under Settings → Personalization → Taskbar → Other system tray icons.") {
        win::reg::set_dword(WTD_KEY, HINT_SHOWN, 1);
    }
}

fn set_update(u: Option<serde_json::Value>) {
    if let Some(v) = &u {
        update_balloon(v);
    }
    *UPDATE.lock().unwrap() = u;
}

/// Once per version: an update is ready but waits on the user (live sessions, policy `ask`/`never`).
fn update_balloon(u: &serde_json::Value) {
    let (Some(ver), hosted) = (u["staged"].as_str(), u["hosted_sessions"].as_u64().unwrap_or(0)) else { return };
    if hosted == 0 || u["settings"]["restartWithLiveSessions"] == "auto" {
        return;
    }
    let mut last = UPDATE_BALLOON.lock().unwrap();
    if *last != ver && balloon(&format!("WorkTreeDev {ver} is ready"), &format!("Restart to update from the ⚙ menu ({hosted} live session{} will end), or it installs once they've closed.", if hosted == 1 { "" } else { "s" })) {
        *last = ver.to_string();
    }
}

fn balloon(title: &str, text: &str) -> bool {
    let t = TRAY.lock().unwrap();
    let Some(t) = t.as_ref() else { return false };
    let fill = |dst: &mut [u16], s: &str| {
        let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
        dst[..w.len()].copy_from_slice(&w);
    };
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = t.hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        fill(&mut nid.szInfoTitle, title);
        fill(&mut nid.szInfo, text);
        Shell_NotifyIconW(NIM_MODIFY, &nid) != 0
    }
}

/// Stop one worktree's session (the daemon kills its process tree).
pub(crate) fn stop_session(id: &str) {
    spawn_hidden(Command::new(paths::wtd_exe()).args(["stop", id]));
}

pub(crate) fn report(r: Result<()>, what: &str) {
    if let Err(e) = r {
        let text = win::wide(&format!("Couldn't {what}:\n\n{e:#}"));
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), win::wide("WorkTreeDev").as_ptr(), MB_OK | MB_ICONWARNING) };
    }
}

fn spawn_hidden(cmd: &mut Command) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = win::no_window(cmd).spawn();
}

/// Hosted sessions end with the daemon: ask first when there are any. `owner` keeps the box on top.
fn confirm_end_sessions(owner: HWND, question: &str) -> bool {
    let hosted = view().hosted;
    if hosted == 0 {
        return true;
    }
    let text = win::wide(&format!(
        "{hosted} agent session{} running in the daemon and will end.\n\nTheir conversations can be resumed by reopening the worktrees.\n\n{question}",
        if hosted == 1 { " is" } else { "s are" }));
    let title = win::wide("WorkTreeDev");
    unsafe { MessageBoxW(owner, text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) == IDYES }
}

pub(crate) fn toggle_daemon(owner: HWND) {
    let running = view().running;
    if running && !confirm_end_sessions(owner, "Stop the daemon?") {
        return;
    }
    // we may be wtd-tray.exe: daemon control lives in the console CLI next to us
    let args: &[&str] = if running { &["daemon", "stop", "--force"] } else { &["daemon", "start"] };
    spawn_hidden(Command::new(paths::wtd_exe()).args(args));
}

pub(crate) fn restart_daemon(owner: HWND) {
    if confirm_end_sessions(owner, "Restart the daemon?") {
        spawn_hidden(Command::new(paths::wtd_exe()).args(["daemon", "restart"]));
    }
}

/// Apply the staged update now (the CLI hands it to the apply helper, which restarts the daemon and tray).
pub(crate) fn restart_to_update(owner: HWND) {
    if confirm_end_sessions(owner, "Restart to update?") {
        spawn_hidden(Command::new(paths::wtd_exe()).args(["update", "--apply-now", "--yes"]));
    }
}

pub(crate) fn wtd_detached(args: &[&str]) {
    spawn_hidden(Command::new(paths::wtd_exe()).args(args));
}

pub(crate) fn open_vscode() {
    if let Ok(dev) = paths::dev_root() {
        // `code <folder>` focuses the window already showing that folder rather than opening another
        spawn_hidden(Command::new("cmd").args(["/c", "code"]).arg(&dev));
    }
}

/// How to launch the tray: the windowless `wtd-tray.exe` when installed, else `wtd.exe tray`.
fn tray_command() -> Command {
    match paths::tray_exe() {
        Some(t) => Command::new(t),
        None => {
            let mut c = Command::new(paths::wtd_exe());
            c.arg("tray");
            c
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SessionState {
    Running,
    /// the user quit it (⚙ Quit / `wtd tray --quit`): leave it gone until the next sign-in
    Quit,
}

fn session_state() -> Option<SessionState> {
    match win::reg::get_sz(SESSION_KEY, "Tray")?.as_str() {
        "running" => Some(SessionState::Running),
        "quit" => Some(SessionState::Quit),
        _ => None,
    }
}

fn set_session_state(s: SessionState) {
    if win::reg::create(WTD_KEY, false) && win::reg::create(SESSION_KEY, true) {
        win::reg::set_sz(SESSION_KEY, "Tray", if s == SessionState::Running { "running" } else { "quit" });
    }
}

fn tray_alive() -> bool {
    unsafe { !FindWindowW(win::wide(CLASS).as_ptr(), std::ptr::null()).is_null() }
}

pub(crate) fn is_running() -> bool {
    tray_alive()
}

/// Close a running tray (e.g. before replacing wtd.exe).
pub(crate) fn quit() {
    unsafe {
        let hwnd = FindWindowW(win::wide(CLASS).as_ptr(), std::ptr::null());
        if !hwnd.is_null() {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
}

/// No state yet this sign-in means it never started (e.g. the Run entry was disabled in Task
/// Manager): start it if it's meant to run at logon. "running" with no window means it died.
fn should_relaunch(alive: bool, state: Option<SessionState>, logon: bool) -> bool {
    !alive && match state {
        Some(SessionState::Running) => true,
        Some(SessionState::Quit) => false,
        None => logon,
    }
}

/// Relaunch the tray if it should be running and isn't (the daemon's watchdog calls this).
pub fn ensure() {
    if should_relaunch(tray_alive(), session_state(), start_at_logon()) {
        let mut cmd = tray_command();
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let _ = win::spawn_detached(&mut cmd);
    }
}

pub fn start_at_logon() -> bool {
    win::reg::exists(RUN_KEY, RUN_VALUE)
}

/// The user's choice (⚙ menu, settings, `--logon on|off`). Off is remembered so installs don't undo it.
pub fn set_start_at_logon(on: bool) {
    if on {
        win::reg::delete(WTD_KEY, LOGON_OFF);
        write_run_entry();
    } else {
        win::reg::set_dword(WTD_KEY, LOGON_OFF, 1);
        win::reg::delete(RUN_KEY, RUN_VALUE);
    }
}

/// On install/update: start at logon by default, pointing at this install's exe, unless the user turned it off.
pub fn refresh_start_at_logon() {
    if !win::reg::exists(WTD_KEY, LOGON_OFF) {
        write_run_entry();
    }
}

fn write_run_entry() {
    // the windowless wtd-tray.exe, so signing in never opens a console window for the tray
    let cmdline = match paths::tray_exe() {
        Some(t) => format!("\"{}\"", t.display()),
        None => format!("\"{}\" tray", paths::wtd_exe().display()),
    };
    win::reg::set_sz(RUN_KEY, RUN_VALUE, &cmdline);
}

// --- icon drawing: a 32×32 ring glyph, coloured by state -------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Stopped,
    Running,
    NeedsYou,
}

unsafe fn make_icon(kind: Kind) -> HICON {
    const N: i32 = 32;
    let (r, g, b) = match kind {
        Kind::Stopped => (0x8a, 0x8f, 0x98),
        Kind::Running | Kind::NeedsYou => (0x4a, 0xa3, 0xff),
    };
    let mut px = vec![0u32; (N * N) as usize];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // ring (outer radius 14, inner 9) plus a solid core of radius 4.5
            let a_ring = (14.5 - d).clamp(0.0, 1.0) * (d - 8.5).clamp(0.0, 1.0);
            let a_core = (5.0 - d).clamp(0.0, 1.0);
            let a = a_ring.max(a_core);
            let mut col = argb(a, r, g, b);
            if let Kind::NeedsYou = kind {
                // amber "needs you" dot, top-right, drawn over the ring
                let dd = ((x as f32 - 24.5).powi(2) + (y as f32 - 7.5).powi(2)).sqrt();
                let ad = (7.0 - dd).clamp(0.0, 1.0);
                if ad > 0.0 {
                    col = argb(ad.max(a), 0xff, 0xc1, 0x07);
                }
            }
            px[(y * N + x) as usize] = col;
        }
    }
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: N,
        biHeight: -N, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..std::mem::zeroed()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(std::ptr::null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
    let mask = CreateBitmap(N, N, 1, 1, std::ptr::null());
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(color);
    DeleteObject(mask);
    icon
}

/// Premultiplied BGRA for a 32-bit DIB.
fn argb(a: f32, r: u8, g: u8, b: u8) -> u32 {
    let a8 = (a * 255.0).round() as u32;
    let pm = |c: u8| ((c as f32) * a).round() as u32;
    (a8 << 24) | (pm(r) << 16) | (pm(g) << 8) | pm(b)
}

#[cfg(test)]
mod tests {
    use super::{same_exe, should_promote, should_relaunch, SessionState::*};

    #[test]
    fn promotes_only_when_the_user_has_not_chosen() {
        assert!(should_promote(None));
        assert!(!should_promote(Some(0)), "the user hid it");
        assert!(!should_promote(Some(1)));
    }

    #[test]
    fn matches_explorer_exe_paths() {
        let exe = r"D:\dev\worktree-dev\.wtd\bin\wtd-tray.exe";
        assert!(same_exe(r"d:\dev\worktree-dev\.wtd\bin\WTD-TRAY.exe", exe));
        assert!(!same_exe(r"D:\dev\worktree-dev\.wtd\bin\wtd.exe", exe));
        let pf = r"C:\Program Files\WorkTreeDev\wtd-tray.exe";
        assert!(same_exe(r"{6D809377-6AF0-444B-8957-A3773F02200E}\WorkTreeDev\wtd-tray.exe", pf));
        assert!(!same_exe(r"{6D809377-6AF0-444B-8957-A3773F02200E}\Other\wtd-tray.exe", pf));
    }

    #[test]
    fn relaunches_only_a_tray_that_died_or_never_started() {
        assert!(should_relaunch(false, Some(Running), false), "crashed or killed");
        assert!(!should_relaunch(false, Some(Quit), true), "the user quit it this sign-in");
        assert!(should_relaunch(false, None, true), "not started yet this sign-in, logon on");
        assert!(!should_relaunch(false, None, false), "never started, logon off");
        assert!(!should_relaunch(true, Some(Running), true), "already running");
    }
}
