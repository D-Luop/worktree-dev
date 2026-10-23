//! Apply a staged version: stop the daemon + tray, point `current\` at `versions\<ver>\`, reinstall the
//! vsix if it changed, restart, health-check — and switch back if the new build doesn't come up.
//!
//! Runs in a helper process: a copy of the running (known-good) `wtd.exe` at `<program>\update-helper.exe`,
//! so neither the swap nor a broken new build can take down the code doing the rollback.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;

use super::{read_version, running_version, ApplyRecord, Layout, State};
use crate::{client::Client, daemon::now, settings, win};

pub const VSIX: &str = "claude-status.vsix";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER: &str = "update-helper.exe";

/// What `stop` found running, so `start` brings back the same.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Running {
    pub daemon: bool,
    pub tray: bool,
}

/// The processes around an apply, abstracted so apply/rollback can be tested on a temp program dir.
pub trait Host {
    fn stop(&mut self) -> Result<Running>;
    fn start(&mut self, l: &Layout, was: Running) -> Result<()>;
    /// The daemon from `current\` is up within the timeout and reports `ver`.
    fn healthy(&mut self, l: &Layout, ver: &str) -> bool;
    fn install_vsix(&mut self, vsix: &Path) -> Result<()>;
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Applied { from: String, to: String, vsix_changed: bool },
    RolledBack { from: String, to: String, reason: String },
}

pub fn apply(l: &Layout, ver: &str, host: &mut dyn Host) -> Result<Outcome> {
    let _lock = Lock::take(l)?;
    let from = l.current_version().unwrap_or_else(|| running_version().into());
    if read_version(&l.version_dir(ver)).as_deref() != Some(ver) {
        bail!("{ver} isn't staged");
    }
    keep_current(l, &from)?;
    let vsix_changed = differs(&l.current().join(VSIX), &l.version_dir(ver).join(VSIX));
    l.log(&format!("applying {from} → {ver} (vsix {})", if vsix_changed { "changed" } else { "unchanged" }));

    let was = host.stop()?;
    if let Err(e) = swap_current(l, ver) {
        l.log(&format!("swap failed: {e:#}"));
        let _ = swap_current(l, &from);
        let _ = host.start(l, was);
        record(l, &from, ver, false, false, &format!("{e:#}"), false)?;
        return Err(e);
    }
    if vsix_changed {
        if let Err(e) = host.install_vsix(&l.current().join(VSIX)) {
            l.log(&format!("vsix install failed: {e:#}"));
        }
    }
    let started = host.start(l, was);
    let reason = match started {
        Ok(()) if host.healthy(l, ver) => None,
        Ok(()) => Some(format!("{ver} failed its health check (daemon up and `wtd version` OK within {}s)", HEALTH_TIMEOUT.as_secs())),
        Err(e) => Some(format!("starting {ver}: {e:#}")),
    };
    match reason {
        None => {
            record(l, &from, ver, true, vsix_changed, "", false)?;
            tidy(l, &from, ver);
            l.log(&format!("applied {ver}"));
            Ok(Outcome::Applied { from, to: ver.into(), vsix_changed })
        }
        Some(reason) => {
            l.log(&format!("rolling back to {from}: {reason}"));
            let _ = host.stop();
            swap_current(l, &from).context("rolling back")?;
            if vsix_changed {
                let _ = host.install_vsix(&l.current().join(VSIX));
            }
            if let Err(e) = host.start(l, was) {
                l.log(&format!("restarting {from} after rollback: {e:#}"));
            }
            record(l, &from, ver, false, false, &reason, true)?;
            // never retried, so its download and files only take space
            let _ = settings::remove_tree(&l.pending_dir(ver));
            let _ = settings::remove_tree(&l.version_dir(ver));
            Ok(Outcome::RolledBack { from, to: ver.into(), reason })
        }
    }
}

fn record(l: &Layout, from: &str, to: &str, ok: bool, vsix_changed: bool, message: &str, failed: bool) -> Result<()> {
    State::update(l, |s| {
        s.last_apply = Some(ApplyRecord { from: from.into(), to: to.into(), at: now(), ok, vsix_changed, message: message.into() });
        if ok || failed {
            s.staged = None;
        }
        if failed && !s.failed.iter().any(|f| f == to) {
            s.failed.push(to.into());
        }
        if ok {
            s.error = None;
        }
    })?;
    Ok(())
}

fn differs(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (_, Err(_)) => false,
        (Ok(x), Ok(y)) => x != y,
        (Err(_), Ok(_)) => true,
    }
}

/// Make sure the version we're leaving survives in `versions\` (an installer may have laid `current\`
/// down as a plain folder), so there's something to roll back to.
fn keep_current(l: &Layout, from: &str) -> Result<()> {
    let keep = l.version_dir(from);
    if keep.exists() || is_link(&l.current()) || !l.current().is_dir() {
        return Ok(());
    }
    copy_tree(&l.current(), &keep).with_context(|| format!("keeping {from} for rollback"))
}

/// Drop the applied version's download and every version older than the one we left.
fn tidy(l: &Layout, from: &str, to: &str) {
    let _ = settings::remove_tree(&l.pending_dir(to));
    if let Ok(rd) = std::fs::read_dir(l.versions()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if super::is_newer(from, &name) {
                // a process still running from it keeps its exe; that's fine, it goes next time
                let _ = settings::remove_tree(&e.path());
            }
        }
    }
}

pub fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn junction(link: &Path, target: &Path) -> Result<()> {
    let out = win::no_window(Command::new("cmd").raw_arg(format!("/c mklink /J \"{}\" \"{}\"", link.display(), target.display())))
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() || !is_link(link) {
        bail!("mklink /J failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Point `current\` at `versions\<ver>\`: re-target the junction (the running exes inside the old
/// version's folder are untouched), or — if `current\` is a plain copy — move the running exes aside
/// and copy the new version over it.
pub fn swap_current(l: &Layout, ver: &str) -> Result<()> {
    let target = l.version_dir(ver);
    if read_version(&target).as_deref() != Some(ver) {
        bail!("{} doesn't hold {ver}", target.display());
    }
    let cur = l.current();
    let linked = is_link(&cur);
    if linked || !cur.exists() {
        let new = l.root.join("current.new");
        if is_link(&new) {
            std::fs::remove_dir(&new)?;
        }
        if junction(&new, &target).is_ok() {
            if linked {
                std::fs::remove_dir(&cur)?; // removes the junction, not its target
            }
            std::fs::rename(&new, &cur)?;
            return Ok(());
        }
        if linked {
            std::fs::remove_dir(&cur)?;
        }
    }
    replace_copy(&cur, &target)
}

fn replace_copy(cur: &Path, src: &Path) -> Result<()> {
    std::fs::create_dir_all(cur)?;
    let stamp = now();
    for f in files(cur) {
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.contains(".old-") {
            let _ = std::fs::remove_file(&f);
            continue;
        }
        // a running exe can't be deleted or overwritten, but it can be renamed
        if std::fs::remove_file(&f).is_err() {
            std::fs::rename(&f, f.with_file_name(format!("{name}.old-{stamp}"))).with_context(|| format!("moving {} aside", f.display()))?;
        }
    }
    copy_tree(src, cur)
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for f in files(src) {
        if f.file_name().is_some_and(|n| n.to_string_lossy().contains(".old-")) {
            continue;
        }
        let out = dst.join(f.strip_prefix(src)?);
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::copy(&f, &out).with_context(|| format!("copying {}", f.display()))?;
    }
    Ok(())
}

/// One apply at a time (daemon, tray start and `wtd update --apply-now` can all try).
struct Lock(PathBuf);

impl Lock {
    fn take(l: &Layout) -> Result<Lock> {
        std::fs::create_dir_all(&l.root)?;
        let p = l.root.join("update.lock");
        let stale = std::fs::metadata(&p).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(600));
        if stale {
            let _ = std::fs::remove_file(&p);
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(_) => Ok(Lock(p)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!("an update is already being applied"),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// --- the real processes -----------------------------------------------------------------------------

pub struct RealHost;

impl Host for RealHost {
    fn stop(&mut self) -> Result<Running> {
        let mut was = Running { daemon: false, tray: owns_tray() && crate::tray::is_running() };
        if let Some(mut c) = Client::connect()? {
            was.daemon = true;
            let _ = c.request(wtd_core::protocol::method::SHUTDOWN, json!({ "force": true }));
            wait_for(Duration::from_secs(15), || Client::connect().is_ok_and(|c| c.is_none()));
        }
        if was.tray {
            crate::tray::quit();
            wait_for(Duration::from_secs(10), || !crate::tray::is_running());
        }
        Ok(was)
    }

    fn start(&mut self, l: &Layout, was: Running) -> Result<()> {
        let bin = l.current().join("bin");
        // the health check needs the daemon, so it starts even if it wasn't running
        let st = win::no_window(Command::new(bin.join("wtd.exe")).args(["daemon", "start"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())).status()?;
        if !st.success() {
            bail!("`wtd daemon start` exited with {st}");
        }
        if owns_tray() && (was.tray || crate::tray::start_at_logon()) {
            let tray = bin.join("wtd-tray.exe");
            let mut cmd = if tray.is_file() {
                Command::new(tray)
            } else {
                let mut c = Command::new(bin.join("wtd.exe"));
                c.arg("tray");
                c
            };
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            let _ = win::spawn_detached(&mut cmd);
        }
        Ok(())
    }

    fn healthy(&mut self, l: &Layout, ver: &str) -> bool {
        let deadline = Instant::now() + HEALTH_TIMEOUT;
        let pipe_ok = wait_for(HEALTH_TIMEOUT, || {
            Client::connect().ok().flatten().and_then(|mut c| c.hello("updater").ok()).is_some_and(|h| h["version"].as_str() == Some(ver))
        });
        if !pipe_ok || Instant::now() > deadline {
            return false;
        }
        let out = win::no_window(Command::new(l.current().join("bin").join("wtd.exe")).arg("version").stdin(Stdio::null())).output();
        out.is_ok_and(|o| o.status.success() && serde_json::from_slice::<serde_json::Value>(&o.stdout).is_ok_and(|v| v["version"].as_str() == Some(ver)))
    }

    fn install_vsix(&mut self, vsix: &Path) -> Result<()> {
        if settings::which("code").is_none() {
            bail!("VS Code (`code`) isn't on PATH");
        }
        let out = win::no_window(Command::new("cmd").arg("/c").arg("code").arg("--install-extension").arg(vsix).arg("--force")).stdin(Stdio::null()).output()?;
        if !out.status.success() {
            bail!("code --install-extension: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

/// A test daemon (own `WTD_PIPE`) never touches the real tray.
fn owns_tray() -> bool {
    std::env::var_os("WTD_PIPE").is_none()
}

fn wait_for(limit: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    loop {
        if ok() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Start the apply helper for `ver`: a copy of this exe, so the swap never touches the code running it.
/// `wait` (CLI) runs it in this console; otherwise it's detached and logs to `update.log`.
pub fn spawn_helper(l: &Layout, ver: &str, wait: bool) -> Result<i32> {
    let helper = l.root.join(HELPER);
    std::fs::copy(std::env::current_exe()?, &helper).context("an update is already being applied (copying the helper failed)")?;
    let mut cmd = Command::new(&helper);
    cmd.args(["update", "__apply", ver]).env("WTD_PROGRAM_DIR", &l.root);
    if wait {
        return Ok(cmd.status()?.code().unwrap_or(1));
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(l.log_file())?;
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    win::spawn_detached(&mut cmd)?;
    Ok(0)
}

/// `wtd update __apply <ver>` (the helper).
pub fn helper_main(ver: &str) -> Result<i32> {
    let l = Layout::installed()?;
    match apply(&l, ver, &mut RealHost)? {
        Outcome::Applied { from, to, vsix_changed } => {
            println!("updated {from} → {to}{}", if vsix_changed { " (VS Code extension reinstalled: reload the window)" } else { "" });
            Ok(0)
        }
        Outcome::RolledBack { from, reason, .. } => {
            eprintln!("rolled back to {from}: {reason}");
            Ok(1)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::tests::temp;
    use super::*;

    /// Tests that set process env vars hold this.
    pub static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn version(l: &Layout, v: &str) {
        let d = l.version_dir(v);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::create_dir_all(d.join("templates")).unwrap();
        std::fs::write(d.join("bin").join("wtd.exe"), format!("wtd {v}")).unwrap();
        std::fs::write(d.join("templates").join(format!("only-{v}.md")), "x").unwrap();
        std::fs::write(d.join("version.txt"), format!("{v}\n")).unwrap();
        std::fs::write(d.join(VSIX), format!("vsix {v}")).unwrap();
    }

    #[derive(Default)]
    struct Fake {
        healthy: bool,
        log: Vec<String>,
    }

    impl Host for Fake {
        fn stop(&mut self) -> Result<Running> {
            self.log.push("stop".into());
            Ok(Running { daemon: true, tray: true })
        }
        fn start(&mut self, l: &Layout, _: Running) -> Result<()> {
            self.log.push(format!("start {}", l.current_version().unwrap()));
            Ok(())
        }
        fn healthy(&mut self, _: &Layout, ver: &str) -> bool {
            self.log.push(format!("health {ver}"));
            self.healthy
        }
        fn install_vsix(&mut self, vsix: &Path) -> Result<()> {
            self.log.push(std::fs::read_to_string(vsix).unwrap());
            Ok(())
        }
    }

    fn installed(tag: &str) -> Layout {
        let l = Layout::new(temp(tag));
        version(&l, "1.0.0");
        swap_current(&l, "1.0.0").unwrap();
        l
    }

    #[test]
    fn applies_through_the_junction() {
        let l = installed("apply");
        assert!(is_link(&l.current()));
        version(&l, "1.1.0");
        State::update(&l, |s| s.staged = Some("1.1.0".into())).unwrap();
        let mut h = Fake { healthy: true, ..Default::default() };
        let o = apply(&l, "1.1.0", &mut h).unwrap();
        assert_eq!(o, Outcome::Applied { from: "1.0.0".into(), to: "1.1.0".into(), vsix_changed: true });
        assert_eq!(h.log, ["stop", "vsix 1.1.0", "start 1.1.0", "health 1.1.0"]);
        assert!(is_link(&l.current()));
        assert_eq!(l.current_version().as_deref(), Some("1.1.0"));
        assert!(l.version_dir("1.0.0").is_dir(), "the previous version is kept for rollback");
        let st = State::load(&l);
        assert_eq!(st.staged, None);
        assert!(st.last_apply.as_ref().is_some_and(|a| a.ok && a.vsix_changed && a.to == "1.1.0"));
        assert!(!l.root.join("update.lock").exists());
        std::fs::remove_dir_all(&l.root).ok();
    }

    #[test]
    fn rolls_back_a_version_that_fails_its_health_check() {
        let l = installed("rollback");
        version(&l, "1.1.0");
        State::update(&l, |s| s.staged = Some("1.1.0".into())).unwrap();
        let mut h = Fake::default();
        let o = apply(&l, "1.1.0", &mut h).unwrap();
        assert!(matches!(o, Outcome::RolledBack { ref from, .. } if from == "1.0.0"), "{o:?}");
        assert_eq!(h.log, ["stop", "vsix 1.1.0", "start 1.1.0", "health 1.1.0", "stop", "vsix 1.0.0", "start 1.0.0"]);
        assert_eq!(l.current_version().as_deref(), Some("1.0.0"));
        let st = State::load(&l);
        assert_eq!(st.failed, ["1.1.0"]);
        assert_eq!(st.staged, None);
        assert!(st.last_apply.is_some_and(|a| !a.ok));
        assert_eq!(l.staged(), None, "a failed version isn't offered again");
        assert!(!l.version_dir("1.1.0").exists() && l.version_dir("1.0.0").is_dir());
        std::fs::remove_dir_all(&l.root).ok();
    }

    #[test]
    fn unchanged_vsix_is_not_reinstalled_and_old_versions_are_pruned() {
        let l = installed("prune");
        for v in ["1.1.0", "1.2.0"] {
            version(&l, v);
            std::fs::write(l.version_dir(v).join(VSIX), "same").unwrap();
        }
        std::fs::write(l.version_dir("1.0.0").join(VSIX), "same").unwrap();
        let mut h = Fake { healthy: true, ..Default::default() };
        apply(&l, "1.1.0", &mut h).unwrap();
        assert_eq!(h.log, ["stop", "start 1.1.0", "health 1.1.0"]);
        apply(&l, "1.2.0", &mut h).unwrap();
        assert!(!l.version_dir("1.0.0").exists());
        assert!(l.version_dir("1.1.0").is_dir() && l.version_dir("1.2.0").is_dir());
        std::fs::remove_dir_all(&l.root).ok();
    }

    #[test]
    fn plain_copy_current_moves_a_running_exe_aside() {
        let l = Layout::new(temp("copy"));
        version(&l, "1.0.0");
        copy_tree(&l.version_dir("1.0.0"), &l.current()).unwrap();
        settings::remove_tree(&l.version_dir("1.0.0")).unwrap();
        // a real running program at current\bin\wtd.exe
        let exe = l.current().join("bin").join("wtd.exe");
        std::fs::copy(PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32").join("PING.EXE"), &exe).unwrap();
        let mut child = Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(std::fs::remove_file(&exe).is_err(), "the exe is locked while it runs");

        version(&l, "1.1.0");
        let o = apply(&l, "1.1.0", &mut Fake { healthy: true, ..Default::default() }).unwrap();
        assert!(matches!(o, Outcome::Applied { .. }));
        assert!(!is_link(&l.current()));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "wtd 1.1.0");
        assert!(!l.current().join("templates").join("only-1.0.0.md").exists());
        assert!(files(&l.current().join("bin")).iter().any(|f| f.to_string_lossy().contains("wtd.exe.old-")));
        assert!(l.version_dir("1.0.0").join("version.txt").is_file(), "the copy was kept for rollback");
        child.kill().ok();
        child.wait().ok();
        std::fs::remove_dir_all(&l.root).ok();
    }

    #[test]
    fn one_apply_at_a_time() {
        let l = installed("lock");
        version(&l, "1.1.0");
        let held = Lock::take(&l).unwrap();
        assert!(apply(&l, "1.1.0", &mut Fake::default()).is_err());
        drop(held);
        assert!(apply(&l, "1.1.0", &mut Fake { healthy: true, ..Default::default() }).is_ok());
        std::fs::remove_dir_all(&l.root).ok();
    }
}
