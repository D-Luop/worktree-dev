//! Preset proposals: an agent suggests a change to `.wtd/custom` (via MCP `preset_propose`) and nothing
//! is written until the user accepts it — in VS Code (a notification with a diff) or with
//! `wtd custom accept <id>`. Stored as `.wtd/state/preset-proposals/<id>.json`, with the proposed text
//! beside it (`<id>.proposed<ext>`) so editors can diff it against the current file.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{custom, paths, settings};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proposal {
    pub id: String,
    /// worktree id of the proposing session, if known
    pub from: Option<String>,
    pub created: i64,
    pub repo: Option<String>,
    pub platform: Option<String>,
    /// path inside the layer, e.g. `skills/deploy/SKILL.md` or `CLAUDE.md`
    pub path: String,
    /// new contents; None = delete the file
    pub content: Option<String>,
    pub reason: String,
    /// pending | accepted | rejected
    pub state: String,
}

pub fn dir(dev: &Path) -> PathBuf {
    paths::state_dir(dev).join("preset-proposals")
}

/// Where the proposed file lands (validated: only preset kinds, no tools — those run as programs).
pub fn target(dev: &Path, p: &Proposal) -> Result<PathBuf> {
    check_path(&p.path)?;
    Ok(p.path.split('/').fold(custom::layer_dir(dev, p.repo.as_deref(), p.platform.as_deref())?, |d, c| d.join(c)))
}

fn check_path(rel: &str) -> Result<()> {
    let parts: Vec<&str> = rel.split('/').collect();
    let name_ok = |n: &str| settings::valid_name(n.trim_end_matches(".md"));
    let ok = match parts.as_slice() {
        ["CLAUDE.md" | "AGENTS.md" | "hooks.json" | "disabled"] => true,
        ["skills", n, file] => settings::valid_name(n) && !file.starts_with('.') && file.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
        ["agents" | "commands" | "rules", f] => f.ends_with(".md") && name_ok(f),
        _ => false,
    };
    if !ok {
        bail!("path must be CLAUDE.md, AGENTS.md, hooks.json, disabled, skills/<name>/<file>, or agents|commands|rules/<name>.md");
    }
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn proposed_path(dev: &Path, p: &Proposal) -> PathBuf {
    let ext = Path::new(&p.path).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    dir(dev).join(format!("{}.proposed{ext}", p.id))
}

fn save(dev: &Path, p: &Proposal) -> Result<()> {
    std::fs::create_dir_all(dir(dev))?;
    std::fs::write(dir(dev).join(format!("{}.json", p.id)), serde_json::to_string_pretty(p)? + "\n")?;
    Ok(())
}

pub fn create(dev: &Path, from: Option<&str>, repo: Option<&str>, platform: Option<&str>, rel: &str, content: Option<&str>, reason: &str) -> Result<Proposal> {
    let p = Proposal {
        id: format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)),
        from: from.map(String::from),
        created: now(),
        repo: repo.filter(|s| !s.is_empty()).map(String::from),
        platform: platform.filter(|s| !s.is_empty()).map(String::from),
        path: rel.trim_start_matches('/').to_string(),
        content: content.map(String::from),
        reason: reason.to_string(),
        state: "pending".into(),
    };
    target(dev, &p)?; // validate before storing
    if p.path == "hooks.json" {
        if let Some(c) = &p.content {
            serde_json::from_str::<serde_json::Value>(c).context("hooks.json content must be valid JSON")?;
        }
    }
    save(dev, &p)?;
    std::fs::write(proposed_path(dev, &p), p.content.clone().unwrap_or_default())?;
    Ok(p)
}

pub fn list(dev: &Path) -> Vec<Proposal> {
    let mut v: Vec<Proposal> = std::fs::read_dir(dir(dev)).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str(&t).ok()))
        .collect();
    v.sort_by_key(|p| p.created);
    v
}

fn load(dev: &Path, id: &str) -> Result<Proposal> {
    let t = std::fs::read_to_string(dir(dev).join(format!("{id}.json"))).with_context(|| format!("no proposal '{id}'"))?;
    Ok(serde_json::from_str(&t)?)
}

/// Accept: write (or delete) the target and mark it accepted. Returns the target path.
pub fn accept(dev: &Path, id: &str) -> Result<PathBuf> {
    let mut p = load(dev, id)?;
    if p.state != "pending" {
        bail!("proposal {id} is already {}", p.state);
    }
    let t = target(dev, &p)?;
    match &p.content {
        Some(c) => {
            std::fs::create_dir_all(t.parent().unwrap())?;
            std::fs::write(&t, c)?;
        }
        None => {
            let _ = std::fs::remove_file(&t);
        }
    }
    p.state = "accepted".into();
    save(dev, &p)?;
    let _ = std::fs::remove_file(proposed_path(dev, &p));
    Ok(t)
}

pub fn reject(dev: &Path, id: &str) -> Result<()> {
    let mut p = load(dev, id)?;
    p.state = "rejected".into();
    save(dev, &p)?;
    let _ = std::fs::remove_file(proposed_path(dev, &p));
    Ok(())
}

/// `wtd custom proposals | accept <id> | reject <id>` (JSON out)
pub fn main(dev: &Path, args: &[&str]) -> Result<i32> {
    match args {
        ["proposals"] => settings::out(json!(list(dev).into_iter().map(|p| {
            let target = target(dev, &p).map(|t| t.to_string_lossy().to_string()).unwrap_or_default();
            let proposed = proposed_path(dev, &p).to_string_lossy().to_string();
            json!({ "proposal": p, "target": target, "proposed": proposed })
        }).collect::<Vec<_>>())),
        ["accept", id] => settings::out(json!({ "path": accept(dev, id)?.to_string_lossy() })),
        ["reject", id] => {
            reject(dev, id)?;
            settings::out(json!({ "rejected": id }))
        }
        _ => bail!("usage: wtd custom proposals | accept <id> | reject <id>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_limited_to_preset_kinds() {
        for ok in ["CLAUDE.md", "AGENTS.md", "hooks.json", "disabled", "skills/deploy/SKILL.md", "agents/db-expert.md", "rules/sql.md"] {
            assert!(check_path(ok).is_ok(), "{ok}");
        }
        for bad in ["tools/x.sh", "skills/../x/SKILL.md", "agents/x.sh", "../CLAUDE.md", "skills/a/../../b", "commands/a/b.md"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn accept_writes_and_reject_does_not() {
        let dev = std::env::temp_dir().join(format!("wtd-prop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dev);
        std::fs::create_dir_all(dev.join(".wtd").join("state")).unwrap();
        let p = create(&dev, Some("r/feat/x"), Some("r"), None, "agents/helper.md", Some("hi"), "useful").unwrap();
        assert_eq!(list(&dev)[0].state, "pending");
        let t = accept(&dev, &p.id).unwrap();
        assert_eq!(std::fs::read_to_string(&t).unwrap(), "hi");
        assert!(accept(&dev, &p.id).is_err());
        let q = create(&dev, None, None, Some("wsl"), "CLAUDE.md", Some("x"), "").unwrap();
        reject(&dev, &q.id).unwrap();
        assert!(!custom::layer_dir(&dev, None, Some("wsl")).unwrap().join("CLAUDE.md").exists());
        assert!(create(&dev, None, None, Some("beos"), "CLAUDE.md", Some("x"), "").is_err());
        std::fs::remove_dir_all(&dev).ok();
    }
}
