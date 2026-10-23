//! The on-disk status: `<wt>/.claude-status` (+ `.claude-status.resume` for a stashed milestone,
//! `.claude-status.limit` for a usage limit's reset epoch),
//! mirrored to `.wtd/state/status/<key>` so one folder watch sees every change.
//! Files stay authoritative so status keeps working while the daemon is stopped.

use std::path::Path;

use wtd_core::model::id_to_key;
use wtd_core::status::{Status, StatusState};

const FILE: &str = ".claude-status";
const RESUME: &str = ".claude-status.resume";
const LIMIT: &str = ".claude-status.limit";

fn read_word(p: &Path) -> Status {
    std::fs::read_to_string(p).map(|s| Status::parse(&s)).unwrap_or_default()
}

pub fn read(root: &Path) -> StatusState {
    let resets_at = std::fs::read_to_string(root.join(LIMIT)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    StatusState { status: read_word(&root.join(FILE)), stash: read_word(&root.join(RESUME)), resets_at }
}

fn remove(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Persist `new` if it differs from `old`; writes only what changed.
pub fn write(dev: &Path, id: &str, root: &Path, old: StatusState, new: StatusState) -> std::io::Result<()> {
    if new.stash != old.stash {
        let p = root.join(RESUME);
        if new.stash == Status::None {
            remove(&p)?;
        } else {
            std::fs::write(&p, new.stash.as_str())?;
        }
    }
    if new.resets_at != old.resets_at {
        let p = root.join(LIMIT);
        if new.resets_at == 0 {
            remove(&p)?;
        } else {
            std::fs::write(&p, new.resets_at.to_string())?;
        }
    }
    if new.status != old.status {
        std::fs::write(root.join(FILE), new.status.as_str())?;
        let mirror = crate::paths::state_dir(dev).join("status");
        std::fs::create_dir_all(&mirror)?;
        std::fs::write(mirror.join(id_to_key(id)), new.status.as_str())?;
    }
    Ok(())
}
