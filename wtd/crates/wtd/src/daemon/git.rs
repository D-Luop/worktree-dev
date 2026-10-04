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
}
