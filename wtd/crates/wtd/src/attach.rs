//! Terminal side of daemon-hosted sessions (Phase 3).
//!
//!   wtd host [--kind k] [--account a] -- <program> [args…]   open-or-attach this worktree's session
//!   wtd attach [<worktree id>]                              attach to a running one (default: cwd)
//!
//! Puts the console in raw VT mode and relays bytes both ways over one overlapped pipe connection.
//! Closing the terminal only detaches: the agent keeps running in the daemon. If the daemon isn't
//! running, `wtd host` falls back to `wtd run` (the session then lives in this terminal).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

const F_DATA: u8 = 0;
const F_RESIZE: u8 = 1;
const F_EXIT: u8 = 2;

fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
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
    if Client::connect()?.is_none() {
        return crate::run::main(args); // no daemon: the session lives in this terminal, as before
    }
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
    // the hosted process gets this terminal's environment (CLAUDE_CONFIG_DIR / CODEX_HOME, PATH, …)
    let env: Vec<(String, String)> = std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into())).collect();
    let (cols, rows) = console_size().unwrap_or((120, 30));
    relay(method::SESSION_SPAWN, json!({
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

fn relay(method: &str, params: Value) -> Result<i32> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(relay_async(method, params))
}

async fn relay_async(method: &str, params: Value) -> Result<i32> {
    let name = paths::pipe_name();
    let pipe = loop {
        match ClientOptions::new().open(&name) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(231) => tokio::time::sleep(Duration::from_millis(20)).await, // all instances busy
            Err(e) => return Err(e).context("connecting to the wtd daemon"),
        }
    };
    let (r, mut w) = tokio::io::split(pipe);
    let mut rd = BufReader::new(r);
    let mut req = serde_json::to_vec(&json!({ "id": 1, "method": method, "params": params }))?;
    req.push(b'\n');
    w.write_all(&req).await?;
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
    std::thread::spawn(move || {
        let Some(h) = std_handle(STD_INPUT_HANDLE) else { return };
        let mut buf = [0u8; 4096];
        loop {
            let mut n = 0u32;
            if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == FALSE || n == 0 {
                break;
            }
            if tx_in.send(frame(F_DATA, &buf[..n as usize])).is_err() {
                break;
            }
        }
    });
    // window size → resize frames (Windows has no SIGWINCH: poll)
    let tx_sz = tx.clone();
    let stop_sz = stop.clone();
    std::thread::spawn(move || {
        let mut last = console_size();
        while !stop_sz.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
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
