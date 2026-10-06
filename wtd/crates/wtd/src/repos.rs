//! Where a repo lives. Two kinds, resolved by slug:
//!   - **registered**: `.wtd/repos.tsv` + a bare clone at `repos/<slug>/.bare` (managed by wtd)
//!   - **folder**: any git clone under the configured repos folder (`config.json` → `reposDir`), found
//!     by scanning `reposDir/<name>` and `reposDir/<group>/<name>`. Slug = the folder name. Worktrees are
//!     cut straight off the clone — nothing to register.
//!
//! A registered slug wins over a folder clone with the same name. Everything that runs git against a
//! repo uses [`Repo::path`] (`git -C` works on both kinds); per-repo git files (info/exclude, hooks)
//! live under [`Repo::admin`].

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub slug: String,
    /// What `git -C` targets: the bare dir, or the clone's working tree.
    pub path: PathBuf,
    /// The git dir: the bare itself, or `<clone>/.git`.
    pub admin: PathBuf,
    /// `registered` | `folder`
    pub kind: &'static str,
    /// origin URL (`(local)` for a local-only registered repo, empty when a clone has no origin).
    pub url: String,
}

impl Repo {
    pub fn is_folder(&self) -> bool {
        self.kind == "folder"
    }
}

/// The configured repos folder, if any.
pub fn repos_dir(dev: &Path) -> Option<PathBuf> {
    let cfg = crate::settings::load_config(dev);
    let d = cfg.get("reposDir").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())?;
    let p = crate::paths::normalize(d.trim());
    p.is_dir().then_some(p)
}

/// `[remote "origin"] url = …` straight from a git config file (no git process).
fn origin_url(admin: &Path) -> String {
    let text = std::fs::read_to_string(admin.join("config")).unwrap_or_default();
    let mut in_origin = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            in_origin = t == "[remote \"origin\"]";
        } else if in_origin {
            if let Some(v) = t.strip_prefix("url").map(str::trim_start).and_then(|r| r.strip_prefix('=')) {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

fn is_clone(d: &Path) -> bool {
    d.join(".git").is_dir() // a worktree's .git is a file; a bare has no .git
}

fn valid_slug(s: &str) -> bool {
    crate::settings::valid_name(s) && s != "archive" && s != "plan"
}

/// Clones found under the repos folder (two levels deep), first name wins.
pub fn discovered(dev: &Path) -> Vec<Repo> {
    let Some(root) = repos_dir(dev) else { return vec![] };
    let mut out: Vec<Repo> = vec![];
    let push = |d: PathBuf, out: &mut Vec<Repo>| {
        let slug = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !valid_slug(&slug) || out.iter().any(|r| r.slug == slug) {
            return;
        }
        let admin = d.join(".git");
        out.push(Repo { url: origin_url(&admin), slug, path: d, admin, kind: "folder" });
    };
    let mut level1: Vec<PathBuf> = std::fs::read_dir(&root).map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    level1.sort();
    let mut groups = vec![];
    for d in level1 {
        if d.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        if is_clone(&d) {
            push(d, &mut out);
        } else {
            groups.push(d);
        }
    }
    for g in groups {
        let mut kids: Vec<PathBuf> = std::fs::read_dir(&g).map(|r| r.flatten().map(|e| e.path()).filter(|p| is_clone(p)).collect()).unwrap_or_default();
        kids.sort();
        for k in kids {
            push(k, &mut out);
        }
    }
    out
}

/// Registered repos first, then folder clones whose names aren't taken.
pub fn all(dev: &Path) -> Vec<Repo> {
    let mut out: Vec<Repo> = crate::settings::registered(dev)
        .into_iter()
        .map(|(slug, url)| {
            let bare = dev.join("repos").join(&slug).join(".bare");
            Repo { slug, admin: bare.clone(), path: bare, kind: "registered", url }
        })
        .collect();
    for r in discovered(dev) {
        if !out.iter().any(|x| x.slug == r.slug) {
            out.push(r);
        }
    }
    out
}

/// `owner/name` on GitHub: the repo's Settings link, else its origin URL.
pub fn github_repo(dev: &Path, slug: &str) -> Option<String> {
    let cfg = crate::settings::load_config(dev);
    if let Some(r) = cfg.pointer(&format!("/repos/{slug}/github/repo")).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return Some(r.to_string());
    }
    find(dev, slug).and_then(|r| crate::settings::github_repo_from_url(&r.url))
}

pub fn find(dev: &Path, slug: &str) -> Option<Repo> {
    all(dev).into_iter().find(|r| r.slug == slug)
}

/// A repo that exists on disk, or a helpful error.
pub fn require(dev: &Path, slug: &str) -> Result<Repo> {
    match find(dev, slug) {
        Some(r) if r.path.is_dir() => Ok(r),
        Some(r) => bail!("repo '{slug}' is registered but not cloned ({} missing)", r.path.display()),
        None => {
            let known: Vec<String> = all(dev).into_iter().map(|r| r.slug).collect();
            let hint = match repos_dir(dev) {
                Some(d) => format!("Clone it into {} or add it in Settings → Repositories.", d.display()),
                None => "Add it in Settings → Repositories (or set a repos folder there).".into(),
            };
            bail!("no repo '{slug}' (known: {}). {hint}", if known.is_empty() { "none".into() } else { known.join(", ") })
        }
    }
}

/// Per-repo setup shared by both kinds (idempotent): worktree-local files git-ignored, the AI
/// attribution stripper as commit-msg, and the guardrail pre-push hook. Existing hooks that aren't
/// ours are left alone.
pub fn prepare(dev: &Path, repo: &Repo) -> Result<()> {
    let excl = repo.admin.join("info").join("exclude");
    std::fs::create_dir_all(excl.parent().unwrap())?;
    let mut text = std::fs::read_to_string(&excl).unwrap_or_default();
    let before = text.len();
    for ign in WORKTREE_LOCAL {
        if !text.lines().any(|l| l.trim() == *ign) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(ign);
            text.push('\n');
        }
    }
    if text.len() != before {
        std::fs::write(&excl, text)?;
    }
    let hooks = repo.admin.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let wtd = dev.join(".wtd");
    let strip = wtd.join("hooks").join("strip-claude-attribution.sh").to_string_lossy().replace('\\', "/");
    install_hook(&hooks.join("commit-msg"), &format!("#!/bin/sh\n# worktree-dev: strip AI attribution lines from commit messages\nexec \"{strip}\" \"$@\"\n"))?;
    let exe = crate::paths::wtd_exe().to_string_lossy().replace('\\', "/");
    install_hook(&hooks.join("pre-push"), &format!("#!/bin/sh\n# worktree-dev: PR guardrails (blocked push branches) — configure in Settings → Guardrails\nexec \"{exe}\" guard pre-push \"$@\"\n"))?;
    Ok(())
}

/// Files every worktree keeps out of git.
pub const WORKTREE_LOCAL: &[&str] =
    &["CLAUDE.md", "pr-notes.md", ".claude-status", ".claude-status.resume", ".claude/issue.md", ".claude/skills/", ".claude-ticket.md",
      // ours too: the session's plans and the hooks we merge. info/exclude only hides UNTRACKED files, so a
      // repo that commits its own .claude/settings.json still sees its changes.
      ".claude/plans/", ".claude/settings.json", ".claude/settings.local.json"];

fn install_hook(p: &Path, body: &str) -> Result<()> {
    match std::fs::read_to_string(p) {
        Ok(cur) if !cur.contains("worktree-dev") => return Ok(()), // the user's own hook
        Ok(cur) if cur == body => return Ok(()),
        _ => {}
    }
    std::fs::write(p, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_origin_url() {
        let t = std::env::temp_dir().join(format!("wtd-repos-{}", std::process::id()));
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("config"), "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:o/n.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n").unwrap();
        assert_eq!(origin_url(&t), "git@github.com:o/n.git");
        std::fs::remove_dir_all(&t).ok();
    }
}
