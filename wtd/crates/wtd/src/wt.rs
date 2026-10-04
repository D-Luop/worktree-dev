//! Worktree lifecycle shared by the CLI (`wtd agent`, `wtd archive`) and the daemon (which owns
//! archive/remove while it runs, so it can stop the session and release its file watch first).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::gitx::{self, s};
use crate::paths;

/// Legacy session name (`<slug>-<name>`, `.`/`:` → `-`) — the key for durable per-session state
/// (resume id, account binding) shared with the Linux/tmux tooling.
pub fn session_name(slug: &str, name: &str) -> String {
    format!("{slug}-{name}").replace(['.', ':'], "-")
}

/// Filename-safe form of a session name ('/' → '__').
pub fn session_key(session: &str) -> String {
    session.replace('/', "__")
}

pub fn state(dev: &Path, sub: &str) -> PathBuf {
    paths::state_dir(dev).join(sub)
}

pub fn read_state(dev: &Path, sub: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(state(dev, sub).join(key)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn write_state(dev: &Path, sub: &str, key: &str, value: &str) -> Result<()> {
    let d = state(dev, sub);
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join(key), format!("{value}\n"))?;
    Ok(())
}

pub fn forget_state(dev: &Path, sub: &str, key: &str) {
    let _ = std::fs::remove_file(state(dev, sub).join(key));
}

/// Everything a worktree leaves in `.wtd/state` (resume ids, account binding, status mirror, previews).
pub fn forget_worktree_state(dev: &Path, slug: &str, name: &str) {
    let key = session_key(&session_name(slug, name));
    forget_state(dev, "session-ids", &key);
    forget_state(dev, "session-ids", &format!("{key}.codex"));
    forget_state(dev, "session-accounts", &key);
    forget_state(dev, "status", &format!("{slug}__{}", name.replace('/', "__")));
    let pv = name.split('/').fold(state(dev, "previews").join(slug), |p, c| p.join(c));
    let _ = crate::settings::remove_tree(&pv);
}

/// Accept a full name (`feat/x`) or a bare leaf (`x`) that's unique one namespace down.
pub fn resolve_name(dev: &Path, slug: &str, name: &str) -> Result<String> {
    let root = dev.join("worktrees").join(slug);
    if paths::worktree_path(dev, &format!("{slug}/{name}")).is_dir() {
        return Ok(name.to_string());
    }
    let mut found = vec![];
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let ns = e.file_name().to_string_lossy().to_string();
            if ns != "archive" && e.path().join(name).is_dir() {
                found.push(format!("{ns}/{name}"));
            }
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => bail!("no worktree '{name}' in '{slug}'"),
        _ => bail!("'{name}' is ambiguous in '{slug}': {} — pass the full name", found.join(", ")),
    }
}

/// `git worktree remove` (retrying briefly: a just-killed session can hold handles on Windows), then
/// optionally delete the branch. Planning agents (slug `plan`) are plain folders.
pub fn remove(dev: &Path, slug: &str, name: &str, force: bool, delete_branch: bool) -> Result<Vec<String>> {
    let mut log = vec![];
    let wt = paths::worktree_path(dev, &format!("{slug}/{name}"));
    if slug == "plan" {
        if wt.is_dir() {
            crate::settings::remove_tree(&wt).with_context(|| format!("removing {} (close anything using it, then retry)", wt.display()))?;
            log.push(format!("removed {}", wt.display()));
        }
        let _ = std::fs::remove_dir(dev.join("worktrees").join("plan"));
        forget_worktree_state(dev, slug, name);
        return Ok(log);
    }
    let bare = gitx::require_repo(dev, slug)?;
    let bs = s(&bare);
    if wt.is_dir() {
        let mut args = vec!["-C", bs.as_str(), "worktree", "remove"];
        if force {
            args.push("--force");
        }
        let wts = s(&wt);
        args.push(&wts);
        let mut last = String::new();
        let mut removed = false;
        for _ in 0..5 {
            match gitx::out(&args) {
                Ok(_) => { removed = true; break; }
                Err(e) => {
                    last = format!("{e:#}");
                    if last.contains("Permission denied") || last.contains("used by another process") {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    } else {
                        break;
                    }
                }
            }
        }
        if !removed {
            if last.contains("Permission denied") || last.contains("used by another process") {
                bail!("{last}\n(a process is still using the worktree — close its terminal/editor tabs, then retry)");
            }
            bail!("{last}\n(uncommitted changes? remove with --force to discard them, or commit/push first)");
        }
        let _ = std::fs::remove_dir(&wt); // an emptied shell a held handle left behind
        prune_empty_parents(dev, &wt);
        log.push(format!("removed worktree {}", wt.display()));
    } else {
        let _ = gitx::ok(&["-C", &bs, "worktree", "prune"]);
        log.push("worktree not present; pruned stale entries".into());
    }
    forget_worktree_state(dev, slug, name);
    if delete_branch {
        if gitx::ref_exists(&bare, &format!("refs/heads/{name}")) {
            gitx::run(&["-C", &bs, "branch", "-D", name])?;
            log.push(format!("deleted branch {name}"));
        } else {
            log.push(format!("no local branch '{name}' to delete"));
        }
    }
    Ok(log)
}

/// Remove now-empty namespace folders (`worktrees/<slug>/feat/`) up to, not including, `worktrees/`.
fn prune_empty_parents(dev: &Path, from: &Path) {
    let stop = dev.join("worktrees");
    let mut p = from.parent().map(Path::to_path_buf);
    while let Some(d) = p {
        if d == stop || !d.starts_with(&stop) || std::fs::remove_dir(&d).is_err() {
            break;
        }
        p = d.parent().map(Path::to_path_buf);
    }
}

/// Move a worktree to `worktrees/<slug>/archive/<name>` (still a valid worktree; branch kept).
pub fn archive(dev: &Path, slug: &str, name: &str) -> Result<PathBuf> {
    let bare = gitx::require_repo(dev, slug)?;
    let wt = paths::worktree_path(dev, &format!("{slug}/{name}"));
    if !wt.is_dir() {
        bail!("no worktree '{name}' in '{slug}'");
    }
    let arc = name.split('/').fold(dev.join("worktrees").join(slug).join("archive"), |p, c| p.join(c));
    if arc.exists() {
        bail!("archive target already exists: {}", arc.display());
    }
    std::fs::create_dir_all(arc.parent().unwrap())?;
    gitx::out(&["-C", &s(&bare), "worktree", "move", &s(&wt), &s(&arc)])
        .context("git worktree move failed (worktree locked, or a process is using it?)")?;
    crate::wt::forget_state(dev, "status", &format!("{slug}__{}", name.replace('/', "__")));
    prune_empty_parents(dev, &wt);
    Ok(arc)
}

/// Move an archived worktree back into the active rotation.
pub fn restore(dev: &Path, slug: &str, name: &str) -> Result<PathBuf> {
    let bare = gitx::require_repo(dev, slug)?;
    let arc = name.split('/').fold(dev.join("worktrees").join(slug).join("archive"), |p, c| p.join(c));
    let wt = paths::worktree_path(dev, &format!("{slug}/{name}"));
    std::fs::create_dir_all(wt.parent().unwrap())?;
    gitx::out(&["-C", &s(&bare), "worktree", "move", &s(&arc), &s(&wt)])?;
    // tidy now-empty archive/ parents
    let mut p = arc.parent().map(Path::to_path_buf);
    while let Some(d) = p {
        if std::fs::remove_dir(&d).is_err() {
            break;
        }
        p = d.parent().map(Path::to_path_buf);
    }
    Ok(wt)
}

/// Merge `.wtd/repo-hooks/<slug>.json` (Claude hook fragments) into the worktree's project settings.
pub fn merge_repo_hooks(dev: &Path, slug: &str, wt: &Path) -> Result<bool> {
    let frag = dev.join(".wtd").join("repo-hooks").join(format!("{slug}.json"));
    let Ok(text) = std::fs::read_to_string(&frag) else { return Ok(false) };
    let rendered = text.replace("__DEV__", &dev.to_string_lossy().replace('\\', "/"));
    let frag: Value = serde_json::from_str(&rendered).context("repo-hooks fragment is not JSON")?;
    let Some(fhooks) = frag.get("hooks").and_then(Value::as_object) else { return Ok(false) };
    let proj = wt.join(".claude").join("settings.json");
    std::fs::create_dir_all(proj.parent().unwrap())?;
    let mut cur: Value = std::fs::read_to_string(&proj).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| serde_json::json!({}));
    let first_cmd = rendered.split("\"command\"").nth(1).and_then(|r| r.split('"').nth(1)).unwrap_or("").to_string();
    if !first_cmd.is_empty() && cur.to_string().contains(&first_cmd) {
        return Ok(false); // already wired
    }
    let hooks = cur.as_object_mut().unwrap().entry("hooks").or_insert_with(|| serde_json::json!({}));
    for (ev, entries) in fhooks {
        let list = hooks.as_object_mut().unwrap().entry(ev.clone()).or_insert_with(|| serde_json::json!([]));
        if let (Some(l), Some(add)) = (list.as_array_mut(), entries.as_array()) {
            l.extend(add.iter().cloned());
        }
    }
    std::fs::write(&proj, serde_json::to_string_pretty(&cur)? + "\n")?;
    Ok(true)
}
