//! `wtd update …`.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use super::{apply, installed_layout, running_version, Channel, LivePolicy, Settings};
use crate::client::Client;
use wtd_core::protocol::method;

const USAGE: &str = "usage: wtd update [--check] [--channel stable|beta] [--apply-now [--yes]]
       wtd update status                  what's available / staged, as JSON
       wtd update config [--auto on|off] [--channel stable|beta] [--live never|ask|auto]";

pub fn main(args: &[String]) -> Result<i32> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["__apply", ver] => apply::helper_main(ver),
        ["status"] => {
            println!("{}", serde_json::to_string_pretty(&status()?)?);
            Ok(0)
        }
        ["config", rest @ ..] => config(rest),
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            Ok(0)
        }
        rest => update(rest),
    }
}

/// The daemon's live view when it's running (it owns checks and downloads), else read from disk.
fn status() -> Result<Value> {
    if let Some(mut c) = Client::connect()? {
        if let Ok(v) = c.request(method::UPDATE_STATUS, json!({})) {
            return Ok(v);
        }
    }
    Ok(super::status_json("idle", None))
}

fn config(args: &[&str]) -> Result<i32> {
    let mut s = Settings::load();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let val = it.next().copied().unwrap_or("");
        match *flag {
            "--auto" => s.auto = matches!(val, "on" | "true" | "1"),
            "--channel" => s.channel = Channel::parse(val).ok_or_else(|| anyhow::anyhow!("channel is stable or beta"))?,
            "--live" => s.live = LivePolicy::parse(val).ok_or_else(|| anyhow::anyhow!("--live is never, ask or auto"))?,
            _ => bail!("{USAGE}"),
        }
    }
    if !args.is_empty() {
        s.save()?;
        // let the daemon (and through it the tray and panel) pick the change up now
        if let Some(mut c) = Client::connect()? {
            let _ = c.request(method::UPDATE_STATUS, json!({}));
        }
    }
    println!("{}", s.to_json());
    Ok(0)
}

fn update(args: &[&str]) -> Result<i32> {
    let (mut check_only, mut apply_now, mut yes) = (false, false, false);
    let mut s = Settings::load();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--check" => check_only = true,
            "--apply-now" => apply_now = true,
            "--yes" | "-y" => yes = true,
            "--channel" => {
                s.channel = it.next().and_then(|c| Channel::parse(c)).ok_or_else(|| anyhow::anyhow!("channel is stable or beta"))?;
                s.save()?;
            }
            _ => bail!("{USAGE}"),
        }
    }
    let Some(l) = installed_layout() else {
        println!("dev build {} (a source checkout): automatic updates are off — update with git + install.sh", running_version());
        return Ok(0);
    };

    if check_only {
        let m = super::check(s.channel)?;
        let cur = l.current_version().unwrap_or_else(|| running_version().into());
        let newer = super::is_newer(&m.version, &cur);
        println!("{}", json!({ "current": cur, "latest": m.version(), "channel": s.channel.as_str(), "update_available": newer, "notes_url": m.notes_url }));
        return Ok(0);
    }

    let ready = if apply_now { l.staged() } else { None };
    let staged = if ready.is_some() { ready } else { match Client::connect()? {
        // one downloader: the daemon
        Some(mut c) => c.request(method::UPDATE_CHECK, json!({ "wait": true }))?["staged"].as_str().map(String::from),
        None => {
            eprint!("checking {} …", s.channel.as_str());
            let r = super::check_and_stage(&l, s.channel, &|d, t| if t > 0 { eprint!("\rdownloading {}%   ", d * 100 / t) });
            eprintln!();
            r?
        }
    } };
    let Some(ver) = staged else {
        println!("up to date ({})", l.current_version().unwrap_or_else(|| running_version().into()));
        return Ok(0);
    };
    if !apply_now {
        println!("{ver} is ready — it's applied when no hosted session is live, or now with `wtd update --apply-now`");
        return Ok(0);
    }
    let hosted = hosted_sessions();
    if hosted > 0 && !yes {
        bail!("{hosted} hosted session(s) are live and would end — rerun with --yes to update now");
    }
    apply::spawn_helper(&l, &ver, true)
}

pub fn hosted_sessions() -> usize {
    let Ok(Some(mut c)) = Client::connect() else { return 0 };
    c.request(method::FLEET_LIST, json!({}))
        .ok()
        .and_then(|v| v.as_array().map(|a| a.iter().filter(|w| w["hosted"].as_bool() == Some(true)).count()))
        .unwrap_or(0)
}
