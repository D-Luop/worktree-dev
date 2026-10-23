//! `wtd tray`: notification-area icon that lists sessions (open each in VS Code / Zed / a terminal),
//! starts/stops the daemon and opens the dev-root window.
//!
//! A separate process from the daemon on purpose: if the daemon owned the icon, stopping it would
//! remove the only button that can start it again. Holds one daemon subscription (on its own thread)
//! to keep the icon and tooltip in sync; when the daemon is down it retries the pipe every 2s.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wtd_core::model::{Worktree, WorktreeKind};
use wtd_core::protocol::{method, Push, ServerLine};
use wtd_core::status::Status;

use crate::frontend::{self, Frontend};
use crate::{client::Client, paths, win};

const WM_TRAY: u32 = WM_APP + 1;
const WM_STATE: u32 = WM_APP + 2;
const ID_TOGGLE: usize = 1;
const ID_OPEN: usize = 2;
const ID_LOGON: usize = 3;
const ID_QUIT: usize = 4;
/// "New session" items: ID_NEW_BASE + index into Frontend::ALL
const ID_NEW_BASE: usize = 20;
/// "Default frontend" radio items: ID_DEFAULT_BASE + index into Frontend::ALL
const ID_DEFAULT_BASE: usize = 10;
/// session rows: ID_ROW_BASE + row * ROW_STRIDE + action (0..3 = Frontend::ALL, ACT_STOP)
const ID_ROW_BASE: usize = 1000;
const ROW_STRIDE: usize = 8;
const ACT_STOP: usize = 3;
const MAX_ROWS: usize = 40;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "WorkTreeDev";

#[derive(Clone, Copy, PartialEq, Default)]
struct View {
    running: bool,
    sessions: u32,
    need_you: u32,
    /// sessions the daemon hosts (stopping it ends them)
    hosted: u32,
}

struct Tray {
    hwnd: HWND,
    view: View,
    icons: [HICON; 3], // stopped, running, needs-you
    taskbar_created: u32,
}
unsafe impl Send for Tray {}

static TRAY: Mutex<Option<Tray>> = Mutex::new(None);
/// the daemon's worktrees, kept by the watcher thread for the menu
static FLEET: Mutex<Vec<Worktree>> = Mutex::new(Vec::new());
/// worktree ids of the rows in the menu last shown (WM_COMMAND arrives after it closes)
static ROWS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `wtd tray [--spawn | --quit | --logon on|off]`
pub fn main(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        // start detached (from install / a terminal) so it doesn't die with the caller
        Some("--spawn") => {
            let mut cmd = tray_command();
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            win::spawn_detached(&mut cmd)?;
            return Ok(0);
        }
        // close a running tray (e.g. before replacing wtd.exe)
        Some("--quit") => unsafe {
            let hwnd = FindWindowW(win::wide("wtd-tray").as_ptr(), std::ptr::null());
            if !hwnd.is_null() {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            return Ok(0);
        },
        Some("--logon") => {
            set_start_at_logon(args.get(1).map(String::as_str) != Some("off"));
            return Ok(0);
        }
        Some(a) => bail!("unknown option '{a}' (wtd tray [--spawn | --quit | --logon on|off])"),
        None => {}
    }
    unsafe {
        // one tray per user session
        CreateMutexW(std::ptr::null(), 0, win::wide("Local\\wtd-tray").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(0);
        }
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = win::wide("wtd-tray");
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

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
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
            if c.hello("tray").is_ok() && c.request(method::SUBSCRIBE, serde_json::json!({})).is_ok() {
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
                                    approvals = snapshot.messages.iter().filter(|m| m.state == wtd_core::model::MessageState::Pending).count() as u32;
                                    wts = snapshot.worktrees.into_iter().map(|w| (w.id.clone(), w)).collect();
                                }
                                Push::Messages { messages, .. } => {
                                    approvals = messages.iter().filter(|m| m.state == wtd_core::model::MessageState::Pending).count() as u32;
                                }
                                Push::Upsert { worktree, .. } => { wts.insert(worktree.id.clone(), worktree); }
                                Push::Remove { id, .. } => { wts.remove(&id); }
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
        post(View::default());
        std::thread::sleep(Duration::from_secs(2));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP => open_vscode(),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
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
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        WM_COMMAND => {
            match (wp & 0xFFFF) as usize {
                ID_TOGGLE => toggle_daemon(),
                ID_OPEN => open_vscode(),
                ID_LOGON => set_start_at_logon(!start_at_logon()),
                ID_QUIT => PostQuitMessage(0),
                id if (ID_DEFAULT_BASE..ID_DEFAULT_BASE + Frontend::ALL.len()).contains(&id) => {
                    if let Ok(dev) = paths::dev_root() {
                        let _ = frontend::set_default_frontend(&dev, Frontend::ALL[id - ID_DEFAULT_BASE]);
                    }
                }
                id if (ID_NEW_BASE..ID_NEW_BASE + Frontend::ALL.len()).contains(&id) => {
                    let f = Frontend::ALL[id - ID_NEW_BASE];
                    std::thread::spawn(move || report(paths::dev_root().and_then(|dev| frontend::new_session(&dev, f)), &format!("start a session in {}", f.label())));
                }
                id if id >= ID_ROW_BASE => row_command(id - ID_ROW_BASE),
                _ => {}
            }
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

unsafe fn show_menu(hwnd: HWND) {
    let view = TRAY.lock().unwrap().as_ref().map(|t| t.view).unwrap_or_default();
    let m = CreatePopupMenu();
    let add = |flags: u32, id: usize, text: &str| {
        let w = win::wide(text);
        AppendMenuW(m, flags, id, w.as_ptr());
    };
    add(MF_STRING | MF_GRAYED, 0, &tooltip(view));
    AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
    let def = paths::dev_root().map(|d| frontend::default_frontend(&d)).unwrap_or(Frontend::VsCode);
    let rows = session_rows();
    for (i, w) in rows.iter().enumerate() {
        let sub = CreatePopupMenu();
        let base = ID_ROW_BASE + i * ROW_STRIDE;
        for (a, f) in Frontend::ALL.iter().enumerate() {
            AppendMenuW(sub, MF_STRING, base + a, win::wide(&format!("Open in {}", f.label())).as_ptr());
            if *f == def {
                SetMenuDefaultItem(sub, (base + a) as u32, 0);
            }
        }
        if w.live {
            AppendMenuW(sub, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(sub, MF_STRING, base + ACT_STOP, win::wide("Stop session").as_ptr());
        }
        AppendMenuW(m, MF_POPUP, sub as usize, win::wide(&row_label(w)).as_ptr());
    }
    if rows.is_empty() && view.running {
        add(MF_STRING | MF_GRAYED, 0, "No worktrees yet");
    }
    *ROWS.lock().unwrap() = rows.into_iter().map(|w| w.id).collect();
    let nsub = CreatePopupMenu();
    for (i, f) in Frontend::ALL.iter().enumerate() {
        AppendMenuW(nsub, MF_STRING, ID_NEW_BASE + i, win::wide(&format!("in {}", f.label())).as_ptr());
        if *f == def {
            SetMenuDefaultItem(nsub, (ID_NEW_BASE + i) as u32, 0);
        }
    }
    AppendMenuW(m, MF_POPUP | if view.running { 0 } else { MF_GRAYED }, nsub as usize, win::wide("New session").as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
    let fsub = CreatePopupMenu();
    for (i, f) in Frontend::ALL.iter().enumerate() {
        AppendMenuW(fsub, MF_STRING | if *f == def { MF_CHECKED } else { MF_UNCHECKED }, ID_DEFAULT_BASE + i, win::wide(f.label()).as_ptr());
    }
    AppendMenuW(m, MF_POPUP, fsub as usize, win::wide("Open sessions in").as_ptr());
    add(MF_STRING, ID_TOGGLE, if view.running { "Stop daemon" } else { "Start daemon" });
    add(MF_STRING, ID_OPEN, "Open VS Code");
    AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
    add(MF_STRING | if start_at_logon() { MF_CHECKED } else { MF_UNCHECKED }, ID_LOGON, "Start tray at logon");
    add(MF_STRING, ID_QUIT, "Quit tray");
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // so the menu closes when you click elsewhere
    TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, std::ptr::null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(m);
}

/// Worktrees for the menu: needs-you first, then by activity rank, then name; the dev base excluded.
fn session_rows() -> Vec<Worktree> {
    let rank = |s: Status| match s {
        Status::Input => 0,
        Status::Working => 1,
        Status::Reviewing => 2,
        Status::Pr => 3,
        Status::None | Status::Stopped => 4,
        Status::Done => 5,
    };
    let mut v: Vec<Worktree> = FLEET.lock().unwrap().iter().filter(|w| w.kind != WorktreeKind::Dev).cloned().collect();
    v.sort_by(|a, b| (rank(a.status), !a.live, &a.id).cmp(&(rank(b.status), !b.live, &b.id)));
    v.truncate(MAX_ROWS);
    v
}

fn row_label(w: &Worktree) -> String {
    let glyph = match w.status {
        Status::Input => "●",
        Status::Working => "◐",
        Status::Reviewing => "◆",
        Status::Pr => "◇",
        Status::Done => "✓",
        Status::None | Status::Stopped => if w.live { "○" } else { "·" },
    };
    // `&` is a menu mnemonic marker; `	` right-aligns the repo
    format!("{glyph}  {}	{}", w.name.replace('&', "&&"), w.slug.replace('&', "&&"))
}

fn row_command(off: usize) {
    let Some(id) = ROWS.lock().unwrap().get(off / ROW_STRIDE).cloned() else { return };
    let act = off % ROW_STRIDE;
    if act == ACT_STOP {
        spawn_hidden(Command::new(paths::wtd_exe()).args(["stop", &id]));
        return;
    }
    let Some(&f) = Frontend::ALL.get(act) else { return };
    // off the UI thread: opening VS Code waits for `code` to focus the window first
    std::thread::spawn(move || report(paths::dev_root().and_then(|dev| frontend::open(&dev, &id, f)), &format!("open {id} in {}", f.label())));
}

fn report(r: Result<()>, what: &str) {
    if let Err(e) = r {
        let text = win::wide(&format!("Couldn't {what}:\n\n{e:#}"));
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), win::wide("WorkTreeDev").as_ptr(), MB_OK | MB_ICONWARNING) };
    }
}

fn spawn_hidden(cmd: &mut Command) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = win::no_window(cmd).spawn();
}

fn toggle_daemon() {
    let (running, hosted, hwnd) = TRAY.lock().unwrap().as_ref().map(|t| (t.view.running, t.view.hosted, t.hwnd)).unwrap_or((false, 0, std::ptr::null_mut()));
    if running && hosted > 0 {
        let text = win::wide(&format!(
            "{hosted} agent session{} running in the daemon and will end.\n\nTheir conversations can be resumed by reopening the worktrees.\n\nStop the daemon?",
            if hosted == 1 { " is" } else { "s are" }));
        let title = win::wide("WorkTreeDev");
        if unsafe { MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) } != IDYES {
            return;
        }
    }
    // we may be wtd-tray.exe: daemon control lives in the console CLI next to us
    let args: &[&str] = if running { &["daemon", "stop", "--force"] } else { &["daemon", "start"] };
    spawn_hidden(Command::new(paths::wtd_exe()).args(args));
}

fn open_vscode() {
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

pub fn start_at_logon() -> bool {
    unsafe {
        let mut size: u32 = 0;
        RegGetValueW(HKEY_CURRENT_USER, win::wide(RUN_KEY).as_ptr(), win::wide(RUN_VALUE).as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) == 0
    }
}

pub fn set_start_at_logon(on: bool) {
    unsafe {
        if on {
            // the windowless wtd-tray.exe, so signing in never opens a console window for the tray
            let cmdline = match paths::tray_exe() {
                Some(t) => format!("\"{}\"", t.display()),
                None => format!("\"{}\" tray", paths::wtd_exe().display()),
            };
            let cmdline = win::wide(&cmdline);
            RegSetKeyValueW(HKEY_CURRENT_USER, win::wide(RUN_KEY).as_ptr(), win::wide(RUN_VALUE).as_ptr(), REG_SZ,
                cmdline.as_ptr() as *const _, (cmdline.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, win::wide(RUN_KEY).as_ptr(), win::wide(RUN_VALUE).as_ptr());
        }
    }
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
