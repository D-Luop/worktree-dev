//! The daemon's durable state: user-defined groups, group membership, and agent messages.
//! Small enough for a JSON file (`.wtd/state/store.json`), written atomically on every change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wtd_core::model::{Group, Message, MessageState};

/// Delivered/denied messages kept for the inbox history.
const KEEP_CLOSED: usize = 200;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub groups: Vec<Group>,
    /// worktree id → group id
    #[serde(default)]
    pub membership: BTreeMap<String, String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub next_id: u64,
    /// Deferred `wtd <kind> <args…>` runs (review retries after a usage-limit reset).
    #[serde(default)]
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub kind: String,
    pub args: Vec<String>,
    /// Unix seconds.
    pub at: i64,
    /// Dedupe key (e.g. the worktree path): one job per kind+key.
    pub key: String,
}

fn path(dev: &Path) -> PathBuf {
    crate::paths::state_dir(dev).join("store.json")
}

impl Store {
    pub fn load(dev: &Path) -> Store {
        let p = path(dev);
        let Ok(text) = std::fs::read_to_string(&p) else { return Store::default() };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                // keep the unreadable file for inspection rather than silently overwriting it
                let bad = p.with_extension(format!("json.bad-{}", super::now()));
                let _ = std::fs::rename(&p, &bad);
                eprintln!("[{}] store.json unreadable ({e}); moved to {}", super::now(), bad.display());
                Store::default()
            }
        }
    }

    pub fn save(&mut self, dev: &Path) -> std::io::Result<()> {
        let closed = self.messages.iter().filter(|m| is_closed(m)).count();
        if closed > KEEP_CLOSED {
            let mut drop = closed - KEEP_CLOSED; // oldest first: messages are in creation order
            self.messages.retain(|m| if drop > 0 && is_closed(m) { drop -= 1; false } else { true });
        }
        let p = path(dev);
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, p)
    }

    pub fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Messages still in flight (pending approval or queued for delivery).
    pub fn open_messages(&self) -> Vec<Message> {
        self.messages.iter().filter(|m| !is_closed(m)).cloned().collect()
    }
}

fn is_closed(m: &Message) -> bool {
    matches!(m.state, MessageState::Delivered | MessageState::Denied)
}
