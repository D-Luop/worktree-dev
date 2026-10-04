//! Per-worktree file watching (ReadDirectoryChangesW via `notify`): a change schedules that worktree's
//! debounced `git status`, so git state follows real edits/commits instead of a timer. Watched:
//!   - the worktree itself, recursively (ignoring .git, node_modules, build output, status files)
//!   - its admin dir in the bare repo (`repos/<slug>/.bare/worktrees/<n>`: HEAD, index → commits)
//! The daemon drops a watch before it moves/removes a worktree (a watch handle blocks that on Windows).

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::Daemon;

/// Path components whose changes never affect `git status` for our purposes.
const IGNORED_DIRS: [&str; 11] = [".git", "node_modules", "target", "dist", "build", "out", ".next", ".turbo", ".cache", "__pycache__", ".venv"];

fn relevant(p: &Path) -> bool {
    let ignored_dir = p.components().any(|c| matches!(c, Component::Normal(n) if IGNORED_DIRS.iter().any(|i| n.eq_ignore_ascii_case(i))));
    let status_file = p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".claude-status"));
    !ignored_dir && !status_file
}

/// `<wt>/.git` is a file: `gitdir: <bare>/worktrees/<n>` — that admin dir holds HEAD and index.
fn admin_dir(wt: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(wt.join(".git")).ok()?;
    let p = crate::paths::normalize(text.trim().strip_prefix("gitdir:")?.trim());
    p.is_dir().then_some(p)
}

pub fn watch(d: &Arc<Daemon>, id: &str, path: &Path) -> Option<RecommendedWatcher> {
    let weak: Weak<Daemon> = Arc::downgrade(d);
    let id_cb = id.to_string();
    let admin = admin_dir(path);
    let admin_cb = admin.clone();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        let hit = ev.paths.iter().any(|p| admin_cb.as_ref().is_some_and(|a| p.starts_with(a)) || relevant(p));
        if !hit {
            return;
        }
        if let Some(d) = weak.upgrade() {
            let mut inner = d.lock();
            if inner.worktrees.contains_key(&id_cb) {
                Daemon::schedule_git(&mut inner, &id_cb, true);
            }
        }
    })
    .ok()?;
    if w.watch(path, RecursiveMode::Recursive).is_err() {
        return None;
    }
    if let Some(a) = admin {
        let _ = w.watch(&a, RecursiveMode::NonRecursive);
    }
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::relevant;
    use std::path::Path;

    #[test]
    fn filters_noise() {
        assert!(relevant(Path::new(r"D:\w\src\app.ts")));
        assert!(relevant(Path::new(r"D:\w\.claude\plans\active-plan.md")));
        assert!(!relevant(Path::new(r"D:\w\node_modules\x\index.js")));
        assert!(!relevant(Path::new(r"D:\w\web\.next\cache\a")));
        assert!(!relevant(Path::new(r"D:\w\.claude-status")));
        assert!(!relevant(Path::new(r"D:\w\Target\debug\x.exe")));
    }
}
