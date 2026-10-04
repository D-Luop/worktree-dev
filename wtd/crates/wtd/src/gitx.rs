//! Git plumbing shared by the CLI commands: our bare repos (`repos/<slug>/.bare`), worktree paths,
//! default branches, and the read-only reference checkouts under `refs/<slug>/<branch>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::win;

/// `git -c safe.bareRepository=all …` (VSCode injects `explicit`, which breaks bare repos).
pub fn git() -> Command {
    let mut c = Command::new("git");
    c.arg("-c").arg("safe.bareRepository=all");
    win::no_window(&mut c);
    c
}

/// Run git with inherited stdio (progress visible); error if it fails.
pub fn run(args: &[&str]) -> Result<()> {
    let st = git().args(args).stdin(Stdio::null()).status().context("running git")?;
    if !st.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

/// Run git quietly; Ok(true) on success.
pub fn ok(args: &[&str]) -> bool {
    git().args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Trimmed stdout, or an error carrying git's stderr.
pub fn out(args: &[&str]) -> Result<String> {
    let o = git().args(args).stdin(Stdio::null()).output().context("running git")?;
    if !o.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

pub fn bare(dev: &Path, slug: &str) -> PathBuf {
    dev.join("repos").join(slug).join(".bare")
}

pub fn s(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

pub fn registered(dev: &Path, slug: &str) -> bool {
    crate::settings::registered(dev).iter().any(|(s, _)| s == slug)
}

pub fn require_repo(dev: &Path, slug: &str) -> Result<PathBuf> {
    if !registered(dev, slug) {
        let known: Vec<String> = crate::settings::registered(dev).into_iter().map(|(s, _)| s).collect();
        bail!("repo '{slug}' is not registered (known: {}). Add it in Settings → Repositories or `wtd repo add`.", if known.is_empty() { "none".into() } else { known.join(", ") });
    }
    let b = bare(dev, slug);
    if !b.is_dir() {
        bail!("repo '{slug}' is registered but not cloned ({} missing)", b.display());
    }
    Ok(b)
}

pub fn has_origin(bare: &Path) -> bool {
    ok(&["-C", &s(bare), "remote", "get-url", "origin"])
}

/// origin/HEAD's branch, else the bare's HEAD branch (local-only repos), else `main`.
pub fn default_branch(bare: &Path) -> String {
    out(&["-C", &s(bare), "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .ok()
        .and_then(|r| r.strip_prefix("refs/remotes/origin/").map(String::from))
        .or_else(|| out(&["-C", &s(bare), "symbolic-ref", "--quiet", "HEAD"]).ok().and_then(|r| r.strip_prefix("refs/heads/").map(String::from)))
        .unwrap_or_else(|| "main".into())
}

pub fn ref_exists(bare: &Path, r: &str) -> bool {
    ok(&["-C", &s(bare), "rev-parse", "--verify", "--quiet", r])
}

// --- reference checkouts ---------------------------------------------------------------------------

/// `<slug>` or `<slug>@<branch>` → (slug, branch); a bare slug means the default branch.
pub fn parse_ref_token(dev: &Path, token: &str) -> (String, String) {
    match token.split_once('@') {
        Some((s, b)) => (s.to_string(), b.to_string()),
        None => (token.to_string(), default_branch(&bare(dev, token))),
    }
}

pub fn ref_path(dev: &Path, slug: &str, branch: &str) -> PathBuf {
    branch.split('/').fold(dev.join("refs").join(slug), |p, c| p.join(c))
}

/// Create or refresh a detached, read-only checkout of slug@branch under `refs/`. Returns its path.
pub fn ensure_ref(dev: &Path, slug: &str, branch: &str) -> Result<PathBuf> {
    let b = require_repo(dev, slug)?;
    let bs = s(&b);
    let _ = ok(&["-C", &bs, "fetch", "--quiet", "origin"]);
    let r = if ref_exists(&b, &format!("refs/remotes/origin/{branch}")) {
        format!("origin/{branch}")
    } else if ref_exists(&b, &format!("refs/heads/{branch}")) {
        branch.to_string()
    } else {
        bail!("branch '{branch}' not found in '{slug}' (origin or local)");
    };
    let p = ref_path(dev, slug, branch);
    if p.is_dir() {
        let _ = ok(&["-C", &s(&p), "checkout", "--quiet", "--detach", "--force", &r]);
    } else {
        std::fs::create_dir_all(p.parent().unwrap())?;
        run(&["-C", &bs, "worktree", "add", "--quiet", "--detach", &s(&p), &r])?;
    }
    Ok(p)
}

pub fn remove_ref(dev: &Path, slug: &str, branch: &str) -> bool {
    let p = ref_path(dev, slug, branch);
    if !p.is_dir() {
        return false;
    }
    let b = bare(dev, slug);
    if !ok(&["-C", &s(&b), "worktree", "remove", "--force", &s(&p)]) {
        let _ = crate::settings::remove_tree(&p);
        let _ = ok(&["-C", &s(&b), "worktree", "prune"]);
    }
    let _ = std::fs::remove_dir(dev.join("refs").join(slug)); // only if now empty
    true
}

/// Every loaded reference checkout: (slug@branch, path, short sha).
pub fn list_refs(dev: &Path) -> Vec<(String, PathBuf, String)> {
    let root = dev.join("refs");
    let mut v = Vec::new();
    for (slug, _) in crate::settings::registered(dev) {
        let b = bare(dev, &slug);
        if !b.is_dir() {
            continue;
        }
        let Ok(list) = out(&["-C", &s(&b), "worktree", "list", "--porcelain"]) else { continue };
        let mut path: Option<PathBuf> = None;
        for line in list.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(crate::paths::normalize(p));
            } else if let (Some(h), Some(p)) = (line.strip_prefix("HEAD "), path.as_ref()) {
                if let Ok(rel) = p.strip_prefix(&root.join(&slug)) {
                    let branch = rel.to_string_lossy().replace('\\', "/");
                    v.push((format!("{slug}@{branch}"), p.clone(), h.chars().take(10).collect()));
                }
            }
        }
    }
    v
}

pub fn ref_main(args: &[String]) -> Result<i32> {
    let dev = crate::paths::dev_root()?;
    let (sub, rest) = args.split_first().map(|(a, r)| (a.as_str(), r)).unwrap_or(("ls", &[]));
    match sub {
        "add" | "sync" | "refresh" if !rest.is_empty() => {
            for t in rest {
                let (slug, branch) = parse_ref_token(&dev, t);
                match ensure_ref(&dev, &slug, &branch) {
                    Ok(p) => println!("{:<8}{:<26}{}", if sub == "add" { "added" } else { "synced" }, format!("{slug}@{branch}"), p.display()),
                    Err(e) => eprintln!("skipping {t}: {e:#}"),
                }
            }
        }
        "sync" | "refresh" => {
            let all = list_refs(&dev);
            if all.is_empty() {
                println!("no refs loaded (add one with: ref add <slug>[@<branch>])");
            }
            for (label, _, _) in all {
                let (slug, branch) = label.split_once('@').map(|(a, b)| (a.to_string(), b.to_string())).unwrap();
                match ensure_ref(&dev, &slug, &branch) {
                    Ok(p) => println!("synced  {label:<26}{}", p.display()),
                    Err(e) => eprintln!("skipping {label}: {e:#}"),
                }
            }
        }
        "rm" | "remove" if !rest.is_empty() => {
            for t in rest {
                let (slug, branch) = parse_ref_token(&dev, t);
                println!("{} {slug}@{branch}", if remove_ref(&dev, &slug, &branch) { "removed" } else { "absent " });
            }
        }
        "ls" | "list" => {
            let all = list_refs(&dev);
            if all.is_empty() {
                println!("no refs loaded (add one with: ref add <slug>[@<branch>])");
            }
            for (label, path, sha) in all {
                println!("{label:<26}{sha:<12}{}", path.display());
            }
        }
        _ => bail!("usage: ref add|sync|rm <slug>[@<branch>]… | ref sync | ref ls"),
    }
    Ok(0)
}
