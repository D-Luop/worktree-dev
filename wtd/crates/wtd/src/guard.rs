//! PR guardrails, configurable per repo (Settings → Guardrails). Stored in `.wtd/config.json`:
//! `guardrails` = the defaults for every repo, `repos.<slug>.guardrails` = a repo's own (replaces the
//! defaults wholesale). Shape:
//!
//! ```json
//! { "enabled": true,
//!   "preflight": ["go build ./...", "go vet ./..."],   // commands that must pass, run in the worktree
//!   "requireClean": true,                             // no uncommitted changes
//!   "requirePushed": true,                            // the branch is on its upstream
//!   "blockPush": ["planning", "main"] }               // branch globs a worktree may never push
//! ```
//!
//! Enforcement isn't advisory: `wtd preflight` records a pass stamp at HEAD, and `agent pr` (the only
//! way a worktree becomes PR-ready) refuses unless HEAD passed (running preflight itself if needed),
//! the tree is clean, and it's pushed. `blockPush` is a git pre-push hook (`wtd guard pre-push`)
//! installed into every repo, so it holds for any push — the agent's, yours, or a tool's.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::gitx::{self, s};
use crate::{paths, wt};

pub fn defaults() -> Value {
    json!({ "enabled": false, "preflight": [], "requireClean": true, "requirePushed": true, "blockPush": [] })
}

/// The guardrails in force for a repo, with `"source": "repo" | "defaults"`.
pub fn effective(dev: &Path, slug: &str) -> Value {
    let cfg = crate::settings::load_config(dev);
    let (mut v, source) = match cfg.pointer(&format!("/repos/{slug}/guardrails")).filter(|v| v.is_object()) {
        Some(r) => (r.clone(), "repo"),
        None => (cfg.get("guardrails").filter(|v| v.is_object()).cloned().unwrap_or_else(defaults), "defaults"),
    };
    let d = defaults();
    for (k, dv) in d.as_object().unwrap() {
        if v.get(k).is_none() {
            v[k] = dv.clone();
        }
    }
    v["source"] = json!(source);
    v
}

pub fn validate(v: &Value) -> Result<()> {
    if v.is_null() {
        return Ok(());
    }
    let o = v.as_object().context("guardrails must be an object")?;
    for k in o.keys() {
        if !["enabled", "preflight", "requireClean", "requirePushed", "blockPush", "source"].contains(&k.as_str()) {
            bail!("unknown guardrail '{k}'");
        }
    }
    for k in ["preflight", "blockPush"] {
        if let Some(x) = o.get(k) {
            if !x.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
                bail!("{k} must be a list of strings");
            }
        }
    }
    Ok(())
}

fn flag(g: &Value, k: &str) -> bool {
    g.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn list(g: &Value, k: &str) -> Vec<String> {
    g.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)).collect()).unwrap_or_default()
}

/// `*` matches any run of characters (including `/`).
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    true
}

fn stamp_key(id: &str) -> String {
    id.replace('/', "__")
}

fn head(wt: &Path) -> Result<String> {
    gitx::out(&["-C", &s(wt), "rev-parse", "HEAD"]).context("no HEAD commit yet")
}

fn dirty(wt: &Path) -> Vec<String> {
    gitx::out(&["-C", &s(wt), "status", "--porcelain"]).map(|o| o.lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()).collect()).unwrap_or_default()
}

/// Git for Windows' bash (the preflight commands are written for a POSIX shell); `cmd` as a fallback.
fn shell() -> (PathBuf, Vec<&'static str>) {
    let mut cands: Vec<PathBuf> = vec![];
    if let Some(git) = crate::settings::which("git") {
        // <Git>\cmd\git.exe or <Git>\bin\git.exe → <Git>\bin\bash.exe
        if let Some(root) = git.parent().and_then(Path::parent) {
            cands.push(root.join("bin").join("bash.exe"));
        }
    }
    for pf in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(d) = std::env::var_os(pf) {
            cands.push(PathBuf::from(d).join("Git").join("bin").join("bash.exe"));
        }
    }
    match cands.into_iter().find(|p| p.is_file()) {
        Some(b) => (b, vec!["-lc"]),
        None => (PathBuf::from("cmd"), vec!["/C"]),
    }
}

pub struct Outcome {
    pub ok: bool,
    pub lines: Vec<String>,
}

/// Run a worktree's preflight. Streams command output to this console; records the pass stamp.
pub fn run_preflight(dev: &Path, r: &paths::WtRef, g: &Value, echo: bool) -> Result<Outcome> {
    let mut lines: Vec<String> = vec![];
    let mut ok = true;
    // print as we go (the commands stream their own output in between)
    let say = |l: String, lines: &mut Vec<String>| { if echo { println!("{l}"); } lines.push(l); };
    if flag(g, "requireClean") {
        let d = dirty(&r.root);
        if d.is_empty() {
            say("✓ working tree clean".into(), &mut lines);
        } else {
            ok = false;
            say(format!("✗ uncommitted changes ({} file(s)) — commit or stash them first:", d.len()), &mut lines);
            for l in d.iter().take(10) {
                say(format!("    {l}"), &mut lines);
            }
        }
    }
    if ok {
        let (sh, pre) = shell();
        for cmd in list(g, "preflight") {
            let mut c = Command::new(&sh);
            c.args(&pre).arg(&cmd).current_dir(&r.root);
            let (st, tail) = if echo {
                println!("\x1b[1m▸ {cmd}\x1b[0m");
                (c.status().with_context(|| format!("running `{cmd}`"))?, vec![])
            } else {
                crate::win::no_window(&mut c);
                let o = c.output().with_context(|| format!("running `{cmd}`"))?;
                let text = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
                let all: Vec<String> = text.lines().map(String::from).collect();
                (o.status, all[all.len().saturating_sub(20)..].to_vec())
            };
            if st.success() {
                say(format!("✓ {cmd}"), &mut lines);
            } else {
                ok = false;
                say(format!("✗ {cmd} (exit {})", st.code().unwrap_or(-1)), &mut lines);
                lines.extend(tail.into_iter().map(|l| format!("    {l}")));
                break;
            }
        }
    }
    let key = stamp_key(&r.id);
    if ok {
        wt::write_state(dev, "preflight", &key, &head(&r.root)?)?;
    } else {
        wt::forget_state(dev, "preflight", &key);
    }
    Ok(Outcome { ok, lines })
}

fn here_or(dev: &Path, arg: Option<&str>) -> Result<paths::WtRef> {
    match arg {
        Some(id) => {
            let root = paths::worktree_path(dev, id);
            if !root.join(".git").exists() {
                bail!("no worktree '{id}'");
            }
            Ok(paths::WtRef { id: id.to_string(), root })
        }
        None => {
            let cwd = std::env::current_dir()?;
            paths::resolve_worktree(dev, &cwd).filter(|r| r.id != wtd_core::model::DEV_ID).context("not inside a worktree (or pass <slug>/<name>)")
        }
    }
}

/// `wtd preflight [<slug>/<name>] [--json]`
pub fn preflight_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let json_out = args.iter().any(|a| a == "--json");
    let r = here_or(&dev, args.iter().find(|a| !a.starts_with("--")).map(String::as_str))?;
    let slug = r.id.split('/').next().unwrap_or("").to_string();
    let g = effective(&dev, &slug);
    if !flag(&g, "enabled") {
        let msg = format!("no guardrails enabled for '{slug}' (Settings → Guardrails) — nothing to check");
        if json_out { crate::settings::out(json!({ "ok": true, "enabled": false, "lines": [msg] }))?; } else { println!("{msg}"); }
        return Ok(0);
    }
    let o = run_preflight(&dev, &r, &g, !json_out)?;
    if json_out {
        crate::settings::out(json!({ "ok": o.ok, "enabled": true, "lines": o.lines }))?;
    } else {
        println!("{}", if o.ok { "\x1b[1;32mpreflight passed\x1b[0m — stamped this commit; `agent pr` can mark it PR-ready" } else { "\x1b[1;31mpreflight failed\x1b[0m — fix the above, commit, and run `wtd preflight` again" });
    }
    Ok(if o.ok { 0 } else { 1 })
}

/// The gate `agent pr` passes through. Runs preflight when HEAD has no pass stamp.
pub fn check_pr(dev: &Path, r: &paths::WtRef) -> Result<()> {
    let slug = r.id.split('/').next().unwrap_or("");
    let g = effective(dev, slug);
    if !flag(&g, "enabled") {
        return Ok(());
    }
    let h = head(&r.root)?;
    if wt::read_state(dev, "preflight", &stamp_key(&r.id)).as_deref() != Some(h.as_str()) {
        println!("guardrails: this commit hasn't passed preflight yet — running it now");
        let o = run_preflight(dev, r, &g, true)?;
        if !o.ok {
            bail!("guardrails: preflight failed — not marking PR-ready. Fix it, commit, then run `agent pr` again.");
        }
    }
    if flag(&g, "requireClean") && !dirty(&r.root).is_empty() {
        bail!("guardrails: the working tree has uncommitted changes — commit them first");
    }
    if flag(&g, "requirePushed") {
        let rs = s(&r.root);
        if !gitx::ok(&["-C", &rs, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]) {
            bail!("guardrails: the branch has no upstream — push it first: git push -u origin HEAD");
        }
        let ahead = gitx::out(&["-C", &rs, "rev-list", "--count", "@{upstream}..HEAD"]).unwrap_or_default();
        if ahead.trim() != "0" {
            bail!("guardrails: {} commit(s) aren't pushed — push first: git push", ahead.trim());
        }
    }
    Ok(())
}

/// `wtd guard pre-push <remote> <url>` — git's pre-push hook. Refs arrive on stdin as
/// `<local ref> <local sha> <remote ref> <remote sha>` lines.
pub fn guard_main(args: &[String]) -> Result<i32> {
    if args.first().map(String::as_str) != Some("pre-push") {
        bail!("usage: wtd guard pre-push <remote> <url>   (git's pre-push hook)");
    }
    let Ok(dev) = paths::dev_root() else { return Ok(0) };
    let cwd = std::env::current_dir()?;
    // the repo this push is from: a worktree of ours, else a repo whose clone is this folder
    let slug = paths::resolve_worktree(&dev, &cwd).filter(|r| r.id != wtd_core::model::DEV_ID).map(|r| r.id.split('/').next().unwrap_or("").to_string())
        .or_else(|| crate::repos::all(&dev).into_iter().find(|r| cwd.starts_with(&r.path)).map(|r| r.slug));
    let Some(slug) = slug else { return Ok(0) };
    let g = effective(&dev, &slug);
    let blocked = list(&g, "blockPush");
    if !flag(&g, "enabled") || blocked.is_empty() {
        return Ok(0);
    }
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).ok();
    for line in input.lines() {
        let Some(remote_ref) = line.split_whitespace().nth(2) else { continue };
        let branch = remote_ref.strip_prefix("refs/heads/").unwrap_or(remote_ref);
        if let Some(p) = blocked.iter().find(|p| glob(p, branch)) {
            eprintln!("worktree-dev guardrails: pushing '{branch}' is blocked for '{slug}' (matches '{p}'). Change it in Settings → Guardrails.");
            return Ok(1);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("main", "main"));
        assert!(!glob("main", "mainline"));
        assert!(glob("release/*", "release/1.2"));
        assert!(glob("*planning*", "feat/planning-x"));
        assert!(!glob("release/*", "feat/release"));
        assert!(glob("*", "anything"));
    }

    #[test]
    fn validates() {
        assert!(validate(&json!({ "enabled": true, "preflight": ["make test"] })).is_ok());
        assert!(validate(&json!({ "preflight": "make test" })).is_err());
        assert!(validate(&json!({ "bogus": 1 })).is_err());
    }
}
