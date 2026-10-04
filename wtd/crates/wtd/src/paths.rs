//! Locating the dev root and mapping folders to worktree ids.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use wtd_core::model::DEV_ID;

/// The dev base: the folder holding `.wtd/`, `worktrees/`, `repos/`.
/// Resolution: `WTD_DEV` env → the exe's own location (`<dev>/.wtd/bin/wtd.exe`) → the pin file that
/// `install.sh` writes (`~/.config/wtd/dev-root`).
pub fn dev_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("WTD_DEV") {
        let p = PathBuf::from(p);
        if p.join(".wtd").is_dir() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        // <dev>/.wtd/bin/wtd.exe
        if let Some(dev) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            if exe.parent().and_then(Path::parent).is_some_and(|w| w.file_name().is_some_and(|n| n == ".wtd"))
                && dev.join(".wtd").is_dir()
            {
                return Ok(dev.to_path_buf());
            }
        }
    }
    let pin = home_dir()?.join(".config").join("wtd").join("dev-root");
    if let Ok(s) = std::fs::read_to_string(&pin) {
        let p = PathBuf::from(s.trim());
        if p.join(".wtd").is_dir() {
            return Ok(p);
        }
    }
    bail!("can't find the worktree-dev root (set WTD_DEV, or run install.sh)")
}

/// A sibling program next to the running one (`wtd.exe` / `wtd-tray.exe` live side by side).
pub fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).unwrap_or_else(|| PathBuf::from(name))
}
pub fn wtd_exe() -> PathBuf {
    sibling_exe("wtd.exe")
}
/// The windowless tray program, if it's installed.
pub fn tray_exe() -> Option<PathBuf> {
    Some(sibling_exe("wtd-tray.exe")).filter(|p| p.is_file())
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).context("no USERPROFILE/HOME")
}

pub fn user_name() -> String {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "user".into())
}

/// The daemon's pipe. `WTD_PIPE` overrides it so a test daemon (with its own `WTD_DEV`) can run
/// beside the real one without touching it.
pub fn pipe_name() -> String {
    match std::env::var("WTD_PIPE") {
        Ok(p) if !p.is_empty() => format!(r"\\.\pipe\{p}"),
        _ => wtd_core::protocol::pipe_name(&user_name()),
    }
}

pub fn state_dir(dev: &Path) -> PathBuf {
    dev.join(".wtd").join("state")
}

/// Accept MSYS-style (`/d/dev/x`) and forward-slash paths from Git Bash callers.
pub fn normalize(p: &str) -> PathBuf {
    let b = p.as_bytes();
    let s = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
        format!("{}:{}", (b[1] as char).to_ascii_uppercase(), &p[2..])
    } else if b.len() == 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() {
        format!("{}:/", (b[1] as char).to_ascii_uppercase())
    } else {
        p.to_string()
    };
    PathBuf::from(s.replace('/', "\\"))
}

fn eq_ci(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

fn starts_with_ci(p: &Path, base: &Path) -> Option<PathBuf> {
    let ps = p.to_string_lossy().to_string();
    let bs = base.to_string_lossy().trim_end_matches('\\').to_string();
    if ps.len() > bs.len() && ps[..bs.len()].eq_ignore_ascii_case(&bs) && ps.as_bytes()[bs.len()] == b'\\' {
        Some(PathBuf::from(&ps[bs.len() + 1..]))
    } else {
        None
    }
}

/// A resolved worktree: its id and root folder.
#[derive(Debug, Clone)]
pub struct WtRef {
    pub id: String,
    pub root: PathBuf,
}

/// Map any folder inside the dev tree to its worktree (walks up to the nearest `.git`), or `None` if
/// the folder is outside worktree-dev entirely.
pub fn resolve_worktree(dev: &Path, dir: &Path) -> Option<WtRef> {
    if eq_ci(dir, dev) {
        return Some(WtRef { id: DEV_ID.into(), root: dev.to_path_buf() });
    }
    let wts = dev.join("worktrees");
    let rel = starts_with_ci(dir, &wts)?;
    let comps: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    // Walk from the deepest folder up to worktrees/<slug>/<x>, stopping at the first one with a .git.
    for n in (2..=comps.len()).rev() {
        let root = comps[..n].iter().fold(wts.clone(), |p, c| p.join(c));
        if root.join(".git").exists() {
            let id = comps[..n].join("/");
            return Some(WtRef { id, root });
        }
    }
    None
}

pub fn worktree_path(dev: &Path, id: &str) -> PathBuf {
    if id == DEV_ID {
        return dev.to_path_buf();
    }
    id.split('/').fold(dev.join("worktrees"), |p, c| p.join(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msys_paths() {
        assert_eq!(normalize("/d/dev/x/y"), PathBuf::from(r"D:\dev\x\y"));
        assert_eq!(normalize("D:/dev/x"), PathBuf::from(r"D:\dev\x"));
        assert_eq!(normalize(r"D:\dev"), PathBuf::from(r"D:\dev"));
    }

    #[test]
    fn resolves_nested_worktree() {
        let t = std::env::temp_dir().join(format!("wtd-paths-{}", std::process::id()));
        let wt = t.join("worktrees").join("luop").join("feat").join("x");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::create_dir_all(t.join(".wtd")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: x").unwrap();
        let r = resolve_worktree(&t, &wt.join("src")).unwrap();
        assert_eq!(r.id, "luop/feat/x");
        assert!(eq_ci(&r.root, &wt));
        let up = PathBuf::from(t.to_string_lossy().to_uppercase()).join("worktrees").join("luop").join("feat").join("x");
        assert_eq!(resolve_worktree(&t, &up).unwrap().id, "luop/feat/x");
        assert_eq!(resolve_worktree(&t, &t).unwrap().id, DEV_ID);
        assert!(resolve_worktree(&t, Path::new(r"C:\elsewhere")).is_none());
        std::fs::remove_dir_all(&t).ok();
    }
}
