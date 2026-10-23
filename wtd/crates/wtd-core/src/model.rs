//! The fleet state the daemon holds and pushes to clients.

use serde::{Deserialize, Serialize};

use crate::status::Status;

/// Id of the dev-base pseudo-worktree that hosts the fleet assistant.
pub const DEV_ID: &str = "_dev";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeKind {
    /// A branch worktree of a registered repo: `worktrees/<slug>/<name>`.
    #[default]
    Repo,
    /// A repo-less planning agent: `worktrees/plan/<name>`.
    Plan,
    /// A repo's delegator: `worktrees/delegate/<slug>`, a docs/research folder whose agent spawns
    /// worktrees of that repo.
    Delegate,
    /// The dev base itself (the assistant session).
    Dev,
}

/// Where a worktree's files and processes live. Serialized as `windows` or `wsl:<distro>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Platform {
    #[default]
    Windows,
    Wsl(String),
}

impl Platform {
    pub fn distro(&self) -> Option<&str> {
        match self {
            Platform::Wsl(d) => Some(d),
            Platform::Windows => None,
        }
    }

    pub fn is_wsl(&self) -> bool {
        matches!(self, Platform::Wsl(_))
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Windows => f.write_str("windows"),
            Platform::Wsl(d) => write!(f, "wsl:{d}"),
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("windows") {
            return Ok(Platform::Windows);
        }
        match s.split_once(':') {
            Some((k, d)) if k.eq_ignore_ascii_case("wsl") && valid_distro(d) => Ok(Platform::Wsl(d.to_string())),
            Some((k, _)) if k.eq_ignore_ascii_case("wsl") => Err(format!("'{s}': bad WSL distro name")),
            _ => Err(format!("unknown platform '{s}' (expected windows or wsl:<distro>)")),
        }
    }
}

/// Distro names as `wsl.exe --install`/`--import` accept them; anything else can't be passed safely.
fn valid_distro(d: &str) -> bool {
    !d.is_empty() && d.len() <= 64 && d.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Serialize for Platform {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Platform {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GitState {
    /// False until the first `git status` for this worktree has completed.
    pub known: bool,
    pub branch: Option<String>,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    /// Unix seconds of the last completed check.
    pub checked_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Worktree {
    /// Path under `worktrees/` with `/` separators (`luop/feat/x`), or [`DEV_ID`].
    pub id: String,
    pub kind: WorktreeKind,
    pub slug: String,
    pub name: String,
    /// Absolute Windows path.
    pub path: String,
    pub status: Status,
    /// While `limited`: unix seconds the usage limit resets (0 = unknown).
    #[serde(default)]
    pub limit_resets_at: i64,
    /// A `wtd run` session is attached (an agent process is alive).
    pub live: bool,
    /// Claude account the live session runs under (`default` = ~/.claude).
    pub account: Option<String>,
    pub git: GitState,
    /// First `# heading` of `.claude/plans/active-plan.md`, if any.
    pub plan_title: Option<String>,
    /// Unix seconds of the last hook event or status change.
    pub last_activity: i64,
    /// User-defined group id (None = Ungrouped).
    #[serde(default)]
    pub group: Option<String>,
    /// Id of the delegator (`delegate/<slug>`) that started this worktree; the roster nests it there.
    #[serde(default)]
    pub parent: Option<String>,
    /// The live session is hosted by the daemon (a pseudo console that survives VSCode reloads),
    /// as opposed to running inside a terminal under `wtd run`.
    #[serde(default)]
    pub hosted: bool,
    /// Agent program of the live session: `claude` | `codex`.
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub platform: Platform,
    /// For a WSL worktree: its path inside the distro (`/home/<user>/dev/worktree-dev/worktrees/…`).
    /// `path` is then the `\\wsl.localhost\…` view of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wsl_path: Option<String>,
}

/// A user-defined roster group (replaces grouping by repo). Order = position in the list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Group {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessageState {
    /// Waiting for the user to approve / edit / deny.
    #[default]
    Pending,
    /// Approved; waiting for the target to be idle (its turn to end) or to start.
    Queued,
    Delivered,
    Denied,
    /// The target worktree was archived or deleted before delivery.
    Expired,
}

/// A prompt one agent asked to send to another worktree's agent. Never delivered without approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Message {
    pub id: u64,
    /// Sender worktree id.
    pub from: String,
    /// Target worktree id.
    pub to: String,
    pub body: String,
    pub state: MessageState,
    /// Unix seconds.
    pub created: i64,
    #[serde(default)]
    pub delivered_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Limit {
    /// Percent of the window used (0-100).
    pub used: Option<f64>,
    /// Unix seconds when the window resets.
    pub resets_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Account {
    pub name: String,
    pub email: String,
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    /// Unix seconds the usage numbers were fetched (0 = never).
    pub ts: i64,
    /// No credentials found: the account isn't logged in.
    pub nologin: bool,
    /// `claude` | `codex` (empty in older daemons = claude). Codex accounts are named `codex:<name>`.
    #[serde(default)]
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Metrics {
    /// Live `wtd run` sessions of any kind.
    pub sessions: u32,
    /// Interactive agent sessions (incl. the assistant).
    pub agents: u32,
    /// Review / ask workers.
    pub reviews: u32,
    /// Summed CPU of all session process trees, percent of one core (may exceed 100).
    pub cpu_pct: f64,
    /// Summed working set of all session process trees, MB.
    pub mem_mb: u64,
    pub sys_total_mb: u64,
    pub sys_used_mb: u64,
    pub ncpu: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub rev: u64,
    pub dev_root: String,
    pub worktrees: Vec<Worktree>,
    pub accounts: Vec<Account>,
    pub metrics: Option<Metrics>,
    #[serde(default)]
    pub groups: Vec<Group>,
    /// Messages not yet delivered or denied (pending approval or queued).
    #[serde(default)]
    pub messages: Vec<Message>,
}

/// Reorder an already-sorted list so each delegator's workers follow it, keeping list order among
/// them. `parent` is cleared on every row left unnested (its delegator isn't in the list), so a
/// frontend can indent on `parent.is_some()`.
pub fn nest_under_parents(rows: Vec<Worktree>) -> Vec<Worktree> {
    let ids: std::collections::HashSet<String> = rows.iter().map(|w| w.id.clone()).collect();
    let mut kids: std::collections::BTreeMap<String, Vec<Worktree>> = Default::default();
    let mut out = Vec::with_capacity(rows.len());
    for mut w in rows {
        match w.parent.clone().filter(|p| *p != w.id && ids.contains(p)) {
            Some(p) => kids.entry(p).or_default().push(w),
            None => {
                w.parent = None;
                out.push(w);
            }
        }
    }
    let top = std::mem::take(&mut out);
    for w in top {
        let id = w.id.clone();
        out.push(w);
        out.extend(kids.remove(&id).unwrap_or_default());
    }
    // a parent that is itself nested (never recorded: parents are delegators): show its rows unnested
    out.extend(kids.into_values().flatten().map(|w| Worktree { parent: None, ..w }));
    out
}

/// Encode a worktree id as a single path segment (`luop/feat/x` → `luop__feat__x`).
pub fn id_to_key(id: &str) -> String {
    id.replace('/', "__")
}

pub fn key_to_id(key: &str) -> String {
    key.replace("__", "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        for id in ["luop/feat/x", "luop/main", DEV_ID, "plan/new-app"] {
            assert_eq!(key_to_id(&id_to_key(id)), id);
        }
        assert_eq!(id_to_key("luop/feat/x"), "luop__feat__x");
    }

    #[test]
    fn platform_parse_and_display() {
        assert_eq!("windows".parse::<Platform>().unwrap(), Platform::Windows);
        assert_eq!("Windows".parse::<Platform>().unwrap(), Platform::Windows);
        assert_eq!("wsl:Ubuntu-24.04".parse::<Platform>().unwrap(), Platform::Wsl("Ubuntu-24.04".into()));
        assert_eq!(" WSL:Debian ".parse::<Platform>().unwrap(), Platform::Wsl("Debian".into()));
        for bad in ["", "wsl", "wsl:", "wsl:a b", "wsl:x;rm", "linux", "mac"] {
            assert!(bad.parse::<Platform>().is_err(), "{bad} should be rejected");
        }
        for s in ["windows", "wsl:Ubuntu"] {
            assert_eq!(s.parse::<Platform>().unwrap().to_string(), s);
        }
    }

    #[test]
    fn platform_serde() {
        assert_eq!(serde_json::to_string(&Platform::Wsl("Ubuntu".into())).unwrap(), "\"wsl:Ubuntu\"");
        assert_eq!(serde_json::from_str::<Platform>("\"windows\"").unwrap(), Platform::Windows);
        assert!(serde_json::from_str::<Platform>("\"bogus\"").is_err());
        // a worktree from an older daemon has no platform field
        let w: Worktree = serde_json::from_value(serde_json::json!({
            "id": "a/b", "kind": "repo", "slug": "a", "name": "b", "path": "p", "status": "working",
            "live": false, "account": null, "git": {"known": false, "branch": null, "dirty": false, "ahead": 0, "behind": 0, "checked_at": 0},
            "plan_title": null, "last_activity": 0
        }))
        .unwrap();
        assert_eq!(w.platform, Platform::Windows);
        assert_eq!(w.wsl_path, None);
        let v = serde_json::to_value(Worktree { platform: Platform::Wsl("Ubuntu".into()), wsl_path: Some("/home/u/x".into()), ..Default::default() }).unwrap();
        assert_eq!(v["platform"], "wsl:Ubuntu");
        assert_eq!(v["wsl_path"], "/home/u/x");
        assert_eq!(serde_json::to_value(Worktree::default()).unwrap()["platform"], "windows");
    }

    #[test]
    fn nests_children_after_their_parent() {
        let wt = |id: &str, parent: Option<&str>| Worktree { id: id.into(), parent: parent.map(String::from), ..Default::default() };
        let rows = vec![
            wt("luop/a", Some("delegate/luop")),
            wt("other/b", None),
            wt("delegate/luop", None),
            wt("luop/c", Some("delegate/luop")),
            wt("luop/orphan", Some("delegate/gone")),
        ];
        let out = nest_under_parents(rows);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["other/b", "delegate/luop", "luop/a", "luop/c", "luop/orphan"]);
        let nested: Vec<bool> = out.iter().map(|w| w.parent.is_some()).collect();
        assert_eq!(nested, [false, false, true, true, false]);
    }
}
