//! Diff rendering for the TUI: inline or side-by-side, with line numbers and the changed span of each
//! paired -/+ line highlighted.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::changes::{common_affixes, FileDiff, Kind, Line as DLine};

const ADD_BG: Color = Color::Rgb(0x1e, 0x3a, 0x29);
const ADD_HI: Color = Color::Rgb(0x2f, 0x6e, 0x45);
const DEL_BG: Color = Color::Rgb(0x48, 0x1f, 0x24);
const DEL_HI: Color = Color::Rgb(0x8c, 0x2f, 0x38);
const GUTTER: Color = Color::DarkGray;

fn clean(s: &str) -> String {
    s.replace('\t', "    ").replace('\r', "")
}

fn num(n: Option<u32>) -> String {
    n.map(|n| format!("{n:>5}")).unwrap_or_else(|| "     ".into())
}

/// `text` as spans: the middle `[pre, len-suf)` on `hi`, the rest on `bg`.
fn split_spans(text: &str, pre: usize, suf: usize, bg: Color, hi: Color) -> Vec<Span<'static>> {
    let (a, rest) = text.split_at(pre.min(text.len()));
    let (b, c) = rest.split_at(rest.len().saturating_sub(suf));
    [(a, bg), (b, hi), (c, bg)].into_iter().filter(|(s, _)| !s.is_empty()).map(|(s, col)| Span::styled(clean(s), Style::default().bg(col))).collect()
}

fn header(f: &FileDiff) -> Line<'static> {
    let status = match f.status {
        'A' => ("added", Color::Green),
        'D' => ("deleted", Color::Red),
        'R' => ("renamed", Color::Cyan),
        '?' => ("untracked", Color::Green),
        _ => ("modified", Color::Yellow),
    };
    let mut v = vec![
        Span::styled(format!(" {} ", f.path), Style::default().add_modifier(Modifier::BOLD).fg(Color::White).bg(Color::Rgb(0x30, 0x34, 0x3c))),
        Span::styled(format!(" {} ", status.0), Style::default().fg(status.1)),
        Span::styled(format!("+{} ", f.adds), Style::default().fg(Color::Green)),
        Span::styled(format!("-{}", f.dels), Style::default().fg(Color::Red)),
    ];
    if let Some(o) = &f.old_path {
        v.push(Span::styled(format!("  ← {o}"), Style::default().fg(GUTTER)));
    }
    if f.binary {
        v.push(Span::styled("  (binary)", Style::default().fg(GUTTER)));
    }
    Line::from(v)
}

/// Pairs of (old, new) for a hunk: context on both sides, a run of deletions zipped with the run of
/// additions that follows it, leftovers against blanks.
fn pairs(lines: &[DLine]) -> Vec<(Option<&DLine>, Option<&DLine>)> {
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        match lines[i].kind {
            Kind::Context => {
                out.push((Some(&lines[i]), Some(&lines[i])));
                i += 1;
            }
            _ => {
                let dels: Vec<&DLine> = lines[i..].iter().take_while(|l| l.kind == Kind::Del).collect();
                i += dels.len();
                let adds: Vec<&DLine> = lines[i..].iter().take_while(|l| l.kind == Kind::Add).collect();
                i += adds.len();
                for k in 0..dels.len().max(adds.len()) {
                    out.push((dels.get(k).copied(), adds.get(k).copied()));
                }
            }
        }
    }
    out
}

/// One row per diff line: `old new │ text` (or two columns when `width` is set for side-by-side).
pub fn render(files: &[FileDiff], side_by_side: Option<u16>) -> Vec<Line<'static>> {
    let mut out = vec![];
    if files.is_empty() {
        out.push(Line::styled("  no changes", Style::default().fg(GUTTER)));
    }
    for f in files {
        out.push(header(f));
        for h in &f.hunks {
            out.push(Line::styled(clean(&h.header), Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)));
            for (old, new) in pairs(&h.lines) {
                let paired = matches!((old, new), (Some(a), Some(b)) if a.kind == Kind::Del && b.kind == Kind::Add);
                let (pre, suf) = if paired { common_affixes(&old.unwrap().text, &new.unwrap().text) } else { (0, 0) };
                let side = |l: Option<&DLine>, is_old: bool| -> Vec<Span<'static>> {
                    let Some(l) = l else { return vec![Span::raw("")] };
                    let (bg, hi, mark) = match l.kind {
                        Kind::Add => (Some(ADD_BG), ADD_HI, "+"),
                        Kind::Del => (Some(DEL_BG), DEL_HI, "-"),
                        Kind::Context => (None, ADD_HI, " "),
                    };
                    let n = if is_old { l.old } else { l.new };
                    let mut v = vec![Span::styled(format!("{} {mark} ", num(n)), Style::default().fg(GUTTER))];
                    match bg {
                        Some(bg) if paired => v.extend(split_spans(&l.text, pre, suf, bg, hi)),
                        Some(bg) => v.push(Span::styled(clean(&l.text), Style::default().bg(bg))),
                        None => v.push(Span::raw(clean(&l.text))),
                    }
                    v
                };
                match side_by_side {
                    Some(w) => {
                        let half = (w / 2) as usize;
                        let mut left = side(old, true);
                        let used: usize = left.iter().map(|s| s.content.chars().count()).sum();
                        if used < half {
                            left.push(Span::raw(" ".repeat(half - used)));
                        } else {
                            left = truncate(left, half);
                        }
                        left.push(Span::styled("│", Style::default().fg(GUTTER)));
                        left.extend(side(new, false));
                        out.push(Line::from(left));
                    }
                    None => {
                        if let Some(o) = old.filter(|l| l.kind == Kind::Del) {
                            out.push(inline_row(o, if paired { Some((pre, suf)) } else { None }));
                        }
                        match new {
                            Some(n) if n.kind == Kind::Add => out.push(inline_row(n, if paired { Some((pre, suf)) } else { None })),
                            Some(n) if n.kind == Kind::Context => out.push(inline_row(n, None)),
                            _ => {}
                        }
                    }
                }
            }
        }
        out.push(Line::raw(""));
    }
    out
}

fn inline_row(l: &DLine, hi: Option<(usize, usize)>) -> Line<'static> {
    let (bg, hc, mark) = match l.kind {
        Kind::Add => (Some(ADD_BG), ADD_HI, "+"),
        Kind::Del => (Some(DEL_BG), DEL_HI, "-"),
        Kind::Context => (None, ADD_HI, " "),
    };
    let mut v = vec![Span::styled(format!("{} {} {mark} ", num(l.old), num(l.new)), Style::default().fg(GUTTER))];
    match (bg, hi) {
        (Some(bg), Some((p, s))) => v.extend(split_spans(&l.text, p, s, bg, hc)),
        (Some(bg), None) => v.push(Span::styled(clean(&l.text), Style::default().bg(bg))),
        _ => v.push(Span::raw(clean(&l.text))),
    }
    Line::from(v)
}

fn truncate(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut left = width;
    let mut out = vec![];
    for s in spans {
        if left == 0 {
            break;
        }
        let n = s.content.chars().count();
        if n <= left {
            left -= n;
            out.push(s);
        } else {
            let t: String = s.content.chars().take(left).collect();
            out.push(Span::styled(t, s.style));
            left = 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::parse_patch;

    const P: &str = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b1\n-b2\n+B1\n c\n";

    #[test]
    fn side_by_side_pairs_runs() {
        let f = parse_patch(P);
        let pr = pairs(&f[0].hunks[0].lines);
        assert_eq!(pr.len(), 4); // a, b1/B1, b2/-, c
        assert!(pr[2].1.is_none());
        let rows = render(&f, Some(60));
        assert_eq!(rows.len(), 1 + 1 + 4 + 1);
    }

    #[test]
    fn inline_shows_each_line_once() {
        let rows = render(&parse_patch(P), None);
        assert_eq!(rows.len(), 1 + 1 + 5 + 1);
    }
}
