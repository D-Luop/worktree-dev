//! Wire protocol between `wtd daemon` and its clients (extension, CLI, hook, tray, MCP, `wtd run`).
//!
//! Transport: a per-user named pipe carrying newline-delimited JSON objects.
//! - Client → daemon: a [`Request`]. With an `id` it gets exactly one [`Response`]; without one it is a
//!   fire-and-forget notification (used by `wtd hook`).
//! - Daemon → client: [`Response`]s, plus [`Push`] events after `subscribe`: one `snapshot`, then
//!   `upsert` / `remove` / `metrics` as state changes. Every push carries the state `rev` it produced.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Account, Group, Message, Metrics, Snapshot, Worktree};

pub const PROTOCOL_VERSION: u32 = 1;

/// `\\.\pipe\wtd-<user>`: one daemon per Windows user.
pub fn pipe_name(user: &str) -> String {
    let safe: String = user.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!(r"\\.\pipe\wtd-{safe}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Push {
    Snapshot { rev: u64, snapshot: Snapshot },
    Upsert { rev: u64, worktree: Worktree },
    Remove { rev: u64, id: String },
    Accounts { rev: u64, accounts: Vec<Account> },
    Metrics { rev: u64, metrics: Metrics },
    /// The full group list (order matters) after any group change.
    Groups { rev: u64, groups: Vec<Group> },
    /// Undelivered messages (pending approval or queued) after any message change.
    Messages { rev: u64, messages: Vec<Message> },
    /// The daemon is shutting down (clients should show "stopped").
    Shutdown,
    /// Sent to a `wtd run` connection: end your session (terminate its job).
    Terminate,
}

/// Any line the daemon writes: a response or a push.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerLine {
    Response(Response),
    Push(Push),
}

pub mod method {
    /// `{client, protocol}` → `{protocol, version, dev_root, pid}`.
    pub const HELLO: &str = "hello";
    /// `{metrics?: bool}` → `{}` then pushes on this connection.
    pub const SUBSCRIBE: &str = "subscribe";
    /// Notification from `wtd hook` after it applied a status change: [`super::HookParams`].
    pub const HOOK: &str = "hook";
    /// From `wtd run`, held for the session's lifetime: [`super::SessionParams`] → `{}`.
    /// The daemon may later push `{"event":"terminate"}` on that connection.
    pub const SESSION_REGISTER: &str = "session.register";
    /// `{id}` → `{}`: terminate a live session (kills its job → whole process tree).
    pub const SESSION_STOP: &str = "session.stop";
    /// `{}` → `[Worktree]`.
    pub const FLEET_LIST: &str = "fleet.list";
    /// `{id}` → `Worktree`.
    pub const FLEET_GET: &str = "fleet.get";
    /// `{id?}` → `{}`: rescan worktrees and re-check git (one, or all).
    pub const REFRESH: &str = "refresh";
    /// `{}` → `{}`, then the daemon exits.
    pub const SHUTDOWN: &str = "shutdown";

    // --- groups (Phase 2) ---
    /// `{name}` → `{id}`
    pub const GROUP_CREATE: &str = "group.create";
    /// `{id, name?, collapsed?}` → `{}`
    pub const GROUP_UPDATE: &str = "group.update";
    /// `{id}` → `{}`: members return to Ungrouped
    pub const GROUP_DELETE: &str = "group.delete";
    /// `{ids: [..]}` → `{}`: new order
    pub const GROUP_REORDER: &str = "group.reorder";
    /// `{worktree, group: id|null}` → `{}`
    pub const GROUP_ASSIGN: &str = "group.assign";

    // --- agent messaging (Phase 2) ---
    /// `{from_dir, to, body}` → `{id, state}`. Always created `pending`: only the user releases it.
    pub const MESSAGE_SEND: &str = "message.send";
    /// `{id, approve: bool, body?}` → `{}` (from the UI, on the user's decision)
    pub const MESSAGE_DECIDE: &str = "message.decide";
    /// `{worktree?}` → `[Message]` (sent and received, most recent first)
    pub const MESSAGE_LIST: &str = "message.list";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookParams {
    /// Worktree root the hook resolved (absolute).
    pub dir: String,
    /// The event word (`tool`, `stop`, …).
    pub event: String,
    /// The status after applying the event.
    pub status: crate::status::Status,
    /// Whether the event changed the persisted state.
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParams {
    /// Worktree root (absolute).
    pub dir: String,
    /// `agent` | `assistant` | `review` | `ask`.
    pub kind: String,
    /// Account name, if not the default.
    pub account: Option<String>,
    /// Named Job Object holding the session's process tree.
    pub job: String,
    /// Pid of the launched program (claude).
    pub pid: u32,
    /// Program name (`claude` | `codex`), for the roster.
    #[serde(default)]
    pub program: Option<String>,
}
