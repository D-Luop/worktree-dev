//! Agent-to-agent messages. An agent can only *request* a send (`message.send` → pending); nothing is
//! delivered until the user approves it in VSCode (`message.decide`). Approved messages are typed
//! into the target's hosted session as a prompt — when its turn has ended (never mid-turn), one per
//! turn — or wait in its inbox until its session runs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use wtd_core::model::{Message, MessageState};
use wtd_core::protocol::Push;
use wtd_core::status::Status;

use super::{now, Daemon, Inner};

const MAX_BODY: usize = 32 * 1024;
/// Open (pending) requests one sender may have at a time.
const MAX_PENDING_PER_SENDER: usize = 5;

impl Daemon {
    fn messages_changed(&self, inner: &mut Inner) {
        if let Err(e) = inner.store.save(&self.dev) {
            eprintln!("[{}] saving store: {e}", now());
        }
        inner.rev += 1;
        let _ = self.tx.send(Push::Messages { rev: inner.rev, messages: inner.store.open_messages() });
    }
}

pub fn send(d: &Arc<Daemon>, p: &Value) -> Result<Value> {
    let from_dir = p.get("from_dir").and_then(Value::as_str).context("missing from_dir")?;
    let from = d.worktree_id_for(from_dir).context("the sender isn't a worktree-dev worktree")?;
    let to = p.get("to").and_then(Value::as_str).context("missing to")?.trim().to_string();
    let body = p.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if body.is_empty() {
        bail!("the message is empty");
    }
    if body.len() > MAX_BODY {
        bail!("the message is too long ({} bytes; max {MAX_BODY})", body.len());
    }
    let mut inner = d.lock();
    if !inner.worktrees.contains_key(&to) {
        bail!("no worktree '{to}' — use fleet_list for valid ids");
    }
    if to == from {
        bail!("a worktree can't message itself");
    }
    let pending = inner.store.messages.iter().filter(|m| m.from == from && m.state == MessageState::Pending).count();
    if pending >= MAX_PENDING_PER_SENDER {
        bail!("{pending} of your messages are already waiting for the user's approval — wait for a decision");
    }
    let id = inner.store.next();
    inner.store.messages.push(Message { id, from: from.clone(), to: to.clone(), body, state: MessageState::Pending, created: now(), delivered_at: 0 });
    d.messages_changed(&mut inner);
    eprintln!("[{}] message #{id} {from} → {to} pending approval", now());
    Ok(json!({ "id": id, "state": "pending", "from": from, "to": to }))
}

pub fn decide(d: &Arc<Daemon>, p: &Value) -> Result<Value> {
    let approve = p.get("approve").and_then(Value::as_bool).context("missing approve")?;
    if let Some(ids) = p.get("ids").and_then(Value::as_array) {
        let ids: Vec<u64> = ids.iter().filter_map(Value::as_u64).collect();
        return decide_many(d, &ids, approve);
    }
    let id = p.get("id").and_then(Value::as_u64).context("missing id")?;
    let to = {
        let mut inner = d.lock();
        let m = inner.store.messages.iter_mut().find(|m| m.id == id).with_context(|| format!("no message #{id}"))?;
        if m.state != MessageState::Pending {
            bail!("message #{id} was already decided ({})", state_name(m.state));
        }
        if approve {
            if let Some(b) = p.get("body").and_then(Value::as_str).map(str::trim).filter(|b| !b.is_empty()) {
                m.body = b.chars().take(MAX_BODY).collect();
            }
        }
        m.state = if approve { MessageState::Queued } else { MessageState::Denied };
        let to = m.to.clone();
        d.messages_changed(&mut inner);
        to
    };
    eprintln!("[{}] message #{id} {}", now(), if approve { "approved" } else { "denied" });
    if approve {
        deliver_next(d, &to);
    }
    Ok(json!({ "decided": [id] }))
}

/// Bulk approve/deny (e.g. clearing stale requests). Ids that aren't pending are skipped, not errors.
fn decide_many(d: &Arc<Daemon>, ids: &[u64], approve: bool) -> Result<Value> {
    let (decided, targets) = {
        let mut inner = d.lock();
        let next = if approve { MessageState::Queued } else { MessageState::Denied };
        let mut decided = Vec::new();
        let mut targets = std::collections::BTreeSet::new();
        for m in inner.store.messages.iter_mut().filter(|m| ids.contains(&m.id) && m.state == MessageState::Pending) {
            m.state = next;
            decided.push(m.id);
            targets.insert(m.to.clone());
        }
        if !decided.is_empty() {
            d.messages_changed(&mut inner);
        }
        (decided, targets)
    };
    eprintln!("[{}] messages {decided:?} {}", now(), if approve { "approved" } else { "denied" });
    for to in &targets {
        deliver_next(d, to);
    }
    Ok(json!({ "decided": decided }))
}

/// Expire undelivered messages whose target worktree is gone from disk (archived/deleted): they could
/// never be delivered, and would otherwise sit pending forever.
pub(super) fn expire_orphans(d: &Daemon, inner: &mut Inner) {
    let gone = |to: &str| !inner.worktrees.contains_key(to) && !crate::paths::worktree_path(&d.dev, to).is_dir();
    let orphans: Vec<u64> = inner.store.messages.iter().filter(|m| matches!(m.state, MessageState::Pending | MessageState::Queued) && gone(&m.to)).map(|m| m.id).collect();
    if orphans.is_empty() {
        return;
    }
    for m in inner.store.messages.iter_mut().filter(|m| orphans.contains(&m.id)) {
        m.state = MessageState::Expired;
    }
    eprintln!("[{}] messages {orphans:?} expired (target worktree gone)", now());
    d.messages_changed(inner);
}

fn state_name(s: MessageState) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

pub fn list(d: &Arc<Daemon>, p: &Value) -> Result<Value> {
    let inner = d.lock();
    let wt = p.get("worktree").and_then(Value::as_str);
    let mut v: Vec<&Message> = inner.store.messages.iter().filter(|m| wt.is_none_or(|w| m.from == w || m.to == w)).collect();
    v.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(serde_json::to_value(v)?)
}

/// Deliver the oldest queued message for `to` if its hosted session is idle (its turn has ended).
/// Called on approval, on the target's turn ending, and shortly after its session starts.
pub fn deliver_next(d: &Arc<Daemon>, to: &str) {
    let (msg, pty) = {
        let mut inner = d.lock();
        let busy = inner.worktrees.get(to).is_some_and(|w| matches!(w.status, Status::Working | Status::Reviewing));
        let Some(pty) = inner.sessions.values().find(|s| s.wt == to).and_then(|s| s.hosted.as_ref()).map(|h| h.pty.clone()) else { return };
        if busy {
            return;
        }
        let Some(m) = inner.store.messages.iter_mut().filter(|m| m.to == to && m.state == MessageState::Queued).min_by_key(|m| m.id) else { return };
        m.state = MessageState::Delivered;
        m.delivered_at = now();
        let msg = m.clone();
        // mark the target busy right away so a second queued message waits for the next turn end
        if let Some(mut w) = inner.worktrees.get(to).cloned() {
            w.status = Status::Working;
            d.upsert(&mut inner, w);
        }
        d.messages_changed(&mut inner);
        (msg, pty)
    };
    let text = format!(
        "[wtd message #{} from {} — sent with the user's approval; to reply, propose a reply to the user and use fleet_send to \"{}\"]\n\n{}",
        msg.id, msg.from, msg.from, msg.body
    );
    eprintln!("[{}] delivering message #{} → {}", now(), msg.id, msg.to);
    // bracketed paste keeps multi-line text as one prompt; Enter separately, once the paste has landed
    std::thread::spawn(move || {
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend_from_slice(text.replace("\r\n", "\n").as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        if pty.write(&paste).is_ok() {
            std::thread::sleep(Duration::from_millis(400));
            let _ = pty.write(b"\r");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;
    use wtd_core::model::Worktree;

    fn daemon(tag: &str, ids: &[&str]) -> Arc<Daemon> {
        let dev = std::env::temp_dir().join(format!("wtd-msg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dev);
        let mut inner = Inner::default();
        for id in ids {
            let p = crate::paths::worktree_path(&dev, id);
            std::fs::create_dir_all(p.join(".git")).unwrap();
            inner.worktrees.insert(id.to_string(), Worktree { id: id.to_string(), path: p.to_string_lossy().into(), ..Default::default() });
        }
        let (tx, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Daemon { dev, inner: Mutex::new(inner), tx, next_conn: AtomicU64::new(1), watchers: Mutex::new(HashMap::new()), updates: Default::default() })
    }

    fn send_from(d: &Arc<Daemon>, from: &str, to: &str) -> u64 {
        let dir = crate::paths::worktree_path(&d.dev, from);
        send(d, &json!({ "from_dir": dir.to_string_lossy(), "to": to, "body": "hi" })).unwrap()["id"].as_u64().unwrap()
    }

    fn state(d: &Arc<Daemon>, id: u64) -> MessageState {
        d.lock().store.messages.iter().find(|m| m.id == id).unwrap().state
    }

    #[test]
    fn approve_queues_until_the_target_session_can_take_it() {
        let d = daemon("approve", &["delegate/r", "r/feat/a"]);
        let id = send_from(&d, "delegate/r", "r/feat/a");
        assert_eq!(state(&d, id), MessageState::Pending);
        decide(&d, &json!({ "id": id, "approve": true, "body": "edited" })).unwrap();
        // no hosted session for the target yet → stays queued
        assert_eq!(state(&d, id), MessageState::Queued);
        assert_eq!(d.lock().store.messages[0].body, "edited");
        assert!(decide(&d, &json!({ "id": id, "approve": false })).is_err());
    }

    #[test]
    fn bulk_deny_skips_already_decided() {
        let d = daemon("bulk", &["delegate/r", "r/feat/a"]);
        let (a, b, c) = (send_from(&d, "delegate/r", "r/feat/a"), send_from(&d, "delegate/r", "r/feat/a"), send_from(&d, "delegate/r", "r/feat/a"));
        decide(&d, &json!({ "id": a, "approve": true })).unwrap();
        let r = decide(&d, &json!({ "ids": [a, b, c], "approve": false })).unwrap();
        assert_eq!(r["decided"], json!([b, c]));
        assert_eq!((state(&d, a), state(&d, b), state(&d, c)), (MessageState::Queued, MessageState::Denied, MessageState::Denied));
        assert!(d.lock().store.open_messages().iter().all(|m| m.id == a));
    }

    #[test]
    fn messages_to_a_removed_worktree_expire() {
        let d = daemon("expire", &["delegate/r", "r/feat/a", "r/feat/b"]);
        let (to_a, to_b) = (send_from(&d, "delegate/r", "r/feat/a"), send_from(&d, "delegate/r", "r/feat/b"));
        std::fs::remove_dir_all(crate::paths::worktree_path(&d.dev, "r/feat/a")).unwrap();
        let mut inner = d.lock();
        inner.worktrees.remove("r/feat/a");
        expire_orphans(&d, &mut inner);
        drop(inner);
        assert_eq!((state(&d, to_a), state(&d, to_b)), (MessageState::Expired, MessageState::Pending));
    }
}
