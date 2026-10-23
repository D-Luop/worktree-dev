//! Where a worktree can run: `windows`, plus each WSL distro with the tools a session needs.
//!
//!   wtd platforms [--refresh]   JSON list (from the daemon's 60s cache when it's up)

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use wtd_core::model::Platform;
use wtd_core::protocol::method;

use crate::win;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Tools {
    pub git: bool,
    pub claude: bool,
    pub codex: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlatformInfo {
    pub id: String,
    /// `windows` | `wsl`
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_user: Option<String>,
    pub tools: Tools,
    /// git and claude are both there: a session can start.
    pub ok: bool,
    pub hints: Vec<String>,
}

/// Text from wsl.exe: its own messages (`-l -q`) are UTF-16LE, a Linux command's output is UTF-8.
pub fn decode_wsl_output(b: &[u8]) -> String {
    let b = b.strip_prefix(&[0xFF, 0xFE]).unwrap_or(b);
    let looks_utf16 = b.len() >= 2 && b.iter().skip(1).step_by(2).filter(|&&c| c == 0).count() * 2 >= b.len() / 2;
    if looks_utf16 {
        let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
    .replace('\0', "")
}

/// Distro names from `wsl.exe -l -q`. Docker Desktop's internal distros can't host a session.
pub fn parse_distros(raw: &[u8]) -> Vec<String> {
    decode_wsl_output(raw)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}').trim())
        .filter(|l| !l.is_empty() && !l.starts_with("docker-desktop"))
        .map(String::from)
        .collect()
}

/// `user=<name>` then one `command -v` line per tool found.
fn parse_probe(out: &str) -> (Option<String>, Tools) {
    let mut user = None;
    let mut t = Tools::default();
    for l in out.lines().map(str::trim) {
        if let Some(u) = l.strip_prefix("user=") {
            user = Some(u.to_string()).filter(|u| !u.is_empty());
            continue;
        }
        match l.rsplit('/').next().unwrap_or(l) {
            "git" => t.git = true,
            "claude" => t.claude = true,
            "codex" => t.codex = true,
            _ => {}
        }
    }
    (user, t)
}

fn hints(t: &Tools) -> Vec<String> {
    let mut h = vec![];
    if !t.git {
        h.push("git is missing — install it in the distro: sudo apt install -y git".into());
    }
    if !t.claude {
        h.push("claude is missing — install it in the distro: curl -fsSL https://claude.ai/install.sh | bash".into());
    }
    if !t.codex {
        h.push("codex (optional) is missing — npm install -g @openai/codex".into());
    }
    h
}

/// Run to completion, killing it after `timeout` (a distro that hangs on boot mustn't hang us).
fn output_within(mut cmd: Command, timeout: Duration) -> Result<(bool, Vec<u8>)> {
    win::no_window(&mut cmd);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = stdout.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("no answer within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok((status.success(), reader.join().unwrap_or_default()))
}

fn wsl_exe() -> Command {
    Command::new("wsl.exe")
}

pub fn list_distros() -> Vec<String> {
    let mut c = wsl_exe();
    c.args(["-l", "-q"]);
    match output_within(c, PROBE_TIMEOUT) {
        Ok((true, out)) => parse_distros(&out),
        _ => vec![], // no WSL, or no distros installed (wsl.exe exits non-zero)
    }
}

fn probe_distro(distro: &str) -> PlatformInfo {
    let mut c = wsl_exe();
    c.args(["-d", distro, "--", "sh", "-lc", "echo \"user=$(id -un)\"; command -v git claude codex"]);
    let mut info = PlatformInfo {
        id: Platform::Wsl(distro.to_string()).to_string(),
        kind: "wsl",
        distro: Some(distro.to_string()),
        default_user: None,
        tools: Tools::default(),
        ok: false,
        hints: vec![],
    };
    // `command -v` exits non-zero when any tool is missing, so the exit code says nothing here
    match output_within(c, PROBE_TIMEOUT) {
        Ok((_, out)) => {
            let (user, tools) = parse_probe(&decode_wsl_output(&out));
            if user.is_none() {
                info.hints.push(format!("couldn't run a shell in {distro} — open it once with: wsl -d {distro}"));
            }
            info.hints.extend(hints(&tools));
            info.ok = user.is_some() && tools.git && tools.claude;
            info.default_user = user;
            info.tools = tools;
        }
        Err(e) => info.hints.push(format!("{distro} didn't respond ({e:#}) — check it with: wsl -d {distro}")),
    }
    info
}

fn windows() -> PlatformInfo {
    let tools = Tools {
        git: crate::settings::which("git").is_some(),
        claude: crate::settings::which("claude").is_some(),
        codex: crate::settings::which("codex").is_some(),
    };
    let mut h = vec![];
    if !tools.git {
        h.push("git is missing — install Git for Windows: winget install Git.Git".into());
    }
    if !tools.claude {
        h.push("claude is missing — install it: irm https://claude.ai/install.ps1 | iex".into());
    }
    PlatformInfo { id: "windows".into(), kind: "windows", distro: None, default_user: None, ok: tools.git && tools.claude, tools, hints: h }
}

/// Every platform, probing distros in parallel (each probe may boot its distro).
pub fn detect() -> Vec<PlatformInfo> {
    let probes: Vec<_> = list_distros().into_iter().map(|d| std::thread::spawn(move || probe_distro(&d))).collect();
    let mut out = vec![windows()];
    out.extend(probes.into_iter().filter_map(|h| h.join().ok()));
    out
}

/// The daemon's cache of [`detect`].
#[derive(Default)]
pub struct Cache {
    at: Option<Instant>,
    value: Value,
}

pub const CACHE_TTL: Duration = Duration::from_secs(60);

impl Cache {
    pub fn fresh(&self) -> Option<Value> {
        self.at.filter(|t| t.elapsed() < CACHE_TTL).map(|_| self.value.clone())
    }

    pub fn store(&mut self, v: Value) {
        self.at = Some(Instant::now());
        self.value = v;
    }
}

// --- defaults ---------------------------------------------------------------------------------------

/// A platform setting from config; an unparseable one is ignored so resolution falls through.
fn setting(v: Option<&Value>) -> Option<Platform> {
    v?.as_str()?.parse().ok()
}

pub fn global_default(cfg: &Value) -> Option<Platform> {
    setting(cfg.get("default_platform"))
}

/// The 3rd column of a `.wtd/repos.tsv` line (`<slug>\t<url>\t<platform>`), from before the
/// setting moved to config.json. Bash tooling reads only two columns, so we never write it.
fn tsv_platforms(dev: &Path) -> Vec<(String, Platform)> {
    std::fs::read_to_string(crate::settings::registry(dev))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split('\t');
            let slug = it.next()?.trim();
            Some((slug.to_string(), it.nth(1)?.trim().parse().ok()?))
        })
        .collect()
}

/// This repo's own default: `repos.<slug>.platform` in config.json, else a legacy tsv column.
pub fn repo_default(dev: &Path, cfg: &Value, slug: &str) -> Option<Platform> {
    setting(cfg.pointer(&format!("/repos/{slug}/platform")))
        .or_else(|| tsv_platforms(dev).into_iter().find(|(s, _)| s == slug).map(|(_, p)| p))
}

/// Explicit choice, then the repo default, then the global default, then windows.
pub fn resolve(explicit: Option<Platform>, repo: Option<Platform>, global: Option<Platform>) -> Platform {
    explicit.or(repo).or(global).unwrap_or_default()
}

/// Planning agents and delegators always run on Windows: the defaults are for repo worktrees.
pub fn resolve_for(dev: &Path, slug: &str, explicit: Option<Platform>) -> Platform {
    if crate::paths::repo_less(slug) {
        return explicit.unwrap_or_default();
    }
    let cfg = crate::settings::load_config(dev);
    resolve(explicit, repo_default(dev, &cfg, slug), global_default(&cfg))
}

/// Copy legacy tsv platforms into config.json (an existing config value wins). True if any existed.
fn migrate_into(dev: &Path, cfg: &mut Value) -> Result<bool> {
    let legacy = tsv_platforms(dev);
    for (slug, p) in &legacy {
        let entry = repo_entry(cfg, slug)?;
        if setting(entry.get("platform")).is_none() {
            entry["platform"] = json!(p.to_string());
        }
    }
    Ok(!legacy.is_empty())
}

/// Back to two columns, once config.json holds the platforms.
fn strip_tsv_column(dev: &Path) -> Result<()> {
    let reg = crate::settings::registry(dev);
    let text = std::fs::read_to_string(&reg)?;
    let lines: Vec<String> =
        text.lines().map(|l| if l.starts_with('#') { l.to_string() } else { l.split('\t').take(2).collect::<Vec<_>>().join("\t") }).collect();
    std::fs::write(&reg, lines.join("\n") + "\n")?;
    Ok(())
}

fn repo_entry<'a>(cfg: &'a mut Value, slug: &str) -> Result<&'a mut Value> {
    let repos = cfg.as_object_mut().context("config.json is not an object")?.entry("repos").or_insert_with(|| json!({}));
    Ok(repos.as_object_mut().context("config.repos is not an object")?.entry(slug.to_string()).or_insert_with(|| json!({})))
}

fn show(dev: &Path) -> Value {
    let cfg = crate::settings::load_config(dev);
    let repos: serde_json::Map<String, Value> = crate::repos::all(dev)
        .into_iter()
        .filter_map(|r| Some((r.slug.clone(), json!(repo_default(dev, &cfg, &r.slug)?.to_string()))))
        .collect();
    json!({ "default_platform": global_default(&cfg).unwrap_or_default().to_string(), "repos": repos })
}

/// `wtd config platform [--repo <slug>] [<id>|--clear]`
pub fn config_main(args: &[String]) -> Result<i32> {
    let dev = crate::paths::dev_root()?;
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let usage = "usage: wtd config platform [--repo <slug>] [windows|wsl:<distro>|--clear]";
    let ["platform", rest @ ..] = a.as_slice() else { bail!("{usage}") };
    let (repo, rest) = match rest {
        ["--repo", slug, rest @ ..] => (Some(*slug), rest),
        _ => (None, rest),
    };
    let value = match rest {
        [] => return crate::settings::out(show(&dev)),
        ["--clear"] => None,
        [id] => Some(id.parse::<Platform>().map_err(|e| anyhow::anyhow!(e))?),
        _ => bail!("{usage}"),
    };
    if let Some(slug) = repo {
        if crate::repos::find(&dev, slug).is_none() {
            bail!("no repo '{slug}'");
        }
    }
    let mut cfg = crate::settings::load_config(&dev);
    let migrated = migrate_into(&dev, &mut cfg)?;
    let (obj, key) = match repo {
        Some(slug) => (repo_entry(&mut cfg, slug)?, "platform"),
        None => (&mut cfg, "default_platform"),
    };
    let obj = obj.as_object_mut().context("config entry is not an object")?;
    match &value {
        Some(p) => obj.insert(key.into(), json!(p.to_string())),
        None => obj.remove(key),
    };
    if let (Some(slug), true) = (repo, obj.is_empty()) {
        cfg["repos"].as_object_mut().map(|r| r.remove(slug));
    }
    crate::settings::update_config(&dev, |c| *c = cfg)?;
    if migrated {
        strip_tsv_column(&dev)?;
    }
    crate::settings::out(show(&dev))
}

// --- WSL worktrees ----------------------------------------------------------------------------------

/// A worktree living inside a distro. Windows worktrees are found by scanning `worktrees/`; these
/// can't be (they're not on the Windows disk), so they're recorded when created.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct WslWorktree {
    pub platform: Platform,
    pub wsl_path: String,
}

fn wsl_registry_path(dev: &Path) -> std::path::PathBuf {
    dev.join(".wtd").join("state").join("wsl-worktrees.json")
}

pub fn wsl_worktrees(dev: &Path) -> std::collections::BTreeMap<String, WslWorktree> {
    std::fs::read_to_string(wsl_registry_path(dev)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn register_wsl(dev: &Path, id: &str, wt: Option<WslWorktree>) -> Result<()> {
    let mut all = wsl_worktrees(dev);
    match wt {
        Some(w) => all.insert(id.to_string(), w),
        None => all.remove(id),
    };
    let p = wsl_registry_path(dev);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&all)? + "\n")?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// The Windows view of a path inside a distro (read-only uses: status file, plan title).
pub fn unc_path(distro: &str, wsl_path: &str) -> String {
    format!(r"\\wsl.localhost\{distro}{}", wsl_path.replace('/', "\\"))
}

/// Which platform `id` already lives on, if it exists. One branch gets one worktree.
pub fn existing_platform(dev: &Path, id: &str) -> Option<Platform> {
    if crate::paths::worktree_path(dev, id).exists() {
        return Some(Platform::Windows);
    }
    wsl_worktrees(dev).remove(id).map(|w| w.platform)
}

/// The platform `agent` opens `id` on: where it already is, else the resolved default.
pub fn choose(existing: Option<Platform>, explicit: Option<Platform>, id: &str, resolved: impl FnOnce() -> Platform) -> Result<Platform> {
    match (existing, explicit) {
        (Some(e), Some(x)) if e != x => {
            bail!("{id} already exists as a {e} worktree; a branch can have only one worktree (asked for {x}). Open it without --platform, or remove it first.")
        }
        (Some(e), _) => Ok(e),
        (None, x) => Ok(x.unwrap_or_else(resolved)),
    }
}

pub fn main(args: &[String]) -> Result<i32> {
    let refresh = args.iter().any(|a| a == "--refresh");
    if let Some(a) = args.iter().find(|a| *a != "--refresh" && *a != "--json") {
        anyhow::bail!("unknown argument '{a}' (usage: wtd platforms [--refresh])");
    }
    let from_daemon = crate::client::Client::connect().ok().flatten().and_then(|mut c| c.request(method::PLATFORMS_LIST, json!({ "refresh": refresh })).ok());
    let v = match from_daemon {
        Some(v) => v,
        None => serde_json::to_value(detect())?,
    };
    crate::settings::out(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut b = if bom { vec![0xFF, 0xFE] } else { vec![] };
        b.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        b
    }

    #[test]
    fn distros_from_utf16_output() {
        // what `wsl.exe -l -q` really prints: UTF-16LE, CRLF, sometimes a BOM, a trailing NUL
        let fixtures = [
            utf16le("Ubuntu\r\nDebian\r\n", false),
            utf16le("Ubuntu\r\nDebian\r\n", true),
            utf16le("Ubuntu\r\ndocker-desktop\r\ndocker-desktop-data\r\nDebian\r\n\0", false),
            utf16le("\u{feff}Ubuntu\r\n\r\n  Debian  \r\n", false),
        ];
        for f in fixtures {
            assert_eq!(parse_distros(&f), ["Ubuntu", "Debian"]);
        }
        assert_eq!(parse_distros(&utf16le("Ubuntu-24.04\r\n", false)), ["Ubuntu-24.04"]);
        assert!(parse_distros(&[]).is_empty());
    }

    #[test]
    fn utf8_passthrough() {
        assert_eq!(decode_wsl_output(b"user=dev\n/usr/bin/git\n"), "user=dev\n/usr/bin/git\n");
        assert_eq!(decode_wsl_output(&utf16le("hi", false)), "hi");
    }

    #[test]
    fn probe_output() {
        let (u, t) = parse_probe("user=dev\n/usr/bin/git\n/home/dev/.local/bin/claude\n");
        assert_eq!(u.as_deref(), Some("dev"));
        assert_eq!(t, Tools { git: true, claude: true, codex: false });
        assert_eq!(hints(&t).len(), 1);
        let (u, t) = parse_probe("user=\n");
        assert_eq!(u, None);
        assert_eq!(t, Tools::default());
        assert_eq!(hints(&t).len(), 3);
    }

    #[test]
    fn resolution_order() {
        let (w, u, d) = (Some(Platform::Windows), Some(Platform::Wsl("Ubuntu".into())), Some(Platform::Wsl("Debian".into())));
        assert_eq!(resolve(w.clone(), u.clone(), d.clone()), Platform::Windows);
        assert_eq!(resolve(None, u.clone(), d.clone()), u.clone().unwrap());
        assert_eq!(resolve(None, None, d.clone()), d.clone().unwrap());
        assert_eq!(resolve(None, None, None), Platform::Windows);
        assert_eq!(resolve(d.clone(), None, u.clone()), d.unwrap());
    }

    fn dev_root(tag: &str, tsv: &str, cfg: Value) -> std::path::PathBuf {
        let t = std::env::temp_dir().join(format!("wtd-platforms-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(t.join(".wtd")).unwrap();
        std::fs::write(t.join(".wtd").join("repos.tsv"), tsv).unwrap();
        std::fs::write(t.join(".wtd").join("config.json"), cfg.to_string()).unwrap();
        t
    }

    #[test]
    fn defaults_from_config_and_legacy_tsv() {
        let t = dev_root(
            "defaults",
            "# comment\twith\ttabs\napp\tgit@x:a.git\twsl:Ubuntu\napi\tgit@x:b.git\nweb\tgit@x:c.git\tbogus\n",
            json!({ "default_platform": "wsl:Debian", "repos": { "api": { "platform": "windows" }, "web": { "platform": "nope" } } }),
        );
        let cfg = crate::settings::load_config(&t);
        assert_eq!(global_default(&cfg), Some(Platform::Wsl("Debian".into())));
        assert_eq!(repo_default(&t, &cfg, "app"), Some(Platform::Wsl("Ubuntu".into())));
        assert_eq!(repo_default(&t, &cfg, "api"), Some(Platform::Windows));
        assert_eq!(repo_default(&t, &cfg, "web"), None, "bad values in both places fall through");
        assert_eq!(resolve_for(&t, "web", None), Platform::Wsl("Debian".into()));
        assert_eq!(resolve_for(&t, "plan", None), Platform::Windows);
        assert_eq!(resolve_for(&t, "delegate", None), Platform::Windows);
        assert_eq!(resolve_for(&t, "app", Some(Platform::Windows)), Platform::Windows);
        // the third column doesn't leak into the URL
        assert_eq!(crate::settings::registered(&t)[0], ("app".to_string(), "git@x:a.git".to_string()));
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn migrates_tsv_column_into_config() {
        let t = dev_root("migrate", "# head\napp\tgit@x:a.git\twsl:Ubuntu\napi\tgit@x:b.git\twsl:Debian\n", json!({ "version": 1, "repos": { "api": { "platform": "windows" } } }));
        let mut cfg = crate::settings::load_config(&t);
        assert!(migrate_into(&t, &mut cfg).unwrap());
        assert_eq!(cfg["repos"]["app"]["platform"], "wsl:Ubuntu");
        assert_eq!(cfg["repos"]["api"]["platform"], "windows", "config wins over the tsv");
        strip_tsv_column(&t).unwrap();
        assert_eq!(std::fs::read_to_string(crate::settings::registry(&t)).unwrap(), "# head\napp\tgit@x:a.git\napi\tgit@x:b.git\n");
        let mut again = cfg.clone();
        assert!(!migrate_into(&t, &mut again).unwrap());
        assert_eq!(again, cfg);
        // an old config with no platform keys at all
        let old = dev_root("old", "app\tgit@x:a.git\n", json!({ "version": 1, "repos": {} }));
        assert_eq!(resolve_for(&old, "app", None), Platform::Windows);
        std::fs::remove_dir_all(&t).ok();
        std::fs::remove_dir_all(&old).ok();
    }

    #[test]
    fn one_worktree_per_branch() {
        let (w, u) = (Platform::Windows, Platform::Wsl("Ubuntu".into()));
        let never = || -> Platform { panic!("existing worktrees don't consult the defaults") };
        assert_eq!(choose(Some(u.clone()), None, "a/b", never).unwrap(), u);
        assert_eq!(choose(Some(w.clone()), Some(w.clone()), "a/b", never).unwrap(), w);
        let e = choose(Some(w.clone()), Some(u.clone()), "a/b", never).unwrap_err().to_string();
        assert!(e.contains("already exists as a windows worktree") && e.contains("wsl:Ubuntu"), "{e}");
        assert_eq!(choose(None, None, "a/b", || u.clone()).unwrap(), u);
        assert_eq!(choose(None, Some(w.clone()), "a/b", || u.clone()).unwrap(), w);
    }

    #[test]
    fn wsl_registry_roundtrip() {
        let t = dev_root("registry", "", json!({}));
        let u = WslWorktree { platform: Platform::Wsl("Ubuntu".into()), wsl_path: "/home/dev/dev/worktree-dev/worktrees/app/feat/x".into() };
        register_wsl(&t, "app/feat/x", Some(u.clone())).unwrap();
        assert_eq!(existing_platform(&t, "app/feat/x"), Some(u.platform.clone()));
        assert_eq!(existing_platform(&t, "app/feat/y"), None);
        std::fs::create_dir_all(crate::paths::worktree_path(&t, "app/feat/y")).unwrap();
        assert_eq!(existing_platform(&t, "app/feat/y"), Some(Platform::Windows));
        register_wsl(&t, "app/feat/x", None).unwrap();
        assert!(wsl_worktrees(&t).is_empty());
        assert_eq!(unc_path("Ubuntu", "/home/dev/x"), r"\\wsl.localhost\Ubuntu\home\dev\x");
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn cache_expires() {
        let mut c = Cache::default();
        assert!(c.fresh().is_none());
        c.store(json!([1]));
        assert_eq!(c.fresh(), Some(json!([1])));
        c.at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        assert!(c.fresh().is_none());
    }
}
