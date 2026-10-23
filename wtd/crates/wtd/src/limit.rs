//! The `limited` status's reset time: read from Claude Code's limit message, shown as a short label.

use crate::daemon::usage_parse_date;

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// "You've hit your session limit · resets 10:20pm" (or "… resets Oct 25, 3pm") → unix seconds of the
/// next such local time. `offset` = seconds local is ahead of UTC.
pub fn parse_reset(text: &str, now: i64, offset: i64) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    let rest = lower.split("resets ").nth(1)?;
    let mut words = rest.split_whitespace();
    let first = words.next()?;
    let (date, clock) = match MONTHS.iter().position(|m| first.starts_with(m)) {
        Some(mo) => {
            let day: u32 = words.next()?.trim_end_matches(',').parse().ok()?;
            (Some((mo as u32 + 1, day)), words.next()?)
        }
        None => (None, first),
    };
    let secs = clock_secs(clock)?;
    let local_now = now + offset;
    let at = match date {
        None => {
            let t = local_now - local_now.rem_euclid(86400) + secs;
            if t <= local_now { t + 86400 } else { t }
        }
        Some((mo, day)) => {
            let year = 1970 + local_now.div_euclid(31_556_952);
            (year - 1..=year + 1)
                .filter_map(|y| usage_parse_date(&format!("{y:04}-{mo:02}-{day:02}")))
                .map(|midnight| midnight + secs)
                .find(|t| *t > local_now - 86400)?
        }
    };
    Some(at - offset)
}

/// "10:20pm" | "3pm" | "15:00" → seconds after local midnight.
fn clock_secs(s: &str) -> Option<i64> {
    let s = s.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    let (num, pm) = match (s.strip_suffix("am"), s.strip_suffix("pm")) {
        (Some(n), _) => (n, Some(false)),
        (_, Some(n)) => (n, Some(true)),
        _ => (s, None),
    };
    let (h, m) = num.split_once(':').unwrap_or((num, "0"));
    let (mut h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    if let Some(pm) = pm {
        if !(1..=12).contains(&h) {
            return None;
        }
        h = h % 12 + if pm { 12 } else { 0 };
    }
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 3600 + m * 60)
}

/// "limit · resets 10:20pm" (within a day) / "limit · resets Sun 14:05" / "limit · reset passed" / "limit".
pub fn label(resets_at: i64, now: i64, offset: i64) -> String {
    if resets_at <= 0 {
        return "limit".into();
    }
    if resets_at <= now {
        return "limit · reset passed".into();
    }
    if resets_at - now > 86400 {
        return format!("limit · resets {}", crate::daemon::fmt_local(resets_at));
    }
    let t = (resets_at + offset).rem_euclid(86400);
    let (h, m) = (t / 3600, t % 3600 / 60);
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let ampm = if h < 12 { "am" } else { "pm" };
    if m == 0 { format!("limit · resets {h12}{ampm}") } else { format!("limit · resets {h12}:{m:02}{ampm}") }
}

/// [`label`] for now, in this machine's time zone.
pub fn label_now(resets_at: i64) -> String {
    label(resets_at, crate::daemon::now(), crate::daemon::local_offset())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-23 20:00:00 UTC; local = UTC+1
    const NOW: i64 = 1_792_785_600;
    const OFF: i64 = 3600;

    #[test]
    fn parses_time_of_day() {
        let msg = "You've hit your session limit · resets 10:20pm";
        assert_eq!(parse_reset(msg, NOW, OFF), Some(NOW + 3600 + 20 * 60), "21:00 local now → 22:20 today");
        assert_eq!(parse_reset("resets 3pm", NOW, OFF), Some(NOW + 18 * 3600), "15:00 already passed → tomorrow");
        assert_eq!(parse_reset("Limit reached · resets 12am (Europe/London)", NOW, OFF), Some(NOW + 3 * 3600));
        assert_eq!(parse_reset("resets 21:30", NOW, OFF), Some(NOW + 30 * 60));
    }

    #[test]
    fn parses_dated_reset() {
        assert_eq!(parse_reset("You've hit your weekly limit · resets Oct 25, 3pm", NOW, OFF), Some(NOW + 42 * 3600));
        assert_eq!(parse_reset("resets Jan 2, 9am", NOW, OFF), usage_parse_date("2027-01-02").map(|t| t + 9 * 3600 - OFF));
    }

    #[test]
    fn rejects_junk() {
        for s in ["", "no reset here", "resets soon", "resets 13pm", "resets 25:00"] {
            assert_eq!(parse_reset(s, NOW, OFF), None, "{s}");
        }
    }

    #[test]
    fn labels() {
        assert_eq!(label(0, NOW, OFF), "limit");
        assert_eq!(label(NOW - 1, NOW, OFF), "limit · reset passed");
        assert_eq!(label(NOW + 3600 + 20 * 60, NOW, OFF), "limit · resets 10:20pm");
        assert_eq!(label(NOW + 3 * 3600, NOW, OFF), "limit · resets 12am");
    }
}
