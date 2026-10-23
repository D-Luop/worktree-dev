//! Terminal side of daemon-hosted sessions (Phase 3).
//!
//!   wtd host [--kind k] [--account a] -- <program> [args…]   open-or-attach this worktree's session
//!   wtd attach [<worktree id>]                              attach to a running one (default: cwd)
//!
//! Puts the console in raw VT mode and relays bytes both ways over one overlapped pipe connection.
//! Closing the terminal only detaches: the agent keeps running in the daemon. If the daemon isn't
//! running, `wtd host` falls back to `wtd run` (the session then lives in this terminal).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;
use windows_sys::Win32::Foundation::{FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleCP, SetConsoleMode,
    SetConsoleOutputCP, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WRAP_AT_EOL_OUTPUT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use wtd_core::protocol::method;

use crate::client::Client;
use crate::{paths, win};

pub(crate) const F_DATA: u8 = 0;
pub(crate) const F_RESIZE: u8 = 1;
pub(crate) const F_EXIT: u8 = 2;

pub(crate) fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(t);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn std_handle(id: u32) -> Option<HANDLE> {
    let h = unsafe { GetStdHandle(id) };
    (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(h)
}

pub fn console_size() -> Option<(u16, u16)> {
    let out = std_handle(STD_OUTPUT_HANDLE)?;
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(out, &mut info) } == FALSE {
        return None;
    }
    let w = info.srWindow;
    Some(((w.Right - w.Left + 1).max(20) as u16, (w.Bottom - w.Top + 1).max(5) as u16))
}

/// Raw VT console for the relay's lifetime; the original modes and code pages come back on drop.
struct RawConsole {
    input: Option<(HANDLE, u32)>,
    output: Option<(HANDLE, u32)>,
    cp: (u32, u32),
}

impl RawConsole {
    fn enter() -> RawConsole {
        unsafe {
            let cp = (GetConsoleCP(), GetConsoleOutputCP());
            SetConsoleCP(65001);
            SetConsoleOutputCP(65001);
            let mut input = None;
            if let Some(h) = std_handle(STD_INPUT_HANDLE) {
                let mut m = 0;
                if GetConsoleMode(h, &mut m) != FALSE {
                    // no line editing / echo / Ctrl+C processing: keys arrive as VT bytes (Ctrl+C = 0x03)
                    SetConsoleMode(h, ENABLE_VIRTUAL_TERMINAL_INPUT);
                    input = Some((h, m));
                }
            }
            let mut output = None;
            if let Some(h) = std_handle(STD_OUTPUT_HANDLE) {
                let mut m = 0;
                if GetConsoleMode(h, &mut m) != FALSE {
                    SetConsoleMode(h, ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN);
                    output = Some((h, m));
                }
            }
            RawConsole { input, output, cp }
        }
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        unsafe {
            if let Some((h, m)) = self.input {
                SetConsoleMode(h, m);
            }
            if let Some((h, m)) = self.output {
                SetConsoleMode(h, m);
            }
            SetConsoleCP(self.cp.0);
            SetConsoleOutputCP(self.cp.1);
        }
    }
}

/// Byte-exact console write (Rust's stdout rejects UTF-8 sequences split across chunks).
fn write_out(data: &[u8]) {
    let Some(h) = std_handle(STD_OUTPUT_HANDLE) else { return };
    let mut off = 0;
    while off < data.len() {
        let mut n = 0u32;
        if unsafe { WriteFile(h, data[off..].as_ptr(), (data.len() - off) as u32, &mut n, std::ptr::null_mut()) } == FALSE || n == 0 {
            return;
        }
        off += n as usize;
    }
}

pub fn host_main(args: &[String]) -> Result<i32> {
    // WTD_DETACH: start (or find) the hosted session and return; a frontend attaches on its own
    if std::env::var_os("WTD_DETACH").is_some() {
        host_detached(args)?;
        return Ok(0);
    }
    if Client::connect()?.is_none() {
        return crate::run::main(args); // no daemon: the session lives in this terminal, as before
    }
    relay(method::SESSION_SPAWN, spawn_params(args)?)
}

/// Start (or find) the session in the daemon without attaching: it runs in the background until
/// someone opens it from the roster.
pub fn host_detached(args: &[String]) -> Result<Value> {
    if Client::connect()?.is_none() {
        bail!("the wtd daemon isn't running; start it to launch a background session");
    }
    spawn_detached(spawn_params(args)?)
}

/// Start (or find) a session from ready-made `session.spawn` params without attaching.
pub fn spawn_detached(params: Value) -> Result<Value> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let (mut rd, _w) = request(method::SESSION_SPAWN, params).await?;
        let mut line = String::new();
        rd.read_line(&mut line).await?;
        let resp: Value = serde_json::from_str(line.trim()).context("bad response from daemon")?;
        match resp.get("error").and_then(Value::as_str) {
            Some(e) => bail!("{e}"),
            None => Ok(resp["result"].clone()),
        }
    })
}

/// Open-or-attach a session from ready-made `session.spawn` params (a WSL session builds its own).
pub fn host_prepared(params: Value) -> Result<i32> {
    relay(method::SESSION_SPAWN, params)
}

fn spawn_params(args: &[String]) -> Result<Value> {
    let mut kind = "agent".to_string();
    let mut account = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => { kind = args.get(i + 1).cloned().context("--kind needs a value")?; i += 2; }
            "--account" => { account = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--" => { i += 1; break; }
            _ => break,
        }
    }
    let Some(program) = args.get(i) else { bail!("usage: wtd host [--kind k] [--account a] -- <program> [args…]") };
    let (exe, prefix) = crate::run::command_for(program);
    let stem = std::path::Path::new(program).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let cwd = std::env::current_dir()?;
    if stem == "codex" {
        if let Some(home) = crate::codex::home() {
            if let Err(e) = crate::codex::prepare(&home, &cwd) {
                eprintln!("wtd host: preparing Codex ({}): {e:#}", home.display());
            }
        }
    }
    let mut argv: Vec<String> = prefix.iter().map(|p| p.to_string_lossy().to_string()).collect();
    argv.extend(args[i + 1..].iter().cloned());
    // the hosted process gets this terminal's environment (CLAUDE_CONFIG_DIR / CODEX_HOME, PATH, …),
    // minus any Claude session this was run from
    let env = crate::session_env::current();
    let (cols, rows) = console_size().unwrap_or((120, 30));
    Ok(json!({
        "dir": cwd.to_string_lossy(), "kind": kind, "account": account, "program": stem,
        "exe": exe.to_string_lossy(), "args": argv, "env": env, "cols": cols, "rows": rows,
    }))
}

pub fn attach_main(args: &[String]) -> Result<i32> {
    let (cols, rows) = console_size().unwrap_or((120, 30));
    let target = match args.first() {
        Some(id) if !id.contains(['\\', ':']) && !id.starts_with('/') => json!({ "worktree": id }),
        Some(dir) => json!({ "dir": paths::normalize(dir).to_string_lossy() }),
        None => json!({ "dir": std::env::current_dir()?.to_string_lossy() }),
    };
    let mut params = target;
    params["cols"] = json!(cols);
    params["rows"] = json!(rows);
    relay(method::SESSION_ATTACH, params)
}

const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Dropping image files onto the terminal types their (quoted) paths. Copy them to the space-free
/// staging dir the clipboard-image paste uses and send those paths instead, so the agent loads them.
/// `None` (pass the input through) unless the input is nothing but existing image files.
fn stage_dropped_images(input: &[u8], dir: &Path) -> Option<Vec<u8>> {
    let body = input.strip_prefix(b"\x1b[200~").and_then(|b| b.strip_suffix(b"\x1b[201~")).unwrap_or(input);
    let text = std::str::from_utf8(body).ok()?;
    let lower = text.to_ascii_lowercase();
    if !IMAGE_EXTS.iter().any(|e| lower.contains(&format!(".{e}"))) {
        return None;
    }
    let files = split_dropped_paths(text)?;
    let is_image = |f: &Path| f.extension().is_some_and(|e| IMAGE_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()));
    if !files.iter().map(Path::new).all(|f| f.is_absolute() && is_image(f) && f.is_file()) {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis();
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        let ext = Path::new(f).extension()?.to_string_lossy();
        let name = if i == 0 { format!("img-{ms}.{ext}") } else { format!("img-{ms}-{i}.{ext}") };
        let staged = dir.join(name);
        std::fs::copy(f, &staged).ok()?;
        out.push_str(&staged.to_string_lossy());
        out.push(' ');
    }
    Some(out.into_bytes())
}

/// Split dropped paths as VS Code quotes them: `"a b.png"`, `'a b.png'` / `& 'it''s.png'`
/// (PowerShell) or bare. `None` if anything else is mixed in.
fn split_dropped_paths(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        rest = rest.strip_prefix("& ").unwrap_or(rest);
        let (tok, after) = if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"')?;
            (r[..end].to_string(), &r[end + 1..])
        } else if let Some(r) = rest.strip_prefix('\'') {
            let mut tok = String::new();
            let mut i = 0;
            loop {
                let j = i + r[i..].find('\'')?;
                tok.push_str(&r[i..j]);
                if r[j + 1..].starts_with('\'') {
                    tok.push('\'');
                    i = j + 2;
                } else {
                    break (tok, &r[j + 1..]);
                }
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (rest[..end].to_string(), &rest[end..])
        };
        if !after.is_empty() && !after.starts_with(char::is_whitespace) {
            return None;
        }
        out.push(tok);
        rest = after.trim_start();
    }
    (!out.is_empty()).then_some(out)
}

fn relay(method: &str, params: Value) -> Result<i32> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(relay_async(method, params))
}

type PipeRead = BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>;
type PipeWrite = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>;

async fn request(method: &str, params: Value) -> Result<(PipeRead, PipeWrite)> {
    let name = paths::pipe_name();
    let pipe = loop {
        match ClientOptions::new().open(&name) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(231) => tokio::time::sleep(Duration::from_millis(20)).await, // all instances busy
            Err(e) => return Err(e).context("connecting to the wtd daemon"),
        }
    };
    let (r, mut w) = tokio::io::split(pipe);
    let mut req = serde_json::to_vec(&json!({ "id": 1, "method": method, "params": params }))?;
    req.push(b'\n');
    w.write_all(&req).await?;
    Ok((BufReader::new(r), w))
}

async fn relay_async(method: &str, params: Value) -> Result<i32> {
    let sent_size = params["cols"].as_u64().zip(params["rows"].as_u64()).map(|(c, r)| (c as u16, r as u16));
    let (mut rd, mut w) = request(method, params).await?;
    let mut line = String::new();
    rd.read_line(&mut line).await?;
    let resp: Value = serde_json::from_str(line.trim()).context("bad response from daemon")?;
    if let Some(e) = resp.get("error").and_then(Value::as_str) {
        bail!("{e}");
    }

    win::ignore_ctrl_c_in_this_process();
    let raw = RawConsole::enter();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let stop = std::sync::Arc::new(AtomicBool::new(false));

    // keyboard → input frames (a blocking console read, so its own thread)
    let tx_in = tx.clone();
    let image_dir = paths::dev_root().ok().map(|d| paths::state_dir(&d).join("pasted-images"));
    std::thread::spawn(move || {
        let Some(h) = std_handle(STD_INPUT_HANDLE) else { return };
        let mut buf = [0u8; 4096];
        loop {
            let mut n = 0u32;
            if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == FALSE || n == 0 {
                break;
            }
            let input = &buf[..n as usize];
            let staged = image_dir.as_deref().and_then(|d| stage_dropped_images(input, d));
            if tx_in.send(frame(F_DATA, staged.as_deref().unwrap_or(input))).is_err() {
                break;
            }
        }
    });
    // window size → resize frames (Windows has no SIGWINCH: poll). Compared against the size the
    // daemon was given, so a resize while the request was in flight still gets sent.
    let tx_sz = tx.clone();
    let stop_sz = stop.clone();
    std::thread::spawn(move || {
        let mut last = sent_size;
        while !stop_sz.load(Ordering::Relaxed) {
            let now = console_size();
            if now.is_some() && now != last {
                let (c, r) = now.unwrap();
                let mut p = c.to_le_bytes().to_vec();
                p.extend_from_slice(&r.to_le_bytes());
                if tx_sz.send(frame(F_RESIZE, &p)).is_err() {
                    break;
                }
                last = now;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    drop(tx);
    let writer = tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if w.write_all(&f).await.is_err() {
                break;
            }
        }
    });

    // session output → this console, until exit or daemon disconnect
    let mut hdr = [0u8; 5];
    let code = loop {
        if rd.read_exact(&mut hdr).await.is_err() {
            drop(raw);
            write_out(b"\r\n\x1b[2m[wtd] the daemon stopped - this session has ended\x1b[0m\r\n");
            break 1;
        }
        let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut payload = vec![0u8; len];
        if rd.read_exact(&mut payload).await.is_err() {
            break 1;
        }
        match hdr[0] {
            F_DATA => write_out(&payload),
            F_EXIT => {
                drop(raw);
                break if payload.len() >= 4 { i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) } else { 0 };
            }
            _ => {}
        }
    };
    stop.store(true, Ordering::Relaxed);
    writer.abort();
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_vscode_quoted_paths() {
        let v = |s: &str| split_dropped_paths(s);
        let own = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(v(r#""C:\My Pics\a.png""#), own(&[r"C:\My Pics\a.png"]));
        assert_eq!(v(r"C:\x\a.png "), own(&[r"C:\x\a.png"]));
        assert_eq!(v(r"& 'C:\it''s here\a.png'"), own(&[r"C:\it's here\a.png"]));
        assert_eq!(v(r#""C:\a b\1.png" C:\c\2.jpg"#), own(&[r"C:\a b\1.png", r"C:\c\2.jpg"]));
        assert_eq!(v(r#""C:\a.png"x"#), None);
        assert_eq!(v(r#""C:\a.png"#), None);
        assert_eq!(v("  "), None);
    }

    #[test]
    fn stages_only_image_drops() {
        let t = std::env::temp_dir().join(format!("wtd-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let src = t.join("my pics");
        std::fs::create_dir_all(&src).unwrap();
        let (png, txt) = (src.join("a.PNG"), src.join("notes.txt"));
        std::fs::write(&png, b"png").unwrap();
        std::fs::write(&txt, b"txt").unwrap();
        let stage = t.join("pasted-images");

        let dropped = format!("\x1b[200~\"{}\"\x1b[201~", png.display());
        let out = String::from_utf8(stage_dropped_images(dropped.as_bytes(), &stage).unwrap()).unwrap();
        let staged = Path::new(out.trim_end());
        assert!(out.ends_with(' '));
        assert_eq!(staged.parent(), Some(stage.as_path()));
        assert!(staged.file_name().unwrap().to_string_lossy().starts_with("img-"));
        assert_eq!(std::fs::read(staged).unwrap(), b"png");

        let passthrough = [
            format!("\"{}\"", txt.display()),
            format!("\"{}\" \"{}\"", png.display(), txt.display()),
            "look at a.png".into(),
            r"C:\missing\b.png".into(),
            "x".into(),
        ];
        for p in passthrough {
            assert_eq!(stage_dropped_images(p.as_bytes(), &stage), None, "{p}");
        }
        std::fs::remove_dir_all(&t).ok();
    }
}
