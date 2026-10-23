//! A worktree's branch commits and working changes, parsed into files/hunks/lines: what the Changes
//! panel shows, for frontends without the VS Code extension (`wtd tui`, `wtd changes`).

use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use crate::{gitx, paths, settings};

const MAX_COMMITS: usize = 200;
const FALLBACK_COMMITS: usize = 30;
const MAX_FILE_LINES: usize = 4000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub when: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub branch: String,
    /// the ref this branch forked from ("" on trunk / unknown)
    pub base: String,
    /// commits since `base`; recent history instead when `fallback`
    pub commits: Vec<Commit>,
    pub fallback: bool,
    /// changed paths in the working tree (porcelain entries)
    pub changes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Line {
    pub kind: Kind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    /// A | M | D | R | ? (untracked)
    pub status: char,
    pub binary: bool,
    pub adds: usize,
    pub dels: usize,
    pub hunks: Vec<Hunk>,
}

fn git(wt: &Path, args: &[&str]) -> Option<String> {
    if let Some((distro, linux)) = crate::wsl_backend::path::from_unc(wt.to_str()?) {
        let mut a = vec!["git"];
        a.extend_from_slice(args);
        return crate::wsl_backend::run(&crate::wsl_backend::exec_args(&distro, Some(&linux), &a)).ok();
    }
    let mut a = vec!["-C", wt.to_str()?];
    a.extend_from_slice(args);
    gitx::out(&a).ok()
}

/// origin/HEAD, else the usual trunk names (local-only repos); "" when sitting on trunk.
pub fn base_ref(wt: &Path, branch: &str) -> String {
    if let Some(s) = git(wt, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"]).filter(|s| !s.is_empty()) {
        return s.trim_start_matches("refs/remotes/").to_string();
    }
    for c in ["origin/main", "origin/master", "main", "master"] {
        if c != branch && git(wt, &["rev-parse", "--verify", "--quiet", &format!("{c}^{{commit}}")]).is_some_and(|s| !s.is_empty()) {
            return c.to_string();
        }
    }
    String::new()
}

fn log(wt: &Path, extra: &[&str]) -> Vec<Commit> {
    let fmt = "--format=%H%x1f%h%x1f%an%x1f%ar%x1f%s%x1e";
    let mut args = vec!["log", "--no-color", fmt];
    args.extend_from_slice(extra);
    git(wt, &args).unwrap_or_default().split('\x1e').map(|s| s.trim_start_matches(['\r', '\n'])).filter(|s| !s.trim().is_empty()).map(|s| {
        let p: Vec<&str> = s.split('\x1f').collect();
        let f = |i: usize| p.get(i).copied().unwrap_or("").to_string();
        Commit { sha: f(0), short: f(1), author: f(2), when: f(3), subject: f(4) }
    }).collect()
}

pub fn summary(wt: &Path) -> Summary {
    let branch = git(wt, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into());
    let base = base_ref(wt, &branch);
    let mut commits = if base.is_empty() { vec![] } else { log(wt, &[&format!("--max-count={MAX_COMMITS}"), &format!("{base}..HEAD")]) };
    let fallback = commits.is_empty();
    if fallback {
        commits = log(wt, &[&format!("--max-count={FALLBACK_COMMITS}"), "HEAD"]);
    }
    let changes = porcelain(wt).len();
    Summary { branch, base, commits, fallback, changes }
}

/// (XY, path) for each changed path, ignored files excluded.
fn porcelain(wt: &Path) -> Vec<(String, String)> {
    let out = git(wt, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).unwrap_or_default();
    let mut v = vec![];
    let mut it = out.split('\0');
    while let Some(e) = it.next() {
        if e.len() < 4 || e.starts_with("!!") {
            continue;
        }
        let (xy, path) = e.split_at(3);
        if xy.starts_with('R') || xy.starts_with('C') {
            it.next(); // the rename's source path
        }
        v.push((xy.trim().to_string(), path.to_string()));
    }
    v
}

/// Uncommitted changes vs HEAD, untracked files as all-added.
pub fn working(wt: &Path) -> Vec<FileDiff> {
    let mut files = parse_patch(&git(wt, &["diff", "--no-color", "--find-renames", "HEAD"]).unwrap_or_default());
    for (xy, path) in porcelain(wt) {
        if xy != "??" {
            continue;
        }
        let bytes = std::fs::read(wt.join(&path)).unwrap_or_default();
        let binary = bytes.iter().take(8000).any(|b| *b == 0);
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<Line> = if binary { vec![] } else {
            text.lines().take(MAX_FILE_LINES).enumerate().map(|(i, l)| Line { kind: Kind::Add, old: None, new: Some(i as u32 + 1), text: l.to_string() }).collect()
        };
        let adds = lines.len();
        files.push(FileDiff { path, old_path: None, status: '?', binary, adds, dels: 0,
            hunks: if lines.is_empty() { vec![] } else { vec![Hunk { header: format!("@@ -0,0 +1,{adds} @@ (new file)"), lines }] } });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// One commit's changes (first parent for merges).
pub fn commit(wt: &Path, sha: &str) -> Vec<FileDiff> {
    parse_patch(&git(wt, &["diff-tree", "-p", "--no-color", "--no-commit-id", "--find-renames", "--root", "-m", "--first-parent", sha]).unwrap_or_default())
}

fn unquote(p: &str) -> String {
    let p = p.trim();
    let p = p.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(p);
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
}

/// Parse `git diff` output into files → hunks → numbered lines.
pub fn parse_patch(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = vec![];
    let (mut old_no, mut new_no) = (0u32, 0u32);
    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map(|(_, b)| b.to_string()).unwrap_or_else(|| rest.to_string());
            files.push(FileDiff { path, old_path: None, status: 'M', binary: false, adds: 0, dels: 0, hunks: vec![] });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if f.hunks.is_empty() {
            if raw.starts_with("new file mode") {
                f.status = 'A';
            } else if raw.starts_with("deleted file mode") {
                f.status = 'D';
            } else if let Some(p) = raw.strip_prefix("rename from ") {
                f.status = 'R';
                f.old_path = Some(p.to_string());
            } else if let Some(p) = raw.strip_prefix("rename to ") {
                f.path = p.to_string();
            } else if raw.starts_with("Binary files ") {
                f.binary = true;
            } else if let Some(p) = raw.strip_prefix("+++ ") {
                if p != "/dev/null" {
                    f.path = unquote(p);
                }
            }
        }
        if let Some(h) = raw.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@ context
            let mut nums = h.split(' ');
            let start = |s: Option<&str>| s.and_then(|s| s[1..].split(',').next()).and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
            old_no = start(nums.next()).saturating_sub(1);
            new_no = start(nums.next()).saturating_sub(1);
            f.hunks.push(Hunk { header: raw.to_string(), lines: vec![] });
            continue;
        }
        let Some(h) = f.hunks.last_mut() else { continue };
        if h.lines.len() >= MAX_FILE_LINES {
            continue;
        }
        let (kind, body) = match raw.as_bytes().first() {
            Some(b'+') => (Kind::Add, &raw[1..]),
            Some(b'-') => (Kind::Del, &raw[1..]),
            Some(b' ') => (Kind::Context, &raw[1..]),
            Some(b'\\') => continue, // "\ No newline at end of file"
            _ => (Kind::Context, raw),
        };
        let line = match kind {
            Kind::Add => { new_no += 1; f.adds += 1; Line { kind, old: None, new: Some(new_no), text: body.to_string() } }
            Kind::Del => { old_no += 1; f.dels += 1; Line { kind, old: Some(old_no), new: None, text: body.to_string() } }
            Kind::Context => { old_no += 1; new_no += 1; Line { kind, old: Some(old_no), new: Some(new_no), text: body.to_string() } }
        };
        h.lines.push(line);
    }
    files
}

/// The changed span of a paired -/+ line: (prefix, suffix) byte lengths they share, on char boundaries.
pub fn common_affixes(a: &str, b: &str) -> (usize, usize) {
    let pre = a.char_indices().zip(b.chars()).take_while(|((_, x), y)| x == y).last().map(|((i, c), _)| i + c.len_utf8()).unwrap_or(0);
    let (ra, rb) = (&a[pre..], &b[pre..]);
    let suf = ra.chars().rev().zip(rb.chars().rev()).take_while(|(x, y)| x == y).map(|(c, _)| c.len_utf8()).sum::<usize>();
    (pre, suf.min(ra.len()).min(rb.len()))
}

/// `wtd changes <slug>/<name> [working | <sha>]` — JSON
pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let id = args.first().ok_or_else(|| anyhow::anyhow!("usage: wtd changes <slug>/<name> [working | <sha>]"))?;
    let wt = paths::worktree_root(&dev, id);
    match args.get(1).map(String::as_str) {
        None => settings::out(json!(summary(&wt))),
        Some("working") => settings::out(json!(working(&wt))),
        Some(sha) => settings::out(json!(commit(&wt, sha))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@ fn x()
 keep
-old line
+new line
 tail
\\ No newline at end of file
diff --git a/b.txt b/c.txt
similarity index 90%
rename from b.txt
rename to c.txt
diff --git a/n.md b/n.md
new file mode 100644
--- /dev/null
+++ b/n.md
@@ -0,0 +1,2 @@
+one
+two
";

    #[test]
    fn parses_files_hunks_and_numbers() {
        let f = parse_patch(PATCH);
        assert_eq!(f.len(), 3);
        assert_eq!((f[0].path.as_str(), f[0].status, f[0].adds, f[0].dels), ("src/a.rs", 'M', 1, 1));
        let l = &f[0].hunks[0].lines;
        assert_eq!(l.len(), 4);
        assert_eq!((l[1].kind, l[1].old, l[1].new), (Kind::Del, Some(2), None));
        assert_eq!((l[2].kind, l[2].old, l[2].new), (Kind::Add, None, Some(2)));
        assert_eq!((l[3].old, l[3].new), (Some(3), Some(3)));
        assert_eq!((f[1].status, f[1].path.as_str(), f[1].old_path.as_deref()), ('R', "c.txt", Some("b.txt")));
        assert_eq!((f[2].status, f[2].adds), ('A', 2));
    }

    #[test]
    fn affixes_bound_the_change() {
        assert_eq!(common_affixes("let x = 1;", "let x = 22;"), (8, 1));
        assert_eq!(common_affixes("same", "same"), (4, 0));
        assert_eq!(common_affixes("héllo", "hallo"), (1, 3));
    }
}
