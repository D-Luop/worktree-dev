//! Per-session resource sampling (CPU, memory, disk I/O, top processes) and the cached disk-usage
//! scan. Sampling runs only while someone subscribes to metrics; the disk scan never runs in that loop.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use wtd_core::model::{DiskUsage, Metrics, ProcUsage, SessionUsage, WorktreeDisk};

use crate::win::{self, JobSample, ProcSample};

const TOP_PER_SESSION: usize = 8;
const TOP_OTHERS: usize = 6;
const MB: u64 = 1024 * 1024;

/// What the sampler needs to know about one live session.
#[derive(Debug, Clone)]
pub struct SessionRef {
    pub worktree: String,
    pub kind: String,
    pub job: String,
    pub wsl: bool,
}

/// CPU used over a wall-clock interval, as a percent of all cores (0-100).
pub fn cpu_pct(cpu_delta_100ns: u64, wall_100ns: u64, ncpu: u32) -> f64 {
    if wall_100ns == 0 || ncpu == 0 {
        return 0.0;
    }
    (cpu_delta_100ns as f64 / wall_100ns as f64 / ncpu as f64 * 100.0).clamp(0.0, 100.0)
}

/// Bytes per second between two cumulative counters (0 if the counter went backwards).
pub fn rate(prev: u64, cur: u64, wall_100ns: u64) -> u64 {
    if wall_100ns == 0 {
        return 0;
    }
    (cur.saturating_sub(prev) as f64 * 10_000_000.0 / wall_100ns as f64) as u64
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn is_wsl_vm(name: &str) -> bool {
    name.eq_ignore_ascii_case("vmmemWSL") || name.eq_ignore_ascii_case("vmmem")
}

#[derive(Default)]
pub struct Sampler {
    prev_at: Option<Instant>,
    /// (idle, busy)
    prev_sys: (u64, u64),
    /// job → (cpu, read, write)
    prev_jobs: HashMap<String, (u64, u64, u64)>,
    /// (pid, created) → cpu
    prev_procs: HashMap<(u32, i64), u64>,
}

impl Sampler {
    /// Query the OS and build a sample. Blocking (a few ms); call off the async runtime.
    pub fn sample(&mut self, sessions: &[SessionRef], ncpu: u32) -> Metrics {
        let now = Instant::now();
        let sys = win::system_times();
        let jobs: Vec<Option<JobSample>> = sessions.iter().map(|s| if s.job.is_empty() { None } else { win::job_sample(&s.job) }).collect();
        let procs = win::processes();
        let (total, used) = win::system_memory_mb();
        let mut m = self.build(now, sys, sessions, jobs, procs, ncpu, total);
        m.sys_total_mb = total;
        m.sys_used_mb = used;
        m
    }

    /// The pure half of `sample`: diff against the previous sample and aggregate.
    #[allow(clippy::too_many_arguments)]
    pub fn build(&mut self, now: Instant, sys: (u64, u64), sessions: &[SessionRef], jobs: Vec<Option<JobSample>>, procs: Vec<ProcSample>, ncpu: u32, sys_total_mb: u64) -> Metrics {
        let wall = self.prev_at.map(|p| now.duration_since(p).as_nanos() as u64 / 100).unwrap_or(0);
        let first = self.prev_at.is_none();

        let mut proc_cpu: HashMap<u32, f64> = HashMap::with_capacity(procs.len());
        let mut next_procs = HashMap::with_capacity(procs.len());
        for p in &procs {
            let key = (p.pid, p.created);
            // a process absent from the previous system-wide snapshot started within this interval
            let delta = if first { 0 } else { p.cpu.saturating_sub(self.prev_procs.get(&key).copied().unwrap_or(0)) };
            proc_cpu.insert(p.pid, cpu_pct(delta, wall, ncpu));
            next_procs.insert(key, p.cpu);
        }
        let by_pid: HashMap<u32, &ProcSample> = procs.iter().map(|p| (p.pid, p)).collect();
        let usage = |p: &ProcSample| ProcUsage { name: p.name.clone(), pid: p.pid, cpu_pct: round1(proc_cpu.get(&p.pid).copied().unwrap_or(0.0)), mem_mb: p.working_set / MB, count: 1 };

        let vm = {
            let vms: Vec<&ProcSample> = procs.iter().filter(|p| is_wsl_vm(&p.name)).collect();
            group(vms.iter().map(|p| usage(p)))
        };

        let mut in_jobs: HashSet<u32> = HashSet::new();
        let mut next_jobs = HashMap::new();
        let mut per_session = Vec::with_capacity(sessions.len());
        for (s, j) in sessions.iter().zip(jobs) {
            let mut u = SessionUsage { worktree: s.worktree.clone(), kind: s.kind.clone(), ..Default::default() };
            if let Some(j) = j {
                if let Some(&(pc, pr, pw)) = self.prev_jobs.get(&s.job) {
                    u.cpu_pct = round1(cpu_pct(j.cpu.saturating_sub(pc), wall, ncpu));
                    u.read_bps = rate(pr, j.read_bytes, wall);
                    u.write_bps = rate(pw, j.write_bytes, wall);
                }
                next_jobs.insert(s.job.clone(), (j.cpu, j.read_bytes, j.write_bytes));
                let mut mine: Vec<ProcUsage> = vec![];
                for pid in &j.pids {
                    let Some(p) = by_pid.get(pid) else { continue };
                    in_jobs.insert(*pid);
                    u.mem_mb += p.working_set;
                    u.private_mb += p.private;
                    u.procs += 1;
                    mine.push(usage(p));
                }
                u.mem_mb /= MB;
                u.private_mb /= MB;
                mine.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(b.mem_mb.cmp(&a.mem_mb)));
                mine.truncate(TOP_PER_SESSION);
                u.top = mine;
            }
            if s.wsl {
                u.vm = vm.clone();
            }
            per_session.push(u);
        }
        per_session.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(b.mem_mb.cmp(&a.mem_mb)));

        let others = top_others(procs.iter().filter(|p| p.pid != 0 && !in_jobs.contains(&p.pid)).map(usage), sys_total_mb);

        let agents = sessions.iter().filter(|s| s.kind == "agent" || s.kind == "assistant").count() as u32;
        let fleet_cpu: f64 = per_session.iter().map(|s| s.cpu_pct).sum();
        let sys_cpu = {
            let (pi, pb) = self.prev_sys;
            let (di, db) = (sys.0.saturating_sub(pi), sys.1.saturating_sub(pb));
            if first || di + db == 0 { 0.0 } else { round1(db as f64 * 100.0 / (di + db) as f64) }
        };
        let m = Metrics {
            sessions: sessions.len() as u32,
            agents,
            reviews: sessions.len() as u32 - agents,
            cpu_pct: (fleet_cpu * ncpu as f64).round(),
            mem_mb: per_session.iter().map(|s| s.mem_mb).sum(),
            ncpu,
            ts: super::now(),
            interval_ms: wall / 10_000,
            sys_cpu_pct: sys_cpu,
            read_bps: per_session.iter().map(|s| s.read_bps).sum(),
            write_bps: per_session.iter().map(|s| s.write_bps).sum(),
            per_session,
            others,
            ..Default::default()
        };
        self.prev_at = Some(now);
        self.prev_sys = sys;
        self.prev_jobs = next_jobs;
        self.prev_procs = next_procs;
        m
    }
}

/// Sum same-named processes (one row for all of VS Code's helpers); pid = the biggest one's.
fn group(items: impl Iterator<Item = ProcUsage>) -> Option<ProcUsage> {
    items.fold(None, |acc: Option<ProcUsage>, p| match acc {
        None => Some(p),
        Some(mut a) => {
            if p.mem_mb > a.mem_mb {
                a.pid = p.pid;
            }
            a.cpu_pct = round1(a.cpu_pct + p.cpu_pct);
            a.mem_mb += p.mem_mb;
            a.count += 1;
            Some(a)
        }
    })
}

/// Busiest non-fleet programs, grouped by name, ranked by CPU share plus memory share.
fn top_others(items: impl Iterator<Item = ProcUsage>, sys_total_mb: u64) -> Vec<ProcUsage> {
    let mut by_name: HashMap<String, Vec<ProcUsage>> = HashMap::new();
    for p in items {
        by_name.entry(p.name.to_lowercase()).or_default().push(p);
    }
    let mut v: Vec<ProcUsage> = by_name.into_values().filter_map(|ps| group(ps.into_iter())).collect();
    let score = |p: &ProcUsage| p.cpu_pct + p.mem_mb as f64 * 100.0 / sys_total_mb.max(1) as f64;
    v.sort_by(|a, b| score(b).total_cmp(&score(a)));
    v.truncate(TOP_OTHERS);
    v
}

// --- disk usage -------------------------------------------------------------------------------------

/// Size on disk of a tree, with `target/` and `node_modules/` directories (at any depth) broken out.
/// Reparse points (junctions, symlinks: pnpm stores, WSL links) are not followed, and a file with
/// several hard links (cargo links each binary into `deps/`) counts once across `seen`, like `du`.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct TreeSize {
    pub total: u64,
    pub target: u64,
    pub node_modules: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Bucket {
    Plain,
    Target,
    NodeModules,
}

pub fn tree_size(root: &Path, skip: &HashSet<PathBuf>, seen: &mut HashSet<i64>) -> TreeSize {
    let mut t = TreeSize::default();
    let mut stack = vec![(root.to_path_buf(), Bucket::Plain)];
    while let Some((dir, bucket)) = stack.pop() {
        for e in win::list_dir(&dir) {
            if e.reparse {
                continue;
            }
            if e.dir {
                let p = dir.join(&e.name);
                if skip.contains(&p) {
                    continue;
                }
                let b = match (bucket, e.name.as_str()) {
                    (Bucket::Plain, "target") => Bucket::Target,
                    (Bucket::Plain, "node_modules") => Bucket::NodeModules,
                    (b, _) => b,
                };
                stack.push((p, b));
            } else if seen.insert(e.file_id) {
                t.total += e.allocated;
                match bucket {
                    Bucket::Target => t.target += e.allocated,
                    Bucket::NodeModules => t.node_modules += e.allocated,
                    Bucket::Plain => {}
                }
            }
        }
    }
    t
}

/// Walk every Windows worktree, then the rest of the dev root. Slow (seconds to minutes).
/// `worktrees`: (id, path, measurable). WSL worktrees aren't measurable from Windows at a sane speed.
pub fn scan_disk(dev: &Path, worktrees: &[(String, PathBuf, bool)]) -> DiskUsage {
    let mut skip = HashSet::new();
    let mut seen = HashSet::new();
    let mut rows = vec![];
    let mut sum = 0;
    for (id, path, measurable) in worktrees {
        if !*measurable {
            rows.push(WorktreeDisk { id: id.clone(), bytes: None, ..Default::default() });
            continue;
        }
        let t = tree_size(path, &HashSet::new(), &mut seen);
        skip.insert(path.clone());
        sum += t.total;
        rows.push(WorktreeDisk { id: id.clone(), bytes: Some(t.total), target_bytes: t.target, node_modules_bytes: t.node_modules });
    }
    let rest = tree_size(dev, &skip, &mut seen).total;
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
    let (free, total) = win::disk_space(dev);
    DiskUsage { ts: super::now(), busy: false, dev_root_bytes: sum + rest, volume_free_bytes: free, volume_total_bytes: total, worktrees: rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cpu_delta_math() {
        // one full core for one second on a 4-core machine = 25% of all cores
        assert_eq!(cpu_pct(10_000_000, 10_000_000, 4), 25.0);
        assert_eq!(cpu_pct(40_000_000, 10_000_000, 4), 100.0);
        assert_eq!(cpu_pct(80_000_000, 10_000_000, 4), 100.0, "clamped");
        assert_eq!(cpu_pct(5, 0, 4), 0.0, "no interval yet");
        assert_eq!(rate(1000, 3000, 20_000_000), 1000, "2000 bytes over 2 s");
        assert_eq!(rate(3000, 1000, 10_000_000), 0, "counter reset");
    }

    fn proc(pid: u32, name: &str, cpu: u64, ws_mb: u64) -> ProcSample {
        ProcSample { pid, name: name.into(), created: pid as i64 * 7, cpu, working_set: ws_mb * MB, private: ws_mb * MB / 2 }
    }

    fn job(cpu: u64, read: u64, write: u64, pids: &[u32]) -> Option<JobSample> {
        Some(JobSample { cpu, read_bytes: read, write_bytes: write, pids: pids.to_vec() })
    }

    #[test]
    fn aggregates_sessions_processes_and_others() {
        let sessions = vec![
            SessionRef { worktree: "r/a".into(), kind: "agent".into(), job: "ja".into(), wsl: false },
            SessionRef { worktree: "r/b".into(), kind: "review".into(), job: "jb".into(), wsl: true },
        ];
        let mut s = Sampler::default();
        let t0 = Instant::now();
        let first = s.build(t0, (0, 0), &sessions, vec![job(0, 0, 0, &[10]), job(0, 0, 0, &[20])],
            vec![proc(10, "claude.exe", 0, 300), proc(20, "wsl.exe", 0, 10), proc(30, "Code.exe", 0, 500), proc(40, "VmmemWSL", 0, 4000)], 2, 16_000);
        assert_eq!(first.interval_ms, 0);
        assert!(first.per_session.iter().all(|u| u.cpu_pct == 0.0));

        // one second later: job a burned a full core (incl. a rustc that already exited), b nothing
        let t1 = t0 + Duration::from_secs(1);
        let m = s.build(t1, (5_000_000, 15_000_000), &sessions,
            vec![job(10_000_000, 0, 4 * MB, &[10, 11]), job(0, 0, 0, &[20])],
            vec![
                proc(10, "claude.exe", 1_000_000, 300),
                proc(11, "cargo.exe", 4_000_000, 50), // new this interval: all its time counts
                proc(20, "wsl.exe", 0, 10),
                proc(30, "Code.exe", 2_000_000, 500),
                proc(31, "Code.exe", 2_000_000, 300),
                proc(40, "VmmemWSL", 6_000_000, 4000),
            ], 2, 16_000);
        assert_eq!(m.interval_ms, 1000);
        assert_eq!(m.sys_cpu_pct, 75.0);
        let a = &m.per_session[0];
        assert_eq!((a.worktree.as_str(), a.cpu_pct, a.mem_mb, a.private_mb, a.procs), ("r/a", 50.0, 350, 175, 2));
        assert_eq!(a.write_bps, 4 * MB);
        assert_eq!(a.top[0].name, "cargo.exe");
        assert_eq!(a.top[0].cpu_pct, 20.0);
        assert!(a.vm.is_none());
        let b = &m.per_session[1];
        assert_eq!(b.vm.as_ref().map(|v| (v.name.as_str(), v.cpu_pct, v.mem_mb)), Some(("VmmemWSL", 30.0, 4000)));
        // legacy fields: percent of one core, summed working set
        assert_eq!((m.cpu_pct, m.mem_mb, m.sessions, m.agents, m.reviews), (100.0, 360, 2, 1, 1));
        // non-fleet: VS Code's helpers summed into one row; the VM leads (most memory + CPU)
        assert_eq!(m.others[0].name, "VmmemWSL");
        let code = m.others.iter().find(|o| o.name == "Code.exe").unwrap();
        assert_eq!((code.cpu_pct, code.mem_mb, code.count, code.pid), (20.0, 800, 2, 30));
        assert!(!m.others.iter().any(|o| o.pid == 10 || o.pid == 11 || o.pid == 20));

        // a session that ended drops out of the diff state
        let m2 = s.build(t1 + Duration::from_secs(1), (5_000_000, 15_000_000), &sessions[..1], vec![job(10_000_000, 0, 4 * MB, &[10])], vec![proc(10, "claude.exe", 1_000_000, 300)], 2, 16_000);
        assert_eq!(m2.per_session[0].cpu_pct, 0.0);
        assert!(!s.prev_jobs.contains_key("jb"));
    }

    #[test]
    fn disk_tree_buckets() {
        const K: usize = 4096; // whole clusters, so size on disk = file size
        let t = std::env::temp_dir().join(format!("wtd-disk-{}", std::process::id()));
        let w = |rel: &str, n: usize| {
            let p = t.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![b'x'; n * K]).unwrap();
        };
        w("src/main.rs", 1);
        w("wtd/target/debug/x.exe", 10);
        std::fs::create_dir_all(t.join("wtd/target/debug/deps")).unwrap();
        std::fs::hard_link(t.join("wtd/target/debug/x.exe"), t.join("wtd/target/debug/deps/x-123.exe")).unwrap();
        w("ui/node_modules/a/index.js", 3);
        w("ui/node_modules/a/node_modules/b/i.js", 2);
        w("ui/node_modules/target/y", 1); // inside node_modules: stays node_modules
        w("skipme/big", 50);
        let skip = HashSet::from([t.join("skipme")]);
        let k = K as u64;
        assert_eq!(tree_size(&t, &skip, &mut HashSet::new()), TreeSize { total: 17 * k, target: 10 * k, node_modules: 6 * k });

        let d = scan_disk(&t, &[("r/src".into(), t.join("src"), true), ("r/wsl".into(), t.join("nope"), false)]);
        assert_eq!(d.worktrees[0].id, "r/src");
        assert_eq!(d.worktrees[0].bytes, Some(k));
        assert_eq!(d.worktrees[1].bytes, None);
        assert_eq!(d.dev_root_bytes, 67 * k);
        assert!(d.volume_total_bytes > 0 && d.ts > 0);
        std::fs::remove_dir_all(&t).ok();
    }
}
