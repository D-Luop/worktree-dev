//! Automatic updates of an installed WorkTreeDev (see docs/distribution-contract.md, "Layout").
//!
//! check `latest.json` → download the payload zip to `pending\<ver>\` → verify sha256 (+ Authenticode
//! when the running build is signed) → unzip to `versions\<ver>\` (staged) → apply at a safe point:
//! swap `current\`, restart the daemon and tray, health-check, roll back on failure.
//! A source checkout (dev mode) never updates itself.

pub mod apply;
pub mod fetch;
mod sign;

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{paths, settings};

pub const RELEASES: &str = "https://github.com/D-Luop/worktree-dev/releases";
const RELEASES_API: &str = "https://api.github.com/repos/D-Luop/worktree-dev/releases?per_page=30";

pub fn running_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// --- versions ----------------------------------------------------------------------------------------

/// A semantic version: `X.Y.Z[-pre][+build]` (a leading `v` is accepted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches('v');
        let s = s.split_once('+').map_or(s, |(v, _)| v);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, p.split('.').map(String::from).collect::<Vec<_>>()),
            Some(_) => return None,
            None => (s, vec![]),
        };
        if pre.iter().any(|p| p.is_empty()) {
            return None;
        }
        let mut n = core.split('.').map(|p| if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) { None } else { p.parse().ok() });
        let v = Version { major: n.next()??, minor: n.next()??, patch: n.next()??, pre };
        n.next().is_none().then_some(v)
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(o.major, o.minor, o.patch)).then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&o.pre) {
                    let c = match (a.parse::<u64>(), b.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        (Err(_), Err(_)) => a.cmp(b),
                    };
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                self.pre.len().cmp(&o.pre.len())
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// `candidate` is a strictly newer, valid version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(c), Some(r)) => c > r,
        _ => false,
    }
}

// --- latest.json -------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub published: String,
    pub payload: Asset,
    #[serde(default)]
    pub setup: Option<Asset>,
    #[serde(default)]
    pub min_daemon_protocol: u32,
    #[serde(default)]
    pub notes_url: Option<String>,
}

impl Manifest {
    pub fn parse(s: &str) -> Result<Manifest> {
        let m: Manifest = serde_json::from_str(s.trim_start_matches('\u{feff}')).context("parsing latest.json")?;
        if Version::parse(&m.version).is_none() {
            bail!("latest.json: bad version '{}'", m.version);
        }
        if !is_sha256_hex(&m.payload.sha256) {
            bail!("latest.json: payload.sha256 isn't a sha256 hex digest");
        }
        if m.payload.url.is_empty() {
            bail!("latest.json: no payload.url");
        }
        Ok(m)
    }

    pub fn version(&self) -> String {
        self.version.trim_start_matches('v').to_string()
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// --- settings (dev root `.wtd/config.json` → "update") -------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub fn parse(s: &str) -> Option<Channel> {
        match s {
            "stable" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }
}

/// `update.restartWithLiveSessions`: what to do with a staged update while hosted sessions are live
/// (restarting the daemon ends them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LivePolicy {
    /// wait until no hosted session is live, or the next tray start / logon
    Never,
    /// offer "Restart to update" and let the user decide
    Ask,
    /// restart anyway
    Auto,
}

impl LivePolicy {
    pub fn parse(s: &str) -> Option<LivePolicy> {
        match s {
            "never" => Some(LivePolicy::Never),
            "ask" => Some(LivePolicy::Ask),
            "auto" => Some(LivePolicy::Auto),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            LivePolicy::Never => "never",
            LivePolicy::Ask => "ask",
            LivePolicy::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub auto: bool,
    pub channel: Channel,
    pub live: LivePolicy,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto: true, channel: Channel::Stable, live: LivePolicy::Ask }
    }
}

impl Settings {
    pub fn from_config(cfg: &Value) -> Settings {
        let u = &cfg["update"];
        let d = Settings::default();
        Settings {
            auto: u["auto"].as_bool().unwrap_or(d.auto),
            channel: u["channel"].as_str().and_then(Channel::parse).unwrap_or(d.channel),
            live: u["restartWithLiveSessions"].as_str().and_then(LivePolicy::parse).unwrap_or(d.live),
        }
    }

    pub fn load() -> Settings {
        paths::dev_root().map(|dev| Settings::from_config(&settings::load_config(&dev))).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let dev = paths::dev_root()?;
        settings::update_config(&dev, |cfg| {
            cfg["update"] = json!({ "auto": self.auto, "channel": self.channel.as_str(), "restartWithLiveSessions": self.live.as_str() });
        })
    }

    pub fn to_json(self) -> Value {
        json!({ "auto": self.auto, "channel": self.channel.as_str(), "restartWithLiveSessions": self.live.as_str() })
    }
}

// --- program dir layout + persisted state -------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Layout {
        Layout { root: root.into() }
    }
    pub fn installed() -> Result<Layout> {
        Ok(Layout::new(paths::program_dir()?))
    }
    pub fn current(&self) -> PathBuf {
        self.root.join("current")
    }
    pub fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }
    pub fn version_dir(&self, ver: &str) -> PathBuf {
        self.versions().join(ver)
    }
    pub fn pending_dir(&self, ver: &str) -> PathBuf {
        self.root.join("pending").join(ver)
    }
    pub fn state_file(&self) -> PathBuf {
        self.root.join("update-state.json")
    }
    pub fn log_file(&self) -> PathBuf {
        self.root.join("update.log")
    }
    /// The version `current\` holds (its `version.txt`).
    pub fn current_version(&self) -> Option<String> {
        read_version(&self.current())
    }
    /// A downloaded, verified, unpacked version that's newer than `current\` and not yet applied.
    pub fn staged(&self) -> Option<String> {
        let st = State::load(self);
        let v = st.staged?;
        let cur = self.current_version().unwrap_or_else(|| running_version().into());
        (is_newer(&v, &cur) && !st.failed.contains(&v) && read_version(&self.version_dir(&v)).as_deref() == Some(v.as_str())).then_some(v)
    }

    pub fn log(&self, msg: &str) {
        use std::io::Write;
        let _ = std::fs::create_dir_all(&self.root);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.log_file()) {
            let _ = writeln!(f, "[{}] {msg}", crate::daemon::now());
        }
    }
}

pub fn read_version(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("version.txt")).ok().map(|s| s.trim().trim_start_matches('v').to_string()).filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApplyRecord {
    pub from: String,
    pub to: String,
    pub at: i64,
    pub ok: bool,
    #[serde(default)]
    pub vsix_changed: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub last_check: i64,
    /// the newest version the last check found
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
    #[serde(default)]
    pub staged: Option<String>,
    /// versions that failed their health check: never retried
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub last_apply: Option<ApplyRecord>,
    #[serde(default)]
    pub error: Option<String>,
}

impl State {
    pub fn load(l: &Layout) -> State {
        std::fs::read_to_string(l.state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, l: &Layout) -> Result<()> {
        std::fs::create_dir_all(&l.root)?;
        let p = l.state_file();
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    pub fn update(l: &Layout, f: impl FnOnce(&mut State)) -> Result<State> {
        let mut s = State::load(l);
        f(&mut s);
        s.save(l)?;
        Ok(s)
    }
}

// --- mode ---------------------------------------------------------------------------------------------

/// Updates run only for an installed build: the running exe lives under the program dir. A source
/// checkout (`<dev>/.wtd/bin`, `cargo run`) or `WTD_DEV_SOURCE=1` is a dev build.
pub fn installed_layout() -> Option<Layout> {
    if std::env::var("WTD_DEV_SOURCE").is_ok_and(|v| v == "1") {
        return None;
    }
    let l = Layout::installed().ok()?;
    let exe = std::env::current_exe().ok()?;
    let under = |base: &Path| {
        let b = base.to_string_lossy().trim_end_matches('\\').to_lowercase();
        exe.to_string_lossy().to_lowercase().starts_with(&format!("{b}\\"))
    };
    let canon = std::fs::canonicalize(&l.root).ok();
    (under(&l.root) || canon.as_deref().is_some_and(|c| under(&strip_verbatim(c)))).then_some(l)
}

pub(crate) fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()),
    }
}

/// The manifest URL(s) for a channel. `WTD_UPDATE_URL` (http(s) or file://) replaces them all.
fn manifest_urls(channel: Channel) -> Result<Vec<String>> {
    if let Ok(u) = std::env::var("WTD_UPDATE_URL") {
        if !u.is_empty() {
            return Ok(vec![u]);
        }
    }
    let stable = format!("{RELEASES}/latest/download/latest.json");
    Ok(match channel {
        Channel::Stable => vec![stable],
        // releases/latest skips prereleases: find the newest prerelease's latest-beta.json
        Channel::Beta => {
            let mut v = vec![stable];
            if let Some(u) = beta_manifest_url()? {
                v.push(u);
            }
            v
        }
    })
}

fn beta_manifest_url() -> Result<Option<String>> {
    let body = fetch::get_text(RELEASES_API)?;
    let releases: Value = serde_json::from_str(&body).context("parsing the GitHub releases list")?;
    Ok(releases.as_array().into_iter().flatten().filter(|r| r["prerelease"].as_bool() == Some(true) && r["draft"].as_bool() != Some(true)).find_map(|r| {
        r["assets"].as_array()?.iter().find(|a| a["name"] == "latest-beta.json").and_then(|a| a["browser_download_url"].as_str()).map(String::from)
    }))
}

/// The newest release on `channel` (beta also sees stable releases).
pub fn check(channel: Channel) -> Result<Manifest> {
    let mut best: Option<Manifest> = None;
    let mut last_err = None;
    for url in manifest_urls(channel)? {
        match fetch::get_text(&url).and_then(|s| Manifest::parse(&s)) {
            Ok(m) => {
                if best.as_ref().is_none_or(|b| is_newer(&m.version, &b.version)) {
                    best = Some(m);
                }
            }
            Err(e) => last_err = Some(e.context(format!("fetching {url}"))),
        }
    }
    match (best, last_err) {
        (Some(m), _) => Ok(m),
        (None, Some(e)) => Err(e),
        (None, None) => bail!("no release manifest found"),
    }
}

/// Check, and download + verify + unpack anything newer. Returns the staged version, if any.
pub fn check_and_stage(l: &Layout, channel: Channel, progress: &dyn Fn(u64, u64)) -> Result<Option<String>> {
    let res = (|| {
        let m = check(channel)?;
        let v = m.version();
        let cur = l.current_version().unwrap_or_else(|| running_version().into());
        State::update(l, |s| {
            s.last_check = crate::daemon::now();
            s.latest = Some(v.clone());
            s.notes_url = m.notes_url.clone();
        })?;
        if !is_newer(&v, &cur) || State::load(l).failed.contains(&v) {
            return Ok(None);
        }
        if l.staged().as_deref() == Some(v.as_str()) {
            return Ok(Some(v));
        }
        fetch::stage(l, &m, &fetch::Opts::default(), progress)?;
        State::update(l, |s| s.staged = Some(v.clone()))?;
        l.log(&format!("staged {v}"));
        Ok(Some(v))
    })();
    let err = res.as_ref().err().map(|e| format!("{e:#}"));
    let _ = State::update(l, |s| s.error = err);
    res
}

/// Everything a UI needs: mode, settings, what's available / staged, and the last apply.
pub fn status_json(phase: &str, progress: Option<(u64, u64)>) -> Value {
    let s = Settings::load();
    let Some(l) = installed_layout() else {
        return json!({ "mode": "dev", "version": running_version(), "phase": "dev", "settings": s.to_json() });
    };
    let st = State::load(&l);
    json!({
        "mode": "installed",
        "version": running_version(),
        "settings": s.to_json(),
        "phase": phase,
        "progress": progress.map(|(d, t)| json!({ "done": d, "total": t })),
        "latest": st.latest,
        "notes_url": st.notes_url,
        "staged": l.staged(),
        "failed": st.failed,
        "last_check": st.last_check,
        "last_apply": st.last_apply,
        "error": st.error,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn temp(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("wtd-upd-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn version_compare() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("1.2.3") > v("1.2.2"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.0.0") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta.10") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha.5"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-beta.11"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha.beta") > v("1.0.0-alpha.1"));
        assert_eq!(v("v1.2.3+build.5"), v("1.2.3"));
        for bad in ["", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.-3", "1.2.3-", "1.2.3-a..b", "1.2.x"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn parses_latest_json() {
        let sha = "ab".repeat(32);
        let s = format!(
            r#"{{"version":"1.4.0","channel":"stable","published":"2026-10-23T10:00:00Z",
               "payload":{{"url":"https://x/worktree-dev-1.4.0-windows-x64.zip","sha256":"{sha}","size":123}},
               "setup":{{"url":"https://x/WorkTreeDev-Setup-1.4.0.exe","sha256":"{sha}"}},
               "min_daemon_protocol":1,"notes_url":"https://x/notes","extra":true}}"#
        );
        let m = Manifest::parse(&s).unwrap();
        assert_eq!(m.version(), "1.4.0");
        assert_eq!(m.payload.size, 123);
        assert_eq!(m.setup.unwrap().size, 0);
        assert_eq!(m.notes_url.as_deref(), Some("https://x/notes"));
        // a BOM and a `v` prefix are tolerated
        let m = Manifest::parse(&format!("\u{feff}{{\"version\":\"v2.0.0-beta.1\",\"payload\":{{\"url\":\"u\",\"sha256\":\"{sha}\"}}}}")).unwrap();
        assert_eq!(m.version(), "2.0.0-beta.1");
        for bad in [
            format!(r#"{{"version":"1.x","payload":{{"url":"u","sha256":"{sha}"}}}}"#),
            r#"{"version":"1.0.0","payload":{"url":"u","sha256":"abc"}}"#.to_string(),
            format!(r#"{{"version":"1.0.0","payload":{{"url":"","sha256":"{sha}"}}}}"#),
            r#"{"version":"1.0.0"}"#.to_string(),
            "not json".to_string(),
        ] {
            assert!(Manifest::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_defaults_and_overrides() {
        assert_eq!(Settings::from_config(&json!({})), Settings::default());
        let s = Settings::from_config(&json!({ "update": { "auto": false, "channel": "beta", "restartWithLiveSessions": "never" } }));
        assert_eq!(s, Settings { auto: false, channel: Channel::Beta, live: LivePolicy::Never });
        let s = Settings::from_config(&json!({ "update": { "channel": "nightly", "restartWithLiveSessions": 3 } }));
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn staged_needs_a_newer_unpacked_unfailed_version() {
        let l = Layout::new(temp("staged"));
        let mk = |dir: PathBuf, v: &str| {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("version.txt"), format!("{v}\n")).unwrap();
        };
        mk(l.current(), "1.0.0");
        State::update(&l, |s| s.staged = Some("1.1.0".into())).unwrap();
        assert_eq!(l.staged(), None, "not unpacked yet");
        mk(l.version_dir("1.1.0"), "1.1.0");
        assert_eq!(l.staged().as_deref(), Some("1.1.0"));
        State::update(&l, |s| s.failed.push("1.1.0".into())).unwrap();
        assert_eq!(l.staged(), None, "failed versions aren't retried");
        State::update(&l, |s| s.failed.clear()).unwrap();
        mk(l.current(), "1.1.0");
        assert_eq!(l.staged(), None, "already current");
        std::fs::remove_dir_all(&l.root).ok();
    }
}
