//! The daemon's side of updates: check on startup and every ~6h, download in the background, and apply
//! a staged version at a safe point (no hosted session live, or `restartWithLiveSessions: auto`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::time::Instant;
use wtd_core::protocol::Push;

use super::{now, Daemon};
use crate::updater::{self, apply, safe_to_apply, Layout, Settings};

const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const JITTER: u64 = 30 * 60;
const TICK: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct Updates {
    live: Mutex<Live>,
    /// one check/download at a time
    checking: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Live {
    phase: &'static str,
    progress: Option<(u64, u64)>,
    applying: bool,
    pushed: Value,
}

/// ±30 min, so a fleet of machines doesn't hit the release server in lockstep.
fn jitter() -> Duration {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0) ^ std::process::id() as u64;
    Duration::from_secs(seed % (2 * JITTER))
}

fn hosted(d: &Daemon) -> usize {
    d.lock().sessions.values().filter(|s| s.hosted.is_some()).count()
}

pub(super) fn status(d: &Daemon) -> Value {
    let (phase, progress, applying) = {
        let l = d.updates.live.lock().unwrap_or_else(|p| p.into_inner());
        (l.phase, l.progress, l.applying)
    };
    let phase = if applying { "applying" } else if phase.is_empty() { "idle" } else { phase };
    let mut v = updater::status_json(phase, progress);
    if v["phase"] == "idle" && !v["staged"].is_null() {
        v["phase"] = json!("ready");
    }
    v["hosted_sessions"] = json!(hosted(d));
    v
}

/// Push the status to subscribers if it changed.
pub(super) fn broadcast(d: &Daemon) -> Value {
    let v = status(d);
    let mut l = d.updates.live.lock().unwrap_or_else(|p| p.into_inner());
    if l.pushed != v {
        l.pushed = v.clone();
        let _ = d.tx.send(Push::Update { update: v.clone() });
    }
    v
}

fn set_phase(d: &Daemon, phase: &'static str, progress: Option<(u64, u64)>) {
    {
        let mut l = d.updates.live.lock().unwrap_or_else(|p| p.into_inner());
        // progress pushes only on whole-percent steps
        let pct = |p: Option<(u64, u64)>| p.map(|(a, t)| if t > 0 { a * 100 / t } else { 0 });
        if l.phase == phase && pct(l.progress) == pct(progress) {
            return;
        }
        l.phase = phase;
        l.progress = progress;
    }
    broadcast(d);
}

/// Check and download now; returns the status afterwards.
pub(super) async fn check(d: &Arc<Daemon>) -> Result<Value> {
    let Some(l) = updater::installed_layout() else { return Ok(status(d)) };
    let _one = d.updates.checking.lock().await;
    set_phase(d, "checking", None);
    let (d2, channel) = (d.clone(), Settings::load().channel);
    let r = tokio::task::spawn_blocking(move || updater::check_and_stage(&l, channel, &|done, total| set_phase(&d2, "downloading", Some((done, total))))).await;
    set_phase(d, "idle", None);
    match r {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => eprintln!("[{}] update check: {e:#}", now()),
        Err(e) => eprintln!("[{}] update check panicked: {e}", now()),
    }
    Ok(broadcast(d))
}

/// Hand the staged version to the apply helper (it stops this daemon). `force` ends live hosted sessions.
pub(super) fn apply_now(d: &Arc<Daemon>, force: bool) -> Result<Value> {
    let Some(l) = updater::installed_layout() else { bail!("dev build: updates are off") };
    let Some(ver) = l.staged() else { bail!("no update is staged") };
    let n = hosted(d);
    if n > 0 && !force {
        bail!("{n} hosted session(s) are live and would end — confirm to update now");
    }
    start_apply(d, &l, &ver)?;
    Ok(json!({ "version": ver }))
}

fn start_apply(d: &Arc<Daemon>, l: &Layout, ver: &str) -> Result<()> {
    {
        let mut live = d.updates.live.lock().unwrap_or_else(|p| p.into_inner());
        if live.applying {
            bail!("an update is already being applied");
        }
        live.applying = true;
    }
    broadcast(d);
    eprintln!("[{}] applying update {ver}", now());
    if let Err(e) = apply::spawn_helper(l, ver, false) {
        d.updates.live.lock().unwrap_or_else(|p| p.into_inner()).applying = false;
        broadcast(d);
        return Err(e);
    }
    Ok(())
}

pub(super) async fn update_loop(d: Arc<Daemon>) {
    let Some(l) = updater::installed_layout() else { return };
    let mut next_check = Instant::now() + FIRST_CHECK;
    loop {
        tokio::time::sleep(TICK).await;
        let s = Settings::load();
        if s.auto && Instant::now() >= next_check {
            let _ = check(&d).await;
            next_check = Instant::now() + CHECK_EVERY - Duration::from_secs(JITTER) + jitter();
        }
        if let Some(ver) = l.staged() {
            if safe_to_apply(&s, hosted(&d)) && !d.updates.live.lock().unwrap_or_else(|p| p.into_inner()).applying {
                if let Err(e) = start_apply(&d, &l, &ver) {
                    eprintln!("[{}] applying {ver}: {e:#}", now());
                }
            }
        }
        broadcast(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::LivePolicy;

    #[test]
    fn safe_point_policy() {
        let s = |auto, live| Settings { auto, live, channel: updater::Channel::Stable };
        assert!(safe_to_apply(&s(true, LivePolicy::Never), 0));
        assert!(!safe_to_apply(&s(true, LivePolicy::Never), 1));
        assert!(!safe_to_apply(&s(true, LivePolicy::Ask), 2));
        assert!(safe_to_apply(&s(true, LivePolicy::Auto), 2));
        assert!(!safe_to_apply(&s(false, LivePolicy::Auto), 0), "auto off: only an explicit Restart to update");
    }

    #[test]
    fn jitter_stays_in_range() {
        assert!(jitter() < Duration::from_secs(2 * JITTER));
    }
}
