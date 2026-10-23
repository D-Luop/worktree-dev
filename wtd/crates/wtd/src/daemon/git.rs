//! One `git status` for one worktree, parsed into [`GitState`].

use std::path::Path;
use std::process::{Command, Stdio};

use wtd_core::model::GitState;

use crate::win;

/// `--no-optional-locks`: never take `index.lock` for a background refresh — otherwise our status
/// check can collide with an agent's own git command ("index.lock exists").
pub fn status(path: &Path, now: i64) -> Option<GitState> {
    let mut cmd = Command::new("git");
    cmd.args(["--no-optional-locks", "status", "--porcelain=v2", "--branch"])
        .current_dir(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let out = win::no_window(&mut cmd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse(&String::from_utf8_lossy(&out.stdout), now))
}

/// `git status` for several worktrees in one script: each preceded by `\x1e<path>`, a failed one
/// followed by a `\x15` line.
pub const STATUS_BATCH: &str = r#"for d in "$@"; do printf '\036%s\n' "$d"; git -C "$d" --no-optional-locks status --porcelain=v2 --branch 2>/dev/null || printf '\025\n'; done"#;

/// [`status`] for WSL worktrees of one distro, in one `wsl.exe` round trip. Results follow `paths`.
pub fn status_wsl(distro: &str, paths: &[String], now: i64) -> Vec<Option<GitState>> {
    let args: Vec<&str> = paths.iter().map(String::as_str).collect();
    match crate::wsl_backend::repo::Sh::Wsl(distro.to_string()).out(STATUS_BATCH, &args) {
        Ok(text) => parse_batch(&text, paths, now),
        Err(_) => vec![None; paths.len()],
    }
}

pub fn parse_batch(text: &str, paths: &[String], now: i64) -> Vec<Option<GitState>> {
    let mut by_path: std::collections::HashMap<&str, Option<GitState>> = Default::default();
    for rec in text.split('\x1e').filter(|r| !r.is_empty()) {
        let (path, body) = rec.split_once('\n').unwrap_or((rec, ""));
        let ok = !body.lines().any(|l| l == "\x15");
        by_path.insert(path, ok.then(|| parse(body, now)));
    }
    paths.iter().map(|p| by_path.get(p.as_str()).cloned().flatten()).collect()
}

pub fn parse(text: &str, now: i64) -> GitState {
    let mut g = GitState { known: true, checked_at: now, ..Default::default() };
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            g.branch = (h != "(detached)").then(|| h.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    g.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    g.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            g.dirty = true;
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let g = parse("# branch.oid abc\n# branch.head feat/x\n# branch.upstream origin/feat/x\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/x.rs\n", 5);
        assert_eq!(g, GitState { known: true, branch: Some("feat/x".into()), dirty: true, ahead: 2, behind: 1, checked_at: 5 });
        let clean = parse("# branch.oid abc\n# branch.head (detached)\n", 1);
        assert!(!clean.dirty && clean.branch.is_none() && clean.ahead == 0);
    }

    #[test]
    fn parses_a_batch_in_input_order() {
        let text = "\x1e/w/b\n# branch.head b\n1 .M N... x\n\x1e/w/gone\n\x15\n\x1e/w/a\n# branch.head a\n# branch.ab +1 -0\n";
        let paths: Vec<String> = ["/w/a", "/w/gone", "/w/b", "/w/missing"].map(String::from).to_vec();
        let r = parse_batch(text, &paths, 7);
        assert_eq!(r[0].as_ref().map(|g| (g.branch.clone(), g.ahead, g.dirty)), Some((Some("a".into()), 1, false)));
        assert!(r[1].is_none() && r[3].is_none());
        assert!(r[2].as_ref().unwrap().dirty);
    }

    // the batch script itself, run by this machine's sh against real worktrees
    #[test]
    fn batch_script_runs() {
        if !Command::new("sh").args(["-c", "exit 0"]).status().is_ok_and(|s| s.success()) {
            return;
        }
        let t = std::env::temp_dir().join(format!("wtd-gitbatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let repo = t.join("repo");
        crate::gitx::run(&["init", "-q", "-b", "main", &repo.to_string_lossy()]).unwrap();
        std::fs::write(repo.join("f"), "x").unwrap();
        let paths = vec![repo.to_string_lossy().replace('\\', "/"), t.join("nope").to_string_lossy().replace('\\', "/")];
        let args: Vec<&str> = paths.iter().map(String::as_str).collect();
        let text = crate::wsl_backend::repo::Sh::Local.out(STATUS_BATCH, &args).unwrap();
        let r = parse_batch(&text, &paths, 1);
        assert_eq!(r[0].as_ref().map(|g| (g.branch.as_deref(), g.dirty)), Some((Some("main"), true)));
        assert!(r[1].is_none());
        std::fs::remove_dir_all(&t).ok();
    }
}
