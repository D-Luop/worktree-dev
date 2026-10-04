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
    let id = p.get("id").and_then(Value::as_u64).context("missing id")?;
    let approve = p.get("approve").and_then(Value::as_bool).context("missing approve")?;
    let to = {
        let mut inner = d.lock();
        let m = inner.store.messages.iter_mut().find(|m| m.id == id).with_context(|| format!("no message #{id}"))?;
        if m.state != MessageState::Pending {
            bail!("message #{id} was already decided");
        }
        if approve {
            if let Some(b) = p.get("body").and_then(Value::as_str).map(str::trim).filter(|b| !b.is_empty()) {
                m.body = b.chars().take(MAX_BODY).collect();
            }
            m.state = MessageState::Queued;
        } else {
            m.state = MessageState::Denied;
        }
        let to = m.to.clone();
        d.messages_changed(&mut inner);
        to
    };
    if approve {
        deliver_next(d, &to);
    }
    Ok(json!({}))
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
