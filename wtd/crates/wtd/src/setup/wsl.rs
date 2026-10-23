//! `wtd setup --wsl <distro>`: wire a distro's Claude Code and Codex to this machine's `wtd.exe`
//! through WSL interop — status hooks, the `wtd` MCP server, statusline, attribution, the refs grant
//! and the `agent`/`wtd`/… commands in `~/.local/bin`. Files are edited through `\\wsl.localhost`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{merge, read_text, shell, write_text, Setup};
use crate::wsl_backend::{self, path};

/// The status hooks for a distro: `<exe> hook <event>` through `/mnt`. `plan-nudge.sh` becomes the
/// built-in `hook plan-nudge`; `plan-hint.sh` (jq, python and the preview shim in the distro) is left out.
pub fn render_hooks(template: &str, exe_mnt: &str) -> Result<Value> {
    let mut v = merge::render_hooks(template, "", "", exe_mnt)?;
    let exe = merge::shell_path(exe_mnt);
    let hooks = v.get_mut("hooks").and_then(Value::as_object_mut).context("claude-hooks.json has no hooks")?;
    for entries in hooks.values_mut().filter_map(Value::as_array_mut) {
        entries.retain(|e| !e.to_string().contains("plan-hint.sh"));
        for h in entries.iter_mut().filter_map(|e| e.get_mut("hooks")).filter_map(Value::as_array_mut).flatten() {
            if h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("plan-nudge.sh")) {
                h["command"] = json!(format!("{exe} hook plan-nudge"));
            }
        }
    }
    hooks.retain(|_, l| l.as_array().is_none_or(|a| !a.is_empty()));
    Ok(v)
}

/// Everything `wtd setup` puts in `~/.claude/settings.json`, for the distro's copy.
pub fn apply_settings(settings: &mut Value, rendered: &Value, refs_root: &str) {
    merge::merge_hooks(settings, rendered);
    merge::set_statusline(settings);
    merge::set_attribution(settings);
    merge::grant_refs(settings, refs_root);
}

/// `mcpServers.wtd` in a Claude config file (`~/.claude.json`), as `claude mcp add --scope user` writes it.
pub fn set_mcp(claude_json: &mut Value, exe_mnt: &str) {
    if !claude_json.is_object() {
        *claude_json = json!({});
    }
    let servers = claude_json.as_object_mut().unwrap().entry("mcpServers").or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers["wtd"] = json!({ "type": "stdio", "command": exe_mnt, "args": ["mcp"], "env": {} });
}

fn save_json(p: &Path, v: &Value) -> Result<bool> {
    let text = merge::to_text(v, 2);
    if read_text(p).as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    if p.is_file() {
        std::fs::copy(p, p.with_extension("json.bak"))?;
    }
    write_text(p, &text)?;
    Ok(true)
}

fn load_json(p: &Path) -> Result<Value> {
    match read_text(p) {
        None => Ok(json!({})),
        Some(t) => merge::parse_object(&t).with_context(|| format!("{} is not valid JSON; fix it and re-run", p.display())),
    }
}

impl Setup {
    pub(super) fn wsl(&mut self, distro: &str) -> Result<()> {
        let known = wsl_backend::distros();
        if !known.iter().any(|d| d.eq_ignore_ascii_case(distro)) {
            bail!("no WSL distro '{distro}' (installed: {})", if known.is_empty() { "none".to_string() } else { known.join(", ") });
        }
        let home = wsl_backend::home(&self.dev, distro)?;
        let root = wsl_backend::dev_root(&self.dev, distro)?;
        let unc = |linux: &str| path::unc(distro, linux);
        let exe_mnt = path::to_mnt(&self.layout.exe()).with_context(|| format!("{} isn't on a drive WSL mounts", self.layout.exe().display()))?;
        println!("==> WSL '{distro}': home {home}, dev root {root}, wtd.exe as {exe_mnt}");
        match wsl_backend::run(&wsl_backend::exec_args(distro, None, &[&exe_mnt, "version"])) {
            Ok(_) => println!("    interop ok: the distro can run wtd.exe"),
            Err(e) => println!("    WARNING: running {exe_mnt} from '{distro}' failed ({e:#}); hooks need WSL interop (/etc/wsl.conf [interop] enabled=true)"),
        }

        let distro_s = distro.to_string();
        self.step("wsl-c", "Claude status hooks, statusline, attribution, refs grant (distro ~/.claude/settings.json)", |s| {
            let p = unc(&path::join(&home, ".claude/settings.json"));
            let mut v = load_json(&p)?;
            let template = std::fs::read_to_string(s.layout.template("claude-hooks.json")).context("reading templates\\claude-hooks.json")?;
            apply_settings(&mut v, &render_hooks(&template, &exe_mnt)?, &path::join(&root, "refs"));
            let line = unc(&path::join(&home, ".claude/statusline.sh"));
            let pace = std::fs::read_to_string(s.layout.template("claude-pace.sh")).context("reading the claude-pace statusline")?;
            write_text(&line, &pace.replace("\r\n", "\n"))?;
            wsl_backend::run(&wsl_backend::exec_args(&distro_s, None, &["chmod", "+x", &path::join(&home, ".claude/statusline.sh")]))?;
            println!("    {} {}", if save_json(&p, &v)? { "wrote" } else { "already up to date:" }, p.display());
            Ok(())
        });
        self.step("wsl-mcp", "wtd MCP server for the distro's Claude logins (+ Codex)", |_| {
            let mut files = vec![unc(&path::join(&home, ".claude.json"))];
            files.extend(subdirs(&unc(&path::join(&home, ".claude-accounts"))).into_iter().map(|d| d.join(".claude.json")));
            for f in files {
                let mut v = load_json(&f)?;
                set_mcp(&mut v, &exe_mnt);
                println!("    {} {}", if save_json(&f, &v)? { "registered in" } else { "already in" }, f.display());
            }
            let mut codex = vec![unc(&path::join(&home, ".codex"))];
            codex.extend(subdirs(&unc(&path::join(&home, ".codex-accounts"))));
            for h in codex.into_iter().filter(|h| h.is_dir()) {
                crate::codex::ensure_hooks(&h, Path::new(&exe_mnt))?;
                crate::codex::ensure_mcp(&h, Path::new(&exe_mnt))?;
                println!("    [codex {}] status hooks + MCP server 'wtd'", h.display());
            }
            Ok(())
        });
        self.step("wsl-a", "commands in the distro's ~/.local/bin", |_| {
            let bin = path::join(&home, ".local/bin");
            let mut names = vec![];
            for (name, sub) in shell::SHIMS {
                write_text(&unc(&path::join(&bin, name)), &shell::shim(&exe_mnt, sub))?;
                names.push(path::join(&bin, name));
            }
            write_text(&unc(&path::join(&bin, "wtd")), &shell::wtd_shim(&exe_mnt))?;
            names.push(path::join(&bin, "wtd"));
            let mut argv = vec!["chmod", "+x"];
            argv.extend(names.iter().map(String::as_str));
            wsl_backend::run(&wsl_backend::exec_args(&distro_s, None, &argv))?;
            println!("    shimmed {} commands in {bin} (on PATH via ~/.profile once the folder exists; open a new shell)", names.len());
            Ok(())
        });
        println!("    next: agent <slug> <name> --platform wsl:{distro}");
        Ok(())
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!("../../../../../.wtd/templates/claude-hooks.json");
    const EXE: &str = "/mnt/c/Users/u/AppData/Local/WorkTreeDev/current/bin/wtd.exe";

    fn fixture(name: &str) -> Value {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/setup").join(name);
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    fn install(v: &mut Value) {
        apply_settings(v, &render_hooks(TEMPLATE, EXE).unwrap(), "/home/u/dev/worktree-dev/refs");
    }

    #[test]
    fn hooks_call_wtd_exe_through_mnt() {
        let r = render_hooks(TEMPLATE, EXE).unwrap();
        assert_eq!(r.pointer("/hooks/Stop/0/hooks/0/command").unwrap(), &json!(format!("{EXE} hook stop")));
        assert_eq!(r.pointer("/hooks/PreToolUse/2/hooks/0/command").unwrap(), &json!(format!("{EXE} hook plan-nudge")));
        assert!(r.pointer("/hooks/PostToolUse").is_none(), "plan-hint.sh needs tools the distro may lack");
        assert!(!r.to_string().contains(".sh"), "{r}");
        let spaced = render_hooks(TEMPLATE, "/mnt/c/Program Files/W/wtd.exe").unwrap();
        assert_eq!(spaced.pointer("/hooks/Stop/0/hooks/0/command").unwrap(), "\"/mnt/c/Program Files/W/wtd.exe\" hook stop");
    }

    #[test]
    fn distro_settings_match_fixture() {
        let mut v = fixture("wsl-claude-settings.before.json");
        install(&mut v);
        assert_eq!(v, fixture("wsl-claude-settings.after.json"), "got:\n{}", merge::to_text(&v, 2));
        let once = v.clone();
        install(&mut v);
        assert_eq!(v, once, "re-running setup changes nothing");
    }

    #[test]
    fn mcp_registration() {
        let mut v = json!({ "numStartups": 3, "mcpServers": { "other": { "command": "x" }, "wtd": { "command": "/mnt/d/old/wtd.exe", "args": ["mcp"] } } });
        set_mcp(&mut v, EXE);
        assert_eq!(v["mcpServers"]["wtd"], json!({ "type": "stdio", "command": EXE, "args": ["mcp"], "env": {} }));
        assert_eq!(v["mcpServers"]["other"], json!({ "command": "x" }));
        assert_eq!(v["numStartups"], 3);
        let mut fresh = json!({});
        set_mcp(&mut fresh, EXE);
        assert_eq!(fresh["mcpServers"]["wtd"]["command"], EXE);
    }
}
