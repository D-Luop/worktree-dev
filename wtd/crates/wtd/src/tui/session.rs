//! A daemon-hosted session shown inside the TUI: the attach frame protocol on its own thread, output
//! fed to a vt100 screen, keys encoded back to the bytes a terminal would send.

use std::num::NonZeroU16;
use std::sync::mpsc;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;
use wtd_core::protocol::method;

use crate::attach::{frame, F_DATA, F_EXIT, F_RESIZE};
use crate::paths;

pub enum Out {
    Data(Vec<u8>),
    Exit(i32),
    Closed(String),
}

pub struct Session {
    pub id: String,
    pub parser: vt100::Parser,
    pub exited: Option<String>,
    rx: mpsc::Receiver<Out>,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    size: (u16, u16),
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap()
}

impl Session {
    /// Attach to worktree `id`'s hosted session at `cols`×`rows`.
    pub fn attach(id: &str, cols: u16, rows: u16) -> Session {
        let (out_tx, rx) = mpsc::channel::<Out>();
        let (tx, in_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let params = json!({ "worktree": id, "cols": cols, "rows": rows });
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return drop(out_tx.send(Out::Closed(e.to_string()))),
            };
            if let Err(e) = rt.block_on(pump(params, out_tx.clone(), in_rx)) {
                let _ = out_tx.send(Out::Closed(format!("{e:#}")));
            }
        });
        Session { id: id.to_string(), parser: vt100::Parser::new(nz(rows), nz(cols), 2000), exited: None, rx, tx, size: (cols, rows) }
    }

    /// Feed queued output into the screen; true when anything changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(o) = self.rx.try_recv() {
            changed = true;
            match o {
                Out::Data(d) => self.parser.process(&d),
                Out::Exit(c) => self.exited = Some(format!("session ended (exit {c})")),
                Out::Closed(e) => self.exited = Some(if e.is_empty() { "disconnected".into() } else { e }),
            }
        }
        changed
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == self.size || cols == 0 || rows == 0 {
            return;
        }
        self.size = (cols, rows);
        self.parser.screen_mut().set_size(nz(rows), nz(cols));
        let mut p = cols.to_le_bytes().to_vec();
        p.extend_from_slice(&rows.to_le_bytes());
        let _ = self.tx.send(frame(F_RESIZE, &p));
    }

    pub fn send(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.tx.send(frame(F_DATA, bytes));
        }
    }

    pub fn paste(&self, text: &str) {
        let mut b = b"\x1b[200~".to_vec();
        b.extend_from_slice(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        b.extend_from_slice(b"\x1b[201~");
        self.send(&b);
    }
}

async fn pump(params: Value, out: mpsc::Sender<Out>, mut input: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> anyhow::Result<()> {
    let name = paths::pipe_name();
    let pipe = loop {
        match ClientOptions::new().open(&name) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(231) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(e) => anyhow::bail!("connecting to the daemon: {e}"),
        }
    };
    let (r, mut w) = tokio::io::split(pipe);
    let mut rd = BufReader::new(r);
    let mut req = serde_json::to_vec(&json!({ "id": 1, "method": method::SESSION_ATTACH, "params": params }))?;
    req.push(b'\n');
    w.write_all(&req).await?;
    let mut line = String::new();
    rd.read_line(&mut line).await?;
    let resp: Value = serde_json::from_str(line.trim())?;
    if let Some(e) = resp.get("error").and_then(Value::as_str) {
        anyhow::bail!("{e}");
    }
    let writer = tokio::spawn(async move {
        while let Some(f) = input.recv().await {
            if w.write_all(&f).await.is_err() {
                break;
            }
        }
    });
    let mut hdr = [0u8; 5];
    loop {
        if rd.read_exact(&mut hdr).await.is_err() {
            let _ = out.send(Out::Closed("the daemon stopped".into()));
            break;
        }
        let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut payload = vec![0u8; len];
        if rd.read_exact(&mut payload).await.is_err() {
            break;
        }
        let sent = match hdr[0] {
            F_DATA => out.send(Out::Data(payload)),
            F_EXIT => {
                let c = payload.get(..4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
                let _ = out.send(Out::Exit(c));
                break;
            }
            _ => Ok(()),
        };
        if sent.is_err() {
            break; // the TUI dropped this session (switched or quit): detach
        }
    }
    writer.abort();
    Ok(())
}

/// The bytes a terminal sends for a key (None: not forwarded).
pub fn key_bytes(k: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    let csi = |s: &str| format!("\x1b[{s}").into_bytes();
    let mut b = match k.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            c @ 'a'..='z' => vec![c as u8 & 0x1f],
            ' ' | '@' | '2' => vec![0],
            '[' | '3' => vec![0x1b],
            '\\' | '4' => vec![0x1c],
            '^' | '6' => vec![0x1e],
            '_' | '-' | '7' => vec![0x1f],
            _ => return None,
        },
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter if shift || alt => b"\x1b\r".to_vec(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => csi("Z"),
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi("A"),
        KeyCode::Down => csi("B"),
        KeyCode::Right => csi("C"),
        KeyCode::Left => csi("D"),
        KeyCode::Home => csi("H"),
        KeyCode::End => csi("F"),
        KeyCode::PageUp => csi("5~"),
        KeyCode::PageDown => csi("6~"),
        KeyCode::Insert => csi("2~"),
        KeyCode::Delete => csi("3~"),
        KeyCode::F(n @ 1..=4) => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
        KeyCode::F(n @ 5..=11) => csi(&format!("{}~", [15, 17, 18, 19, 20, 21, 23][(n - 5) as usize])),
        _ => return None,
    };
    if alt && matches!(k.code, KeyCode::Char(_)) {
        b.insert(0, 0x1b);
    }
    Some(b)
}

fn color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Paints the session's screen into a ratatui area (sized to it via `Session::resize`).
pub struct Screen<'a>(pub &'a vt100::Screen);

impl Widget for Screen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.0.size();
        let (rows, cols) = (rows.get(), cols.get());
        for y in 0..area.height.min(rows) {
            for x in 0..area.width.min(cols) {
                let Some(cell) = self.0.cell(y, x) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut st = Style::default();
                if let Some(c) = color(cell.fgcolor()) {
                    st = st.fg(c);
                }
                if let Some(c) = color(cell.bgcolor()) {
                    st = st.bg(c);
                }
                let mut m = Modifier::empty();
                if cell.bold() { m |= Modifier::BOLD; }
                if cell.dim() { m |= Modifier::DIM; }
                if cell.italic() { m |= Modifier::ITALIC; }
                if cell.underline() { m |= Modifier::UNDERLINED; }
                if cell.inverse() { m |= Modifier::REVERSED; }
                st = st.add_modifier(m);
                if let Some(out) = buf.cell_mut((area.x + x, area.y + y)) {
                    let s = cell.contents();
                    out.set_symbol(if s.is_empty() { " " } else { s }).set_style(st);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> Vec<u8> {
        key_bytes(&KeyEvent::new(code, m)).unwrap()
    }

    #[test]
    fn keys_encode_like_a_terminal() {
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(k(KeyCode::Char('é'), KeyModifiers::NONE), "é".as_bytes());
        assert_eq!(k(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(k(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(k(KeyCode::F(2), KeyModifiers::NONE), b"\x1bOQ");
    }
}
