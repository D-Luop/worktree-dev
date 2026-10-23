//! Discover worktrees on disk and read their cheap per-worktree facts (status, plan title).
//! Pure readdir/read work: no processes spawned.

use std::path::Path;

use wtd_core::model::{Worktree, WorktreeKind, DEV_ID};

use crate::statusfile;

/// Every worktree under `worktrees/` (a dir holding `.git`; names may nest, e.g. `feat/x`), skipping
/// `worktrees/<slug>/archive/`, plus the dev base itself (the assistant).
pub fn scan(dev: &Path) -> Vec<Worktree> {
    let mut out = vec![facts(dev, DEV_ID, WorktreeKind::Dev, "", "assistant", dev)];
    let slugs = std::fs::read_dir(dev.join("worktrees")).into_iter().flatten().flatten();
    for slug in slugs.filter(|e| e.path().is_dir()) {
        let slug_name = slug.file_name().to_string_lossy().to_string();
        let kind = match slug_name.as_str() {
            "plan" => WorktreeKind::Plan,
            crate::paths::DELEGATE => WorktreeKind::Delegate,
            _ => WorktreeKind::Repo,
        };
        walk(dev, &slug.path(), &slug_name, "", kind, 0, &mut out);
    }
    let seen: std::collections::HashSet<String> = out.iter().map(|w| w.id.clone()).collect();
    out.extend(wsl(dev).into_iter().filter(|w| !seen.contains(&w.id)));
    out
}

/// Registered WSL worktrees. Their files are read through `\\wsl.localhost` only while the distro
/// runs (touching it would boot it); otherwise the status comes from the Windows-side mirror.
fn wsl(dev: &Path) -> Vec<Worktree> {
    let entries: Vec<_> = crate::wsl_backend::load(dev).into_iter().filter(|e| !e.archived).collect();
    if entries.is_empty() {
        return vec![];
    }
    let running = crate::wsl_backend::running_cached();
    entries
        .into_iter()
        .map(|e| {
            let unc = e.unc();
            let up = running.iter().any(|r| r.eq_ignore_ascii_case(&e.distro));
            let mut w = if up {
                facts(dev, &e.id, WorktreeKind::Repo, &e.slug, &e.name, &unc)
            } else {
                let mirror = crate::paths::state_dir(dev).join("status").join(wtd_core::model::id_to_key(&e.id));
                let status = std::fs::read_to_string(mirror).map(|s| wtd_core::status::Status::parse(&s)).unwrap_or_default();
                Worktree { id: e.id.clone(), kind: WorktreeKind::Repo, slug: e.slug.clone(), name: e.name.clone(), path: unc.to_string_lossy().into(), status, ..Default::default() }
            };
            w.platform = e.platform();
            w.wsl_path = Some(e.wsl_path);
            w
        })
        .collect()
}

fn walk(dev: &Path, dir: &Path, slug: &str, rel: &str, kind: WorktreeKind, depth: usize, out: &mut Vec<Worktree>) {
    if depth > 4 {
        return;
    }
    if !rel.is_empty() && dir.join(".git").exists() {
        let id = format!("{slug}/{rel}");
        out.push(facts(dev, &id, kind, slug, rel, dir));
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if rel.is_empty() && name == "archive" {
            continue;
        }
        if name.starts_with('.') || !e.path().is_dir() {
            continue;
        }
        let child_rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        walk(dev, &e.path(), slug, &child_rel, kind, depth + 1, out);
    }
}

fn facts(_dev: &Path, id: &str, kind: WorktreeKind, slug: &str, name: &str, dir: &Path) -> Worktree {
    let st = statusfile::read(dir);
    Worktree {
        id: id.into(),
        kind,
        slug: slug.into(),
        name: name.into(),
        path: dir.to_string_lossy().into(),
        status: st.status,
        limit_resets_at: st.resets_at,
        plan_title: plan_title(dir),
        ..Default::default()
    }
}

fn plan_title(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(".claude").join("plans").join("active-plan.md")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_slug() {
        let t = std::env::temp_dir().join(format!("wtd-scan-{}", std::process::id()));
        for (slug, name) in [("delegate", "luop"), ("plan", "app"), ("luop", "feat/x")] {
            let p = name.split('/').fold(t.join("worktrees").join(slug), |p, c| p.join(c));
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join(".git"), "").unwrap();
        }
        let kinds: std::collections::HashMap<String, WorktreeKind> = scan(&t).into_iter().map(|w| (w.id, w.kind)).collect();
        assert_eq!(kinds["delegate/luop"], WorktreeKind::Delegate);
        assert_eq!(kinds["plan/app"], WorktreeKind::Plan);
        assert_eq!(kinds["luop/feat/x"], WorktreeKind::Repo);
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn registered_wsl_worktrees_without_touching_a_stopped_distro() {
        let t = std::env::temp_dir().join(format!("wtd-scan-wsl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let distro = "NoSuchDistro-wtd-test";
        crate::wsl_backend::upsert(&t, crate::wsl_backend::Entry {
            id: "r/feat/w".into(), slug: "r".into(), name: "feat/w".into(), distro: distro.into(),
            wsl_path: "/home/u/dev/worktree-dev/worktrees/r/feat/w".into(), archived: false,
        }).unwrap();
        let mirror = crate::paths::state_dir(&t).join("status");
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::write(mirror.join("r__feat__w"), "working").unwrap();
        let w = scan(&t).into_iter().find(|w| w.id == "r/feat/w").unwrap();
        assert_eq!((w.slug.as_str(), w.name.as_str(), w.kind), ("r", "feat/w", WorktreeKind::Repo));
        assert_eq!(w.platform, wtd_core::model::Platform::Wsl(distro.into()));
        assert_eq!(w.status, wtd_core::status::Status::Working);
        assert_eq!(w.wsl_path.as_deref(), Some("/home/u/dev/worktree-dev/worktrees/r/feat/w"));
        assert!(w.path.starts_with(r"\\wsl.localhost\NoSuchDistro-wtd-test\home"), "{}", w.path);
        std::fs::remove_dir_all(&t).ok();
    }
}
