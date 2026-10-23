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
    bail!("can't find the worktree-dev root (set WTD_DEV, or run wtd setup)")
}

pub fn program_dir_pin() -> Option<PathBuf> {
    home_dir().ok().map(|h| h.join(".config").join("wtd").join("program-dir"))
}

/// Where the installed program lives (`%LOCALAPPDATA%\WorkTreeDev`): `WTD_PROGRAM_DIR` env → the
/// `~/.config/wtd/program-dir` pin → the default. It may not exist (a dev-mode install).
pub fn program_dir() -> PathBuf {
    // unit tests never see this machine's real install
    if cfg!(test) && std::env::var_os("WTD_PROGRAM_DIR").is_none() {
        return std::env::temp_dir().join("wtd-test-no-program-dir");
    }
    if let Some(p) = std::env::var_os("WTD_PROGRAM_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(s) = program_dir_pin().and_then(|p| std::fs::read_to_string(p).ok()).filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(s.trim());
    }
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        .or_else(|| home_dir().ok().map(|h| h.join("AppData").join("Local")))
        .unwrap_or_default();
    local.join("WorkTreeDev")
}

/// Where binaries, templates, hooks and scripts come from, and whether the dev root's own copies win.
#[derive(Debug, Clone)]
pub struct Layout {
    pub dev: Option<PathBuf>,
    /// `<program dir>/current`, the active version.
    pub current: PathBuf,
    /// `WTD_DEV_SOURCE=1`: a contributor running from the checkout even with a program dir installed.
    pub dev_source: bool,
}

impl Layout {
    pub fn detect() -> Self {
        Self::with_dev(dev_root().ok())
    }

    pub fn with_dev(dev: Option<PathBuf>) -> Self {
        Layout {
            dev,
            current: program_dir().join("current"),
            dev_source: std::env::var("WTD_DEV_SOURCE").is_ok_and(|v| v == "1"),
        }
    }

    pub fn installed(&self) -> bool {
        self.current.join("bin").join("wtd.exe").is_file()
    }

    /// The dev root is a source checkout and its copies are the ones to use.
    pub fn dev_mode(&self) -> bool {
        self.dev.as_ref().is_some_and(|d| d.join(".wtd").join("templates").is_dir()) && (self.dev_source || !self.installed())
    }

    /// `rel` under the program dir, falling back to `<dev>/.wtd/<rel>` for a file the program dir lacks.
    pub fn asset(&self, rel: &str) -> PathBuf {
        let dev = self.dev.as_ref().map(|d| d.join(".wtd").join(rel));
        if self.dev_mode() {
            return dev.unwrap();
        }
        let prog = self.current.join(rel);
        match dev {
            Some(d) if !prog.exists() && d.exists() => d,
            _ => prog,
        }
    }

    /// The `wtd.exe` that hooks, shims and spawned commands should run.
    pub fn wtd_exe(&self) -> PathBuf {
        if !self.dev_mode() && self.installed() {
            self.current.join("bin").join("wtd.exe")
        } else {
            sibling_exe("wtd.exe")
        }
    }
}

pub fn dev_mode() -> bool {
    Layout::detect().dev_mode()
}

/// A program file (`templates/…`, `hooks/…`, `scripts/…`, `bin/…`), resolved per [`Layout::asset`].
pub fn asset(rel: &str) -> PathBuf {
    Layout::detect().asset(rel)
}

/// [`asset`] for a dev root the caller already has.
pub fn asset_in(dev: &Path, rel: &str) -> PathBuf {
    Layout::with_dev(Some(dev.to_path_buf())).asset(rel)
}

/// A sibling program next to the running one (`wtd.exe` / `wtd-tray.exe` live side by side).
pub fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).unwrap_or_else(|| PathBuf::from(name))
}
pub fn wtd_exe() -> PathBuf {
    Layout::detect().wtd_exe()
}
/// The windowless tray program, if it's installed.
pub fn tray_exe() -> Option<PathBuf> {
    Some(wtd_exe().with_file_name("wtd-tray.exe")).filter(|p| p.is_file())
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
    if let Some(e) = crate::wsl_backend::resolve_host_path(dev, dir) {
        return Some(WtRef { root: e.unc(), id: e.id });
    }
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

/// `worktrees/delegate/<repo slug>`: a repo's long-lived delegator, which spawns worktrees for it.
pub const DELEGATE: &str = "delegate";

pub fn is_delegate_id(id: &str) -> bool {
    id.split_once('/').is_some_and(|(slug, name)| slug == DELEGATE && !name.is_empty() && !name.contains('/'))
}

/// Slugs under `worktrees/` that hold plain folders (their own `git init`), not checkouts of a repo.
pub fn repo_less(slug: &str) -> bool {
    slug == "plan" || slug == DELEGATE
}

/// [`worktree_path`], or a registered WSL worktree's `\\wsl.localhost\…` view.
pub fn worktree_root(dev: &Path, id: &str) -> PathBuf {
    crate::wsl_backend::find(dev, id).map(|e| e.unc()).unwrap_or_else(|| worktree_path(dev, id))
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
    fn delegate_ids() {
        assert!(is_delegate_id("delegate/worktree-dev"));
        for id in ["delegate", "delegate/", "delegate/a/b", "worktree-dev/delegate", "plan/x", DEV_ID] {
            assert!(!is_delegate_id(id), "{id}");
        }
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    fn layout(tag: &str, checkout: bool, installed: bool, dev_source: bool) -> (PathBuf, Layout) {
        let t = std::env::temp_dir().join(format!("wtd-layout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let dev = t.join("dev");
        std::fs::create_dir_all(dev.join(".wtd").join("state")).unwrap();
        if checkout {
            touch(&dev.join(".wtd/templates/CLAUDE.md"));
            touch(&dev.join(".wtd/templates/dev-only.md"));
        }
        let current = t.join("WorkTreeDev").join("current");
        if installed {
            touch(&current.join("bin/wtd.exe"));
            touch(&current.join("templates/CLAUDE.md"));
        }
        (t, Layout { dev: Some(dev), current, dev_source })
    }

    #[test]
    fn program_mode_uses_program_dir() {
        let (t, l) = layout("prog", false, true, false);
        assert!(!l.dev_mode());
        assert_eq!(l.asset("templates/CLAUDE.md"), l.current.join("templates/CLAUDE.md"));
        assert_eq!(l.asset("hooks/missing.sh"), l.current.join("hooks/missing.sh"));
        assert_eq!(l.wtd_exe(), l.current.join("bin").join("wtd.exe"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn dev_mode_without_a_program_dir() {
        let (t, l) = layout("dev", true, false, false);
        assert!(l.dev_mode());
        let dev = l.dev.clone().unwrap();
        assert_eq!(l.asset("templates/CLAUDE.md"), dev.join(".wtd").join("templates/CLAUDE.md"));
        assert_eq!(l.wtd_exe(), sibling_exe("wtd.exe"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn dev_source_beats_an_installed_program_dir() {
        let (t, l) = layout("devsrc", true, true, true);
        assert!(l.dev_mode());
        assert_eq!(l.asset("templates/CLAUDE.md"), l.dev.clone().unwrap().join(".wtd").join("templates/CLAUDE.md"));
        assert_eq!(l.wtd_exe(), sibling_exe("wtd.exe"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn mixed_mode_falls_back_per_file() {
        let (t, l) = layout("mixed", true, true, false);
        assert!(!l.dev_mode());
        assert_eq!(l.asset("templates/CLAUDE.md"), l.current.join("templates/CLAUDE.md"));
        assert_eq!(l.asset("templates/dev-only.md"), l.dev.clone().unwrap().join(".wtd").join("templates/dev-only.md"));
        assert_eq!(l.wtd_exe(), l.current.join("bin").join("wtd.exe"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn no_dev_root_and_nothing_installed() {
        let l = Layout { dev: None, current: PathBuf::from(r"Z:\nowhere\current"), dev_source: false };
        assert!(!l.dev_mode() && !l.installed());
        assert_eq!(l.asset("templates/CLAUDE.md"), PathBuf::from(r"Z:\nowhere\current").join("templates/CLAUDE.md"));
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
