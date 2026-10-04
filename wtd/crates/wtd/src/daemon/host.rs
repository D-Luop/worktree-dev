//! Daemon-hosted sessions (Phase 3): agents run in daemon-owned pseudo consoles; terminals attach.
//!
//! Attach protocol: the client sends `session.spawn` (open-or-attach) or `session.attach` as a normal
//! JSON line; after the JSON response the connection carries binary frames both ways:
//!   [type u8][len u32 LE][payload]
//!   client → daemon:  0 = input bytes · 1 = resize (cols u16, rows u16)
//!   daemon → client:  0 = output bytes · 2 = exit (code i32)
//! Detach = close the connection; the session keeps running.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader, ReadHalf};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::{broadcast, mpsc};
use wtd_core::status::{apply, Event};

use super::{now, pty::Pty, Daemon, Session};
use crate::{paths, statusfile};

pub const F_DATA: u8 = 0;
pub const F_RESIZE: u8 = 1;
pub const F_EXIT: u8 = 2;
const MAX_FRAME: usize = 1 << 20;

#[derive(Clone)]
pub enum HostEvent {
    Data(Arc<Vec<u8>>),
    Exit(i32),
}

pub struct Hosted {
    pub pty: Arc<Pty>,
    pub tx: broadcast::Sender<HostEvent>,
}

pub fn frame(t: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(t);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

#[derive(Deserialize)]
struct SpawnParams {
    dir: String,
    #[serde(default = "agent")]
    kind: String,
    account: Option<String>,
    program: Option<String>,
    exe: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default = "cols")]
    cols: u16,
    #[serde(default = "rows")]
    rows: u16,
}
fn agent() -> String {
    "agent".into()
}
fn cols() -> u16 {
    120
}
fn rows() -> u16 {
    30
}

/// The live hosted session for a worktree, if any.
fn find(d: &Daemon, wt: &str) -> Option<(u64, Arc<Hosted>)> {
    d.lock().sessions.iter().find_map(|(id, s)| (s.wt == wt).then(|| s.hosted.clone().map(|h| (*id, h))).flatten())
}

/// `session.spawn`: attach to the worktree's hosted session if it has one, else start it.
/// `session.attach {worktree|dir}`: attach only. Returns (session, size to use, created).
pub fn open(d: &Arc<Daemon>, method: &str, p: &Value) -> Result<(Arc<Hosted>, Value, bool)> {
    let dir = p.get("dir").and_then(Value::as_str);
    let wt = match (p.get("worktree").and_then(Value::as_str), dir) {
        (Some(w), _) => w.to_string(),
        (None, Some(dir)) => d.worktree_id_for(dir).with_context(|| format!("{dir} is not a worktree-dev worktree"))?,
        _ => bail!("missing worktree/dir"),
    };
    if let Some((id, h)) = find(d, &wt) {
        return Ok((h, json!({ "session": id, "worktree": wt, "created": false }), false));
    }
    if method == wtd_core::protocol::method::SESSION_ATTACH {
        bail!("no hosted session for '{wt}'");
    }
    let sp: SpawnParams = serde_json::from_value(p.clone()).context("bad session.spawn params")?;
    let id = d.next_conn.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, _) = broadcast::channel(4096);
    let tx_out = tx.clone();
    let tx_exit = tx.clone();
    let d_exit = d.clone();
    let wt_exit = wt.clone();
    let pty = Pty::spawn(
        &PathBuf::from(&sp.exe),
        &sp.args,
        &paths::normalize(&sp.dir),
        &sp.env,
        sp.cols,
        sp.rows,
        &wt.replace('/', "_"),
        move |b| {
            let _ = tx_out.send(HostEvent::Data(Arc::new(b.to_vec())));
        },
        move |code| {
            let _ = tx_exit.send(HostEvent::Exit(code));
            exited(&d_exit, id, &wt_exit, code);
        },
    )?;
    eprintln!("[{}] hosted {} for {wt} (pid {})", now(), sp.program.as_deref().unwrap_or("?"), pty.pid);
    let hosted = Arc::new(Hosted { pty: pty.clone(), tx });
    {
        // (a brand-new worktree the scanner hasn't seen yet gets its liveness on the next rescan)
        let mut inner = d.lock();
        inner.sessions.insert(id, Session {
            wt: wt.clone(),
            kind: sp.kind,
            account: sp.account.filter(|a| !a.is_empty() && a != "default"),
            job: pty.job_name.clone(),
            program: sp.program,
            hosted: Some(hosted.clone()),
        });
        d.refresh_liveness(&mut inner, &wt);
    }
    // queued messages for this worktree: deliver once the agent is up and waiting (not mid-boot)
    let d_msg = d.clone();
    let wt_msg = wt.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        super::messages::deliver_next(&d_msg, &wt_msg);
    });
    Ok((hosted, json!({ "session": id, "worktree": wt, "created": true }), true))
}

/// The process ended: drop the session, and record SessionEnd (a killed agent never ran its hook).
fn exited(d: &Arc<Daemon>, id: u64, wt: &str, code: i32) {
    eprintln!("[{}] hosted session for {wt} exited ({code})", now());
    let root = paths::worktree_path(&d.dev, wt);
    let old = statusfile::read(&root);
    let new = apply(old, &Event::SessionEnd);
    if new != old {
        let _ = statusfile::write(&d.dev, wt, &root, old, new);
    }
    let mut inner = d.lock();
    if let Some(s) = inner.sessions.remove(&id) {
        inner.cpu_prev.remove(&s.job);
    }
    if let Some(mut w) = inner.worktrees.get(wt).cloned() {
        w.status = new.status;
        w.last_activity = now();
        d.upsert(&mut inner, w);
    }
    d.refresh_liveness(&mut inner, wt);
}

/// Relay a hosted session over this connection until the client disconnects (the session survives)
/// or the session ends.
pub async fn attach(
    hosted: Arc<Hosted>,
    created: bool,
    cols: u16,
    rows: u16,
    mut rd: BufReader<ReadHalf<NamedPipeServer>>,
    out: mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut rx = hosted.tx.subscribe();
    if !created {
        // a re-attaching terminal starts blank: clear it, then make conpty repaint the viewport
        let _ = out.send(frame(F_DATA, b"\x1b[2J\x1b[3J\x1b[H"));
        hosted.pty.repaint(cols, rows);
    }
    let pty_lag = hosted.pty.clone();
    let out_task = out.clone();
    let pump = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(HostEvent::Data(b)) => {
                    if out_task.send(frame(F_DATA, &b)).is_err() {
                        break;
                    }
                }
                Ok(HostEvent::Exit(code)) => {
                    let _ = out_task.send(frame(F_EXIT, &code.to_le_bytes()));
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // we dropped output: repaint so the terminal shows the true screen again
                    let (c, r) = pty_lag.size();
                    let _ = out_task.send(frame(F_DATA, b"\x1b[2J\x1b[H"));
                    pty_lag.repaint(c, r);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let mut hdr = [0u8; 5];
    loop {
        if rd.read_exact(&mut hdr).await.is_err() {
            break; // client detached (terminal closed / VSCode reloaded)
        }
        let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        if len > MAX_FRAME {
            break;
        }
        let mut payload = vec![0u8; len];
        if rd.read_exact(&mut payload).await.is_err() {
            break;
        }
        match hdr[0] {
            F_DATA => {
                let pty = hosted.pty.clone();
                // a pipe write can block if conpty is momentarily not reading; keep the runtime free
                if tokio::task::spawn_blocking(move || pty.write(&payload)).await.map(|r| r.is_err()).unwrap_or(true) {
                    break;
                }
            }
            F_RESIZE if payload.len() >= 4 => {
                let c = u16::from_le_bytes([payload[0], payload[1]]);
                let r = u16::from_le_bytes([payload[2], payload[3]]);
                hosted.pty.resize(c, r);
            }
            _ => {}
        }
    }
    pump.abort();
}
