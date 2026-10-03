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
    let Ok(slugs) = std::fs::read_dir(&root) else { return out };
    for slug in slugs.flatten().filter(|e| e.path().is_dir()) {
        let slug_name = slug.file_name().to_string_lossy().to_string();
        let kind = if slug_name == "plan" { WorktreeKind::Plan } else { WorktreeKind::Repo };
        walk(dev, &slug.path(), &slug_name, "", kind, 0, &mut out);
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
    Worktree {
        id: id.into(),
        kind,
        slug: slug.into(),
        name: name.into(),
        path: dir.to_string_lossy().into(),
        status: statusfile::read(dir).status,
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
