//! Discover worktrees on disk and read their cheap per-worktree facts (status, plan title).
//! Pure readdir/read work: no processes spawned.

use std::path::Path;

use wtd_core::model::{Worktree, WorktreeKind, DEV_ID};

use crate::statusfile;

/// Every worktree under `worktrees/` (a dir holding `.git`; names may nest, e.g. `feat/x`), skipping
/// `worktrees/<slug>/archive/`, plus the dev base itself (the assistant).
pub fn scan(dev: &Path) -> Vec<Worktree> {
    let mut out = vec![facts(dev, DEV_ID, WorktreeKind::Dev, "", "assistant", dev)];
    let root = dev.join("worktrees");
    let slugs = std::fs::read_dir(&root).into_iter().flatten().flatten();
    for slug in slugs.filter(|e| e.path().is_dir()) {
        let slug_name = slug.file_name().to_string_lossy().to_string();
        let kind = match slug_name.as_str() {
            "plan" => WorktreeKind::Plan,
            crate::paths::DELEGATE => WorktreeKind::Delegate,
            _ => WorktreeKind::Repo,
        };
        walk(dev, &slug.path(), &slug_name, "", kind, 0, &mut out);
    }
    for (id, w) in crate::platforms::wsl_worktrees(dev) {
        let (Some(distro), Some((slug, name))) = (w.platform.distro(), id.split_once('/')) else { continue };
        if out.iter().any(|x| x.id == id) {
            continue;
        }
        let unc = crate::platforms::unc_path(distro, &w.wsl_path);
        let mut wt = facts(dev, &id, WorktreeKind::Repo, slug, name, Path::new(&unc));
        wt.platform = w.platform;
        wt.wsl_path = Some(w.wsl_path);
        out.push(wt);
    }
    out
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
    fn wsl_worktrees_join_the_scan() {
        use wtd_core::model::Platform;
        let t = std::env::temp_dir().join(format!("wtd-scan-wsl-{}", std::process::id()));
        let wsl = crate::platforms::WslWorktree { platform: Platform::Wsl("Ubuntu".into()), wsl_path: "/home/dev/wt/app/feat/y".into() };
        crate::platforms::register_wsl(&t, "app/feat/y", Some(wsl)).unwrap();
        let w = scan(&t).into_iter().find(|w| w.id == "app/feat/y").expect("registered WSL worktree is listed");
        assert_eq!((w.slug.as_str(), w.name.as_str(), w.kind), ("app", "feat/y", WorktreeKind::Repo));
        assert_eq!(w.platform, Platform::Wsl("Ubuntu".into()));
        assert_eq!(w.wsl_path.as_deref(), Some("/home/dev/wt/app/feat/y"));
        assert_eq!(w.path, r"\\wsl.localhost\Ubuntu\home\dev\wt\app\feat\y");
        std::fs::remove_dir_all(&t).ok();
    }
}
