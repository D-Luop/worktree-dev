//! `wtd tui`: the fleet in a terminal — roster, the selected worktree's live session (attached to the
//! daemon-hosted pty), and its Changes (branch commits + working changes, inline or side-by-side).
//! For terminal-only setups and editors without a WorkTreeDev extension (Zed, …).

mod diff;
mod session;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use wtd_core::model::{Worktree, WorktreeKind};
use wtd_core::protocol::{method, Push, ServerLine};
use wtd_core::status::Status;

use crate::changes::{self, FileDiff, Summary};
use crate::client::Client;
use crate::{new, paths, repos, win};
use session::Session;

const ACCENT: Color = Color::Rgb(0x4a, 0xa3, 0xff);

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Roster,
    Main,
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Session,
    Changes,
}

enum Form {
    Repo { repos: Vec<String>, sel: usize },
    Branch { slug: String, text: String, err: Option<&'static str> },
    ConfirmStop(String),
}

struct ChangesView {
    id: String,
    wt: PathBuf,
    summary: Summary,
    /// 0 = working changes, i = commits[i-1]
    sel: usize,
    files: Vec<FileDiff>,
    shown: Option<usize>,
    scroll: (u16, u16),
    focus_diff: bool,
    loaded: Instant,
}

enum Bg {
    /// a detached `wtd agent` finished: attach to `id` (or report its error)
    Started { id: String, err: Option<String> },
}

struct App {
    dev: PathBuf,
    fleet: Arc<Mutex<(bool, BTreeMap<String, Worktree>, u64)>>,
    roster: ListState,
    focus: Focus,
    view: View,
    session: Option<Session>,
    changes: Option<ChangesView>,
    side_by_side: bool,
    form: Option<Form>,
    status: String,
    bg_tx: mpsc::Sender<Bg>,
    bg_rx: mpsc::Receiver<Bg>,
    main_area: Rect,
    /// `wtd tui <id>`: open this worktree's session once the fleet has loaded
    pending: Option<String>,
    quit: bool,
}

/// `wtd tui [<slug>/<name> | --new]`
pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    if Client::connect()?.is_none() {
        bail!("the WorkTreeDev daemon isn't running — start it (`wtd daemon start` or the tray) and retry");
    }
    let fleet = Arc::new(Mutex::new((false, BTreeMap::new(), 0)));
    let f2 = fleet.clone();
    std::thread::spawn(move || watch(f2));
    let (bg_tx, bg_rx) = mpsc::channel();
    let mut app = App {
        dev, fleet, roster: ListState::default().with_selected(Some(0)), focus: Focus::Roster, view: View::Session, session: None,
        changes: None, side_by_side: true, form: None, status: String::new(), bg_tx, bg_rx, main_area: Rect::default(),
        pending: args.first().filter(|a| !a.starts_with("--")).cloned(), quit: false,
    };
    if args.iter().any(|a| a == "--new") {
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    }
    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let r = app.run(&mut term);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    r.map(|_| 0)
}

/// Mirror the daemon's worktrees (like the tray's watcher).
fn watch(fleet: Arc<Mutex<(bool, BTreeMap<String, Worktree>, u64)>>) {
    loop {
        if let Ok(Some(mut c)) = Client::connect() {
            if c.hello("tui").is_ok() && c.request(method::SUBSCRIBE, serde_json::json!({})).is_ok() {
                loop {
                    match c.read() {
                        Ok(Some(ServerLine::Push(p))) => {
                            let mut f = fleet.lock().unwrap();
                            f.0 = true;
                            f.2 += 1;
                            match p {
                                Push::Snapshot { snapshot, .. } => f.1 = snapshot.worktrees.into_iter().map(|w| (w.id.clone(), w)).collect(),
                                Push::Upsert { worktree, .. } => { f.1.insert(worktree.id.clone(), worktree); }
                                Push::Remove { id, .. } => { f.1.remove(&id); }
                                Push::Shutdown => break,
                                _ => {}
                            }
                        }
                        Ok(Some(_)) => continue,
                        _ => break,
                    }
                }
            }
        }
        {
            let mut f = fleet.lock().unwrap();
            f.0 = false;
            f.2 += 1;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn rank(s: Status) -> u8 {
    match s {
        Status::Input | Status::Limited => 0,
        Status::Working => 1,
        Status::Reviewing => 2,
        Status::Pr => 3,
        Status::None | Status::Stopped => 4,
        Status::Done => 5,
    }
}

fn glyph(w: &Worktree) -> (&'static str, Color) {
    match w.status {
        Status::Input => ("●", Color::Yellow),
        Status::Working => ("◐", ACCENT),
        Status::Limited => ("⚠", Color::Rgb(0xff, 0x8c, 0x00)),
        Status::Reviewing => ("◆", Color::Magenta),
        Status::Pr => ("◇", Color::Cyan),
        Status::Done => ("✓", Color::Green),
        Status::Stopped => ("■", Color::Red),
        Status::None => (if w.live { "○" } else { "·" }, Color::DarkGray),
    }
}

impl App {
    fn rows(&self) -> Vec<Worktree> {
        let mut v: Vec<Worktree> = self.fleet.lock().unwrap().1.values().filter(|w| w.kind != WorktreeKind::Dev).cloned().collect();
        v.sort_by(|a, b| (rank(a.status), !a.live, &a.id).cmp(&(rank(b.status), !b.live, &b.id)));
        wtd_core::model::nest_under_parents(v)
    }

    fn selected(&self) -> Option<Worktree> {
        self.rows().get(self.roster.selected().unwrap_or(0)).cloned()
    }

    fn run(&mut self, term: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut last_tick = Instant::now();
        let (mut dirty, mut seen, mut drawn) = (true, 0u64, Instant::now());
        while !self.quit {
            if let Some(s) = &mut self.session {
                dirty |= s.drain();
            }
            let v = self.fleet.lock().unwrap().2;
            if v != seen {
                (seen, dirty) = (v, true);
            }
            if let Some(id) = self.pending.clone() {
                if let Some(i) = self.rows().iter().position(|w| w.id == id) {
                    self.pending = None;
                    self.roster.select(Some(i));
                    let w = self.rows()[i].clone();
                    self.open(&w);
                }
            }
            while let Ok(Bg::Started { id, err }) = self.bg_rx.try_recv() {
                dirty = true;
                match err {
                    Some(e) => self.status = format!("couldn't start {id}: {e}"),
                    None => self.attach(&id),
                }
            }
            if self.view == View::Changes && last_tick.elapsed() > Duration::from_secs(3) {
                self.reload_changes(false);
                last_tick = Instant::now();
                dirty = true;
            }
            if dirty || drawn.elapsed() > Duration::from_secs(1) {
                term.draw(|f| self.draw(f))?;
                (dirty, drawn) = (false, Instant::now());
            }
            if event::poll(Duration::from_millis(30))? {
                dirty = true;
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
                    Event::Paste(t) => match (&self.session, self.focus, self.view) {
                        (Some(s), Focus::Main, View::Session) => s.paste(&t),
                        _ => {
                            if let Some(Form::Branch { text, .. }) = &mut self.form {
                                text.push_str(t.trim());
                            }
                        }
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    }

    // ---- actions ----------------------------------------------------------------------------------

    fn attach(&mut self, id: &str) {
        let a = self.main_area;
        self.session = Some(Session::attach(id, a.width.saturating_sub(2).max(20), a.height.saturating_sub(2).max(5)));
        self.view = View::Session;
        self.focus = Focus::Main;
        self.status = format!("attached to {id} — Ctrl+] (or F12) returns to the roster");
    }

    /// Show `w`'s session: attach when the daemon hosts it, else start it (detached) and then attach.
    fn open(&mut self, w: &Worktree) {
        if self.session.as_ref().is_some_and(|s| s.id == w.id && s.exited.is_none()) {
            self.view = View::Session;
            self.focus = Focus::Main;
            return;
        }
        if w.live && !w.hosted {
            self.status = format!("{} runs in another terminal (not hosted by the daemon) — open it there", w.id);
            return;
        }
        if w.live {
            return self.attach(&w.id);
        }
        self.start(&w.slug, &w.name);
    }

    fn start(&mut self, slug: &str, name: &str) {
        let id = format!("{slug}/{name}");
        self.status = format!("starting {id}…");
        let (tx, dev, slug, name) = (self.bg_tx.clone(), self.dev.clone(), slug.to_string(), name.to_string());
        std::thread::spawn(move || {
            let out = win::no_window(Command::new(paths::wtd_exe()).args(["agent", &slug, &name]))
                .current_dir(&dev).env("WTD_DETACH", "1").stdin(Stdio::null()).output();
            let err = match out {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("failed").to_string()),
                Err(e) => Some(e.to_string()),
            };
            let _ = tx.send(Bg::Started { id, err });
        });
    }

    fn show_changes(&mut self, w: &Worktree) {
        let wt = PathBuf::from(&w.path);
        self.changes = Some(ChangesView { id: w.id.clone(), summary: changes::summary(&wt), wt, sel: 0, files: vec![], shown: None, scroll: (0, 0), focus_diff: false, loaded: Instant::now() });
        self.view = View::Changes;
        self.reload_changes(true);
    }

    fn reload_changes(&mut self, force: bool) {
        let Some(c) = &mut self.changes else { return };
        if !force && c.loaded.elapsed() < Duration::from_secs(2) {
            return;
        }
        if !force {
            c.summary = changes::summary(&c.wt);
            c.sel = c.sel.min(c.summary.commits.len());
        }
        if force || c.sel == 0 || c.shown != Some(c.sel) {
            c.files = if c.sel == 0 { changes::working(&c.wt) } else { changes::commit(&c.wt, &c.summary.commits[c.sel - 1].sha) };
            if c.shown != Some(c.sel) {
                c.scroll = (0, 0);
            }
            c.shown = Some(c.sel);
        }
        c.loaded = Instant::now();
    }

    // ---- keys -------------------------------------------------------------------------------------

    fn key(&mut self, k: KeyEvent) {
        if self.form.is_some() {
            return self.form_key(k);
        }
        let leave = matches!(k.code, KeyCode::F(12)) || (k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5')));
        if self.focus == Focus::Main && self.view == View::Session {
            if leave {
                self.focus = Focus::Roster;
                return;
            }
            if let Some(s) = &self.session {
                if s.exited.is_none() {
                    if let Some(b) = session::key_bytes(&k) {
                        s.send(&b);
                    }
                    return;
                }
            }
        }
        if self.focus == Focus::Main && self.view == View::Changes {
            if self.changes_key(&k) {
                return;
            }
        }
        let n = self.rows().len();
        let sel = self.roster.selected().unwrap_or(0);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Roster => self.roster.select(Some(sel.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Roster => self.roster.select(Some((sel + 1).min(n.saturating_sub(1)))),
            KeyCode::Enter | KeyCode::Char('s') => {
                if let Some(w) = self.selected() {
                    self.open(&w);
                }
            }
            KeyCode::Char('d') => {
                if let Some(w) = self.selected() {
                    self.show_changes(&w);
                    self.focus = Focus::Main;
                }
            }
            KeyCode::Tab => {
                self.focus = if self.focus == Focus::Roster { Focus::Main } else { Focus::Roster };
            }
            KeyCode::Char('n') => {
                let repos: Vec<String> = repos::all(&self.dev).into_iter().map(|r| r.slug).collect();
                self.form = Some(if repos.is_empty() { Form::Branch { slug: String::new(), text: String::new(), err: Some("no repos — add one with `wtd repo add`") } } else { Form::Repo { repos, sel: 0 } });
            }
            KeyCode::Char('x') => {
                if let Some(w) = self.selected().filter(|w| w.live) {
                    self.form = Some(Form::ConfirmStop(w.id));
                }
            }
            KeyCode::Char('v') => self.side_by_side = !self.side_by_side,
            _ => {}
        }
    }

    /// Changes-pane keys; false = not handled here (falls through to global keys).
    fn changes_key(&mut self, k: &KeyEvent) -> bool {
        let Some(c) = &mut self.changes else { return false };
        let page = self.main_area.height.saturating_sub(4);
        match k.code {
            KeyCode::Esc if c.focus_diff => c.focus_diff = false,
            KeyCode::Esc => self.focus = Focus::Roster,
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') if !c.focus_diff => c.focus_diff = true,
            KeyCode::Left | KeyCode::Char('h') if c.focus_diff && c.scroll.1 == 0 => c.focus_diff = false,
            KeyCode::Left | KeyCode::Char('h') if c.focus_diff => c.scroll.1 = c.scroll.1.saturating_sub(8),
            KeyCode::Right | KeyCode::Char('l') => c.scroll.1 += 8,
            KeyCode::Up | KeyCode::Char('k') if !c.focus_diff => {
                c.sel = c.sel.saturating_sub(1);
                self.reload_changes(true);
            }
            KeyCode::Down | KeyCode::Char('j') if !c.focus_diff => {
                c.sel = (c.sel + 1).min(c.summary.commits.len());
                self.reload_changes(true);
            }
            KeyCode::Up | KeyCode::Char('k') => c.scroll.0 = c.scroll.0.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => c.scroll.0 += 1,
            KeyCode::PageUp => c.scroll.0 = c.scroll.0.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => c.scroll.0 += page,
            KeyCode::Home | KeyCode::Char('g') => c.scroll = (0, 0),
            _ => return false,
        }
        true
    }

    fn form_key(&mut self, k: KeyEvent) {
        let Some(form) = self.form.take() else { return };
        self.form = match form {
            Form::Repo { repos, sel } => match k.code {
                KeyCode::Esc => None,
                KeyCode::Up => Some(Form::Repo { sel: sel.saturating_sub(1), repos }),
                KeyCode::Down => Some(Form::Repo { sel: (sel + 1).min(repos.len() - 1), repos }),
                KeyCode::Enter => Some(Form::Branch { slug: repos[sel].clone(), text: String::new(), err: None }),
                _ => Some(Form::Repo { repos, sel }),
            },
            Form::Branch { slug, mut text, .. } => match k.code {
                KeyCode::Esc => None,
                KeyCode::Backspace => {
                    text.pop();
                    Some(Form::Branch { slug, text, err: None })
                }
                KeyCode::Char(ch) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    text.push(ch);
                    Some(Form::Branch { slug, text, err: None })
                }
                KeyCode::Enter if !slug.is_empty() => match new::branch_error(text.trim()) {
                    Some(e) => Some(Form::Branch { slug, text, err: Some(e) }),
                    None => {
                        self.start(&slug, text.trim());
                        None
                    }
                },
                _ => Some(Form::Branch { slug, text, err: None }),
            },
            Form::ConfirmStop(id) => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    let _ = win::no_window(Command::new(paths::wtd_exe()).args(["stop", &id])).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
                    self.status = format!("stopped {id}");
                    if self.session.as_ref().is_some_and(|s| s.id == id) {
                        self.session = None;
                    }
                }
                None
            }
        };
    }

    // ---- drawing ----------------------------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [body, foot] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
        let rw = (body.width / 4).clamp(24, 40);
        let [left, main] = Layout::horizontal([Constraint::Length(rw), Constraint::Min(20)]).areas(body);
        self.main_area = main;
        self.draw_roster(f, left);
        match self.view {
            View::Session => self.draw_session(f, main),
            View::Changes => self.draw_changes(f, main),
        }
        let keys = match (self.focus, self.view) {
            (Focus::Main, View::Session) => "Ctrl+] / F12 roster",
            (Focus::Main, View::Changes) => "↑↓ commits · →/Enter diff · PgUp/PgDn · v side-by-side · Esc back",
            _ => "↑↓ select · Enter session · d changes · n new · x stop · Tab focus · v diff layout · q quit",
        };
        let line = Line::from(vec![Span::styled(format!(" {keys} "), Style::default().fg(Color::Black).bg(ACCENT)), Span::raw(" "), Span::styled(self.status.clone(), Style::default().fg(Color::Gray))]);
        f.render_widget(Paragraph::new(line), foot);
        if let Some(form) = &self.form {
            draw_form(f, form);
        }
    }

    fn block(&self, title: Line<'static>, focused: bool) -> Block<'static> {
        Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(title)
            .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
    }

    fn draw_roster(&mut self, f: &mut Frame, area: Rect) {
        let (up, _) = { let g = self.fleet.lock().unwrap(); (g.0, ()) };
        let rows = self.rows();
        if self.roster.selected().unwrap_or(0) >= rows.len() && !rows.is_empty() {
            self.roster.select(Some(rows.len() - 1));
        }
        let items: Vec<ListItem> = rows.iter().map(|w| {
            let (g, col) = glyph(w);
            let attached = self.session.as_ref().is_some_and(|s| s.id == w.id);
            let indent = if w.parent.is_some() { "  " } else { "" };
            ListItem::new(vec![
                Line::from(vec![Span::raw(indent), Span::styled(format!("{g} "), Style::default().fg(col)), Span::styled(w.name.clone(), Style::default().add_modifier(if attached { Modifier::BOLD } else { Modifier::empty() }))]),
                Line::from(Span::styled(format!("{indent}  {}{}{}{}", w.slug, if w.git.dirty { " ●" } else { "" }, if w.git.ahead > 0 { format!(" ↑{}", w.git.ahead) } else { String::new() },
                    if w.status == Status::Limited { format!(" · {}", crate::limit::label_now(w.limit_resets_at)) } else { String::new() }), Style::default().fg(Color::DarkGray))),
            ])
        }).collect();
        let title = Line::from(vec![Span::styled(" WorkTreeDev ", Style::default().add_modifier(Modifier::BOLD)), Span::styled(if up { "" } else { "daemon down " }, Style::default().fg(Color::Red))]);
        let list = List::new(items).block(self.block(title, self.focus == Focus::Roster))
            .highlight_style(Style::default().bg(Color::Rgb(0x26, 0x33, 0x45)));
        f.render_stateful_widget(list, area, &mut self.roster);
    }

    fn tabs(&self, id: &str) -> Line<'static> {
        let t = |label: &str, on: bool| Span::styled(format!(" {label} "), if on { Style::default().fg(Color::Black).bg(ACCENT) } else { Style::default().fg(Color::Gray) });
        Line::from(vec![t("Session", self.view == View::Session), Span::raw(" "), t("Changes", self.view == View::Changes), Span::styled(format!("  {id} "), Style::default().fg(Color::DarkGray))])
    }

    fn draw_session(&mut self, f: &mut Frame, area: Rect) {
        let id = self.session.as_ref().map(|s| s.id.clone()).unwrap_or_default();
        let block = self.block(self.tabs(&id), self.focus == Focus::Main);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(s) = &mut self.session else {
            let hint = match self.selected() {
                Some(w) if w.live => format!("Enter attaches to {}'s session.", w.id),
                Some(w) => format!("Enter starts {}'s session.", w.id),
                None => "No worktrees yet — n starts a new session.".into(),
            };
            f.render_widget(Paragraph::new(hint).style(Style::default().fg(Color::Gray)), inner);
            return;
        };
        s.resize(inner.width, inner.height);
        f.render_widget(session::Screen(s.parser.screen()), inner);
        if let Some(e) = &s.exited {
            let r = Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner };
            f.render_widget(Paragraph::new(format!(" {e} — Enter reopens ")).style(Style::default().fg(Color::Black).bg(Color::Yellow)), r);
        } else if self.focus == Focus::Main && !s.parser.screen().hide_cursor() {
            let (y, x) = s.parser.screen().cursor_position();
            f.set_cursor_position((inner.x + x, inner.y + y));
        }
    }

    fn draw_changes(&mut self, f: &mut Frame, area: Rect) {
        let id = self.changes.as_ref().map(|c| c.id.clone()).unwrap_or_default();
        let block = self.block(self.tabs(&id), self.focus == Focus::Main);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let side = self.side_by_side;
        let focused = self.focus == Focus::Main;
        let Some(c) = &mut self.changes else {
            f.render_widget(Paragraph::new("d shows the selected worktree's changes.").style(Style::default().fg(Color::Gray)), inner);
            return;
        };
        let lw = (inner.width / 3).clamp(24, 48);
        let [list, diff] = Layout::horizontal([Constraint::Length(lw), Constraint::Min(10)]).areas(inner);
        let mut items = vec![ListItem::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("Working changes ({})", c.summary.changes)),
        ]))];
        items.extend(c.summary.commits.iter().map(|m| ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", m.short), Style::default().fg(Color::Yellow)),
            Span::raw(m.subject.clone()),
        ]))));
        let head = format!(" {} {} ", c.summary.branch, if c.summary.fallback { "· recent".to_string() } else if c.summary.base.is_empty() { String::new() } else { format!("vs {}", c.summary.base) });
        let mut st = ListState::default().with_selected(Some(c.sel));
        f.render_stateful_widget(List::new(items)
            .block(Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(Color::DarkGray)).title(Span::styled(head, Style::default().fg(Color::Gray))))
            .highlight_style(Style::default().bg(if focused && !c.focus_diff { Color::Rgb(0x26, 0x33, 0x45) } else { Color::Rgb(0x22, 0x24, 0x28) })), list, &mut st);
        let rows = diff::render(&c.files, if side { Some(diff.width.saturating_sub(1)) } else { None });
        let max = (rows.len() as u16).saturating_sub(diff.height);
        c.scroll.0 = c.scroll.0.min(max);
        let title = if c.sel == 0 { "working tree".to_string() } else { c.summary.commits[c.sel - 1].subject.clone() };
        f.render_widget(Paragraph::new(rows).scroll(c.scroll).block(Block::default().title(Span::styled(format!(" {title} "), Style::default().fg(if c.focus_diff && focused { ACCENT } else { Color::Gray })))), diff);
    }
}

fn draw_form(f: &mut Frame, form: &Form) {
    let a = f.area();
    let w = 56.min(a.width.saturating_sub(4));
    let h = match form { Form::Repo { repos, .. } => (repos.len() as u16 + 4).min(a.height.saturating_sub(4)), _ => 6 };
    let r = Rect { x: a.x + (a.width - w) / 2, y: a.y + (a.height.saturating_sub(h)) / 3, width: w, height: h };
    f.render_widget(Clear, r);
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(ACCENT));
    match form {
        Form::Repo { repos, sel } => {
            let items: Vec<ListItem> = repos.iter().map(|s| ListItem::new(format!("  {s}"))).collect();
            let mut st = ListState::default().with_selected(Some(*sel));
            f.render_stateful_widget(List::new(items).block(block.title(" New session — repository ")).highlight_style(Style::default().bg(Color::Rgb(0x26, 0x33, 0x45))), r, &mut st);
        }
        Form::Branch { slug, text, err } => {
            let lines = vec![
                Line::from(vec![Span::raw(" branch: "), Span::styled(format!("{text}▏"), Style::default().add_modifier(Modifier::BOLD))]),
                Line::styled(format!(" {}", err.unwrap_or("e.g. feat/thing · Enter starts · Esc cancels")), Style::default().fg(if err.is_some() { Color::Red } else { Color::DarkGray })),
            ];
            f.render_widget(Paragraph::new(lines).block(block.title(format!(" New session in {slug} "))), r);
        }
        Form::ConfirmStop(id) => {
            f.render_widget(Paragraph::new(format!(" End {id}'s session? y / n")).block(block.title(" Stop session ")), r);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app() -> App {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("..");
        let w = Worktree { id: "wd/feat/x".into(), slug: "wd".into(), name: "feat/x".into(), path: repo.to_string_lossy().into(), status: Status::Input, ..Default::default() };
        let (bg_tx, bg_rx) = mpsc::channel();
        App {
            dev: repo, fleet: Arc::new(Mutex::new((true, [(w.id.clone(), w)].into_iter().collect(), 0))), roster: ListState::default().with_selected(Some(0)),
            focus: Focus::Roster, view: View::Session, session: None, changes: None, side_by_side: true, form: None, status: String::new(),
            bg_tx, bg_rx, main_area: Rect::default(), pending: None, quit: false,
        }
    }

    fn text(t: &Terminal<TestBackend>) -> String {
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn renders_roster_session_hint_changes_and_forms() {
        let mut a = app();
        let mut t = Terminal::new(TestBackend::new(140, 36)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        let s = text(&t);
        assert!(s.contains("WorkTreeDev") && s.contains("feat/x") && s.contains("Enter starts wd/feat/x"));
        a.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        t.draw(|f| a.draw(f)).unwrap();
        assert!(text(&t).contains("Working changes"));
        for (code, layout) in [(KeyCode::Char('v'), "inline"), (KeyCode::Down, "commit"), (KeyCode::Enter, "diff"), (KeyCode::PageDown, "page")] {
            a.key(KeyEvent::new(code, KeyModifiers::NONE));
            t.draw(|f| a.draw(f)).unwrap_or_else(|e| panic!("{layout}: {e}"));
        }
        a.focus = Focus::Roster;
        a.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        t.draw(|f| a.draw(f)).unwrap();
        assert!(text(&t).contains("New session"));
        let mut tiny = Terminal::new(TestBackend::new(30, 8)).unwrap();
        tiny.draw(|f| a.draw(f)).unwrap();
    }
}
