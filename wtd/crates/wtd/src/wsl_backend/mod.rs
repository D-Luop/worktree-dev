//! WSL worktrees: a separate clone per repo inside the distro, with everything (git, the agent, its
//! shells) running there through `wsl.exe`. Windows keeps only a registry of them
//! (`.wtd/state/wsl-worktrees.json`) and reads their files through `\\wsl.localhost\…`.
//! Contract: docs/wsl-platform-contract.md.

pub mod path;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Vars a WSL session gets from Windows (Linux values, no path translation). `WSL_DISTRO_NAME` is
/// never set on the Windows side; listing it carries it back out to `wtd.exe hook` through interop.
pub const SESSION_VARS: [&str; 5] = ["WTD_SESSION", "WTD_WORKTREE", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "WSL_DISTRO_NAME"];

pub fn wsl_exe() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("wsl.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("wsl.exe"))
}

// --- command lines ---------------------------------------------------------------------------------

fn base_args(distro: &str, cd: Option<&str>) -> Vec<String> {
    let mut v = vec!["-d".to_string(), distro.to_string()];
    if let Some(d) = cd {
        v.extend(["--cd".into(), d.to_string()]);
    }
    v
}

/// `wsl.exe` args running `argv` directly (`--exec`): no Linux shell re-parses the arguments.
pub fn exec_args(distro: &str, cd: Option<&str>, argv: &[&str]) -> Vec<String> {
    let mut v = base_args(distro, cd);
    v.push("--exec".into());
    v.extend(argv.iter().map(|a| a.to_string()));
    v
}

/// `argv` under a login shell, so `~/.profile` puts `~/.local/bin`, nvm etc. on PATH (agents live there).
pub fn login_args(distro: &str, cd: Option<&str>, argv: &[String]) -> Vec<String> {
    let mut v = exec_args(distro, cd, &["bash", "-lc", r#"exec "$0" "$@""#]);
    v.extend(argv.iter().cloned());
    v
}

/// An interactive login shell (the user's default) in `cd`.
pub fn shell_args(distro: &str, cd: &str) -> Vec<String> {
    base_args(distro, Some(cd))
}

/// `sh -c <script> sh <args…>`: one `wsl.exe` round trip for a batch of commands.
pub fn sh_args(distro: &str, cd: Option<&str>, script: &str, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["sh", "-c", script, "sh"];
    argv.extend_from_slice(args);
    exec_args(distro, cd, &argv)
}

/// The environment for a `wsl.exe` session: this process's (minus a parent Claude session's
/// identity, #52) with `WSLENV` replaced so only [`SESSION_VARS`] cross into the distro.
pub fn session_env(base: Vec<(String, String)>, vars: &[(&str, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = crate::session_env::scrub(base)
        .into_iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("WSLENV") && !SESSION_VARS.iter().any(|v| v.eq_ignore_ascii_case(k)))
        .collect();
    env.extend(vars.iter().filter(|(k, _)| SESSION_VARS.contains(k)).map(|(k, v)| (k.to_string(), v.clone())));
    env.push(("WSLENV".into(), SESSION_VARS.join(":")));
    env
}

// --- running things in a distro ------------------------------------------------------------------

/// wsl.exe's own messages are UTF-16; output of the Linux command is UTF-8.
pub fn decode(b: &[u8]) -> String {
    if b.len() >= 2 && b.iter().skip(1).step_by(2).take(8).all(|c| *c == 0) {
        let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u).replace('\0', "");
    }
    String::from_utf8_lossy(b).to_string()
}

fn wsl(args: &[String]) -> Command {
    let mut c = Command::new(wsl_exe());
    c.args(args).env_remove("WSLENV");
    crate::win::no_window(&mut c);
    c
}

/// Run `wsl.exe <args>`: trimmed stdout, or an error with stderr.
pub fn run(args: &[String]) -> Result<String> {
    let o = wsl(args).stdin(Stdio::null()).output().context("running wsl.exe")?;
    if !o.status.success() {
        let err = decode(&o.stderr);
        let out = decode(&o.stdout);
        bail!("{}", if err.trim().is_empty() { out.trim().to_string() } else { err.trim().to_string() });
    }
    Ok(decode(&o.stdout).trim_end().to_string())
}

/// [`run`] with progress on this terminal (clone / fetch / worktree add).
pub fn run_visible(args: &[String]) -> Result<()> {
    let st = wsl(args).stdin(Stdio::null()).status().context("running wsl.exe")?;
    if !st.success() {
        bail!("wsl.exe {} failed", args.iter().skip(2).cloned().collect::<Vec<_>>().join(" "));
    }
    Ok(())
}

fn list(extra: &[&str]) -> Vec<String> {
    let mut args = vec!["-l", "-q"];
    args.extend_from_slice(extra);
    let mut c = Command::new(wsl_exe());
    c.args(&args).stdin(Stdio::null()).stderr(Stdio::null());
    crate::win::no_window(&mut c);
    match c.output() {
        Ok(o) if o.status.success() => parse_list(&decode(&o.stdout)),
        _ => vec![],
    }
}

pub fn parse_list(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim().trim_start_matches('\u{feff}')).filter(|l| !l.is_empty()).map(String::from).collect()
}

pub fn distros() -> Vec<String> {
    list(&[])
}

pub fn running_distros() -> Vec<String> {
    list(&["--running"])
}

// --- per-distro dev root -----------------------------------------------------------------------------

/// The distro's dev root: `config.json → wsl.<distro>.root`, else `$HOME/dev/worktree-dev` (resolved
/// once and recorded, so it stays put).
pub fn dev_root(dev: &Path, distro: &str) -> Result<String> {
    let cfg = crate::settings::load_config(dev);
    if let Some(r) = cfg.pointer(&format!("/wsl/{distro}/root")).and_then(|v| v.as_str()).filter(|s| path::is_linux_abs(s)) {
        return Ok(r.trim_end_matches('/').to_string());
    }
    let home = run(&exec_args(distro, None, &["sh", "-c", r#"printf %s "$HOME""#])).with_context(|| format!("reaching WSL distro '{distro}'"))?;
    if !path::is_linux_abs(&home) {
        bail!("WSL distro '{distro}' reported no usable $HOME ({home:?})");
    }
    let root = path::join(&home, "dev/worktree-dev");
    let d = distro.to_string();
    let r = root.clone();
    crate::settings::update_config(dev, move |c| {
        if !c.get("wsl").is_some_and(|v| v.is_object()) {
            c["wsl"] = serde_json::json!({});
        }
        if !c["wsl"].get(&d).is_some_and(|v| v.is_object()) {
            c["wsl"][&d] = serde_json::json!({});
        }
        c["wsl"][&d]["root"] = serde_json::json!(r);
    })?;
    Ok(root)
}

pub struct Layout {
    pub root: String,
}

impl Layout {
    pub fn bare(&self, slug: &str) -> String {
        path::join(&self.root, &format!("repos/{slug}/.bare"))
    }
    pub fn worktree(&self, slug: &str, name: &str) -> String {
        path::join(&self.root, &format!("worktrees/{slug}/{name}"))
    }
    pub fn archived(&self, slug: &str, name: &str) -> String {
        path::join(&self.root, &format!("worktrees/{slug}/archive/{name}"))
    }
    pub fn reference(&self, slug: &str, branch: &str) -> String {
        path::join(&self.root, &format!("refs/{slug}/{branch}"))
    }
}

// --- registry of WSL worktrees (Windows side) --------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub distro: String,
    /// Path inside the distro (the archive location while `archived`).
    pub wsl_path: String,
    #[serde(default)]
    pub archived: bool,
}

impl Entry {
    /// The Windows view of the worktree folder.
    pub fn unc(&self) -> PathBuf {
        path::unc(&self.distro, &self.wsl_path)
    }
    pub fn platform(&self) -> wtd_core::platform::Platform {
        wtd_core::platform::Platform::Wsl(self.distro.clone())
    }
}

fn registry_file(dev: &Path) -> PathBuf {
    crate::paths::state_dir(dev).join("wsl-worktrees.json")
}

pub fn load(dev: &Path) -> Vec<Entry> {
    std::fs::read_to_string(registry_file(dev)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save(dev: &Path, entries: &[Entry]) -> Result<()> {
    let f = registry_file(dev);
    std::fs::create_dir_all(f.parent().unwrap())?;
    let tmp = f.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(entries)? + "\n")?;
    std::fs::rename(&tmp, &f)?;
    Ok(())
}

/// The active (not archived) WSL worktree with this id.
pub fn find(dev: &Path, id: &str) -> Option<Entry> {
    load(dev).into_iter().find(|e| e.id == id && !e.archived)
}

pub fn find_any(dev: &Path, id: &str) -> Option<Entry> {
    load(dev).into_iter().find(|e| e.id == id)
}

pub fn upsert(dev: &Path, e: Entry) -> Result<()> {
    let mut all = load(dev);
    all.retain(|x| x.id != e.id);
    all.push(e);
    all.sort_by(|a, b| a.id.cmp(&b.id));
    save(dev, &all)
}

pub fn unregister(dev: &Path, id: &str) -> Result<()> {
    let mut all = load(dev);
    let n = all.len();
    all.retain(|x| x.id != id);
    if all.len() != n {
        save(dev, &all)?;
    }
    Ok(())
}

/// The registered worktree a path belongs to: a `\\wsl.localhost\…` view, or a Linux path (with the
/// distro it came from) — the deepest registered root containing it.
pub fn resolve(dev: &Path, distro: &str, linux: &str) -> Option<Entry> {
    load(dev)
        .into_iter()
        .filter(|e| !e.archived && e.distro.eq_ignore_ascii_case(distro))
        .filter(|e| linux == e.wsl_path || linux.starts_with(&format!("{}/", e.wsl_path.trim_end_matches('/'))))
        .max_by_key(|e| e.wsl_path.len())
}

pub fn resolve_host_path(dev: &Path, p: &Path) -> Option<Entry> {
    let (distro, linux) = path::from_unc(&p.to_string_lossy())?;
    resolve(dev, &distro, &linux)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_lines() {
        assert_eq!(exec_args("Ubuntu", Some("/home/u/w"), &["git", "status", "--porcelain=v2"]), sv(&["-d", "Ubuntu", "--cd", "/home/u/w", "--exec", "git", "status", "--porcelain=v2"]));
        assert_eq!(
            login_args("Ubuntu", Some("/w"), &sv(&["claude", "--permission-mode", "auto", "fix the 'bug'"])),
            sv(&["-d", "Ubuntu", "--cd", "/w", "--exec", "bash", "-lc", r#"exec "$0" "$@""#, "claude", "--permission-mode", "auto", "fix the 'bug'"])
        );
        assert_eq!(shell_args("Debian", "/w"), sv(&["-d", "Debian", "--cd", "/w"]));
        assert_eq!(sh_args("U", None, "echo \"$1\"", &["a b"]), sv(&["-d", "U", "--exec", "sh", "-c", "echo \"$1\"", "sh", "a b"]));
    }

    #[test]
    fn session_env_passes_only_whats_needed() {
        let base = sv(&["PATH", "SystemRoot", "WSLENV", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CONFIG_DIR", "CLAUDECODE"]).into_iter().map(|k| (k, "win".to_string())).collect();
        let env = session_env(base, &[("WTD_WORKTREE", "r/feat/x".into()), ("CLAUDE_CONFIG_DIR", "/home/u/.claude-accounts/a".into()), ("NOT_ALLOWED", "x".into())]);
        let get = |k: &str| env.iter().filter(|(n, _)| n == k).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
        assert_eq!(get("WSLENV"), ["WTD_SESSION:WTD_WORKTREE:CLAUDE_CONFIG_DIR:CODEX_HOME:WSL_DISTRO_NAME"]);
        assert_eq!(get("CLAUDE_CONFIG_DIR"), ["/home/u/.claude-accounts/a"], "the Windows value never reaches the distro");
        assert_eq!(get("WTD_WORKTREE"), ["r/feat/x"]);
        assert!(get("CLAUDE_CODE_CHILD_SESSION").is_empty() && get("CLAUDECODE").is_empty() && get("NOT_ALLOWED").is_empty());
        assert_eq!(get("SystemRoot"), ["win"]);
    }

    #[test]
    fn decodes_both_encodings() {
        let utf16: Vec<u8> = "Ubuntu\r\nDebian\r\n".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        assert_eq!(parse_list(&decode(&utf16)), ["Ubuntu", "Debian"]);
        assert_eq!(decode("héllo".as_bytes()), "héllo");
        assert_eq!(parse_list("\u{feff}Ubuntu\n\n"), ["Ubuntu"]);
    }

    #[test]
    fn layout_paths() {
        let l = Layout { root: "/home/u/dev/worktree-dev".into() };
        assert_eq!(l.bare("r"), "/home/u/dev/worktree-dev/repos/r/.bare");
        assert_eq!(l.worktree("r", "feat/x"), "/home/u/dev/worktree-dev/worktrees/r/feat/x");
        assert_eq!(l.archived("r", "feat/x"), "/home/u/dev/worktree-dev/worktrees/r/archive/feat/x");
        assert_eq!(l.reference("r", "main"), "/home/u/dev/worktree-dev/refs/r/main");
    }

    #[test]
    fn registry_resolves_paths() {
        let t = std::env::temp_dir().join(format!("wtd-wslreg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let e = |id: &str, p: &str| Entry { id: id.into(), slug: "r".into(), name: id[2..].into(), distro: "Ubuntu".into(), wsl_path: p.into(), archived: false };
        upsert(&t, e("r/feat/x", "/home/u/dev/worktree-dev/worktrees/r/feat/x")).unwrap();
        upsert(&t, e("r/feat/xy", "/home/u/dev/worktree-dev/worktrees/r/feat/xy")).unwrap();
        assert_eq!(resolve(&t, "ubuntu", "/home/u/dev/worktree-dev/worktrees/r/feat/x/src").unwrap().id, "r/feat/x");
        assert_eq!(resolve(&t, "Ubuntu", "/home/u/dev/worktree-dev/worktrees/r/feat/xy").unwrap().id, "r/feat/xy");
        assert!(resolve(&t, "Debian", "/home/u/dev/worktree-dev/worktrees/r/feat/x").is_none());
        let unc = path::unc("Ubuntu", "/home/u/dev/worktree-dev/worktrees/r/feat/x/a");
        assert_eq!(resolve_host_path(&t, &unc).unwrap().id, "r/feat/x");
        let mut arc = e("r/feat/x", "/home/u/dev/worktree-dev/worktrees/r/archive/feat/x");
        arc.archived = true;
        upsert(&t, arc).unwrap();
        assert!(find(&t, "r/feat/x").is_none() && find_any(&t, "r/feat/x").is_some());
        unregister(&t, "r/feat/xy").unwrap();
        assert_eq!(load(&t).len(), 1);
        std::fs::remove_dir_all(&t).ok();
    }
}
