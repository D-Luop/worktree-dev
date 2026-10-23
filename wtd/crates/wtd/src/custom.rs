//! Presets: the skills, subagents, commands, rules, tools, instructions and hooks every session gets,
//! for Claude and Codex alike. Layers, lowest to highest (a later layer's item replaces an earlier one
//! of the same name):
//!
//! ```text
//! built-in                      .wtd/templates/.claude/skills (the shipped skills)
//! global                        .wtd/custom/
//! platform                      .wtd/custom/platforms/<windows|wsl|linux|mac>/
//! repo                          .wtd/custom/repos/<slug>/
//! repo + platform               .wtd/custom/repos/<slug>/platforms/<os>/
//! ```
//!
//! Each layer may hold:
//!
//! ```text
//!   skills/<name>/SKILL.md      → .claude/skills/ (Claude) and .agents/skills/ (Codex)
//!   agents/<name>.md            → .claude/agents/   (subagents)
//!   commands/<name>.md          → .claude/commands/ (slash commands)
//!   rules/<name>.md             → .claude/rules/
//!   tools/<file>                → on PATH in every session (and its shell)
//!   CLAUDE.md                   → a marked block at the end of CLAUDE.md
//!   AGENTS.md                   → AGENTS.override.md for Codex (the repo's AGENTS.md + this, + a pointer
//!                                 to CLAUDE.md), so tracked files are never edited
//!   hooks.json                  → Claude hooks ({"hooks": {...}}) merged into .claude/settings.json
//!   disabled                    → one `<kind>/<name>` per line to drop, e.g. `skills/blueprint`
//!   env/                        → (repo layers) files copied into new worktrees when absent
//! ```
//!
//! Applied by `agent` on every open, and by `wtd custom apply` to worktrees that are already open.
//! Files a repo tracks are never overwritten; stale copies from an earlier apply are removed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{gitx, paths, settings};

const BEGIN: &str = "<!-- BEGIN custom (worktree-dev: from .wtd/custom — edit there, not here) -->";
const END: &str = "<!-- END custom -->";
const HOOKS_KEY: &str = "_wtdCustomHooks"; // the entries we merged last time, so a re-apply replaces them
const MANIFEST: &str = ".claude/.wtd-custom.json"; // the item paths we copied last time
const OVERRIDE: &str = "AGENTS.override.md";

pub const PLATFORMS: [&str; 4] = ["windows", "wsl", "linux", "mac"];

/// Directory kinds: layer subdir → where its items land in a worktree.
const DIR_KINDS: [(&str, &[&str]); 4] = [
    ("skills", &[".claude/skills", ".agents/skills"]),
    ("agents", &[".claude/agents"]),
    ("commands", &[".claude/commands"]),
    ("rules", &[".claude/rules"]),
];

pub fn root(dev: &Path) -> PathBuf {
    dev.join(".wtd").join("custom")
}

/// The OS this process runs on, as a preset platform name (`WTD_PLATFORM` overrides).
pub fn platform() -> &'static str {
    if let Ok(p) = std::env::var("WTD_PLATFORM") {
        if let Some(k) = PLATFORMS.iter().find(|k| **k == p) {
            return k;
        }
    }
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else if std::fs::read_to_string("/proc/version").is_ok_and(|v| v.to_lowercase().contains("microsoft")) {
        "wsl"
    } else {
        "linux"
    }
}

/// The custom layers for a repo on `os`, lowest first, with a label for `effective`.
fn layers_for(dev: &Path, slug: &str, os: &str) -> Vec<(String, PathBuf)> {
    let r = root(dev);
    let mut v = vec![("global".to_string(), r.clone()), (format!("platform:{os}"), r.join("platforms").join(os))];
    if !slug.is_empty() {
        let rr = r.join("repos").join(slug);
        v.push((format!("repo:{slug}"), rr.clone()));
        v.push((format!("repo:{slug}+{os}"), rr.join("platforms").join(os)));
    }
    v
}

fn layers(dev: &Path, slug: &str) -> Vec<PathBuf> {
    layers_for(dev, slug, platform()).into_iter().map(|(_, p)| p).collect()
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

fn copy_item(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        copy_tree(src, dst, true)?;
    } else {
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn remove_item(p: &Path) {
    let _ = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
}

fn disabled(layers: &[(String, PathBuf)]) -> BTreeSet<String> {
    layers.iter()
        .filter_map(|(_, l)| std::fs::read_to_string(l.join("disabled")).ok())
        .flat_map(|t| t.lines().map(|l| l.trim().trim_end_matches('/').to_string()).collect::<Vec<_>>())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// `kind/name` → (layer label, source path), later layers winning, disabled items dropped.
fn merged_items(dev: &Path, slug: &str, os: &str) -> BTreeMap<String, (String, PathBuf)> {
    let mut all = vec![("built-in".to_string(), crate::paths::asset_in(dev, "templates/.claude"))];
    all.extend(layers_for(dev, slug, os));
    let off = disabled(&all);
    let mut items = BTreeMap::new();
    for (label, l) in &all {
        for (kind, _) in DIR_KINDS {
            if label == "built-in" && kind != "skills" {
                continue;
            }
            for e in std::fs::read_dir(l.join(kind)).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                items.insert(format!("{kind}/{name}"), (label.clone(), e.path()));
            }
        }
    }
    items.retain(|k, _| !off.contains(k) && !off.contains(k.trim_end_matches(".md")));
    items
}

/// Worktree-relative files git tracks under the preset targets (never overwritten).
fn tracked(wtp: &Path) -> BTreeSet<String> {
    gitx::out(&["-C", &gitx::s(wtp), "ls-files", "--", ".claude", ".agents", OVERRIDE])
        .map(|o| o.lines().map(String::from).collect())
        .unwrap_or_default()
}

/// Copy the merged directory items into the worktree and drop what an earlier apply left behind.
fn sync_items(dev: &Path, slug: &str, wtp: &Path) -> Result<Vec<String>> {
    let tracked = tracked(wtp);
    let is_tracked = |rel: &str| tracked.iter().any(|t| t == rel || t.starts_with(&format!("{rel}/")));
    let mut now: Vec<String> = vec![];
    let mut kept = vec![];
    for (key, (_, src)) in merged_items(dev, slug, platform()) {
        let (kind, name) = key.split_once('/').unwrap();
        for target in DIR_KINDS.iter().find(|(k, _)| *k == kind).unwrap().1 {
            let rel = format!("{target}/{name}");
            if is_tracked(&rel) {
                kept.push(rel);
                continue;
            }
            copy_item(&src, &wtp.join(&rel))?;
            now.push(rel);
        }
    }
    let mpath = wtp.join(MANIFEST);
    let before: Vec<String> = std::fs::read_to_string(&mpath).ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| serde_json::from_value(v["items"].clone()).ok())
        .unwrap_or_default();
    // also drop disabled items the built-in copy in `agent` just put back
    let off = disabled(&layers_for(dev, slug, platform()));
    let stale: Vec<String> = before.iter().filter(|p| !now.contains(p)).cloned()
        .chain(off.iter().flat_map(|k| {
            let (kind, name) = k.split_once('/').unwrap_or((k, ""));
            DIR_KINDS.iter().filter(|(dk, _)| *dk == kind).flat_map(|(_, ts)| ts.iter().map(|t| format!("{t}/{name}"))).collect::<Vec<_>>()
        }))
        .filter(|p| !p.ends_with('/') && !is_tracked(p))
        .collect();
    for p in &stale {
        remove_item(&wtp.join(p));
    }
    if now != before {
        std::fs::create_dir_all(mpath.parent().unwrap())?;
        std::fs::write(&mpath, serde_json::to_string_pretty(&json!({ "items": now }))? + "\n")?;
    }
    exclude(wtp, &now);
    Ok(kept.into_iter().map(|k| format!("kept the repo's own {k}")).collect())
}

/// Hide the files we place from `git status` (shared info/exclude, anchored per item).
fn exclude(wtp: &Path, rels: &[String]) {
    let Ok(common) = gitx::out(&["-C", &gitx::s(wtp), "rev-parse", "--path-format=absolute", "--git-common-dir"]) else { return };
    let excl = PathBuf::from(common.trim()).join("info").join("exclude");
    let mut text = std::fs::read_to_string(&excl).unwrap_or_default();
    let before = text.len();
    let wanted = rels.iter().map(|r| format!("/{r}")).chain([format!("/{MANIFEST}"), format!("/{OVERRIDE}")]);
    for line in wanted {
        if !text.lines().any(|l| l.trim() == line) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&line);
            text.push('\n');
        }
    }
    if text.len() != before {
        let _ = std::fs::create_dir_all(excl.parent().unwrap());
        let _ = std::fs::write(&excl, text);
    }
}

fn layer_text(dev: &Path, slug: &str, file: &str) -> String {
    layers(dev, slug)
        .iter()
        .filter_map(|l| std::fs::read_to_string(l.join(file)).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `text` with its custom block replaced by `body` (or removed when empty).
fn with_block(text: &str, body: &str) -> String {
    let base = match (text.find(BEGIN), text.find(END)) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &text[..a], &text[b + END.len()..]).trim_end().to_string() + "\n",
        _ => text.to_string(),
    };
    match (body.is_empty(), base.trim_end()) {
        (true, _) => base,
        (false, "") => format!("{BEGIN}\n{body}\n{END}\n"),
        (false, head) => format!("{head}\n\n{BEGIN}\n{body}\n{END}\n"),
    }
}

/// Replace (or add, or drop) the custom block at the end of a worktree's CLAUDE.md.
fn sync_claude_md(dev: &Path, slug: &str, wtp: &Path) -> Result<bool> {
    let p = wtp.join("CLAUDE.md");
    let Ok(text) = std::fs::read_to_string(&p) else { return Ok(false) };
    let new = with_block(&text, &layer_text(dev, slug, "CLAUDE.md"));
    if new != text {
        std::fs::write(&p, new)?;
        return Ok(true);
    }
    Ok(false)
}

/// Codex reads AGENTS.override.md instead of AGENTS.md: write the repo's AGENTS.md plus a block with
/// the AGENTS.md presets and a pointer to the worktree's CLAUDE.md (its conventions apply to Codex too).
/// A user-made AGENTS.override.md (no block of ours) is left alone.
fn sync_agents_md(dev: &Path, slug: &str, wtp: &Path) -> Result<bool> {
    let p = wtp.join(OVERRIDE);
    let cur = std::fs::read_to_string(&p).ok();
    if cur.as_deref().is_some_and(|c| !c.contains(BEGIN)) {
        return Ok(false);
    }
    let mut body = vec![];
    if wtp.join("CLAUDE.md").is_file() {
        body.push("Read and follow `CLAUDE.md` in this worktree: its conventions (plan, commits, PRs, reviews) apply to you too.".to_string());
    }
    let extra = layer_text(dev, slug, "AGENTS.md");
    if !extra.is_empty() {
        body.push(extra);
    }
    let base = std::fs::read_to_string(wtp.join("AGENTS.md")).unwrap_or_default();
    let new = if body.is_empty() { String::new() } else { with_block(&base, &body.join("\n\n")) };
    match (cur, new.is_empty()) {
        (None, true) => Ok(false),
        (Some(_), true) => {
            std::fs::remove_file(&p)?;
            Ok(true)
        }
        (Some(c), false) if c == new => Ok(false),
        _ => {
            std::fs::write(&p, new)?;
            Ok(true)
        }
    }
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

/// Apply every preset layer to a worktree. `fresh` = just created (seed env files too).
pub fn apply(dev: &Path, slug: &str, wtp: &Path, fresh: bool) -> Result<Vec<String>> {
    let mut log = sync_items(dev, slug, wtp)?;
    for l in layers(dev, slug) {
        let env = l.join("env");
        if env.is_dir() && l.starts_with(root(dev).join("repos")) {
            let n = copy_tree(&env, wtp, false)?;
            if n > 0 && fresh {
                log.push(format!("seeded {n} custom env file(s)"));
            }
        }
    }
    if sync_claude_md(dev, slug, wtp)? {
        log.push("CLAUDE.md custom section updated".into());
    }
    if sync_agents_md(dev, slug, wtp)? {
        log.push(format!("{OVERRIDE} (Codex) updated"));
    }
    if sync_hooks(dev, slug, wtp)? {
        log.push("custom hooks merged into .claude/settings.json".into());
    }
    Ok(log)
}

fn names(dir: &Path, dirs: Option<bool>) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| r.flatten().filter(|e| dirs.is_none_or(|d| e.path().is_dir() == d)).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn layer_info(l: &Path) -> Value {
    json!({
        "path": l.to_string_lossy(),
        "skills": names(&l.join("skills"), Some(true)),
        "agents": names(&l.join("agents"), None),
        "commands": names(&l.join("commands"), None),
        "rules": names(&l.join("rules"), None),
        "tools": names(&l.join("tools"), Some(false)),
        "claudeMd": l.join("CLAUDE.md").is_file(),
        "agentsMd": l.join("AGENTS.md").is_file(),
        "hooks": l.join("hooks.json").is_file(),
        "disabled": std::fs::read_to_string(l.join("disabled")).map(|t| t.lines().map(str::trim).filter(|s| !s.is_empty() && !s.starts_with('#')).map(String::from).collect::<Vec<_>>()).unwrap_or_default(),
        "env": std::fs::read_dir(l.join("env")).map(|r| r.count()).unwrap_or(0),
    })
}

fn platform_infos(l: &Path) -> Value {
    let mut m = serde_json::Map::new();
    for os in names(&l.join("platforms"), Some(true)) {
        m.insert(os.clone(), layer_info(&l.join("platforms").join(&os)));
    }
    Value::Object(m)
}

pub fn ls(dev: &Path) -> Value {
    let r = root(dev);
    let mut repos = serde_json::Map::new();
    for slug in names(&r.join("repos"), Some(true)) {
        let rr = r.join("repos").join(&slug);
        let mut info = layer_info(&rr);
        info["platforms"] = platform_infos(&rr);
        repos.insert(slug.clone(), info);
    }
    let builtin = names(&crate::paths::asset_in(dev, "templates/.claude/skills"), Some(true));
    json!({ "root": r.to_string_lossy(), "platform": platform(), "platforms_all": PLATFORMS, "builtin": { "skills": builtin }, "global": layer_info(&r), "platforms": platform_infos(&r), "repos": repos })
}

/// What a repo's worktrees get on `os`: each item with the layer it comes from.
pub fn effective(dev: &Path, slug: &str, os: &str) -> Value {
    let items: Vec<Value> = merged_items(dev, slug, os).into_iter().map(|(k, (layer, src))| json!({ "item": k, "layer": layer, "path": src.to_string_lossy() })).collect();
    let files = |f: &str| -> Vec<Value> {
        layers_for(dev, slug, os).into_iter().filter(|(_, l)| l.join(f).is_file()).map(|(lab, l)| json!({ "layer": lab, "path": l.join(f).to_string_lossy() })).collect()
    };
    json!({
        "slug": slug, "platform": os, "items": items,
        "claudeMd": files("CLAUDE.md"), "agentsMd": files("AGENTS.md"), "hooks": files("hooks.json"),
        "tools": layers_for(dev, slug, os).into_iter().filter(|(_, l)| l.join("tools").is_dir()).map(|(lab, l)| json!({ "layer": lab, "path": l.join("tools").to_string_lossy() })).collect::<Vec<_>>(),
    })
}

/// A layer directory from `--repo` / `--platform`.
pub fn layer_dir(dev: &Path, repo: Option<&str>, os: Option<&str>) -> Result<PathBuf> {
    let base = match repo {
        Some(s) if !settings::valid_name(s) => bail!("bad repo name '{s}'"),
        Some(s) => root(dev).join("repos").join(s),
        None => root(dev),
    };
    Ok(match os {
        Some(p) if !PLATFORMS.contains(&p) => bail!("platform is one of {}", PLATFORMS.join(", ")),
        Some(p) => base.join("platforms").join(p),
        None => base,
    })
}

const SKILL_TEMPLATE: &str = "---
name: __NAME__
description: >
  What this skill does and WHEN to use it (the trigger phrases matter — the agent reads this to
  decide whether to invoke the skill). Invoked by the user as /__NAME__.
allowed-tools: Read, Grep, Glob, Bash
---

# __NAME__

Steps to follow, in order. Be concrete: the commands to run, the files to read, what to report back.
";

const AGENT_TEMPLATE: &str = "---
name: __NAME__
description: When Claude should hand work to this subagent.
tools: Read, Grep, Glob, Bash
---

You are __NAME__. Your job, the steps you follow, and what you return.
";

const COMMAND_TEMPLATE: &str = "---
description: What /__NAME__ does.
---

The prompt this command runs. `$ARGUMENTS` is what the user typed after /__NAME__.
";

const RULE_TEMPLATE: &str = "---
paths: [\"**/*\"]
---

# __NAME__

A rule Claude follows when working on matching files.
";

/// Create `<kind>/<name>` in a layer from a starter template; returns the file to edit.
pub fn new_item(layer: &Path, kind: &str, name: &str) -> Result<PathBuf> {
    if !settings::valid_name(name) {
        bail!("names are letters, digits, - or _");
    }
    let (file, tpl) = match kind {
        "skill" | "skills" => (layer.join("skills").join(name).join("SKILL.md"), SKILL_TEMPLATE),
        "agent" | "agents" => (layer.join("agents").join(format!("{name}.md")), AGENT_TEMPLATE),
        "command" | "commands" => (layer.join("commands").join(format!("{name}.md")), COMMAND_TEMPLATE),
        "rule" | "rules" => (layer.join("rules").join(format!("{name}.md")), RULE_TEMPLATE),
        _ => bail!("kind is skill, agent, command or rule"),
    };
    if !file.exists() {
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, tpl.replace("__NAME__", name))?;
    }
    Ok(file)
}

/// Worktrees `apply --all` reaches: every one `agent` applies presets to on open, delegators included.
fn bulk_target(id: &str, slug: &str) -> bool {
    id != wtd_core::model::DEV_ID && slug != "plan"
}

const USAGE: &str = "usage: wtd custom ls | effective <slug> [--platform os] | dir | new <skill|agent|command|rule> <name> \
| disable|enable <kind>/<name> | apply [<slug>/<name> | --all] | proposals | accept|reject <id>   (layer flags: --repo <slug> --platform <os>)";

/// `wtd custom …` — see USAGE
pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let flag = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).map(String::as_str);
    let (repo, os) = (flag("--repo"), flag("--platform"));
    let pos: Vec<&str> = {
        let mut v = vec![];
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--repo" || args[i] == "--platform" { i += 2; continue; }
            v.push(args[i].as_str());
            i += 1;
        }
        v
    };
    match pos.as_slice() {
        [] | ["ls"] => settings::out(ls(&dev)),
        ["proposals"] | ["accept", _] | ["reject", _] => crate::proposal::main(&dev, &pos),
        ["effective", slug] => settings::out(effective(&dev, slug, os.unwrap_or(platform()))),
        ["dir"] => {
            let d = layer_dir(&dev, repo, os)?;
            std::fs::create_dir_all(&d)?;
            settings::out(json!({ "path": d.to_string_lossy() }))
        }
        ["new-skill", name] => settings::out(json!({ "path": new_item(&layer_dir(&dev, repo, os)?, "skill", name)?.to_string_lossy() })),
        ["new", kind, name] => settings::out(json!({ "path": new_item(&layer_dir(&dev, repo, os)?, kind, name)?.to_string_lossy() })),
        [verb @ ("disable" | "enable"), item] => {
            let d = layer_dir(&dev, repo, os)?;
            let f = d.join("disabled");
            let mut lines: Vec<String> = std::fs::read_to_string(&f).unwrap_or_default().lines().map(String::from).filter(|l| l.trim() != *item).collect();
            if *verb == "disable" {
                lines.push(item.to_string());
            }
            std::fs::create_dir_all(&d)?;
            std::fs::write(&f, lines.join("\n") + "\n")?;
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
            for w in crate::daemon::scan::scan(&dev).into_iter().filter(|w| bulk_target(&w.id, &w.slug)) {
                match apply(&dev, &w.slug, Path::new(&w.path), false) {
                    Ok(_) => n += 1,
                    Err(e) => eprintln!("{}: {e:#}", w.id),
                }
            }
            println!("applied customizations to {n} worktree(s)");
            Ok(0)
        }
        _ => bail!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let t = std::env::temp_dir().join(format!("wtd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join(".wtd").join("custom").join("repos").join("r")).unwrap();
        std::fs::create_dir_all(t.join("wt")).unwrap();
        t
    }

    #[test]
    fn apply_all_includes_delegators() {
        assert!(bulk_target("delegate/worktree-dev", crate::paths::DELEGATE));
        assert!(bulk_target("luop/feat/x", "luop"));
        assert!(!bulk_target(wtd_core::model::DEV_ID, ""));
        assert!(!bulk_target("plan/x", "plan"));
    }

    #[test]
    fn claude_md_block_is_replaced_not_duplicated() {
        let t = tmp("custom");
        let wt = t.join("wt");
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
        let t = tmp("customh");
        let wt = t.join("wt");
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

    #[test]
    fn layers_override_by_name_and_disable_drops() {
        let t = tmp("customl");
        let c = t.join(".wtd/custom");
        std::fs::create_dir_all(t.join(".wtd/templates/.claude/skills/blueprint")).unwrap();
        std::fs::create_dir_all(t.join(".wtd/templates/.claude/skills/commit")).unwrap();
        std::fs::create_dir_all(c.join("agents")).unwrap();
        std::fs::write(c.join("agents/helper.md"), "g").unwrap();
        std::fs::create_dir_all(c.join("platforms/wsl/agents")).unwrap();
        std::fs::write(c.join("platforms/wsl/agents/helper.md"), "wsl").unwrap();
        std::fs::create_dir_all(c.join("repos/r/skills/commit")).unwrap();
        std::fs::write(c.join("repos/r/disabled"), "skills/blueprint\n").unwrap();
        let win = merged_items(&t, "r", "windows");
        assert_eq!(win["agents/helper.md"].0, "global");
        assert_eq!(win["skills/commit"].0, "repo:r");
        assert!(!win.contains_key("skills/blueprint"));
        assert_eq!(merged_items(&t, "r", "wsl")["agents/helper.md"].0, "platform:wsl");
        assert!(merged_items(&t, "other", "windows").contains_key("skills/blueprint"));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn agents_override_wraps_repo_agents_md() {
        let t = tmp("customa");
        let wt = t.join("wt");
        std::fs::write(wt.join("CLAUDE.md"), "# conventions").unwrap();
        std::fs::write(wt.join("AGENTS.md"), "# repo agents").unwrap();
        std::fs::write(t.join(".wtd/custom/AGENTS.md"), "codex rule").unwrap();
        assert!(sync_agents_md(&t, "r", &wt).unwrap());
        let s = std::fs::read_to_string(wt.join(OVERRIDE)).unwrap();
        assert!(s.starts_with("# repo agents") && s.contains("codex rule") && s.contains("CLAUDE.md"));
        assert!(!sync_agents_md(&t, "r", &wt).unwrap());
        std::fs::write(wt.join(OVERRIDE), "mine").unwrap();
        assert!(!sync_agents_md(&t, "r", &wt).unwrap()); // a user's own override is left alone
        std::fs::remove_dir_all(&t).ok();
    }
}
