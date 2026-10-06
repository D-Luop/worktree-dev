//! `wtd daemon start|stop|status|run`: the resident fleet service.
//!
//! Holds fleet state in memory and pushes changes to subscribers over the pipe. Spawns processes only
//! for `git status` (debounced, per worktree, at most 2 at a time); everything else is in-process.

pub mod git;
pub mod scan;
mod host;
mod messages;
mod pty;
mod store;
mod watch;
mod usage;

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, mpsc, Semaphore};
use wtd_core::model::{Account, Group, Metrics, Snapshot, Worktree, DEV_ID};
use wtd_core::protocol::{method, HookParams, Push, Request, Response, ServerLine, SessionParams, PROTOCOL_VERSION};

use crate::{client::Client, paths, win};

const SCAN_EVERY: Duration = Duration::from_secs(10);
const USAGE_EVERY: Duration = Duration::from_secs(60);
const METRICS_EVERY: Duration = Duration::from_secs(5);
/// After hook activity, wait this long for quiet before `git status`…
const GIT_DEBOUNCE: Duration = Duration::from_secs(2);
/// …but never longer than this after the first event of a burst.
const GIT_MAX_DELAY: Duration = Duration::from_secs(10);
// file watchers drive refreshes; these sweeps are only a backstop (missed events, external git ops)
const GIT_SWEEP_LIVE: Duration = Duration::from_secs(300);
const GIT_SWEEP_IDLE: Duration = Duration::from_secs(900);
const GIT_CONCURRENCY: usize = 2;

pub fn main(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("start") => start(),
        Some("stop") => stop(args.iter().any(|a| a == "--force" || a == "-f")),
        Some("status") => status(),
        Some("run") => run_foreground(),
        _ => bail!("usage: wtd daemon start|stop|status|run"),
    }
}

fn start() -> Result<i32> {
    if Client::connect()?.is_some() {
        println!("daemon already running");
        return Ok(0);
    }
    let dev = paths::dev_root()?;
    let state = paths::state_dir(&dev);
    std::fs::create_dir_all(&state)?;
    let log_path = state.join("daemon.log");
    let log = OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["daemon", "run"]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    win::spawn_detached(&mut cmd).context("starting the daemon")?;
    for _ in 0..100 {
        if Client::connect()?.is_some() {
            println!("daemon started");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("daemon didn't come up; see {}", log_path.display())
}

fn stop(force: bool) -> Result<i32> {
    let Some(mut c) = Client::connect()? else {
        println!("daemon not running");
        return Ok(0);
    };
    c.request(method::SHUTDOWN, json!({ "force": force }))?;
    for _ in 0..100 {
        if Client::connect()?.is_none() {
            println!("daemon stopped");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("daemon didn't stop")
}

fn status() -> Result<i32> {
    match Client::connect()? {
        None => {
            println!("stopped");
            Ok(3)
        }
        Some(mut c) => {
            let h = c.hello("cli")?;
            println!("running  pid {}  v{}  dev {}", h["pid"], h["version"].as_str().unwrap_or("?"), h["dev_root"].as_str().unwrap_or("?"));
            Ok(0)
        }
    }
}

fn run_foreground() -> Result<i32> {
    let dev = paths::dev_root()?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(serve(dev))?;
    Ok(0)
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------------

struct Session {
    wt: String,
    kind: String,
    account: Option<String>,
    job: String,
    program: Option<String>,
    /// Set for daemon-hosted sessions (Phase 3); None for terminal sessions under `wtd run`.
    hosted: Option<Arc<host::Hosted>>,
}

#[derive(Clone, Copy)]
struct GitDue {
    due: Instant,
    first: Instant,
}

#[derive(Default)]
struct Inner {
    rev: u64,
    worktrees: BTreeMap<String, Worktree>,
    /// Keyed by connection id: a session lives exactly as long as its `wtd run` connection.
    sessions: HashMap<u64, Session>,
    accounts: Vec<Account>,
    metrics: Option<Metrics>,
    metrics_subs: usize,
    git_due: HashMap<String, GitDue>,
    git_running: HashMap<String, ()>,
    /// Last (job CPU 100ns, sample instant) per job for CPU% deltas.
    cpu_prev: HashMap<String, (u64, Instant)>,
    /// Groups, membership, messages (persisted).
    store: store::Store,
}

struct Daemon {
    dev: PathBuf,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Push>,
    next_conn: AtomicU64,
    /// worktree id → its file watcher (dropping it releases the directory handles)
    watchers: Mutex<HashMap<String, notify::RecommendedWatcher>>,
}

impl Daemon {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Insert/replace a worktree and push it if anything changed. Call with the lock held.
    fn upsert(&self, inner: &mut Inner, wt: Worktree) {
        if inner.worktrees.get(&wt.id) == Some(&wt) {
            return;
        }
        inner.rev += 1;
        inner.worktrees.insert(wt.id.clone(), wt.clone());
        let _ = self.tx.send(Push::Upsert { rev: inner.rev, worktree: wt });
    }

    /// Release a worktree's file watch (before moving/removing it, or once it's gone).
    fn unwatch(&self, id: &str) {
        self.watchers.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }

    fn remove(&self, inner: &mut Inner, id: &str) {
        if inner.worktrees.remove(id).is_some() {
            inner.rev += 1;
            inner.git_due.remove(id);
            let _ = self.tx.send(Push::Remove { rev: inner.rev, id: id.into() });
        }
    }

    fn snapshot(&self, inner: &Inner) -> Snapshot {
        Snapshot {
            rev: inner.rev,
            dev_root: self.dev.to_string_lossy().into(),
            worktrees: inner.worktrees.values().cloned().collect(),
            accounts: inner.accounts.clone(),
            metrics: inner.metrics.clone(),
            groups: inner.store.groups.clone(),
            messages: inner.store.open_messages(),
        }
    }

    /// Write the store and push the group list. Call with the lock held after a group change.
    fn groups_changed(&self, inner: &mut Inner) {
        if let Err(e) = inner.store.save(&self.dev) {
            eprintln!("[{}] saving store: {e}", now());
        }
        inner.rev += 1;
        let _ = self.tx.send(Push::Groups { rev: inner.rev, groups: inner.store.groups.clone() });
    }

    /// Set a worktree's group field from the membership table and push it.
    fn apply_membership(&self, inner: &mut Inner, id: &str) {
        let g = inner.store.membership.get(id).cloned();
        if let Some(mut wt) = inner.worktrees.get(id).cloned() {
            wt.group = g;
            self.upsert(inner, wt);
        }
    }

    /// Recompute a worktree's `live`/`account` from the registered sessions.
    fn refresh_liveness(&self, inner: &mut Inner, id: &str) {
        let Some(mut wt) = inner.worktrees.get(id).cloned() else { return };
        let s = inner.sessions.values().find(|s| s.wt == id && s.kind != "review" && s.kind != "ask");
        wt.live = s.is_some();
        wt.account = s.map(|s| s.account.clone().unwrap_or_else(|| "default".into()));
        wt.program = s.and_then(|s| s.program.clone());
        wt.hosted = s.is_some_and(|s| s.hosted.is_some());
        self.upsert(inner, wt);
    }

    fn schedule_git(inner: &mut Inner, id: &str, debounced: bool) {
        let now = Instant::now();
        let e = inner.git_due.entry(id.to_string()).or_insert(GitDue { due: now, first: now });
        if debounced {
            if e.due > now + GIT_DEBOUNCE || e.first + GIT_MAX_DELAY < now {
                // fresh burst (or a far-future sweep entry): start the debounce window now
                *e = GitDue { due: now + GIT_DEBOUNCE, first: now };
            } else {
                e.due = (now + GIT_DEBOUNCE).min(e.first + GIT_MAX_DELAY);
            }
        } else {
            *e = GitDue { due: now, first: now };
        }
    }

    fn worktree_id_for(&self, dir: &str) -> Option<String> {
        paths::resolve_worktree(&self.dev, &paths::normalize(dir)).map(|r| r.id)
    }
}

async fn serve(dev: PathBuf) -> Result<()> {
    let name = paths::pipe_name();
    let sa = win::owner_only_security_attributes() as usize;
    let create = |first: bool| -> std::io::Result<NamedPipeServer> {
        let mut o = ServerOptions::new();
        o.first_pipe_instance(first).reject_remote_clients(true);
        unsafe { o.create_with_security_attributes_raw(&name, sa as *mut std::ffi::c_void) }
    };
    let mut server = create(true).context("another wtd daemon is already running")?;

    let (tx, _) = broadcast::channel(1024);
    let inner = Inner { store: store::Store::load(&dev), ..Default::default() };
    let d = Arc::new(Daemon { dev: dev.clone(), inner: Mutex::new(inner), tx, next_conn: AtomicU64::new(1), watchers: Mutex::new(HashMap::new()) });
    eprintln!("[{}] wtd daemon {} up: pid {}, dev {}", now(), env!("CARGO_PKG_VERSION"), std::process::id(), dev.display());

    rescan(&d).await;
    tokio::spawn(scan_loop(d.clone()));
    tokio::spawn(git_loop(d.clone()));
    tokio::spawn(usage_loop(d.clone()));
    tokio::spawn(metrics_loop(d.clone()));
    tokio::spawn(jobs_loop(d.clone()));
    tokio::spawn(tickets_loop(d.clone()));

    loop {
        server.connect().await?;
        let conn = server;
        server = create(false)?;
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(d, conn).await {
                eprintln!("[{}] connection error: {e:#}", now());
            }
        });
    }
}

fn line(msg: &ServerLine) -> Vec<u8> {
    let mut s = serde_json::to_vec(msg).unwrap_or_default();
    s.push(b'\n');
    s
}

async fn handle(d: Arc<Daemon>, conn: NamedPipeServer) -> Result<()> {
    let id = d.next_conn.fetch_add(1, Ordering::Relaxed);
    let (r, mut w) = tokio::io::split(conn);
    let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(l) = out_rx.recv().await {
            if w.write_all(&l).await.is_err() {
                break;
            }
        }
    });

    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
    let mut wants_metrics = false;
    let mut attach_to: Option<(Arc<host::Hosted>, bool, u16, u16)> = None;
    let mut lines = BufReader::new(r).lines();
    while let Some(l) = lines.next_line().await? {
        if l.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&l) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[{}] bad request: {e}: {l}", now());
                continue;
            }
        };
        let rid = req.id;
        let mut start_sub = false;
        let result = match req.method.as_str() {
            method::SUBSCRIBE => {
                wants_metrics = req.params.get("metrics").and_then(Value::as_bool).unwrap_or(false);
                start_sub = forwarder.is_none();
                Ok(json!({}))
            }
            method::SESSION_SPAWN | method::SESSION_ATTACH => {
                let known = d.worktree_id_for(req.params.get("dir").and_then(Value::as_str).unwrap_or(""));
                if known.is_some_and(|w| !d.lock().worktrees.contains_key(&w)) {
                    rescan(&d).await; // a worktree created moments ago
                }
                match host::open(&d, &req.method, &req.params) {
                    Ok((h, info, created)) => {
                        let c = req.params.get("cols").and_then(Value::as_u64).unwrap_or(120) as u16;
                        let r = req.params.get("rows").and_then(Value::as_u64).unwrap_or(30) as u16;
                        attach_to = Some((h, created, c, r));
                        Ok(info)
                    }
                    Err(e) => Err(e),
                }
            }
            _ => dispatch(&d, id, &req, &out).await,
        };
        if let Some(rid) = rid {
            let resp = match result {
                Ok(v) => Response { id: rid, result: Some(v), error: None },
                Err(e) => Response { id: rid, result: None, error: Some(format!("{e:#}")) },
            };
            let _ = out.send(line(&ServerLine::Response(resp)));
        }
        // after the response, so a client that waits for it never skips past the snapshot
        if start_sub {
            forwarder = Some(subscribe(&d, out.clone(), wants_metrics));
        }
        if attach_to.is_some() {
            break; // the rest of this connection is the binary attach stream
        }
    }
    if let Some((h, created, c, r)) = attach_to {
        host::attach(h, created, c, r, lines.into_inner(), out.clone()).await;
    }

    // disconnected
    if let Some(f) = forwarder {
        f.abort();
    }
    {
        let mut inner = d.lock();
        if wants_metrics {
            inner.metrics_subs = inner.metrics_subs.saturating_sub(1);
        }
        if let Some(s) = inner.sessions.remove(&id) {
            inner.cpu_prev.remove(&s.job);
            d.refresh_liveness(&mut inner, &s.wt);
            Daemon::schedule_git(&mut inner, &s.wt, true);
        }
    }
    drop(out);
    let _ = writer.await;
    Ok(())
}

/// Send a snapshot, then forward every later push. Snapshot + subscribe happen under one lock so no
/// change can slip between them.
fn subscribe(d: &Arc<Daemon>, out: mpsc::UnboundedSender<Vec<u8>>, metrics: bool) -> tokio::task::JoinHandle<()> {
    let (mut rx, snap) = {
        let mut inner = d.lock();
        if metrics {
            inner.metrics_subs += 1;
        }
        (d.tx.subscribe(), d.snapshot(&inner))
    };
    let _ = out.send(line(&ServerLine::Push(Push::Snapshot { rev: snap.rev, snapshot: snap })));
    let d = d.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Push::Metrics { .. }) if !metrics => continue,
                Ok(p) => {
                    if out.send(line(&ServerLine::Push(p))).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let snap = d.snapshot(&d.lock());
                    let _ = out.send(line(&ServerLine::Push(Push::Snapshot { rev: snap.rev, snapshot: snap })));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

async fn dispatch(d: &Arc<Daemon>, conn: u64, req: &Request, out: &mpsc::UnboundedSender<Vec<u8>>) -> Result<Value> {
    let p = &req.params;
    match req.method.as_str() {
        method::HELLO => Ok(json!({
            "protocol": PROTOCOL_VERSION,
            "version": env!("CARGO_PKG_VERSION"),
            "dev_root": d.dev.to_string_lossy(),
            "pid": std::process::id(),
        })),
        method::HOOK => {
            let h: HookParams = serde_json::from_value(p.clone())?;
            let Some(id) = d.worktree_id_for(&h.dir) else { return Ok(Value::Null) };
            let known = {
                let mut inner = d.lock();
                match inner.worktrees.get(&id).cloned() {
                    Some(mut wt) => {
                        wt.status = h.status;
                        wt.last_activity = now();
                        d.upsert(&mut inner, wt);
                        Daemon::schedule_git(&mut inner, &id, true);
                        true
                    }
                    None => false,
                }
            };
            if known && h.status == wtd_core::status::Status::Input {
                messages::deliver_next(d, &id);
            }
            if !known {
                rescan(d).await; // a worktree we haven't seen yet (just created)
            }
            Ok(Value::Null)
        }
        method::SESSION_REGISTER => {
            let s: SessionParams = serde_json::from_value(p.clone())?;
            let id = d.worktree_id_for(&s.dir).with_context(|| format!("{} is not a worktree-dev worktree", s.dir))?;
            if !d.lock().worktrees.contains_key(&id) {
                rescan(d).await;
            }
            let mut inner = d.lock();
            inner.sessions.insert(conn, Session { wt: id.clone(), kind: s.kind, account: s.account, job: s.job, program: s.program, hosted: None });
            let _ = out; // the session's own connection; `terminate` is pushed on it via session.stop
            d.refresh_liveness(&mut inner, &id);
            Ok(json!({}))
        }
        method::SESSION_STOP => {
            let id = p.get("id").and_then(Value::as_str).context("missing id")?.to_string();
            Ok(json!({ "stopped": stop_sessions(d, &id) }))
        }
        method::WORKTREE_REMOVE | method::WORKTREE_ARCHIVE => {
            let id = p.get("id").and_then(Value::as_str).context("missing id")?.to_string();
            let (slug, name) = id.split_once('/').map(|(a, b)| (a.to_string(), b.to_string())).context("id must be <slug>/<name>")?;
            // editors close their terminals / tabs / watches in the folder (on Windows any of those
            // blocks the move or delete), then the session's process tree goes
            let editors = d.tx.send(Push::Release { id: id.clone() }).unwrap_or(0) > 0;
            let stopped = stop_sessions(d, &id) > 0;
            if editors || stopped {
                tokio::time::sleep(Duration::from_millis(900)).await; // let them release the folder
            }
            d.unwatch(&id);
            let dev = d.dev.clone();
            let archive = req.method == method::WORKTREE_ARCHIVE;
            let force = p.get("force").and_then(Value::as_bool).unwrap_or(false);
            let branch = p.get("branch").and_then(Value::as_bool).unwrap_or(false);
            let res = tokio::task::spawn_blocking(move || -> Result<Value> {
                if archive {
                    Ok(json!({ "path": crate::wt::archive(&dev, &slug, &name)? }))
                } else {
                    Ok(json!({ "log": crate::wt::remove(&dev, &slug, &name, force, branch)? }))
                }
            })
            .await?;
            rescan(d).await;
            res
        }
        method::FLEET_LIST => Ok(serde_json::to_value(d.lock().worktrees.values().cloned().collect::<Vec<_>>())?),
        method::FLEET_GET => {
            let id = p.get("id").and_then(Value::as_str).context("missing id")?;
            let wt = d.lock().worktrees.get(id).cloned().with_context(|| format!("no worktree '{id}'"))?;
            Ok(serde_json::to_value(wt)?)
        }
        method::REFRESH => {
            rescan(d).await;
            let mut inner = d.lock();
            match p.get("id").and_then(Value::as_str) {
                Some(id) => Daemon::schedule_git(&mut inner, id, false),
                None => {
                    let ids: Vec<String> = inner.worktrees.keys().cloned().collect();
                    for id in ids {
                        Daemon::schedule_git(&mut inner, &id, false);
                    }
                }
            }
            Ok(json!({}))
        }
        method::SHUTDOWN => {
            let hosted = d.lock().sessions.values().filter(|s| s.hosted.is_some()).count();
            if hosted > 0 && !p.get("force").and_then(Value::as_bool).unwrap_or(false) {
                bail!("{hosted} hosted session(s) are running and would end — stop with force to confirm");
            }
            eprintln!("[{}] shutdown requested ({hosted} hosted session(s) end)", now());
            let _ = d.tx.send(Push::Shutdown);
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                std::process::exit(0);
            });
            Ok(json!({}))
        }
        method::GROUP_CREATE => {
            let name = group_name(p)?;
            let mut inner = d.lock();
            let id = format!("g{}", inner.store.next());
            inner.store.groups.push(Group { id: id.clone(), name, collapsed: false });
            d.groups_changed(&mut inner);
            Ok(json!({ "id": id }))
        }
        method::GROUP_UPDATE => {
            let id = p.get("id").and_then(Value::as_str).context("missing id")?;
            let name = p.get("name").map(|_| group_name(p)).transpose()?;
            let mut inner = d.lock();
            let g = inner.store.groups.iter_mut().find(|g| g.id == id).with_context(|| format!("no group '{id}'"))?;
            if let Some(n) = name {
                g.name = n;
            }
            if let Some(c) = p.get("collapsed").and_then(Value::as_bool) {
                g.collapsed = c;
            }
            d.groups_changed(&mut inner);
            Ok(json!({}))
        }
        method::GROUP_DELETE => {
            let id = p.get("id").and_then(Value::as_str).context("missing id")?.to_string();
            let mut inner = d.lock();
            inner.store.groups.retain(|g| g.id != id);
            let members: Vec<String> = inner.store.membership.iter().filter(|(_, g)| **g == id).map(|(w, _)| w.clone()).collect();
            for w in &members {
                inner.store.membership.remove(w);
                d.apply_membership(&mut inner, w);
            }
            d.groups_changed(&mut inner);
            Ok(json!({ "ungrouped": members.len() }))
        }
        method::GROUP_REORDER => {
            let ids: Vec<String> = serde_json::from_value(p.get("ids").cloned().unwrap_or_default())?;
            let mut inner = d.lock();
            let mut rest = std::mem::take(&mut inner.store.groups);
            let mut ordered: Vec<Group> = ids.iter().filter_map(|id| rest.iter().position(|g| &g.id == id).map(|i| rest.remove(i))).collect();
            ordered.extend(rest); // anything not named keeps its relative order at the end
            inner.store.groups = ordered;
            d.groups_changed(&mut inner);
            Ok(json!({}))
        }
        method::GROUP_ASSIGN => {
            let wt = p.get("worktree").and_then(Value::as_str).context("missing worktree")?.to_string();
            let group = p.get("group").and_then(Value::as_str).map(String::from);
            let mut inner = d.lock();
            if !inner.worktrees.contains_key(&wt) {
                bail!("no worktree '{wt}'");
            }
            match &group {
                Some(g) if !inner.store.groups.iter().any(|x| &x.id == g) => bail!("no group '{g}'"),
                Some(g) => { inner.store.membership.insert(wt.clone(), g.clone()); }
                None => { inner.store.membership.remove(&wt); }
            }
            if let Err(e) = inner.store.save(&d.dev) {
                eprintln!("[{}] saving store: {e}", now());
            }
            d.apply_membership(&mut inner, &wt);
            Ok(json!({}))
        }
        method::MESSAGE_SEND => messages::send(d, p),
        method::MESSAGE_DECIDE => messages::decide(d, p),
        method::MESSAGE_LIST => messages::list(d, p),
        method::JOB_SCHEDULE => {
            let kind = p.get("kind").and_then(Value::as_str).context("missing kind")?.to_string();
            if kind != "review" {
                bail!("unsupported job kind '{kind}'");
            }
            let args: Vec<String> = serde_json::from_value(p.get("args").cloned().unwrap_or_default())?;
            let at = p.get("at").and_then(Value::as_i64).context("missing at")?;
            let key = p.get("key").and_then(Value::as_str).unwrap_or("").to_string();
            let mut inner = d.lock();
            inner.store.jobs.retain(|j| !(j.kind == kind && j.key == key));
            let id = inner.store.next();
            inner.store.jobs.push(store::Job { id, kind, args, at, key });
            let _ = inner.store.save(&d.dev);
            Ok(json!({ "id": id }))
        }
        method::JOB_CANCEL => {
            let kind = p.get("kind").and_then(Value::as_str).unwrap_or("");
            let key = p.get("key").and_then(Value::as_str).unwrap_or("");
            let mut inner = d.lock();
            let before = inner.store.jobs.len();
            inner.store.jobs.retain(|j| !(j.kind == kind && j.key == key));
            if inner.store.jobs.len() != before {
                let _ = inner.store.save(&d.dev);
            }
            Ok(json!({}))
        }
        m => bail!("unknown method '{m}'"),
    }
}

// --- background loops ------------------------------------------------------------------------------

/// Re-read the worktree list + cheap facts from disk and merge (keeping git/live/activity state).
async fn rescan(d: &Arc<Daemon>) {
    let dev = d.dev.clone();
    let found = match tokio::task::spawn_blocking(move || scan::scan(&dev)).await {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut inner = d.lock();
    let mut seen = std::collections::HashSet::new();
    for mut wt in found {
        seen.insert(wt.id.clone());
        wt.group = inner.store.membership.get(&wt.id).cloned();
        match inner.worktrees.get(&wt.id) {
            Some(old) => {
                wt.git = old.git.clone();
                wt.live = old.live;
                wt.account = old.account.clone();
                wt.hosted = old.hosted;
                wt.program = old.program.clone();
                wt.last_activity = if wt.status != old.status { now() } else { old.last_activity };
            }
            None => {
                if wt.id != DEV_ID {
                    Daemon::schedule_git(&mut inner, &wt.id, false);
                }
            }
        }
        let id = wt.id.clone();
        let fresh = !inner.worktrees.contains_key(&id);
        d.upsert(&mut inner, wt);
        if fresh {
            d.refresh_liveness(&mut inner, &id);
        }
    }
    let gone: Vec<String> = inner.worktrees.keys().filter(|k| !seen.contains(*k)).cloned().collect();
    for id in &gone {
        d.remove(&mut inner, id);
    }
    let want: Vec<(String, PathBuf)> = inner.worktrees.values().filter(|w| w.id != DEV_ID).map(|w| (w.id.clone(), PathBuf::from(&w.path))).collect();
    drop(inner);
    for id in &gone {
        d.unwatch(id);
    }
    let mut ws = d.watchers.lock().unwrap_or_else(|p| p.into_inner());
    for (id, path) in want {
        if !ws.contains_key(&id) {
            if let Some(w) = watch::watch(d, &id, &path) {
                ws.insert(id, w);
            }
        }
    }
}

async fn scan_loop(d: Arc<Daemon>) {
    let mut t = tokio::time::interval(SCAN_EVERY);
    t.tick().await;
    loop {
        t.tick().await;
        rescan(&d).await;
    }
}

async fn git_loop(d: Arc<Daemon>) {
    let sem = Arc::new(Semaphore::new(GIT_CONCURRENCY));
    let mut t = tokio::time::interval(Duration::from_millis(500));
    loop {
        t.tick().await;
        let ready: Vec<(String, PathBuf)> = {
            let mut inner = d.lock();
            let now_i = Instant::now();
            let ids: Vec<String> = inner
                .git_due
                .iter()
                .filter(|(id, g)| g.due <= now_i && !inner.git_running.contains_key(*id))
                .map(|(id, _)| id.clone())
                .take(sem.available_permits())
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    inner.git_due.remove(&id);
                    let path = PathBuf::from(&inner.worktrees.get(&id)?.path);
                    inner.git_running.insert(id.clone(), ());
                    Some((id, path))
                })
                .collect()
        };
        for (id, path) in ready {
            let Ok(permit) = sem.clone().acquire_owned().await else { return };
            let d = d.clone();
            tokio::spawn(async move {
                let res = tokio::task::spawn_blocking(move || git::status(&path, now())).await.ok().flatten();
                let mut inner = d.lock();
                inner.git_running.remove(&id);
                if let Some(mut wt) = inner.worktrees.get(&id).cloned() {
                    if let Some(g) = res {
                        wt.git = g;
                    }
                    let live = wt.live;
                    d.upsert(&mut inner, wt);
                    // background sweep: next check later unless activity schedules one sooner
                    let next = Instant::now() + if live { GIT_SWEEP_LIVE } else { GIT_SWEEP_IDLE };
                    inner.git_due.entry(id).or_insert(GitDue { due: next, first: next });
                }
                drop(permit);
            });
        }
    }
}

async fn usage_loop(d: Arc<Daemon>) {
    let home = match paths::home_dir() {
        Ok(h) => h,
        Err(_) => return,
    };
    let mut last: HashMap<String, Account> = HashMap::new();
    let mut t = tokio::time::interval(USAGE_EVERY);
    loop {
        t.tick().await;
        let h = home.clone();
        let mut l = std::mem::take(&mut last);
        let res = tokio::task::spawn_blocking(move || {
            let a = usage::fetch_all(&h, &mut l, now());
            (a, l)
        })
        .await;
        let Ok((accounts, l)) = res else { continue };
        last = l;
        let mut inner = d.lock();
        if inner.accounts != accounts {
            inner.rev += 1;
            inner.accounts = accounts.clone();
            let _ = d.tx.send(Push::Accounts { rev: inner.rev, accounts });
        }
    }
}

async fn metrics_loop(d: Arc<Daemon>) {
    let ncpu = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let mut t = tokio::time::interval(METRICS_EVERY);
    loop {
        t.tick().await;
        let (jobs, kinds, prev) = {
            let inner = d.lock();
            if inner.metrics_subs == 0 {
                continue;
            }
            let jobs: Vec<String> = inner.sessions.values().map(|s| s.job.clone()).collect();
            let kinds: Vec<String> = inner.sessions.values().map(|s| s.kind.clone()).collect();
            (jobs, kinds, inner.cpu_prev.clone())
        };
        let res = tokio::task::spawn_blocking(move || {
            let mut cpu_pct = 0.0;
            let mut mem: u64 = 0;
            let mut next = HashMap::new();
            let now_i = Instant::now();
            for j in jobs.iter().filter(|j| !j.is_empty()) {
                let Some((cpu, pids)) = win::job_stats(j) else { continue };
                if let Some((pc, pt)) = prev.get(j) {
                    let wall = now_i.duration_since(*pt).as_nanos() as f64 / 100.0; // 100ns units
                    if wall > 0.0 && cpu >= *pc {
                        cpu_pct += (cpu - pc) as f64 / wall * 100.0;
                    }
                }
                next.insert(j.clone(), (cpu, now_i));
                mem += pids.iter().map(|p| win::working_set_bytes(*p)).sum::<u64>();
            }
            let (total, used) = win::system_memory_mb();
            (cpu_pct, mem / (1024 * 1024), total, used, next)
        })
        .await;
        let Ok((cpu_pct, mem_mb, total, used, next)) = res else { continue };
        let agents = kinds.iter().filter(|k| *k == "agent" || *k == "assistant").count() as u32;
        let m = Metrics {
            sessions: kinds.len() as u32,
            agents,
            reviews: kinds.len() as u32 - agents,
            cpu_pct: cpu_pct.round(),
            mem_mb,
            sys_total_mb: total,
            sys_used_mb: used,
            ncpu,
        };
        let mut inner = d.lock();
        inner.cpu_prev = next;
        inner.rev += 1;
        inner.metrics = Some(m.clone());
        let _ = d.tx.send(Push::Metrics { rev: inner.rev, metrics: m });
    }
}

fn group_name(p: &Value) -> Result<String> {
    let n = p.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if n.is_empty() || n.chars().count() > 60 {
        bail!("group name must be 1-60 characters");
    }
    Ok(n)
}

/// End every session of a worktree (hosted: kill its pty's job; `wtd run`: terminate its named job).
fn stop_sessions(d: &Daemon, id: &str) -> u64 {
    let (jobs, hosted): (Vec<String>, Vec<Arc<host::Hosted>>) = {
        let inner = d.lock();
        let mine: Vec<&Session> = inner.sessions.values().filter(|s| s.wt == id).collect();
        (mine.iter().filter(|s| s.hosted.is_none()).map(|s| s.job.clone()).collect(), mine.iter().filter_map(|s| s.hosted.clone()).collect())
    };
    let mut n = 0;
    for h in &hosted {
        h.pty.kill();
        n += 1;
    }
    for j in &jobs {
        if !j.is_empty() && win::terminate_job(j).unwrap_or(false) {
            n += 1;
        }
    }
    n
}

/// Run due scheduled jobs (`wtd <kind> <args…>`, output to a log next to the worktree). A job that
/// came due while the daemon was stopped runs on the next start.
/// Keep live worktrees' `.claude-ticket.md` current (issue comments, PR reviews arrive while they work).
async fn tickets_loop(d: Arc<Daemon>) {
    let mut t = tokio::time::interval(Duration::from_secs(300));
    t.tick().await;
    loop {
        t.tick().await;
        let live: Vec<String> = d.lock().worktrees.values().filter(|w| w.live && w.id != DEV_ID && w.slug != "plan").map(|w| w.id.clone()).collect();
        if live.is_empty() || crate::settings::which("gh").is_none() {
            continue;
        }
        let dev = d.dev.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for id in live {
                if let Err(e) = crate::ticket::sync(&dev, &id) {
                    eprintln!("[{}] ticket sync {id}: {e:#}", now());
                }
            }
        })
        .await;
    }
}

async fn jobs_loop(d: Arc<Daemon>) {
    let mut t = tokio::time::interval(Duration::from_secs(30));
    loop {
        t.tick().await;
        let due: Vec<store::Job> = {
            let mut inner = d.lock();
            let n = now();
            let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut inner.store.jobs).into_iter().partition(|j| j.at <= n);
            inner.store.jobs = keep;
            if !due.is_empty() {
                let _ = inner.store.save(&d.dev);
            }
            due
        };
        for j in due {
            let log_dir = if j.key.is_empty() { paths::state_dir(&d.dev) } else { PathBuf::from(&j.key).join(".claude") };
            let _ = std::fs::create_dir_all(&log_dir);
            let log = OpenOptions::new().create(true).append(true).open(log_dir.join(format!(".{}-retry.log", j.kind)));
            let (Ok(exe), Ok(log)) = (std::env::current_exe(), log) else { continue };
            eprintln!("[{}] running scheduled {} {:?}", now(), j.kind, j.args);
            let mut cmd = Command::new(exe);
            cmd.arg(&j.kind).args(&j.args).stdin(Stdio::null()).stdout(log.try_clone().map(Stdio::from).unwrap_or(Stdio::null())).stderr(Stdio::from(log));
            let _ = win::no_window(&mut cmd).spawn();
        }
    }
}

/// "YYYY-MM-DD" or RFC 3339 → unix seconds.
pub fn usage_parse_date(s: &str) -> Option<i64> {
    if s.len() == 10 {
        return usage::parse_rfc3339(&format!("{s}T00:00:00Z"));
    }
    usage::parse_rfc3339(s)
}

pub fn usage_fetch_dir(dir: &std::path::Path, _last: &mut HashMap<String, Account>) -> Option<Account> {
    usage::fetch_dir(dir)
}

/// Unix seconds → local "Sun 14:05" (offset from the system clock; good enough for messages).
pub fn fmt_local(epoch: i64) -> String {
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let (mut l, mut u) = unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    unsafe {
        GetLocalTime(&mut l);
        GetSystemTime(&mut u);
    }
    let secs = |t: &windows_sys::Win32::Foundation::SYSTEMTIME| t.wDay as i64 * 86400 + t.wHour as i64 * 3600 + t.wMinute as i64 * 60;
    let mut off = secs(&l) - secs(&u);
    if off > 14 * 3600 { off -= 86400 * ((off + 43200) / 86400); }   // month-boundary wrap
    if off < -14 * 3600 { off += 86400 * ((-off + 43200) / 86400); }
    let t = epoch + off;
    let days = t.div_euclid(86400);
    let rem = t.rem_euclid(86400);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    format!("{wd} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}
