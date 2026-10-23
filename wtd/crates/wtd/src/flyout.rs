//! The tray's flyout: a Windows 11-style panel opened from the notification-area icon. Daemon state
//! and controls, per-account usage, and the sessions grouped by repo.
//!
//! Drawn into a 32-bit DIB: shapes (buttons, bars, status dots) are antialiased per pixel here, then
//! GDI draws ClearType text on top. Lives on the tray's UI thread.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE};
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wtd_core::model::{Account, Worktree, WorktreeKind};
use wtd_core::status::Status;

use crate::frontend::{self, Frontend};
use crate::{paths, tray, win};

const CLASS: &str = "wtd-flyout";
const WIDTH: f32 = 360.0;
const PAD: f32 = 16.0;
const ROW_H: f32 = 32.0;
const GROUP_H: f32 = 26.0;
const MAX_ACCOUNTS: usize = 4;
// winuser.h; windows-sys files it under UI::Controls
const WM_MOUSELEAVE: u32 = 0x02A3;

const MI_OPEN: usize = 1; // + index into Frontend::ALL
const MI_STOP: usize = 10;
const MI_DEFAULT: usize = 20; // + index into Frontend::ALL
const MI_LOGON: usize = 30;
const MI_QUIT: usize = 31;

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);
impl Rgb {
    fn cref(self) -> COLORREF {
        self.0 as u32 | (self.1 as u32) << 8 | (self.2 as u32) << 16
    }
}

struct Theme {
    bg: Rgb,
    text: Rgb,
    muted: Rgb,
    divider: Rgb,
    hover: Rgb,
    button: Rgb,
    button_hover: Rgb,
    track: Rgb,
    accent: Rgb,
    on_accent: Rgb,
}

fn theme(dark: bool) -> Theme {
    if dark {
        Theme {
            bg: Rgb(0x24, 0x24, 0x24),
            text: Rgb(0xf3, 0xf3, 0xf3),
            muted: Rgb(0x9d, 0x9d, 0x9d),
            divider: Rgb(0x38, 0x38, 0x38),
            hover: Rgb(0x32, 0x32, 0x32),
            button: Rgb(0x30, 0x30, 0x30),
            button_hover: Rgb(0x3a, 0x3a, 0x3a),
            track: Rgb(0x3a, 0x3a, 0x3a),
            accent: Rgb(0x4a, 0xa3, 0xff),
            on_accent: Rgb(0x0b, 0x1a, 0x2b),
        }
    } else {
        Theme {
            bg: Rgb(0xf9, 0xf9, 0xf9),
            text: Rgb(0x1b, 0x1b, 0x1b),
            muted: Rgb(0x61, 0x61, 0x61),
            divider: Rgb(0xe3, 0xe3, 0xe3),
            hover: Rgb(0xee, 0xee, 0xee),
            button: Rgb(0xff, 0xff, 0xff),
            button_hover: Rgb(0xf0, 0xf0, 0xf0),
            track: Rgb(0xe0, 0xe0, 0xe0),
            accent: Rgb(0x00, 0x67, 0xc0),
            on_accent: Rgb(0xff, 0xff, 0xff),
        }
    }
}

fn status_color(s: Status) -> Rgb {
    match s {
        Status::Input => Rgb(0xff, 0xb9, 0x00),
        Status::Working => Rgb(0x4a, 0xa3, 0xff),
        Status::Limited => Rgb(0xff, 0x8c, 0x00),
        Status::Reviewing => Rgb(0xb3, 0x88, 0xff),
        Status::Pr => Rgb(0x60, 0xcd, 0xff),
        Status::Done => Rgb(0x6c, 0xcb, 0x5f),
        Status::None | Status::Stopped => Rgb(0x8a, 0x8a, 0x8a),
    }
}

fn status_word(w: &Worktree) -> String {
    match w.status {
        Status::Limited => return format!("⚠ {}", crate::limit::label_now(w.limit_resets_at)),
        Status::Input => "needs you",
        Status::Working => "working",
        Status::Reviewing => "reviewing",
        Status::Pr => "PR ready",
        Status::Done => "done",
        Status::None | Status::Stopped => if w.live { "idle" } else { "stopped" },
    }
    .into()
}

#[derive(Clone, PartialEq)]
enum Act {
    Restart,
    Daemon,
    VsCode,
    Row(String),
    New,
    Gear,
}

struct Hit {
    r: RECT,
    act: Act,
    /// inside the scrolling list (only clickable where the list is visible)
    in_list: bool,
}

enum Op {
    Round { x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: Rgb, clip: bool },
    Ring { cx: f32, cy: f32, r: f32, t: f32, c: Rgb, clip: bool },
    Text { r: RECT, s: String, font: Font, c: Rgb, flags: u32, clip: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Font {
    Title,
    Body,
    Small,
    SmallBold,
    Icon,
}

struct Fly {
    hwnd: HWND,
    scale: f32,
    dark: bool,
    fonts: Vec<(Font, HFONT)>,
    icon_face: &'static str,
    hits: Vec<Hit>,
    hover: Option<Act>,
    scroll: f32,
    max_scroll: f32,
    list: (f32, f32), // visible list band (y0, y1), DIP
    height: f32,
    /// when it last closed by losing focus: a click on the tray icon closes it first, so don't reopen
    closed_at: Instant,
    tracking: bool,
}

thread_local! {
    static FLY: RefCell<Option<Fly>> = const { RefCell::new(None) };
}

pub fn toggle() {
    let (exists, visible, recently) = FLY.with(|f| match f.borrow().as_ref() {
        Some(f) => (true, unsafe { IsWindowVisible(f.hwnd) } != 0, f.closed_at.elapsed() < Duration::from_millis(300)),
        None => (false, false, false),
    });
    if visible {
        hide();
        return;
    }
    if recently {
        return;
    }
    if !exists && !create() {
        return;
    }
    show();
}

/// The fleet or daemon state changed: redraw (and resize) if open.
pub fn refresh() {
    let hwnd = FLY.with(|f| f.borrow().as_ref().map(|f| f.hwnd));
    if let Some(h) = hwnd {
        if unsafe { IsWindowVisible(h) } != 0 {
            place();
            unsafe { InvalidateRect(h, std::ptr::null(), 0) };
        }
    }
}

// Win32 calls that move, show or hide the window can deliver messages to wndproc synchronously, so
// none of them run while FLY is borrowed.

fn hide() {
    let hwnd = FLY.with(|f| {
        let mut b = f.try_borrow_mut().ok()?;
        let f = b.as_mut()?;
        f.closed_at = Instant::now();
        f.hover = None;
        Some(f.hwnd)
    });
    if let Some(h) = hwnd {
        unsafe { ShowWindow(h, SW_HIDE) };
    }
}

fn create() -> bool {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = win::wide(CLASS);
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, class.as_ptr(), win::wide("WorkTreeDev").as_ptr(), WS_POPUP,
            0, 0, 10, 10, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null());
        if hwnd.is_null() {
            return false;
        }
        let round = DWMWCP_ROUND;
        DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &round as *const _ as *const _, 4);
        FLY.with(|f| {
            *f.borrow_mut() = Some(Fly {
                hwnd,
                scale: 1.0,
                dark: true,
                fonts: vec![],
                icon_face: "Segoe Fluent Icons",
                hits: vec![],
                hover: None,
                scroll: 0.0,
                max_scroll: 0.0,
                list: (0.0, 0.0),
                height: 0.0,
                closed_at: Instant::now() - Duration::from_secs(1),
                tracking: false,
            })
        });
        true
    }
}

fn show() {
    let hwnd = FLY.with(|f| f.borrow().as_ref().map(|f| f.hwnd)).unwrap();
    unsafe {
        // move onto the icon's monitor first, so the DPI we read is that monitor's
        let (anchor, _) = anchor_rect();
        SetWindowPos(hwnd, HWND_TOPMOST, anchor.left, anchor.top, 10, 10, SWP_NOACTIVATE);
        let dark = !light_theme();
        let mode: i32 = dark as i32;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, &mode as *const _ as *const _, 4);
        FLY.with(|f| {
            let mut b = f.borrow_mut();
            let f = b.as_mut().unwrap();
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            if scale != f.scale || f.fonts.is_empty() || dark != f.dark {
                for (_, h) in f.fonts.drain(..) {
                    DeleteObject(h);
                }
                f.scale = scale;
                f.dark = dark;
                f.icon_face = pick_icon_face();
                f.fonts = make_fonts(scale, f.icon_face);
            }
            f.scroll = 0.0;
        });
        place();
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

/// The tray icon's screen rect and the work area of its monitor.
fn anchor_rect() -> (RECT, RECT) {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let id = NOTIFYICONIDENTIFIER { cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32, hWnd: tray::hwnd(), uID: 1, ..std::mem::zeroed() };
        if Shell_NotifyIconGetRect(&id, &mut r) != 0 {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            r = RECT { left: pt.x, top: pt.y, right: pt.x, bottom: pt.y };
        }
        let mon = MonitorFromPoint(POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        (r, mi.rcWork)
    }
}

/// Size the window to its content (capped to the screen) and put it next to the tray icon.
fn place() {
    let (icon, work) = anchor_rect();
    let pos = FLY.with(|f| {
        let mut b = f.borrow_mut();
        let f = b.as_mut()?;
        let s = f.scale;
        let content = layout(f, &theme(f.dark)).2;
        let max_h = (work.bottom - work.top) as f32 * 0.85 / s;
        f.height = content.min(max_h);
        let (w, h) = ((WIDTH * s).round() as i32, (f.height * s).round() as i32);
        let gap = (12.0 * s) as i32;
        let cx = (icon.left + icon.right) / 2;
        let x = (cx - w / 2).clamp(work.left + gap, work.right - gap - w);
        let y = if icon.top >= work.bottom {
            work.bottom - gap - h // taskbar at the bottom
        } else if icon.bottom <= work.top {
            work.top + gap
        } else {
            ((icon.top + icon.bottom) / 2 - h / 2).clamp(work.top + gap, work.bottom - gap - h)
        };
        Some((f.hwnd, x, y, w, h))
    });
    if let Some((hwnd, x, y, w, h)) = pos {
        unsafe { SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE) };
    }
}

fn light_theme() -> bool {
    let mut v: u32 = 0;
    let mut size = 4u32;
    let key = win::wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let ok = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), win::wide("SystemUsesLightTheme").as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(),
            &mut v as *mut u32 as *mut _, &mut size)
    } == 0;
    ok && v == 1
}

/// Windows 11 ships Segoe Fluent Icons; Windows 10 has the same glyphs in Segoe MDL2 Assets.
fn pick_icon_face() -> &'static str {
    unsafe {
        let dc = GetDC(std::ptr::null_mut());
        let f = CreateFontW(16, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 0, 0, win::wide("Segoe Fluent Icons").as_ptr());
        let old = SelectObject(dc, f);
        let mut buf = [0u16; 64];
        let n = GetTextFaceW(dc, buf.len() as i32, buf.as_mut_ptr());
        SelectObject(dc, old);
        DeleteObject(f);
        ReleaseDC(std::ptr::null_mut(), dc);
        let face = String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize]);
        if face.eq_ignore_ascii_case("Segoe Fluent Icons") { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" }
    }
}

fn make_fonts(s: f32, icon_face: &str) -> Vec<(Font, HFONT)> {
    let mk = |px: f32, weight: i32, face: &str| unsafe {
        CreateFontW(-(px * s).round() as i32, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, CLEARTYPE_QUALITY as u32, 0, win::wide(face).as_ptr())
    };
    vec![
        (Font::Title, mk(15.0, 600, "Segoe UI")),
        (Font::Body, mk(13.0, 400, "Segoe UI")),
        (Font::Small, mk(12.0, 400, "Segoe UI")),
        (Font::SmallBold, mk(11.0, 600, "Segoe UI")),
        (Font::Icon, mk(13.0, 400, icon_face)),
    ]
}

// --- layout -----------------------------------------------------------------------------------------

struct Data {
    view: tray::View,
    rows: Vec<(String, Vec<Worktree>)>,
    accounts: Vec<Account>,
}

fn data() -> Data {
    let rank = |s: Status| match s {
        Status::Input | Status::Limited => 0,
        Status::Working => 1,
        Status::Reviewing => 2,
        Status::Pr => 3,
        Status::None | Status::Stopped => 4,
        Status::Done => 5,
    };
    let mut groups: std::collections::BTreeMap<String, Vec<Worktree>> = Default::default();
    let fleet = tray::FLEET.lock().unwrap();
    let group_of = |w: &Worktree| match w.kind {
        WorktreeKind::Delegate => w.name.clone(),
        WorktreeKind::Plan => "planning".into(),
        _ => w.slug.clone(),
    };
    for w in fleet.iter().filter(|w| w.kind != WorktreeKind::Dev) {
        // a delegator's workers sit in its group, whatever their own
        let parent = w.parent.as_ref().and_then(|p| fleet.iter().find(|x| &x.id == p));
        groups.entry(group_of(parent.unwrap_or(w))).or_default().push(w.clone());
    }
    drop(fleet);
    for v in groups.values_mut() {
        v.sort_by(|a, b| {
            (a.kind != WorktreeKind::Delegate, rank(a.status), !a.live, &a.name).cmp(&(b.kind != WorktreeKind::Delegate, rank(b.status), !b.live, &b.name))
        });
    }
    let accounts = tray::ACCOUNTS.lock().unwrap().iter().filter(|a| !a.nologin && (a.five_hour.is_some() || a.seven_day.is_some())).take(MAX_ACCOUNTS).cloned().collect();
    Data { view: tray::view(), rows: groups.into_iter().map(|(g, v)| (g, wtd_core::model::nest_under_parents(v))).collect(), accounts }
}

fn text_width(f: &Fly, font: Font, s: &str) -> f32 {
    unsafe {
        let dc = GetDC(f.hwnd);
        let old = SelectObject(dc, font_of(f, font));
        let w = win::wide(s);
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, w.as_ptr(), (w.len() - 1) as i32, &mut sz);
        SelectObject(dc, old);
        ReleaseDC(f.hwnd, dc);
        sz.cx as f32 / f.scale
    }
}

fn font_of(f: &Fly, font: Font) -> HFONT {
    f.fonts.iter().find(|(k, _)| *k == font).map(|(_, h)| *h).unwrap_or(std::ptr::null_mut())
}

fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> RECT {
    RECT { left: x0.round() as i32, top: y0.round() as i32, right: x1.round() as i32, bottom: y1.round() as i32 }
}

/// Everything to draw, in DIP; the hit areas; and the full content height (unscrolled).
fn layout(f: &mut Fly, t: &Theme) -> (Vec<Op>, Vec<Hit>, f32) {
    let d = data();
    let mut ops = vec![];
    let mut hits = vec![];
    let hov = |a: &Act| f.hover.as_ref() == Some(a);
    let right = WIDTH - PAD;
    let txt = |ops: &mut Vec<Op>, x0: f32, y0: f32, x1: f32, y1: f32, s: &str, font: Font, c: Rgb, flags: u32| {
        ops.push(Op::Text { r: rect(x0, y0, x1, y1), s: s.into(), font, c, flags: flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX, clip: false });
    };

    // header
    let mut y = 14.0;
    txt(&mut ops, PAD, y, right, y + 22.0, "WorkTreeDev", Font::Title, t.text, DT_LEFT);
    let (state, sc) = if d.view.running { ("Running", Rgb(0x6c, 0xcb, 0x5f)) } else { ("Stopped", Rgb(0x8a, 0x8a, 0x8a)) };
    let sw = text_width(f, Font::Small, state);
    txt(&mut ops, right - sw, y, right, y + 22.0, state, Font::Small, t.muted, DT_RIGHT);
    ops.push(Op::Round { x0: right - sw - 14.0, y0: y + 7.0, x1: right - sw - 6.0, y1: y + 15.0, r: 4.0, c: sc, clip: false });
    y += 22.0;
    let summary = if !d.view.running {
        "The daemon is stopped".to_string()
    } else {
        let mut s = format!("{} live session{}", d.view.sessions, if d.view.sessions == 1 { "" } else { "s" });
        if d.view.need_you > 0 {
            s.push_str(&format!(" · {} need{} you", d.view.need_you, if d.view.need_you == 1 { "s" } else { "" }));
        }
        s
    };
    txt(&mut ops, PAD, y, right, y + 18.0, &summary, Font::Small, t.muted, DT_LEFT);
    y += 18.0 + 12.0;

    // daemon buttons: icon glyph + label
    let mut x = PAD;
    let mut buttons: Vec<(Act, &str, &str)> = vec![];
    if d.view.running {
        buttons.push((Act::Restart, "\u{E72C}", "Restart"));
        buttons.push((Act::Daemon, "\u{E71A}", "Stop"));
    } else {
        buttons.push((Act::Daemon, "\u{E768}", "Start daemon"));
    }
    buttons.push((Act::VsCode, "\u{E8A7}", "Open VS Code"));
    for (act, icon, label) in buttons {
        let w = 12.0 + 14.0 + 6.0 + text_width(f, Font::Small, label) + 12.0;
        let r = rect(x, y, x + w, y + 32.0);
        ops.push(Op::Round { x0: x, y0: y, x1: x + w, y1: y + 32.0, r: 4.0, c: if hov(&act) { t.button_hover } else { t.button }, clip: false });
        txt(&mut ops, x + 12.0, y, x + 26.0, y + 32.0, icon, Font::Icon, t.text, DT_CENTER);
        txt(&mut ops, x + 32.0, y, x + w, y + 32.0, label, Font::Small, t.text, DT_LEFT);
        hits.push(Hit { r, act, in_list: false });
        x += w + 8.0;
    }
    y += 32.0 + 14.0;

    let divider = |ops: &mut Vec<Op>, y: f32| ops.push(Op::Round { x0: 0.0, y0: y, x1: WIDTH, y1: y + 1.0, r: 0.0, c: t.divider, clip: false });

    // usage
    if !d.accounts.is_empty() {
        divider(&mut ops, y);
        y += 1.0 + 10.0;
        for a in &d.accounts {
            let (provider, name) = match a.name.strip_prefix("codex:") {
                Some(n) => ("Codex", n.to_string()),
                None => (if a.provider == "codex" { "Codex" } else { "Claude" }, a.name.clone()),
            };
            let label = format!("{provider} · {name}");
            let lw = text_width(f, Font::Small, &label);
            txt(&mut ops, PAD, y, right, y + 18.0, &label, Font::Small, t.text, DT_LEFT);
            txt(&mut ops, PAD + lw + 8.0, y, right, y + 18.0, &a.email, Font::Small, t.muted, DT_LEFT | DT_END_ELLIPSIS);
            y += 20.0;
            let half = (WIDTH - 2.0 * PAD - 16.0) / 2.0;
            for (i, (tag, lim)) in [("5h", &a.five_hour), ("7d", &a.seven_day)].into_iter().enumerate() {
                let x0 = PAD + i as f32 * (half + 16.0);
                let used = lim.as_ref().and_then(|l| l.used);
                let pct = used.map(|u| format!("{:.0}%", u)).unwrap_or_else(|| "--".into());
                txt(&mut ops, x0, y, x0 + 20.0, y + 14.0, tag, Font::Small, t.muted, DT_LEFT);
                let (bx0, bx1) = (x0 + 22.0, x0 + half - 38.0);
                ops.push(Op::Round { x0: bx0, y0: y + 4.0, x1: bx1, y1: y + 10.0, r: 3.0, c: t.track, clip: false });
                if let Some(u) = used {
                    let c = if u >= 90.0 { Rgb(0xe8, 0x4a, 0x4a) } else if u >= 70.0 { Rgb(0xff, 0xb9, 0x00) } else { t.accent };
                    let fill = bx0 + (bx1 - bx0) * (u.clamp(0.0, 100.0) as f32 / 100.0);
                    if fill > bx0 + 1.0 {
                        ops.push(Op::Round { x0: bx0, y0: y + 4.0, x1: fill.max(bx0 + 6.0), y1: y + 10.0, r: 3.0, c, clip: false });
                    }
                }
                txt(&mut ops, bx1 + 4.0, y, x0 + half, y + 14.0, &pct, Font::Small, t.muted, DT_RIGHT);
            }
            y += 14.0 + 12.0;
        }
        y -= 2.0;
    }

    // sessions
    divider(&mut ops, y);
    y += 1.0 + 6.0;
    let list_top = y;
    let footer_h = 1.0 + 10.0 + 32.0 + 12.0;
    let mut ly = list_top - f.scroll;
    if d.rows.is_empty() {
        let msg = if d.view.running { "No worktrees yet" } else { "Start the daemon to see your sessions" };
        ops.push(Op::Text { r: rect(PAD, ly, right, ly + 44.0), s: msg.into(), font: Font::Body, c: t.muted, flags: DT_SINGLELINE | DT_VCENTER | DT_CENTER, clip: true });
        ly += 44.0;
    }
    for (group, rows) in &d.rows {
        ops.push(Op::Text { r: rect(PAD, ly, right, ly + GROUP_H), s: group.to_uppercase(), font: Font::SmallBold, c: t.muted,
            flags: DT_SINGLELINE | DT_BOTTOM | DT_NOPREFIX, clip: true });
        ly += GROUP_H + 2.0;
        for w in rows {
            let act = Act::Row(w.id.clone());
            if hov(&act) {
                ops.push(Op::Round { x0: 8.0, y0: ly, x1: WIDTH - 8.0, y1: ly + ROW_H, r: 4.0, c: t.hover, clip: true });
            }
            let x = PAD + if w.parent.is_some() { 14.0 } else { 0.0 };
            let (cx, cy) = (x + 4.0, ly + ROW_H / 2.0);
            let c = status_color(w.status);
            if w.live || !matches!(w.status, Status::None | Status::Stopped) {
                ops.push(Op::Round { x0: cx - 4.0, y0: cy - 4.0, x1: cx + 4.0, y1: cy + 4.0, r: 4.0, c, clip: true });
            } else {
                ops.push(Op::Ring { cx, cy, r: 3.5, t: 1.3, c, clip: true });
            }
            let word = status_word(w);
            let ww = text_width(f, Font::Small, &word);
            let name = if w.kind == WorktreeKind::Delegate { "delegator".to_string() } else { w.name.clone() };
            let nc = if w.live { t.text } else { t.muted };
            ops.push(Op::Text { r: rect(x + 16.0, ly, right - ww - 12.0, ly + ROW_H), s: name, font: Font::Body, c: nc,
                flags: DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX, clip: true });
            ops.push(Op::Text { r: rect(right - ww - 4.0, ly, right, ly + ROW_H), s: word, font: Font::Small,
                c: if matches!(w.status, Status::Input | Status::Limited) { c } else { t.muted }, flags: DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX, clip: true });
            hits.push(Hit { r: rect(8.0, ly, WIDTH - 8.0, ly + ROW_H), act, in_list: true });
            ly += ROW_H;
        }
        ly += 4.0;
    }
    let list_h = ly + f.scroll - list_top + 4.0;
    let content = list_top + list_h + footer_h;
    // the visible list band, given the window height place() settled on
    let height = if f.height > 0.0 { f.height } else { content };
    let list_bottom = height - footer_h;
    f.list = (list_top, list_bottom);
    f.max_scroll = (list_h - (list_bottom - list_top)).max(0.0);

    // footer, pinned to the bottom
    let mut y = list_bottom;
    divider(&mut ops, y);
    y += 1.0 + 10.0;
    let act = Act::New;
    let w = WIDTH - 2.0 * PAD - 40.0;
    let mut accent = t.accent;
    if hov(&act) {
        accent = Rgb(accent.0.saturating_add(18), accent.1.saturating_add(18), accent.2.saturating_add(18));
    }
    let enabled = d.view.running;
    ops.push(Op::Round { x0: PAD, y0: y, x1: PAD + w, y1: y + 32.0, r: 4.0, c: if enabled { accent } else { t.button }, clip: false });
    let fg = if enabled { t.on_accent } else { t.muted };
    let label = "New session";
    let lw = 14.0 + 8.0 + text_width(f, Font::Body, label);
    let lx = PAD + (w - lw) / 2.0;
    txt(&mut ops, lx, y, lx + 14.0, y + 32.0, "\u{E710}", Font::Icon, fg, DT_CENTER);
    txt(&mut ops, lx + 22.0, y, PAD + w, y + 32.0, label, Font::Body, fg, DT_LEFT);
    if enabled {
        hits.push(Hit { r: rect(PAD, y, PAD + w, y + 32.0), act, in_list: false });
    }
    let act = Act::Gear;
    let gx = right - 32.0;
    ops.push(Op::Round { x0: gx, y0: y, x1: right, y1: y + 32.0, r: 4.0, c: if hov(&act) { t.button_hover } else { t.button }, clip: false });
    txt(&mut ops, gx, y, right, y + 32.0, "\u{E713}", Font::Icon, t.text, DT_CENTER);
    hits.push(Hit { r: rect(gx, y, right, y + 32.0), act, in_list: false });

    (ops, hits, content)
}

// --- painting ---------------------------------------------------------------------------------------

/// Blend `c` at `cov` (0..1) into a BGRA pixel.
fn blend(p: &mut u32, c: Rgb, cov: f32) {
    if cov <= 0.0 {
        return;
    }
    let mix = |d: u32, s: u8| ((s as f32) * cov + (d as f32) * (1.0 - cov)).round() as u32;
    let (b, g, r) = (*p & 0xff, (*p >> 8) & 0xff, (*p >> 16) & 0xff);
    *p = 0xff00_0000 | mix(r, c.0) << 16 | mix(g, c.1) << 8 | mix(b, c.2);
}

struct Canvas<'a> {
    px: &'a mut [u32],
    w: i32,
    h: i32,
    clip: (i32, i32),
}

impl Canvas<'_> {
    /// Antialiased rounded rectangle (device pixels), via its signed distance field.
    #[allow(clippy::too_many_arguments)]
    fn round(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: Rgb, clip: bool) {
        let (cx, cy, hx, hy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0) / 2.0, (y1 - y0) / 2.0);
        let r = r.min(hx).min(hy);
        let (ylo, yhi) = if clip { self.clip } else { (0, self.h) };
        for y in (y0.floor() as i32).max(ylo)..(y1.ceil() as i32).min(yhi) {
            for x in (x0.floor() as i32).max(0)..(x1.ceil() as i32).min(self.w) {
                let (px, py) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let (qx, qy) = (px.abs() - hx + r, py.abs() - hy + r);
                let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                blend(&mut self.px[(y * self.w + x) as usize], c, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    fn ring(&mut self, cx: f32, cy: f32, r: f32, t: f32, c: Rgb, clip: bool) {
        let (ylo, yhi) = if clip { self.clip } else { (0, self.h) };
        let e = r + t;
        for y in ((cy - e).floor() as i32).max(ylo)..((cy + e).ceil() as i32).min(yhi) {
            for x in ((cx - e).floor() as i32).max(0)..((cx + e).ceil() as i32).min(self.w) {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                blend(&mut self.px[(y * self.w + x) as usize], c, (0.5 - ((d - r).abs() - t / 2.0)).clamp(0.0, 1.0));
            }
        }
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut cr);
    let (w, h) = (cr.right.max(1), cr.bottom.max(1));
    FLY.with(|fl| {
        let mut b = fl.borrow_mut();
        let Some(f) = b.as_mut() else { return };
        let t = theme(f.dark);
        let s = f.scale;
        let (ops, hits, _) = layout(f, &t);
        f.hits = hits;

        let mem = CreateCompatibleDC(dc);
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32,
            biCompression: BI_RGB, ..std::mem::zeroed() };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        let old_bmp = SelectObject(mem, dib);
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        let bg = 0xff00_0000 | (t.bg.0 as u32) << 16 | (t.bg.1 as u32) << 8 | t.bg.2 as u32;
        px.fill(bg);
        let clip = ((f.list.0 * s).round() as i32, (f.list.1 * s).round() as i32);
        let mut cv = Canvas { px, w, h, clip };
        for op in &ops {
            match *op {
                Op::Round { x0, y0, x1, y1, r, c, clip } => cv.round(x0 * s, y0 * s, x1 * s, y1 * s, r * s, c, clip),
                Op::Ring { cx, cy, r, t, c, clip } => cv.ring(cx * s, cy * s, r * s, t * s, c, clip),
                Op::Text { .. } => {}
            }
        }
        GdiFlush();
        SetBkMode(mem, TRANSPARENT as i32);
        for op in &ops {
            if let Op::Text { r, s: ref text, font, c, flags, clip: clipped } = *op {
                let mut rr = RECT { left: (r.left as f32 * s) as i32, top: (r.top as f32 * s) as i32, right: (r.right as f32 * s) as i32, bottom: (r.bottom as f32 * s) as i32 };
                if clipped && (rr.bottom <= clip.0 || rr.top >= clip.1) {
                    continue;
                }
                let saved = SaveDC(mem);
                if clipped {
                    IntersectClipRect(mem, 0, clip.0, w, clip.1);
                }
                SelectObject(mem, font_of(f, font));
                SetTextColor(mem, c.cref());
                let wt = win::wide(text);
                DrawTextW(mem, wt.as_ptr(), (wt.len() - 1) as i32, &mut rr, flags);
                RestoreDC(mem, saved);
            }
        }
        BitBlt(dc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        DeleteObject(dib);
        DeleteDC(mem);
    });
    EndPaint(hwnd, &ps);
}

// --- input ------------------------------------------------------------------------------------------

fn hit_at(f: &Fly, x: i32, y: i32) -> Option<Act> {
    let (x, y) = (x as f32 / f.scale, y as f32 / f.scale);
    f.hits
        .iter()
        .filter(|h| !h.in_list || (y >= f.list.0 && y < f.list.1))
        .find(|h| x >= h.r.left as f32 && x < h.r.right as f32 && y >= h.r.top as f32 && y < h.r.bottom as f32)
        .map(|h| h.act.clone())
}

fn xy(lp: LPARAM) -> (i32, i32) {
    ((lp & 0xffff) as i16 as i32, ((lp >> 16) & 0xffff) as i16 as i32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_ACTIVATE => {
            // losing focus closes it, unless focus went to our own dialog (a confirm box)
            if (wp & 0xffff) as u32 == WA_INACTIVE && GetWindow(lp as HWND, GW_OWNER) != hwnd {
                hide();
            }
            0
        }
        WM_KEYDOWN if wp as u16 == VK_ESCAPE => {
            hide();
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            let changed = FLY.with(|fl| {
                let mut b = fl.borrow_mut();
                let f = b.as_mut()?;
                if !f.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    TrackMouseEvent(&mut tme);
                    f.tracking = true;
                }
                let h = hit_at(f, x, y);
                (h != f.hover).then(|| f.hover = h)
            });
            if changed.is_some() {
                InvalidateRect(hwnd, std::ptr::null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            FLY.with(|fl| {
                if let Some(f) = fl.borrow_mut().as_mut() {
                    f.tracking = false;
                    f.hover = None;
                }
            });
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xffff) as i16 as f32 / 120.0;
            FLY.with(|fl| {
                if let Some(f) = fl.borrow_mut().as_mut() {
                    f.scroll = (f.scroll - delta * ROW_H * 2.0).clamp(0.0, f.max_scroll);
                }
            });
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            let (x, y) = xy(lp);
            let act = FLY.with(|fl| fl.borrow().as_ref().and_then(|f| hit_at(f, x, y)));
            if let Some(a) = act {
                run(hwnd, a, msg == WM_RBUTTONUP);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn run(hwnd: HWND, act: Act, right_click: bool) {
    let dev = paths::dev_root();
    let def = dev.as_ref().map(|d| frontend::default_frontend(d)).unwrap_or(Frontend::VsCode);
    match act {
        Act::Restart => tray::restart_daemon(hwnd),
        Act::Daemon => tray::toggle_daemon(hwnd),
        Act::VsCode => {
            hide();
            tray::open_vscode();
        }
        Act::Row(id) => {
            let choice = if right_click { row_menu(hwnd, &id, def) } else { Some(MI_OPEN + Frontend::ALL.iter().position(|f| *f == def).unwrap_or(0)) };
            match choice {
                Some(MI_STOP) => tray::stop_session(&id),
                Some(c) if (MI_OPEN..MI_OPEN + Frontend::ALL.len()).contains(&c) => {
                    hide();
                    let fe = Frontend::ALL[c - MI_OPEN];
                    std::thread::spawn(move || tray::report(paths::dev_root().and_then(|dev| frontend::open(&dev, &id, fe)), &format!("open {id} in {}", fe.label())));
                }
                _ => {}
            }
        }
        Act::New => {
            hide();
            std::thread::spawn(move || tray::report(paths::dev_root().and_then(|dev| frontend::new_session(&dev, def)), &format!("start a session in {}", def.label())));
        }
        Act::Gear => gear_menu(hwnd, def),
    }
}

/// A native popup menu at the cursor; returns the chosen id.
unsafe fn popup(hwnd: HWND, m: HMENU) -> Option<usize> {
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    let c = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
    DestroyMenu(m);
    (c > 0).then_some(c as usize)
}

fn row_menu(hwnd: HWND, id: &str, def: Frontend) -> Option<usize> {
    let live = tray::FLEET.lock().unwrap().iter().any(|w| w.id == id && w.live);
    unsafe {
        let m = CreatePopupMenu();
        for (i, fe) in Frontend::ALL.iter().enumerate() {
            AppendMenuW(m, MF_STRING, MI_OPEN + i, win::wide(&format!("Open in {}", fe.label())).as_ptr());
            if *fe == def {
                SetMenuDefaultItem(m, (MI_OPEN + i) as u32, 0);
            }
        }
        if live {
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(m, MF_STRING, MI_STOP, win::wide("Stop session").as_ptr());
        }
        popup(hwnd, m)
    }
}

fn gear_menu(hwnd: HWND, def: Frontend) {
    let choice = unsafe {
        let m = CreatePopupMenu();
        let sub = CreatePopupMenu();
        for (i, fe) in Frontend::ALL.iter().enumerate() {
            AppendMenuW(sub, MF_STRING | if *fe == def { MF_CHECKED } else { MF_UNCHECKED }, MI_DEFAULT + i, win::wide(fe.label()).as_ptr());
        }
        AppendMenuW(m, MF_POPUP, sub as usize, win::wide("Open sessions in").as_ptr());
        AppendMenuW(m, MF_STRING | if tray::start_at_logon() { MF_CHECKED } else { MF_UNCHECKED }, MI_LOGON, win::wide("Start tray at logon").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, MF_STRING, MI_QUIT, win::wide("Quit tray").as_ptr());
        popup(hwnd, m)
    };
    match choice {
        Some(c) if (MI_DEFAULT..MI_DEFAULT + Frontend::ALL.len()).contains(&c) => {
            if let Ok(dev) = paths::dev_root() {
                let _ = frontend::set_default_frontend(&dev, Frontend::ALL[c - MI_DEFAULT]);
            }
        }
        Some(MI_LOGON) => tray::set_start_at_logon(!tray::start_at_logon()),
        Some(MI_QUIT) => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}
