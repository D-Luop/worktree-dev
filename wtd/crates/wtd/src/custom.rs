//! Per-install customizations: your own skills, tools, CLAUDE.md additions and Claude hooks, for every
//! worktree or for one repo's. Lives in `.wtd/custom/` (not in git — it's yours):
//!
//! ```text
//! .wtd/custom/
//!   skills/<name>/SKILL.md      → every worktree's .claude/skills/
//!   tools/<file>                → on PATH in every session (and its shell)
//!   CLAUDE.md                   → appended to every worktree's CLAUDE.md (kept in sync on each open)
//!   hooks.json                  → Claude hooks ({"hooks": {...}}) merged into .claude/settings.json
//!   repos/<slug>/…              → the same five, only for that repo's worktrees (+ env/: files copied
//!                                 into new worktrees when absent, e.g. a .env)
//! ```
//!
//! Applied by `agent` on every open, and by `wtd custom apply` to worktrees that are already open.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{paths, settings};

const BEGIN: &str = "<!-- BEGIN custom (worktree-dev: from .wtd/custom — edit there, not here) -->";
const END: &str = "<!-- END custom -->";
const HOOKS_KEY: &str = "_wtdCustomHooks"; // the entries we merged last time, so a re-apply replaces them

pub fn root(dev: &Path) -> PathBuf {
    dev.join(".wtd").join("custom")
}

/// The layers that apply to a repo: global, then the repo's own.
fn layers(dev: &Path, slug: &str) -> Vec<PathBuf> {
    let r = root(dev);
    let mut v = vec![r.clone()];
    if !slug.is_empty() {
        v.push(r.join("repos").join(slug));
    }
    v
}

/// Directories to prepend to PATH for a repo's sessions.
pub fn tool_dirs(dev: &Path, slug: &str) -> Vec<PathBuf> {
    layers(dev, slug).into_iter().map(|l| l.join("tools")).filter(|d| d.is_dir()).collect()
}

fn copy_tree(from: &Path, to: &Path, overwrite: bool) -> Result<usize> {
    let mut n = 0;
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            n += copy_tree(&src, &dst, overwrite)?;
        } else if overwrite || !dst.exists() {
            std::fs::copy(&src, &dst)?;
            n += 1;
        }
    }
    Ok(n)
}

fn claude_md_block(dev: &Path, slug: &str) -> String {
    layers(dev, slug)
        .iter()
        .filter_map(|l| std::fs::read_to_string(l.join("CLAUDE.md")).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Replace (or add, or drop) the custom block at the end of a worktree's CLAUDE.md.
fn sync_claude_md(dev: &Path, slug: &str, wtp: &Path) -> Result<bool> {
    let p = wtp.join("CLAUDE.md");
    let Ok(text) = std::fs::read_to_string(&p) else { return Ok(false) };
    let body = claude_md_block(dev, slug);
    let base = match (text.find(BEGIN), text.find(END)) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &text[..a], &text[b + END.len()..]).trim_end().to_string() + "\n",
        _ => text.clone(),
    };
    let new = if body.is_empty() { base } else { format!("{}\n{BEGIN}\n{body}\n{END}\n", base.trim_end().to_string() + "\n") };
    if new != text {
        std::fs::write(&p, new)?;
        return Ok(true);
    }
    Ok(false)
}

/// Merge the layers' hooks.json into `.claude/settings.json`, replacing what we merged last time.
fn sync_hooks(dev: &Path, slug: &str, wtp: &Path) -> Result<bool> {
    let dev_s = dev.to_string_lossy().replace('\\', "/");
    let mut add: serde_json::Map<String, Value> = serde_json::Map::new();
    for l in layers(dev, slug) {
        let Ok(t) = std::fs::read_to_string(l.join("hooks.json")) else { continue };
        let v: Value = serde_json::from_str(&t.replace("__DEV__", &dev_s)).with_context(|| format!("{} is not valid JSON", l.join("hooks.json").display()))?;
        for (ev, entries) in v.get("hooks").and_then(Value::as_object).cloned().unwrap_or_default() {
            let list = add.entry(ev).or_insert_with(|| json!([]));
            list.as_array_mut().unwrap().extend(entries.as_array().cloned().unwrap_or_default());
        }
    }
    let proj = wtp.join(".claude").join("settings.json");
    let mut cur: Value = std::fs::read_to_string(&proj).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    let before = cur.clone();
    let prev = cur.get(HOOKS_KEY).cloned().unwrap_or_else(|| json!({}));
    let hooks = cur.as_object_mut().context("settings.json is not an object")?.entry("hooks").or_insert_with(|| json!({}));
    // drop last time's entries
    if let (Some(h), Some(p)) = (hooks.as_object_mut(), prev.as_object()) {
        for (ev, old) in p {
            if let Some(list) = h.get_mut(ev).and_then(Value::as_array_mut) {
                let old = old.as_array().cloned().unwrap_or_default();
                list.retain(|e| !old.contains(e));
            }
        }
        h.retain(|_, v| !v.as_array().is_some_and(|a| a.is_empty()));
    }
    for (ev, entries) in &add {
        let list = hooks.as_object_mut().unwrap().entry(ev.clone()).or_insert_with(|| json!([]));
        if let Some(l) = list.as_array_mut() {
            l.extend(entries.as_array().cloned().unwrap_or_default());
        }
    }
    let o = cur.as_object_mut().unwrap();
    if add.is_empty() {
        o.remove(HOOKS_KEY);
        if o.get("hooks").and_then(Value::as_object).is_some_and(|h| h.is_empty()) {
            o.remove("hooks");
        }
    } else {
        o.insert(HOOKS_KEY.into(), Value::Object(add));
    }
    if cur == before {
        return Ok(false);
    }
    if cur.as_object().is_some_and(|o| o.is_empty()) && !proj.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(proj.parent().unwrap())?;
    std::fs::write(&proj, serde_json::to_string_pretty(&cur)? + "\n")?;
    Ok(true)
}

/// Apply every customization layer to a worktree. `fresh` = just created (seed env files too).
pub fn apply(dev: &Path, slug: &str, wtp: &Path, fresh: bool) -> Result<Vec<String>> {
    let mut log = vec![];
    for l in layers(dev, slug) {
        let sk = l.join("skills");
        if sk.is_dir() {
            let n = copy_tree(&sk, &wtp.join(".claude").join("skills"), true)?;
            if n > 0 && fresh {
                log.push(format!("custom skills from {}", l.display()));
            }
        }
        let env = l.join("env");
        if env.is_dir() && l != root(dev) {
            let n = copy_tree(&env, wtp, false)?;
            if n > 0 {
                log.push(format!("seeded {n} custom env file(s)"));
            }
        }
    }
    if sync_claude_md(dev, slug, wtp)? {
        log.push("CLAUDE.md custom section updated".into());
    }
    if sync_hooks(dev, slug, wtp)? {
        log.push("custom hooks merged into .claude/settings.json".into());
    }
    Ok(log)
}

fn names(dir: &Path, dirs: bool) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| r.flatten().filter(|e| e.path().is_dir() == dirs).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn layer_info(l: &Path) -> Value {
    json!({
        "path": l.to_string_lossy(),
        "skills": names(&l.join("skills"), true),
        "tools": names(&l.join("tools"), false),
        "claudeMd": l.join("CLAUDE.md").is_file(),
        "hooks": l.join("hooks.json").is_file(),
        "env": std::fs::read_dir(l.join("env")).map(|r| r.count()).unwrap_or(0),
    })
}

pub fn ls(dev: &Path) -> Value {
    let r = root(dev);
    let mut repos = serde_json::Map::new();
    for slug in names(&r.join("repos"), true) {
        repos.insert(slug.clone(), layer_info(&r.join("repos").join(&slug)));
    }
    json!({ "root": r.to_string_lossy(), "global": layer_info(&r), "repos": repos })
}

fn layer_dir(dev: &Path, repo: Option<&str>) -> Result<PathBuf> {
    Ok(match repo {
        Some(s) if !settings::valid_name(s) => bail!("bad repo name '{s}'"),
        Some(s) => root(dev).join("repos").join(s),
        None => root(dev),
    })
}

const SKILL_TEMPLATE: &str = "---
name: __NAME__
description: >
  What this skill does and WHEN Claude should use it (the trigger phrases matter — Claude reads this
  to decide whether to invoke the skill). Invoked by the user as /__NAME__.
allowed-tools: Read, Grep, Glob, Bash
---

# __NAME__

Steps for Claude to follow, in order. Be concrete: the commands to run, the files to read, what to
report back.
";

/// `wtd custom ls | dir [--repo <slug>] | new-skill <name> [--repo <slug>] | apply [<slug>/<name> | --all]`
pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let repo = args.iter().position(|a| a == "--repo").and_then(|i| args.get(i + 1)).map(String::as_str);
    let pos: Vec<&str> = {
        let mut v = vec![];
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--repo" { i += 2; continue; }
            v.push(args[i].as_str());
            i += 1;
        }
        v
    };
    match pos.as_slice() {
        [] | ["ls"] => settings::out(ls(&dev)),
        ["dir"] => {
            let d = layer_dir(&dev, repo)?;
            std::fs::create_dir_all(&d)?;
            settings::out(json!({ "path": d.to_string_lossy() }))
        }
        ["new-skill", name] => {
            if !settings::valid_name(name) {
                bail!("skill names are letters, digits, - or _");
            }
            let d = layer_dir(&dev, repo)?.join("skills").join(name);
            let f = d.join("SKILL.md");
            if !f.exists() {
                std::fs::create_dir_all(&d)?;
                std::fs::write(&f, SKILL_TEMPLATE.replace("__NAME__", name))?;
            }
            settings::out(json!({ "path": f.to_string_lossy() }))
        }
        ["apply", target] | ["apply", target, ..] if *target != "--all" => {
            let (slug, _) = target.split_once('/').context("expected <slug>/<name>")?;
            let wtp = paths::worktree_path(&dev, target);
            if let Some(r) = crate::repos::find(&dev, slug) {
                let _ = crate::repos::prepare(&dev, &r); // keep what we write git-ignored
            }
            for l in apply(&dev, slug, &wtp, false)? {
                println!("{l}");
            }
            Ok(0)
        }
        ["apply"] | ["apply", "--all"] => {
            let mut n = 0;
            for r in crate::repos::all(&dev) {
                let _ = crate::repos::prepare(&dev, &r); // keep what we write git-ignored
            }
            for w in crate::daemon::scan::scan(&dev).into_iter().filter(|w| w.id != wtd_core::model::DEV_ID && w.slug != "plan") {
                match apply(&dev, &w.slug, Path::new(&w.path), false) {
                    Ok(_) => n += 1,
                    Err(e) => eprintln!("{}: {e:#}", w.id),
                }
            }
            println!("applied customizations to {n} worktree(s)");
            Ok(0)
        }
        _ => bail!("usage: wtd custom ls | dir [--repo <slug>] | new-skill <name> [--repo <slug>] | apply [<slug>/<name> | --all]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_md_block_is_replaced_not_duplicated() {
        let t = std::env::temp_dir().join(format!("wtd-custom-{}", std::process::id()));
        let wt = t.join("wt");
        std::fs::create_dir_all(t.join(".wtd").join("custom").join("repos").join("r")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("CLAUDE.md"), "# Base\n").unwrap();
        std::fs::write(t.join(".wtd/custom/CLAUDE.md"), "global rule").unwrap();
        std::fs::write(t.join(".wtd/custom/repos/r/CLAUDE.md"), "repo rule").unwrap();
        assert!(sync_claude_md(&t, "r", &wt).unwrap());
        assert!(!sync_claude_md(&t, "r", &wt).unwrap()); // idempotent
        std::fs::write(t.join(".wtd/custom/CLAUDE.md"), "changed rule").unwrap();
        sync_claude_md(&t, "r", &wt).unwrap();
        let s = std::fs::read_to_string(wt.join("CLAUDE.md")).unwrap();
        assert!(s.starts_with("# Base\n") && s.contains("changed rule") && s.contains("repo rule") && !s.contains("global rule"));
        assert_eq!(s.matches(BEGIN).count(), 1);
        std::fs::remove_file(t.join(".wtd/custom/CLAUDE.md")).unwrap();
        std::fs::remove_file(t.join(".wtd/custom/repos/r/CLAUDE.md")).unwrap();
        sync_claude_md(&t, "r", &wt).unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("CLAUDE.md")).unwrap(), "# Base\n");
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn hooks_reapply_replaces_previous() {
        let t = std::env::temp_dir().join(format!("wtd-customh-{}", std::process::id()));
        let wt = t.join("wt");
        std::fs::create_dir_all(t.join(".wtd").join("custom")).unwrap();
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        std::fs::write(wt.join(".claude/settings.json"), r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#).unwrap();
        std::fs::write(t.join(".wtd/custom/hooks.json"), r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"a"}]}]}}"#).unwrap();
        sync_hooks(&t, "", &wt).unwrap();
        std::fs::write(t.join(".wtd/custom/hooks.json"), r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"b"}]}]}}"#).unwrap();
        sync_hooks(&t, "", &wt).unwrap();
        let s = std::fs::read_to_string(wt.join(".claude/settings.json")).unwrap();
        assert!(s.contains("\"mine\"") && s.contains("\"b\"") && !s.contains("\"a\""));
        std::fs::remove_dir_all(&t).ok();
    }
}
