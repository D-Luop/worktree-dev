//! Smaller commands ported from bash: `ask`, `preview`, `tokens`, `ship`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::{gitx, paths, statusfile};

// --- ask: grounded per-repo expert ------------------------------------------------------------------

pub fn ask_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let Some(token) = args.first() else {
        println!("usage: ask <slug>[@<branch>] [question]   (no question = interactive expert session)");
        return Ok(1);
    };
    let (slug, branch) = gitx::parse_ref_token(&dev, token);
    eprintln!("preparing a read-only checkout of {slug}@{branch} …");
    let r = gitx::ensure_ref(&dev, &slug, &branch)?;
    let base = format!(
        "You are the expert on the '{slug}' repository, checked out READ-ONLY at: {} (branch {branch}). Answer strictly from that checkout's real code and docs — read them first, cite file:line, verify before you finalize, and never guess. If you can't confirm something, say so.",
        r.display()
    );
    let claude = crate::run::command_for("claude");
    let mut c = Command::new(&claude.0);
    c.args(&claude.1).current_dir(dev.join(".wtd").join("expert"));
    let question = args[1..].join(" ");
    if question.trim().is_empty() {
        c.arg(format!("{base}\n\nAsk me anything about this repo — I'll ground every answer in it."));
    } else {
        c.args(["-p", &format!("{base}\n\nQuestion: {question}"), "--permission-mode", "acceptEdits"]);
    }
    c.args(["--agent", "expert", "--model", "opus", "--add-dir"]).arg(&r).args(["--allowedTools", "Read", "Grep", "Glob", "Bash", "--disallowedTools", "Write(**)", "Edit(**)"]);
    let st = c.status().context("starting claude")?;
    Ok(st.code().unwrap_or(1))
}

// --- preview: stage an HTML mockup for the roster's preview panel ----------------------------------

pub fn preview_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let Some(src) = args.first() else { bail!("usage: preview <file.html> [label]") };
    let src = paths::normalize(src);
    if !src.is_file() {
        bail!("no such file: {}", src.display());
    }
    let label = args.get(1).cloned().unwrap_or_else(|| src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    let label: String = label.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    let label = if label.is_empty() { "preview".to_string() } else { label };
    let cwd = std::env::current_dir()?;
    let r = paths::resolve_worktree(&dev, &cwd).filter(|r| r.id != wtd_core::model::DEV_ID).context("not inside a worktree")?;
    let dest = r.id.split('/').fold(paths::state_dir(&dev).join("previews"), |p, c| p.join(c)).join(format!("{label}.html"));
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::copy(&src, &dest)?;
    println!("preview '{label}' staged for {} → open the panel (🖼) and pick the '{label}' tab", r.id);
    Ok(0)
}

// --- tokens: per-worktree usage + estimated API cost from Claude transcripts -----------------------

/// (input, output, cache-write, cache-read) USD per token, by model family; unknown → opus.
fn price(model: &str) -> (f64, f64, f64, f64) {
    let m = model.to_lowercase();
    if m.contains("haiku") {
        (1e-6, 5e-6, 1.25e-6, 0.1e-6)
    } else if m.contains("sonnet") {
        (3e-6, 15e-6, 3.75e-6, 0.3e-6)
    } else {
        (15e-6, 75e-6, 18.75e-6, 1.5e-6)
    }
}

fn enc(p: &Path) -> String {
    p.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

#[derive(Default, Clone)]
struct Row {
    label: String,
    status: String,
    input: u64,
    cache_w: u64,
    cache_r: u64,
    output: u64,
    msgs: u64,
    cost: f64,
}

fn human(n: u64) -> String {
    let mut f = n as f64;
    for u in ["", "K", "M", "B"] {
        if f.abs() < 1000.0 {
            return if u.is_empty() { format!("{f:.0}") } else { format!("{f:.1}{u}") };
        }
        f /= 1000.0;
    }
    format!("{f:.1}T")
}

pub fn tokens_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let home = paths::home_dir()?;
    let mut filter: Option<String> = None;
    let (mut since, mut sort, mut all) = (None::<i64>, "out".to_string(), false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--since" => { since = args.get(i + 1).and_then(|d| crate::daemon::usage_parse_date(d)); i += 2; }
            "--sort" => { sort = args.get(i + 1).cloned().unwrap_or(sort); i += 2; }
            "--all" => { all = true; i += 1; }
            a => { filter = Some(a.to_string()); i += 1; }
        }
    }
    // worktree project-dir name → (label, current status); plus prefixes to classify historical ones
    let mut known: HashMap<String, (String, String)> = HashMap::new();
    for w in crate::daemon::scan::scan(&dev).into_iter().filter(|w| w.id != wtd_core::model::DEV_ID) {
        let st = statusfile::read(Path::new(&w.path)).status.as_str().to_string();
        known.insert(enc(Path::new(&w.path)), (w.id.clone(), if st.is_empty() { "none".into() } else { st }));
    }
    let prefixes: Vec<(String, String)> = crate::repos::all(&dev).into_iter().map(|r| (format!("{}-worktrees-{}-", enc(&dev), r.slug), r.slug)).collect();
    // every account's transcripts count: usage bills to whichever login a session ran under
    let mut roots: Vec<PathBuf> = vec![home.join(".claude").join("projects")];
    if let Ok(rd) = std::fs::read_dir(home.join(".claude-accounts")) {
        roots.extend(rd.flatten().map(|e| e.path().join("projects")).filter(|p| p.is_dir()));
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for d in rd.flatten() {
            let name = d.file_name().to_string_lossy().to_string();
            let (label, status) = match known.get(&name) {
                Some(k) => k.clone(),
                None if all => match prefixes.iter().find(|(p, _)| name.starts_with(p)) {
                    Some((p, slug)) => (format!("{slug}/{}", &name[p.len()..]), "none".into()),
                    None => continue,
                },
                None => continue,
            };
            if filter.as_ref().is_some_and(|f| !label.contains(f.as_str())) {
                continue;
            }
            let row = rows.entry(label.clone()).or_insert_with(|| Row { label, status, ..Default::default() });
            let Ok(files) = std::fs::read_dir(d.path()) else { continue };
            for f in files.flatten().filter(|f| f.path().extension().is_some_and(|e| e == "jsonl")) {
                let Ok(file) = std::fs::File::open(f.path()) else { continue };
                for line in BufReader::new(file).lines().map_while(|l| l.ok()) {
                    if !line.contains("\"usage\"") {
                        continue;
                    }
                    let Ok(o) = serde_json::from_str::<Value>(&line) else { continue };
                    if let (Some(s), Some(t)) = (since, o.get("timestamp").and_then(Value::as_str)) {
                        if crate::daemon::usage_parse_date(t).is_some_and(|ts| ts < s) {
                            continue;
                        }
                    }
                    let Some(u) = o.pointer("/message/usage").or_else(|| o.get("usage")) else { continue };
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let (inp, out, cw, cr) = (n("input_tokens"), n("output_tokens"), n("cache_creation_input_tokens"), n("cache_read_input_tokens"));
                    let p = price(o.pointer("/message/model").or_else(|| o.get("model")).and_then(Value::as_str).unwrap_or(""));
                    row.input += inp;
                    row.output += out;
                    row.cache_w += cw;
                    row.cache_r += cr;
                    row.cost += inp as f64 * p.0 + out as f64 * p.1 + cw as f64 * p.2 + cr as f64 * p.3;
                    row.msgs += 1;
                }
            }
        }
    }
    let rows: Vec<Row> = rows.into_values().filter(|r| r.msgs > 0).collect();
    let key = |r: &Row| -> f64 {
        match sort.as_str() {
            "in" => r.input as f64,
            "cacheR" => r.cache_r as f64,
            "cacheW" => r.cache_w as f64,
            "total" => (r.input + r.output + r.cache_r + r.cache_w) as f64,
            "cost" => r.cost,
            _ => r.output as f64,
        }
    };
    let w = 38;
    println!("{:<w$}{:>8}{:>9}{:>9}{:>8}{:>7}{:>12}", "repo / worktree", "in", "cacheW", "cacheR", "out", "msgs", "est $");
    println!("{}", "-".repeat(w + 53));
    let groups = [("working", "● working"), ("input", "● input (your turn)"), ("reviewing", "⋯ reviewing"), ("pr", "◆ pr (PR-ready)"), ("done", "✓ done"), ("stopped", "○ stopped"), ("none", "· not running")];
    let mut grand = Row::default();
    for (st, title) in groups {
        let mut g: Vec<&Row> = rows.iter().filter(|r| r.status == st || (st == "none" && !groups.iter().any(|(s, _)| *s == r.status))).collect();
        if g.is_empty() {
            continue;
        }
        g.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap());
        println!("{title}");
        let mut sub = Row::default();
        for r in g {
            println!("  {:<w2$}{:>8}{:>9}{:>9}{:>8}{:>7}{:>12}", r.label.chars().take(w - 2).collect::<String>(), human(r.input), human(r.cache_w), human(r.cache_r), human(r.output), r.msgs, format!("${:.2}", r.cost), w2 = w - 2);
            sub.input += r.input; sub.cache_w += r.cache_w; sub.cache_r += r.cache_r; sub.output += r.output; sub.msgs += r.msgs; sub.cost += r.cost;
        }
        println!("{:<w$}{:>8}{:>9}{:>9}{:>8}{:>7}{:>12}", "  └ subtotal", human(sub.input), human(sub.cache_w), human(sub.cache_r), human(sub.output), sub.msgs, format!("${:.2}", sub.cost));
        println!("{}", "-".repeat(w + 53));
        grand.input += sub.input; grand.cache_w += sub.cache_w; grand.cache_r += sub.cache_r; grand.output += sub.output; grand.msgs += sub.msgs; grand.cost += sub.cost;
    }
    println!("{:<w$}{:>8}{:>9}{:>9}{:>8}{:>7}{:>12}", "TOTAL", human(grand.input), human(grand.cache_w), human(grand.cache_r), human(grand.output), grand.msgs, format!("${:.2}", grand.cost));
    Ok(0)
}

// --- ship: package the engine for a teammate (no worktrees, clones, state or secrets) -------------

fn copy_filtered(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let p = e.path();
        if skip(&p) {
            continue;
        }
        let to = dst.join(e.file_name());
        if p.is_dir() {
            copy_filtered(&p, &to, skip)?;
        } else {
            std::fs::copy(&p, &to)?;
        }
    }
    Ok(())
}

pub fn ship_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let home = paths::home_dir()?;
    let out = args.first().map(|a| paths::normalize(a)).unwrap_or_else(|| home.join(format!("worktree-dev-{}.tgz", crate::review::local_stamp("%Y%m%d"))));
    let stage_root = std::env::temp_dir().join(format!("wtd-ship-{}", std::process::id()));
    let stage = stage_root.join("worktree-dev");
    let _ = crate::settings::remove_tree(&stage_root);
    let newest_vsix = std::fs::read_dir(dev.join(".wtd/templates/vscode-claude-status")).ok()
        .and_then(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".vsix")).max());
    let skip = |p: &Path| -> bool {
        let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let s = p.to_string_lossy().replace('\\', "/");
        s.ends_with("/.wtd/env") || s.ends_with("/.wtd/state") || s.ends_with("/.wtd/bin") || s.ends_with("/wtd/target") || s.contains("/.wtd/reviews/.scratch")
            || n.ends_with(".bak") || n == ".env" || n.ends_with(".env") || n == "config.json" && s.contains("/.wtd/")
            || (n.ends_with(".vsix") && Some(&n) != newest_vsix.as_ref())
    };
    for part in [".wtd", "wtd", ".vscode", "docs"] {
        if dev.join(part).is_dir() {
            copy_filtered(&dev.join(part), &stage.join(part), &skip)?;
        }
    }
    for f in ["Makefile", "README.md", "LICENSE", ".gitignore", ".gitattributes"] {
        if dev.join(f).is_file() {
            std::fs::copy(dev.join(f), stage.join(f))?;
        }
    }
    // never ship credentials
    let mut hits = vec![];
    fn scan(p: &Path, hits: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let q = e.path();
                if q.is_dir() {
                    scan(&q, hits);
                } else if let Ok(t) = std::fs::read_to_string(&q) {
                    let re = ["PASSWORD=", "SECRET=", "_PW=", "API_KEY=", "API-KEY=", "TOKEN="];
                    if re.iter().any(|k| t.contains(k)) && !q.ends_with("tools.rs") && !q.ends_with("ship.sh") {
                        hits.push(q);
                    }
                }
            }
        }
    }
    scan(&stage, &mut hits);
    if !hits.is_empty() {
        let _ = crate::settings::remove_tree(&stage_root);
        bail!("ABORT: possible credentials in the package:\n  {}", hits.iter().map(|h| h.display().to_string()).collect::<Vec<_>>().join("\n  "));
    }
    let st = Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(&stage_root).arg("worktree-dev").stdin(Stdio::null()).status().context("running tar")?;
    let _ = crate::settings::remove_tree(&stage_root);
    if !st.success() {
        bail!("tar failed");
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!("shipped: {} ({:.1} MB)\n\nOn the other machine (Git Bash):\n  mkdir -p ~/dev && tar xzf {} -C ~/dev --strip-components=1\n  ~/dev/.wtd/scripts/install.sh",
        out.display(), size as f64 / 1e6, out.file_name().unwrap().to_string_lossy());
    Ok(0)
}
