//! `wtd top [--json] [--watch] [--refresh-disk]`: the fleet's CPU / memory / disk, per session.

use anyhow::{bail, Result};
use serde_json::json;
use wtd_core::model::Metrics;
use wtd_core::protocol::{method, Push, ServerLine};

use crate::client::Client;

/// Subscribe to metrics; the daemon samples only while a subscriber exists.
fn connect(refresh_disk: bool) -> Result<Client> {
    let mut c = Client::connect_required()?;
    if refresh_disk {
        c.request(method::RESOURCES_DISK_REFRESH, json!({}))?;
    }
    c.request(method::SUBSCRIBE, json!({ "metrics": true }))?;
    Ok(c)
}

/// Next metrics push. With `rated`, skips the first sample (no CPU deltas yet).
fn next(c: &mut Client, rated: bool) -> Result<Option<Metrics>> {
    loop {
        match c.read()? {
            Some(ServerLine::Push(Push::Metrics { metrics, .. })) if !rated || metrics.interval_ms > 0 => return Ok(Some(metrics)),
            Some(ServerLine::Push(Push::Shutdown)) | None => return Ok(None),
            Some(_) => continue,
        }
    }
}

/// One sample with CPU rates (waits a couple of seconds when nobody else is watching).
pub fn sample(refresh_disk: bool) -> Result<Metrics> {
    match next(&mut connect(refresh_disk)?, true)? {
        Some(m) => Ok(m),
        None => bail!("the daemon closed the connection before sending a resource sample"),
    }
}

pub fn main(args: &[String]) -> Result<i32> {
    let has = |f: &str| args.iter().any(|a| a == f);
    let (as_json, watch, refresh) = (has("--json"), has("--watch") || has("-w"), has("--refresh-disk"));
    if let Some(a) = args.iter().find(|a| !["--json", "--watch", "-w", "--refresh-disk"].contains(&a.as_str())) {
        bail!("unknown argument '{a}' (usage: wtd top [--json] [--watch] [--refresh-disk])");
    }
    if !watch {
        let m = sample(refresh)?;
        println!("{}", if as_json { serde_json::to_string_pretty(&m)? } else { render(&m) });
        return Ok(0);
    }
    let mut c = connect(refresh)?;
    while let Some(m) = next(&mut c, true)? {
        if as_json {
            println!("{}", serde_json::to_string(&m)?);
        } else {
            print!("\x1b[2J\x1b[H{}", render(&m));
        }
    }
    Ok(0)
}

pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0 => "0".into(),
        n if n < 1 << 20 => format!("{:.0}K", b / 1024.0),
        n if n < 1 << 30 => format!("{:.0}M", b / (1 << 20) as f64),
        _ => format!("{:.1}G", b / (1u64 << 30) as f64),
    }
}

fn mb(m: u64) -> String {
    size(m << 20)
}

fn ago(ts: i64) -> String {
    let s = (crate::daemon::now() - ts).max(0);
    if s < 90 { format!("{s}s ago") } else if s < 5400 { format!("{}m ago", s / 60) } else { format!("{}h ago", s / 3600) }
}

pub fn render(m: &Metrics) -> String {
    let mut o = String::new();
    let fleet_cpu: f64 = m.per_session.iter().map(|s| s.cpu_pct).sum();
    o += &format!(
        "fleet  {:.1}% CPU · {} RAM · disk ↓{}/s ↑{}/s      system  {:.0}% CPU of {} cores · {}/{} RAM\n\n",
        fleet_cpu, mb(m.mem_mb), size(m.read_bps), size(m.write_bps), m.sys_cpu_pct, m.ncpu, mb(m.sys_used_mb), mb(m.sys_total_mb)
    );
    o += &format!("{:<38} {:<9} {:>6} {:>7} {:>7} {:>5} {:>7} {:>7}  {}\n", "WORKTREE", "KIND", "CPU%", "MEM", "PRIV", "PROCS", "READ/s", "WRITE/s", "TOP PROCESSES");
    if m.per_session.is_empty() {
        o += "  (no live sessions)\n";
    }
    for s in &m.per_session {
        let top = s.top.iter().take(3).map(|p| format!("{} {:.1}%", p.name, p.cpu_pct)).collect::<Vec<_>>().join(", ");
        o += &format!(
            "{:<38} {:<9} {:>6.1} {:>7} {:>7} {:>5} {:>7} {:>7}  {}\n",
            s.worktree, s.kind, s.cpu_pct, mb(s.mem_mb), mb(s.private_mb), s.procs, size(s.read_bps), size(s.write_bps), top
        );
        if let Some(vm) = &s.vm {
            o += &format!("{:<38} {:<9} {:>6.1} {:>7}  (WSL VM {}, shared by every WSL session)\n", "", "  wsl vm", vm.cpu_pct, mb(vm.mem_mb), vm.name);
        }
    }
    if !m.others.is_empty() {
        o += "\nOTHER PROGRAMS (not the fleet)\n";
        for p in &m.others {
            let name = if p.count > 1 { format!("{} ×{}", p.name, p.count) } else { p.name.clone() };
            o += &format!("  {:<36} {:>6.1}% {:>7}\n", name, p.cpu_pct, mb(p.mem_mb));
        }
    }
    match &m.disk {
        None => o += "\nDISK  not scanned yet (runs in the background; `--refresh-disk` to force)\n",
        Some(d) => {
            let when = if d.busy { "scanning…".to_string() } else if d.ts == 0 { "not scanned yet".into() } else { format!("scanned {}", ago(d.ts)) };
            o += &format!("\nDISK  dev root {} · volume {} free of {}  ({when})\n", size(d.dev_root_bytes), size(d.volume_free_bytes), size(d.volume_total_bytes));
            for w in &d.worktrees {
                match w.bytes {
                    Some(b) => o += &format!("  {:<36} {:>7}  target {:>7}  node_modules {:>7}\n", w.id, size(b), size(w.target_bytes), size(w.node_modules_bytes)),
                    None => o += &format!("  {:<36} {:>7}  (inside WSL: not measured)\n", w.id, "?"),
                }
            }
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use wtd_core::model::{DiskUsage, ProcUsage, SessionUsage, WorktreeDisk};

    #[test]
    fn renders_sessions_others_and_disk() {
        let m = Metrics {
            ncpu: 8,
            mem_mb: 2048,
            sys_total_mb: 32768,
            per_session: vec![SessionUsage {
                worktree: "r/feat/x".into(),
                kind: "agent".into(),
                cpu_pct: 42.5,
                mem_mb: 2048,
                top: vec![ProcUsage { name: "rustc.exe".into(), pid: 9, cpu_pct: 40.0, mem_mb: 900, count: 1 }],
                ..Default::default()
            }],
            others: vec![ProcUsage { name: "Code.exe".into(), pid: 3, cpu_pct: 2.0, mem_mb: 1500, count: 12 }],
            disk: Some(DiskUsage { ts: 1, worktrees: vec![WorktreeDisk { id: "r/feat/x".into(), bytes: Some(5 << 30), target_bytes: 4 << 30, node_modules_bytes: 0 }], ..Default::default() }),
            ..Default::default()
        };
        let s = render(&m);
        assert!(s.contains("r/feat/x") && s.contains("rustc.exe 40.0%") && s.contains("42.5"), "{s}");
        assert!(s.contains("Code.exe ×12") && s.contains("target    4.0G"), "{s}");
        assert_eq!(size(1536), "2K");
        assert_eq!(size(3 << 30), "3.0G");
    }
}
