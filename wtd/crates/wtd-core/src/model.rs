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
    /// The dev base itself (the assistant session).
    Dev,
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
    /// A `wtd run` session is attached (an agent process is alive).
    pub live: bool,
    /// Claude account the live session runs under (`default` = ~/.claude).
    pub account: Option<String>,
    pub git: GitState,
    /// First `# heading` of `.claude/plans/active-plan.md`, if any.
    pub plan_title: Option<String>,
    /// Unix seconds of the last hook event or status change.
    pub last_activity: i64,
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
}
