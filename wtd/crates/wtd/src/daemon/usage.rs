//! Claude account discovery + live 5h/7d usage (the endpoint the `/usage` panel uses; zero tokens).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use wtd_core::model::{Account, Limit};

struct AccountDir {
    name: String,
    dir: PathBuf,
    json: PathBuf,
}

/// The default login (`~/.claude`) plus every `~/.claude-accounts/<name>`.
fn account_dirs(home: &Path) -> Vec<AccountDir> {
    let mut v = vec![AccountDir { name: "default".into(), dir: home.join(".claude"), json: home.join(".claude.json") }];
    if let Ok(rd) = std::fs::read_dir(home.join(".claude-accounts")) {
        let mut extra: Vec<_> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| {
                let dir = e.path();
                AccountDir { name: e.file_name().to_string_lossy().into(), json: dir.join(".claude.json"), dir }
            })
            .collect();
        extra.sort_by(|a, b| a.name.cmp(&b.name));
        v.extend(extra);
    }
    v
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Fetch every account. `last` keeps the previous good result per account so a failed fetch (429,
/// offline, expired token) shows the last known numbers (with their old `ts`) rather than nothing.
pub fn fetch_all(home: &Path, last: &mut HashMap<String, Account>, now: i64) -> Vec<Account> {
    account_dirs(home)
        .into_iter()
        .map(|a| {
            let email = read_json(&a.json)
                .and_then(|j| j.pointer("/oauthAccount/emailAddress").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            let token = read_json(&a.dir.join(".credentials.json"))
                .and_then(|j| j.pointer("/claudeAiOauth/accessToken").and_then(Value::as_str).map(String::from));
            if let Some(tok) = token {
                if let Some((five, seven)) = fetch(&tok) {
                    let acct = Account { name: a.name.clone(), email, five_hour: Some(five), seven_day: Some(seven), ts: now, nologin: false };
                    last.insert(a.name, acct.clone());
                    return acct;
                }
            }
            if let Some(prev) = last.get(&a.name) {
                return Account { email: if email.is_empty() { prev.email.clone() } else { email }, ..prev.clone() };
            }
            from_statusline_file(&a.dir, &a.name, &email)
                .unwrap_or(Account { name: a.name, nologin: email.is_empty(), email, ..Default::default() })
        })
        .collect()
}

fn fetch(token: &str) -> Option<(Limit, Limit)> {
    let body = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let j: Value = serde_json::from_str(&body).ok()?;
    let lim = |k: &str| Limit {
        used: j.pointer(&format!("/{k}/utilization")).and_then(Value::as_f64).map(f64::round),
        resets_at: j.pointer(&format!("/{k}/resets_at")).and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or(0),
    };
    let (five, seven) = (lim("five_hour"), lim("seven_day"));
    (five.used.is_some() || seven.used.is_some()).then_some((five, seven))
}

/// The statusline's cache (`rate-limits.json`), used only when there's never been a live fetch.
fn from_statusline_file(dir: &Path, name: &str, email: &str) -> Option<Account> {
    let j = read_json(&dir.join("rate-limits.json"))?;
    let lim = |k: &str| -> Option<Limit> {
        let used = j.pointer(&format!("/{k}/used")).and_then(Value::as_f64)?;
        Some(Limit { used: Some(used), resets_at: j.pointer(&format!("/{k}/resets_at")).and_then(Value::as_i64).unwrap_or(0) })
    };
    let (five, seven) = (lim("five_hour"), lim("seven_day"));
    if five.is_none() && seven.is_none() {
        return None;
    }
    let file_email = j.get("email").and_then(Value::as_str).unwrap_or("");
    Some(Account {
        name: name.into(),
        email: if email.is_empty() { file_email.into() } else { email.into() },
        five_hour: five,
        seven_day: seven,
        ts: j.get("ts").and_then(Value::as_i64).unwrap_or(0),
        nologin: false,
    })
}

/// `2026-10-03T21:00:00(.123)?(Z|±HH:MM)` → unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&sign) if sign == b'+' || sign == b'-' => {
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            let o = oh * 3600 + om * 60;
            if sign == b'+' { o } else { -o }
        }
        _ => return None,
    };
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se - offset)
}

#[cfg(test)]
mod tests {
    use super::parse_rfc3339;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-03T21:00:00.123456+00:00"), Some(1791061200));
        assert_eq!(parse_rfc3339("2026-10-03T23:00:00+02:00"), Some(1791061200));
        assert_eq!(parse_rfc3339("nope"), None);
    }
}
