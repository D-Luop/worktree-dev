//! The JSON edits `wtd setup` makes to files the user also owns (Claude settings, VS Code settings),
//! and their inverses for `--uninstall`. Pure functions over `serde_json::Value`, so they're tested
//! against fixtures without touching the machine.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

pub const STATUSLINE_CMD: &str = "~/.claude/statusline.sh";

fn obj(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().unwrap()
}

/// `key` inside `v` as an object, created (or replaced, if it isn't one) as needed.
fn child_obj<'a>(v: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    obj(obj(v).entry(key).or_insert_with(|| json!({})))
}

fn child_arr<'a>(v: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    let e = obj(v).entry(key).or_insert_with(|| json!([]));
    if !e.is_array() {
        *e = json!([]);
    }
    e.as_array_mut().unwrap()
}

/// `C:\x y\wtd.exe` → `"C:/x y/wtd.exe"` for a hook command line (Claude Code runs it through bash).
pub fn shell_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    if p.contains(' ') { format!("\"{p}\"") } else { p }
}

/// The status-hook template rendered for this machine: placeholders filled, `wt-status.sh <event>`
/// replaced by the Rust hook (`<exe> hook <event>`), the remaining `.sh` hooks pointed at `hooks_dir`.
pub fn render_hooks(template: &str, dev: &str, hooks_dir: &str, exe: &str) -> Result<Value> {
    let dev = dev.replace('\\', "/");
    let hooks_dir = hooks_dir.replace('\\', "/");
    let text = template.replace("__DEV__/.wtd/hooks/", &format!("{hooks_dir}/")).replace("__DEV__", &dev);
    let mut v: Value = serde_json::from_str(&text).context("parsing claude-hooks.json")?;
    let exe = shell_path(exe);
    for entries in v.get_mut("hooks").and_then(Value::as_object_mut).context("claude-hooks.json has no hooks")?.values_mut() {
        for h in entries.as_array_mut().into_iter().flatten().filter_map(|e| e.get_mut("hooks")).filter_map(Value::as_array_mut).flatten() {
            let Some(cmd) = h.get("command").and_then(Value::as_str) else { continue };
            if let Some(i) = cmd.find("wt-status.sh") {
                let event = cmd[i + "wt-status.sh".len()..].trim();
                h["command"] = json!(format!("{exe} hook {event}"));
            }
        }
    }
    Ok(v)
}

fn is_our_command(cmd: &str) -> bool {
    ["wt-status.sh", "plan-nudge.sh", "plan-hint.sh"].iter().any(|s| cmd.contains(s))
        || (cmd.contains("wtd.exe") && cmd.contains(" hook "))
}

/// A hook entry `{matcher?, hooks: [{command}]}` that runs one of our commands.
fn is_our_entry(entry: &Value) -> bool {
    entry.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
        hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(is_our_command))
    })
}

/// Drop our entries from every event; events left empty go too, and `hooks` itself if it empties.
fn strip_our_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else { return };
    hooks.retain(|_, entries| {
        let Some(list) = entries.as_array_mut() else { return true };
        let before = list.len();
        list.retain(|e| !is_our_entry(e));
        !(list.is_empty() && before > 0)
    });
    if hooks.is_empty() {
        obj(settings).shift_remove("hooks");
    }
}

/// Our status hooks in, replacing any earlier copy of ours; the user's own hooks (on the same events
/// too) are kept ahead of ours.
pub fn merge_hooks(settings: &mut Value, rendered: &Value) {
    strip_our_hooks(settings);
    let Some(tmpl) = rendered.get("hooks").and_then(Value::as_object) else { return };
    let hooks = child_obj(settings, "hooks");
    for (event, entries) in tmpl {
        let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        if !list.is_array() {
            *list = json!([]);
        }
        list.as_array_mut().unwrap().extend(entries.as_array().cloned().unwrap_or_default());
    }
}

pub fn unmerge_hooks(settings: &mut Value) {
    strip_our_hooks(settings);
}

pub fn set_statusline(settings: &mut Value) {
    obj(settings).insert("statusLine".into(), json!({ "type": "command", "command": STATUSLINE_CMD }));
}

/// Only our statusline goes; a different one the user set since is left alone.
pub fn unset_statusline(settings: &mut Value) {
    if settings.pointer("/statusLine/command").and_then(Value::as_str) == Some(STATUSLINE_CMD) {
        obj(settings).shift_remove("statusLine");
    }
}

pub fn set_attribution(settings: &mut Value) {
    let a = child_obj(settings, "attribution");
    a.insert("commit".into(), json!(""));
    a.insert("pr".into(), json!(""));
}

fn push_unique(list: &mut Vec<Value>, s: &str) {
    if !list.iter().any(|v| v.as_str() == Some(s)) {
        list.push(json!(s));
    }
}

/// Read access to the reference checkouts for every session; edits denied so they stay read-only.
/// (`Write(...)` deny rules are ignored by Claude Code, so an old one is replaced by `Edit(...)`.)
pub fn grant_refs(settings: &mut Value, refs_root: &str) {
    let root = refs_root.replace('\\', "/");
    let perms = obj(settings).entry("permissions").or_insert_with(|| json!({}));
    push_unique(child_arr(perms, "additionalDirectories"), &root);
    let deny = child_arr(perms, "deny");
    let write = format!("Write({root}/**)");
    deny.retain(|v| v.as_str() != Some(&write));
    push_unique(deny, &format!("Edit({root}/**)"));
}

pub fn revoke_refs(settings: &mut Value, refs_root: &str) {
    let root = refs_root.replace('\\', "/");
    let Some(perms) = settings.get_mut("permissions").and_then(Value::as_object_mut) else { return };
    let drop = [json!(root), json!(format!("Edit({root}/**)")), json!(format!("Write({root}/**)"))];
    for key in ["additionalDirectories", "deny"] {
        if let Some(list) = perms.get_mut(key).and_then(Value::as_array_mut) {
            list.retain(|v| !drop.contains(v));
            if list.is_empty() {
                perms.shift_remove(key);
            }
        }
    }
    if perms.is_empty() {
        obj(settings).shift_remove("permissions");
    }
}

/// VS Code user settings: terminal tab titles from the session's title sequence, a visual (silent) bell.
pub fn vscode_user(settings: &mut Value) {
    let s = obj(settings);
    s.insert("terminal.integrated.tabs.title".into(), json!("${sequence}"));
    s.insert("terminal.integrated.tabs.description".into(), json!("${cwdFolder}"));
    s.insert("terminal.integrated.enableVisualBell".into(), json!(true));
    s.insert("accessibility.signals.terminalBell".into(), json!({ "sound": "off" }));
    s.insert("terminal.integrated.confirmOnKill".into(), json!("never"));
}

/// The dev-root window holds every worktree: keep VS Code's git discovery, file watcher and search
/// off them.
pub fn dev_root_vscode(settings: &mut Value) {
    let s = obj(settings);
    s.insert("git.autoRepositoryDetection".into(), json!("openEditors"));
    s.insert("git.openRepositoryInParentFolders".into(), json!("never"));
    let w = child_obj(settings, "files.watcherExclude");
    for k in ["**/repos/**", "**/refs/**", "**/worktrees/**"] {
        w.insert(k.into(), json!(true));
    }
    let x = child_obj(settings, "search.exclude");
    for k in ["**/repos/**", "**/.wtd/state/**", "**/worktrees/*/archive/**", "**/node_modules"] {
        x.insert(k.into(), json!(true));
    }
}

/// Parse a settings file that must be a plain JSON object (VS Code's JSONC with comments isn't, and is
/// left for the user to edit rather than lose their comments).
pub fn parse_object(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_str(text)?;
    if !v.is_object() {
        bail!("not a JSON object");
    }
    Ok(v)
}

pub fn to_text(v: &Value, indent: usize) -> String {
    use serde::Serialize;
    let pad = " ".repeat(indent);
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, serde_json::ser::PrettyFormatter::with_indent(pad.as_bytes()));
    v.serialize(&mut ser).expect("serializing a Value can't fail");
    String::from_utf8(buf).unwrap() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = include_str!("../../../../../.wtd/templates/claude-hooks.json");
    const EXE: &str = r"D:\dev\worktree-dev\.wtd\bin\wtd.exe";

    fn fixture(name: &str) -> Value {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/setup").join(name);
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    fn rendered() -> Value {
        render_hooks(TEMPLATE, "D:/dev/worktree-dev", "D:/dev/worktree-dev/.wtd/hooks", EXE).unwrap()
    }

    fn install(v: &mut Value) {
        merge_hooks(v, &rendered());
        set_statusline(v);
        set_attribution(v);
        grant_refs(v, r"D:\dev\worktree-dev\refs");
    }

    fn uninstall(v: &mut Value) {
        unmerge_hooks(v);
        unset_statusline(v);
        revoke_refs(v, "D:/dev/worktree-dev/refs");
    }

    #[test]
    fn renders_template() {
        let r = rendered();
        assert_eq!(r.pointer("/hooks/Stop/0/hooks/0/command").unwrap(), "D:/dev/worktree-dev/.wtd/bin/wtd.exe hook stop");
        assert_eq!(r.pointer("/hooks/StopFailure/0/hooks/0/command").unwrap(), "D:/dev/worktree-dev/.wtd/bin/wtd.exe hook limited");
        assert_eq!(r.pointer("/hooks/PostToolUse/0/hooks/0/command").unwrap(), "D:/dev/worktree-dev/.wtd/hooks/plan-hint.sh");
        assert!(!r.to_string().contains("__"));
        let spaced = render_hooks(TEMPLATE, "C:/a", r"C:\Program Files\W\hooks", r"C:\Program Files\W\bin\wtd.exe").unwrap();
        assert_eq!(spaced.pointer("/hooks/Stop/0/hooks/0/command").unwrap(), "\"C:/Program Files/W/bin/wtd.exe\" hook stop");
        assert_eq!(spaced.pointer("/hooks/PostToolUse/0/hooks/0/command").unwrap(), "C:/Program Files/W/hooks/plan-hint.sh");
    }

    #[test]
    fn claude_settings_match_fixture() {
        let mut v = fixture("claude-settings.before.json");
        install(&mut v);
        assert_eq!(v, fixture("claude-settings.after.json"));
        // key order of the user's file is kept
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys[..3], ["model", "hooks", "permissions"]);
    }

    #[test]
    fn claude_settings_idempotent() {
        let mut once = fixture("claude-settings.before.json");
        install(&mut once);
        let mut twice = once.clone();
        install(&mut twice);
        assert_eq!(to_text(&once, 2), to_text(&twice, 2));
    }

    #[test]
    fn uninstall_leaves_only_user_content() {
        let mut v = fixture("claude-settings.before.json");
        install(&mut v);
        uninstall(&mut v);
        assert_eq!(v, fixture("claude-settings.uninstalled.json"));
    }

    #[test]
    fn fresh_file() {
        let mut v = parse_object("").unwrap();
        install(&mut v);
        assert_eq!(v["hooks"], rendered()["hooks"]);
        uninstall(&mut v);
        assert_eq!(v, json!({ "attribution": { "commit": "", "pr": "" } }));
    }

    #[test]
    fn vscode_files_match_fixtures() {
        let mut u = fixture("vscode-user.before.json");
        vscode_user(&mut u);
        assert_eq!(u, fixture("vscode-user.after.json"));
        let mut d = fixture("dev-root-vscode.before.json");
        dev_root_vscode(&mut d);
        assert_eq!(d, fixture("dev-root-vscode.after.json"));
    }

    #[test]
    fn jsonc_is_refused() {
        assert!(parse_object("{ // comment\n \"a\": 1 }").is_err());
        assert!(parse_object("[1]").is_err());
        assert_eq!(to_text(&json!({"a": [1]}), 4), "{\n    \"a\": [\n        1\n    ]\n}\n");
    }
}
