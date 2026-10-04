//! `wtd run [--kind agent] [--account <name>] -- <program> [args…]`
//!
//! Runs a session program (claude) inside a named Job Object so its whole process tree can be
//! measured and killed as one unit, and holds a daemon connection for the session's lifetime: the
//! daemon's liveness for this worktree *is* that connection. If the daemon isn't running the session
//! runs anyway and registers whenever the daemon comes up.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use wtd_core::protocol::{method, Push, ServerLine, SessionParams};

use crate::{client::Client, win};

pub fn main(args: &[String]) -> Result<i32> {
    let mut kind = "agent".to_string();
    let mut account: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => { kind = args.get(i + 1).cloned().context("--kind needs a value")?; i += 2; }
            "--account" => { account = args.get(i + 1).cloned().filter(|a| !a.is_empty() && a != "default"); i += 2; }
            "--" => { i += 1; break; }
            _ => break,
        }
    }
    let Some(program) = args.get(i) else { bail!("usage: wtd run [--kind k] [--account a] -- <program> [args…]") };
    let rest = &args[i + 1..];

    let job_name = win::job_name(&std::process::id().to_string());
    let job = match win::enter_new_job(&job_name) {
        Ok(j) => Some(Arc::new(j)),
        Err(e) => { eprintln!("wtd run: no job object ({e}); session will run untracked"); None }
    };
    win::ignore_ctrl_c_in_this_process();

    let exe = resolve_program(program);
    let mut child = Command::new(&exe).args(rest).spawn().with_context(|| format!("starting {}", exe.display()))?;

    let params = SessionParams {
        dir: std::env::current_dir()?.to_string_lossy().into(),
        kind,
        account,
        job: if job.is_some() { job_name } else { String::new() },
        pid: child.id(),
        program: exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()),
    };
    let job_for_thread = job.clone();
    std::thread::spawn(move || hold_registration(params, job_for_thread));

    let status = child.wait()?;
    Ok(status.code().unwrap_or(1))
}

/// Register with the daemon and stay connected; reconnect if the daemon restarts. Ends the session
/// (terminates the job, i.e. this process too) when the daemon says `terminate`.
fn hold_registration(params: SessionParams, job: Option<Arc<win::Handle>>) {
    loop {
        if let Ok(Some(mut c)) = Client::connect() {
            if c.hello("run").is_ok() && c.request(method::SESSION_REGISTER, &params).is_ok() {
                loop {
                    match c.read() {
                        Ok(Some(ServerLine::Push(Push::Terminate))) => {
                            if let Some(j) = &job {
                                win::terminate_own_job(j);
                            }
                            std::process::exit(1);
                        }
                        Ok(Some(_)) => continue,
                        _ => break, // daemon stopped → retry below
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Find the real executable for `program`. `claude` from npm is a `.cmd` shim that runs
/// `node_modules\@anthropic-ai\claude-code\bin\claude.exe`; a job-wrapped session should run that exe
/// directly rather than through cmd.exe.
pub fn resolve_program(program: &str) -> PathBuf {
    let p = crate::paths::normalize(program);
    if p.is_absolute() || program.contains(['/', '\\']) {
        return p;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let exe = dir.join(format!("{program}.exe"));
        if exe.is_file() {
            return exe;
        }
        let cmd = dir.join(format!("{program}.cmd"));
        if let Some(target) = cmd_shim_target(&cmd) {
            return target;
        }
    }
    p
}

fn cmd_shim_target(cmd: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(cmd).ok()?;
    let dir = cmd.parent()?;
    for line in text.lines() {
        // "%dp0%\node_modules\…\claude.exe"   %*
        if let Some(rest) = line.trim().strip_prefix("\"%dp0%\\") {
            let rel = rest.split('"').next()?;
            if rel.to_ascii_lowercase().ends_with(".exe") {
                let t = dir.join(rel);
                if t.is_file() {
                    return Some(t);
                }
            }
        }
    }
    None
}
