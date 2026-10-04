//! `wtd review <slug> <name> [-i] [--main] [--base <ref>] [--model <m>] [--deep] [--account <a>] [ref…]`
//! and `wtd wt-review [flags]` (current worktree). The separate pre-push reviewer, ported from
//! review.sh: pass 1 (reviewer, Sonnet) writes review.md + highlights.md; pass 2 (adversarial skeptic,
//! Opus) appends what pass 1 missed. Both run headless, READ-ONLY on the worktree, from a neutral
//! workspace (.wtd/reviews). Reports land in <worktree>/.claude/reviews/<timestamp>/.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use wtd_core::protocol::method;
use wtd_core::status::{apply, Event, Status};

use crate::client::Client;
use crate::gitx::{self, s};
use crate::{paths, statusfile};

/// Local time formatted with a small strftime subset (%Y %m %d %H %M %S).
pub fn local_stamp(fmt: &str) -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    fmt.replace("%Y", &format!("{:04}", t.wYear)).replace("%m", &format!("{:02}", t.wMonth)).replace("%d", &format!("{:02}", t.wDay))
        .replace("%H", &format!("{:02}", t.wHour)).replace("%M", &format!("{:02}", t.wMinute)).replace("%S", &format!("{:02}", t.wSecond))
}

fn git_in(wt: &Path, args: &[&str]) -> String {
    let mut a = vec!["-C", wt.to_str().unwrap_or(".")];
    a.extend_from_slice(args);
    gitx::out(&a).unwrap_or_default()
}

/// Files that are generated (linguist-generated, or a "Code generated … DO NOT EDIT" / "@generated"
/// header) — excluded from the diff the reviewer reads, listed separately.
fn generated(wt: &Path, files: &[String]) -> Vec<String> {
    if files.is_empty() {
        return vec![];
    }
    let mut gen: std::collections::HashSet<String> = Default::default();
    let mut args = vec!["check-attr", "linguist-generated", "--"];
    args.extend(files.iter().map(String::as_str));
    for line in git_in(wt, &args).lines() {
        if let Some((path, val)) = line.rsplit_once(": ") {
            if val.trim() == "true" {
                gen.insert(path.trim_end_matches(": linguist-generated").to_string());
            }
        }
    }
    for f in files {
        if gen.contains(f) {
            continue;
        }
        if let Ok(t) = std::fs::read_to_string(wt.join(f)) {
            let head: String = t.lines().take(5).collect::<Vec<_>>().join("\n");
            if head.contains("@generated") || (head.contains("Code generated") && head.contains("DO NOT EDIT")) {
                gen.insert(f.clone());
            }
        }
    }
    files.iter().filter(|f| gen.contains(*f)).cloned().collect()
}

/// Upsert `## <heading>` sections of `delta` into `ledger` (same-headed sections replaced in place,
/// new ones appended, the rest kept) — the old ledger-merge.awk.
pub fn merge_sections(delta: &str, ledger: &str) -> String {
    fn split(t: &str) -> (String, Vec<(String, String)>) {
        let (mut pre, mut secs) = (String::new(), Vec::<(String, String)>::new());
        for line in t.lines() {
            if line.starts_with("## ") {
                secs.push((line.to_string(), format!("{line}\n")));
            } else if let Some(last) = secs.last_mut() {
                last.1.push_str(line);
                last.1.push('\n');
            } else {
                pre.push_str(line);
                pre.push('\n');
            }
        }
        (pre, secs)
    }
    let (_, dsecs) = split(delta);
    let (pre, lsecs) = split(ledger);
    let mut out = pre;
    let mut used = std::collections::HashSet::new();
    for (h, body) in &lsecs {
        match dsecs.iter().find(|(dh, _)| dh == h) {
            Some((_, d)) => { out.push_str(d); used.insert(h.clone()); }
            None => out.push_str(body),
        }
    }
    for (h, d) in &dsecs {
        if !used.contains(h) {
            out.push_str(d);
            used.insert(h.clone());
        }
    }
    // one blank line before every heading, no runs of blank lines
    let mut clean = String::new();
    let mut blank = 0;
    for line in out.lines() {
        if line.starts_with("## ") && !clean.is_empty() && !clean.ends_with("\n\n") {
            clean.push('\n');
            blank = 1;
        }
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        clean.push_str(line);
        clean.push('\n');
    }
    clean
}

/// One line of activity from a stream-json event (what the reviewer is doing right now).
fn activity(ev: &Value) -> Vec<String> {
    let mut out = vec![];
    if ev.get("type").and_then(Value::as_str) != Some("assistant") {
        return out;
    }
    for c in ev.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
        match c.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let n = c.get("name").and_then(Value::as_str).unwrap_or("");
                let i = c.get("input").cloned().unwrap_or(Value::Null);
                let g = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").replace('\n', " ");
                out.push(match n {
                    "Bash" => format!("$ {}", g("command")),
                    "Read" => format!("read {}", g("file_path")),
                    "Grep" => format!("grep {}{}", g("pattern"), if g("path").is_empty() { String::new() } else { format!(" ({})", g("path")) }),
                    "Glob" => format!("glob {}", g("pattern")),
                    "Edit" | "Write" => format!("{} {}", n.to_lowercase(), g("file_path")),
                    "TodoWrite" => "· updated its checklist".into(),
                    other => other.to_string(),
                });
            }
            Some("text") => {
                let t = c.get("text").and_then(Value::as_str).unwrap_or("").trim().replace('\n', " ");
                if !t.is_empty() {
                    out.push(format!("» {t}"));
                }
            }
            _ => {}
        }
    }
    out
}

struct PassResult {
    text: String,
    usage: [f64; 5], // input, output, cache read, cache write, cost
    limit_hit: bool,
    reset_epoch: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
fn run_pass(label: &str, model: &str, agent: &str, prompt: &str, add_dirs: &[PathBuf], deny: &[String], cwd: &Path, stream_file: &Path) -> Result<PassResult> {
    let (exe, prefix) = crate::run::command_for("claude");
    let mut c = Command::new(exe);
    c.args(&prefix).args(["-p", prompt, "--agent", agent, "--model", model]);
    for d in add_dirs {
        c.arg("--add-dir").arg(d);
    }
    c.args(["--permission-mode", "acceptEdits", "--allowedTools", "Read", "Grep", "Glob", "Bash", "TodoWrite", "Write", "Edit", "--disallowedTools"]);
    c.args(deny);
    c.args(["--output-format", "stream-json", "--verbose"]).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().context("starting claude")?;
    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut s);
        s
    });
    let mut sf = std::fs::File::create(stream_file)?;
    let tty = std::io::stdout().is_terminal();
    let start = std::time::Instant::now();
    let mut result: Option<Value> = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(|l| l.ok()) {
        let _ = writeln!(sf, "{line}");
        let Ok(ev) = serde_json::from_str::<Value>(&line) else { continue };
        if ev.get("type").and_then(Value::as_str) == Some("result") {
            result = Some(ev.clone());
        }
        for a in activity(&ev) {
            let el = start.elapsed().as_secs();
            let shown: String = a.chars().take(150).collect();
            if tty {
                println!("  \x1b[2m{}m{:02}s\x1b[0m  {label} · {shown}", el / 60, el % 60);
            } else {
                println!("  {label} · {shown}");
            }
        }
    }
    let _ = child.wait();
    let stderr = err_thread.join().unwrap_or_default();
    let r = result.unwrap_or(Value::Null);
    let text = r.get("result").and_then(Value::as_str).unwrap_or("").to_string();
    let n = |p: &str| r.pointer(p).and_then(Value::as_f64).unwrap_or(0.0);
    let usage = [n("/usage/input_tokens"), n("/usage/output_tokens"), n("/usage/cache_read_input_tokens"), n("/usage/cache_creation_input_tokens"), n("/total_cost_usd")];
    // usage/rate limits: only the final result + stderr (tool output may quote "rate limit" from the diff)
    let sig = format!("{} {} {}", r.get("subtype").and_then(Value::as_str).unwrap_or(""), text, stderr).to_lowercase();
    let limit_hit = ["usage limit", "limit reached", "rate limit", "rate-limit", "too many requests", " 429"].iter().any(|k| sig.contains(k)) && r.get("is_error").and_then(Value::as_bool).unwrap_or(true);
    let reset_epoch = sig.split('|').skip(1).find_map(|t| t.get(..10).and_then(|d| d.parse::<i64>().ok()));
    Ok(PassResult { text, usage, limit_hit, reset_epoch })
}

pub fn wt_review_main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let r = paths::resolve_worktree(&dev, &std::env::current_dir()?)
        .filter(|r| r.id.contains('/'))
        .context("run wt-review from inside a worktree")?;
    let (slug, name) = r.id.split_once('/').unwrap();
    let mut a = vec![slug.to_string(), name.to_string()];
    a.extend(args.iter().cloned());
    main(&a)
}

pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let home = paths::home_dir()?;
    let wtd = dev.join(".wtd");
    let (mut interactive, mut vs_default) = (false, false);
    let (mut base_override, mut account) = (None::<String>, None::<String>);
    let (mut review_model, mut skeptic_model) = ("sonnet".to_string(), "opus".to_string());
    let mut pos: Vec<String> = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-i" | "--interactive" => { interactive = true; i += 1; }
            "--main" | "--vs-main" | "--full" => { vs_default = true; i += 1; }
            "--deep" => { review_model = "opus".into(); i += 1; }
            "--account" => { account = args.get(i + 1).cloned(); i += 2; }
            "--base" => { base_override = args.get(i + 1).cloned(); i += 2; }
            "--model" => { let m = args.get(i + 1).cloned().context("--model needs a value")?; review_model = m.clone(); skeptic_model = m; i += 2; }
            _ => { pos.push(a.to_string()); i += 1; }
        }
    }
    let (Some(slug), Some(name)) = (pos.first().cloned(), pos.get(1).cloned()) else {
        bail!("usage: review <slug> <name> [-i] [--main] [--base <ref>] [--model <m>] [--deep] [--account <a>] [ref…]");
    };
    let refs = pos[2..].to_vec();
    let bare = gitx::require_repo(&dev, &slug)?;
    let wt = paths::worktree_path(&dev, &format!("{slug}/{name}"));
    if !wt.is_dir() {
        bail!("no worktree at {} (create one with: agent {slug} {name})", wt.display());
    }
    // cost routing: --account > $REVIEW_ACCOUNT > the 'review' role > default ~/.claude
    let roles = crate::settings::read_roles(&home);
    let racc = account.or_else(|| std::env::var("REVIEW_ACCOUNT").ok().filter(|v| !v.is_empty()))
        .or_else(|| roles.get("review").and_then(|r| r.strip_prefix("claude:")).map(String::from))
        .filter(|a| a != "default");
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    if let Some(a) = &racc {
        let d = home.join(".claude-accounts").join(a);
        if !d.is_dir() {
            bail!("no Claude account '{a}'");
        }
        std::env::set_var("CLAUDE_CONFIG_DIR", d);
    }

    // base: --base, else --main → the default branch, else the upstream, else the default branch
    let _ = gitx::ok(&["-C", &s(&bare), "fetch", "--quiet", "origin"]);
    let default_br = gitx::default_branch(&bare);
    // local-only repos (New local repo, no remote yet) diff against their local default branch
    let default_ref = if gitx::has_origin(&bare) { format!("origin/{default_br}") } else { default_br.clone() };
    let base_ref = base_override.unwrap_or_else(|| {
        if vs_default {
            return default_ref.clone();
        }
        let up = git_in(&wt, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]);
        if up.is_empty() { default_ref.clone() } else { up }
    });
    let base = { let b = git_in(&wt, &["merge-base", "HEAD", &base_ref]); if b.is_empty() { base_ref.clone() } else { b } };
    let branch = { let b = git_in(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]); if b.is_empty() { "(detached)".into() } else { b } };

    // per-run dirs: staging under .wtd/reviews/.scratch, final reports inside the worktree
    let mut ts = local_stamp("%Y-%m-%d__%H.%M");
    let revdest = wt.join(".claude").join("reviews");
    let mut k = 2;
    while revdest.join(&ts).exists() {
        ts = format!("{}-{k}", local_stamp("%Y-%m-%d__%H.%M"));
        k += 1;
    }
    let wtout = revdest.join(&ts);
    let revdir = wtd.join("reviews");
    let scratch = revdir.join(".scratch").join(format!("{slug}-{}-{ts}", name.replace('/', "-")));
    let (indir, outdir) = (scratch.join("in"), scratch.join("out"));
    std::fs::create_dir_all(&indir)?;
    std::fs::create_dir_all(&outdir)?;
    std::fs::write(indir.join("meta.txt"), format!(
        "repo_slug:      {slug}\nworktree_name:  {name}\nbranch:         {branch}\nworktree_path:  {}\ndefault_branch: {default_br}\nbase_ref:       {base_ref}\nbase_commit:    {base}\n", wt.display()))?;
    std::fs::write(indir.join("stat.txt"), git_in(&wt, &["diff", "--stat", &base]))?;
    std::fs::write(indir.join("commits.txt"), git_in(&wt, &["log", "--oneline", &format!("{base}..HEAD")]))?;
    let untracked = git_in(&wt, &["ls-files", "--others", "--exclude-standard"]);
    std::fs::write(indir.join("untracked.txt"), &untracked)?;
    let changed: Vec<String> = git_in(&wt, &["diff", "--name-only", &base]).lines().map(String::from).collect();
    let gen = generated(&wt, &changed);
    let mut diff_args = vec!["diff".to_string(), base.clone(), "--".into(), ".".into()];
    diff_args.extend(gen.iter().map(|g| format!(":(exclude){g}")));
    let diff_refs: Vec<&str> = diff_args.iter().map(String::as_str).collect();
    let patch = git_in(&wt, &diff_refs);
    std::fs::write(indir.join("diff.patch"), &patch)?;
    if !gen.is_empty() {
        std::fs::write(indir.join("generated-excluded.txt"), gen.join("\n"))?;
    }
    let copy_in = |src: PathBuf, as_name: &str| -> Option<PathBuf> {
        (src.is_file() && std::fs::metadata(&src).map(|m| m.len() > 0).unwrap_or(false)).then(|| {
            let d = indir.join(as_name);
            std::fs::copy(&src, &d).ok().map(|_| d)
        }).flatten()
    };
    let focus = copy_in(wtd.join("review-focus").join(format!("{slug}.md")), "focus.md");
    let ctx = copy_in(wtd.join("review-context").join(format!("{slug}.md")), "repo-context.md");
    let ledgerfile = wtd.join("review-knowledge").join(format!("{slug}.md"));
    let ledger = copy_in(ledgerfile.clone(), "conventions-ledger.md");
    let lessons_file = revdir.join("reviewer-lessons.md");
    let lessons = copy_in(lessons_file.clone(), "reviewer-lessons.md");
    if patch.trim().is_empty() && untracked.trim().is_empty() {
        println!("nothing to review: no changes in {} vs {base_ref} ({base})", wt.display());
        let _ = crate::settings::remove_tree(&scratch);
        return Ok(0);
    }
    let mut add_dirs = vec![wt.clone()];
    for t in &refs {
        let (rs, rb) = gitx::parse_ref_token(&dev, t);
        match gitx::ensure_ref(&dev, &rs, &rb) {
            Ok(p) => add_dirs.push(p),
            Err(e) => eprintln!("warning: skipping reference '{t}': {e:#}"),
        }
    }
    let wts = s(&wt);
    let deny: Vec<String> = vec![format!("Write({wts}/**)"), format!("Edit({wts}/**)"), format!("Edit({}/**)", s(&dev.join("refs")))];
    let opt = |p: &Option<PathBuf>, f: &dyn Fn(&str) -> String| p.as_ref().map(|p| f(&s(p))).unwrap_or_default();
    let prompt = format!(
"Review the unpushed changes for worktree '{name}' of repo '{slug}' before it is pushed.

{}{}{}Inputs are in:        {}
  - meta.txt, diff.patch, stat.txt, commits.txt, untracked.txt{}{}{}{}
  - diff.patch EXCLUDES generated files to save tokens{}.
Target worktree (READ-ONLY, already granted): {wts}
{}
Do a full review on all dimensions (correctness, performance, security, documentation, repo
standards, efficient & minimal code) per your rubric — one line per finding, severity-tagged, no
praise — cite in-repo precedents (same-way / different-way) with file:line, and write your two reports:
  {}
  {}
{}CONVENTIONS LEDGER: per your rubric, for any repo convention you established (full-repo count) or
confirmed this run, also write {} — one '## <convention>' section each (Rule,
Evidence file:line + sample size, Source, dates). Skip the file if you established/confirmed none.
Then print a short summary with the verdict, finding counts, and the report paths.",
        opt(&ctx, &|p| format!("READ FIRST — repo orientation (generated paths, codegen flow, conventions): {p}\n")),
        opt(&ledger, &|p| format!("CONVENTIONS LEDGER (already-counted standards, trust + spot-check per your rubric): {p}\n")),
        opt(&lessons, &|p| format!("BLIND SPOTS — issues a past adversarial pass caught that first passes MISSED; actively CHECK every one this run: {p}\n")),
        s(&indir),
        if focus.is_some() { ", focus.md" } else { "" }, if ctx.is_some() { ", repo-context.md" } else { "" },
        if ledger.is_some() { ", conventions-ledger.md" } else { "" }, if lessons.is_some() { ", reviewer-lessons.md" } else { "" },
        if gen.is_empty() { String::new() } else { format!(" (listed in generated-excluded.txt). Treat them as regenerated artifacts — note that they changed, do not review line-by-line. If an excluded file looks hand-edited, pull its diff yourself with `git -C '{wts}' diff '{base}' -- <file>`") },
        if add_dirs.len() > 1 { "Read-only reference checkouts for precedent-hunting are also granted (see --add-dir).\n" } else { "" },
        s(&outdir.join("review.md")), s(&outdir.join("highlights.md")),
        if focus.is_some() { "Organize highlights.md under the headings in focus.md and emphasize exactly what it asks for.\n" } else { "" },
        s(&outdir.join("ledger-delta.md")),
    );

    if interactive {
        let (exe, prefix) = crate::run::command_for("claude");
        let mut c = Command::new(exe);
        c.args(&prefix).args(["--agent", "reviewer", "--model", &review_model]);
        for d in &add_dirs {
            c.arg("--add-dir").arg(d);
        }
        c.args(["--permission-mode", "acceptEdits", "--disallowedTools"]).args(&deny).arg(&prompt).current_dir(&revdir);
        println!("interactive reviewer — reports will land in {}", outdir.display());
        return Ok(c.status()?.code().unwrap_or(1));
    }

    println!("reviewing {slug}/{name}  (base: {base_ref} @ {})  models: review={review_model} skeptic={skeptic_model}", base.chars().take(12).collect::<String>());
    println!("  inputs : {}\n  reports: {}\n", indir.display(), wtout.display());
    // the row turns 'reviewing' (purple) unless the worktree is already done
    let wref = paths::WtRef { id: format!("{slug}/{name}"), root: wt.clone() };
    let reviewing = statusfile::read(&wt).status != Status::Done;
    let set_status = |ev: Event, word: &str| {
        let old = statusfile::read(&wt);
        let new = apply(old, &ev);
        if new != old {
            let _ = statusfile::write(&dev, &wref.id, &wt, old, new);
        }
        if let Ok(Some(mut c)) = Client::connect() {
            let _ = c.notify(method::HOOK, wtd_core::protocol::HookParams { dir: s(&wt), event: word.into(), status: new.status, changed: new != old });
        }
    };
    if reviewing {
        set_status(Event::Reviewing, "reviewing");
    }
    struct Done<F: FnMut()>(F);
    impl<F: FnMut()> Drop for Done<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    let _restore = Done(|| if reviewing { set_status(Event::Reviewed, "reviewed") });

    let stream = indir.join("activity.jsonl");
    let mut usage_rows: Vec<(String, String, [f64; 5])> = vec![];
    println!("\x1b[1;36m── pass 1: review ({review_model}) ──\x1b[0m");
    let p1 = run_pass("pass 1", &review_model, "reviewer", &prompt, &add_dirs, &deny, &revdir, &stream)?;
    println!("\n{}\n", p1.text);
    usage_rows.push(("pass 1 · review".into(), review_model.clone(), p1.usage));
    if p1.limit_hit && !outdir.join("review.md").is_file() {
        println!("\x1b[1;33m── review incomplete: the reviewer's account is out of usage ──\x1b[0m");
        let at = p1.reset_epoch.or_else(|| reset_epoch(&home)).map(|e| e + 120);
        schedule_retry(args, &wt, at);
        let _ = crate::settings::remove_tree(&scratch);
        return Ok(75);
    }
    if std::env::var_os("REVIEW_NO_ADVERSARIAL").is_none() && outdir.join("review.md").is_file() {
        let sk = format!(
"A first-pass review of worktree '{name}' (repo '{slug}') is at:  {}  (read it first).
Run inputs (diff, stat, commits, untracked{}{}) are in: {}
{}{}Target worktree (READ-ONLY, granted): {wts}

Adversarially re-check the SAME diff: try to BREAK the logic and find what pass 1 missed or wrongly
blessed — edge/branch combinations in ported logic (enumerate the fall-throughs), per-column
NULL/COALESCE inconsistencies, error/empty/boundary paths, performance and security, and any pass-1
finding called \"intentional\"/\"safe\" that deserves a second look. Then EDIT {}: append
a section '## Adversarial pass' with the NEW findings (file:line + fix) and a '### Corrections' list
for any pass-1 finding you think is wrong. If pass 1 missed nothing material, say so in one line.

For every MATERIAL issue you found that pass 1 MISSED, also append a generalized lesson to
{} — one '## <short blind-spot name>' section each: the CATEGORY of issue to
check next time + a terse pointer to this example (file:line). These feed future first-pass reviewers
so the same class of miss isn't repeated. Skip NITs/trivia; only real, recurring-risk misses. Omit
the file if pass 1 missed nothing material.",
            s(&outdir.join("review.md")), if focus.is_some() { ", focus" } else { "" }, if ctx.is_some() { ", repo-context" } else { "" }, s(&indir),
            opt(&ctx, &|p| format!("Read {p} for repo orientation (generated paths, codegen flow, conventions) before judging.\n")),
            opt(&ledger, &|p| format!("Conventions ledger: {p} — if an entry pass 1 relied on is STALE or wrong, add a corrected \"## <convention>\" section to {}.\n", s(&outdir.join("ledger-delta.md")))),
            s(&outdir.join("review.md")), s(&outdir.join("lessons-delta.md")),
        );
        println!("\x1b[1;35m── pass 2: adversarial ({skeptic_model}) ──\x1b[0m");
        let p2 = run_pass("pass 2", &skeptic_model, "skeptic", &sk, &add_dirs, &deny, &revdir, &stream)?;
        println!("\n{}\n", p2.text);
        usage_rows.push(("pass 2 · adversarial".into(), skeptic_model.clone(), p2.usage));
    }

    let review_md = outdir.join("review.md");
    if review_md.is_file() {
        let mut body = std::fs::read_to_string(&review_md)?;
        let t: [f64; 5] = usage_rows.iter().fold([0.0; 5], |mut a, (_, _, u)| { for k in 0..5 { a[k] += u[k]; } a });
        body.push_str("\n---\n\n## Token usage\n\n| pass | model | input | cache write | cache read | output | est. API cost |\n|---|---|--:|--:|--:|--:|--:|\n");
        for (l, m, u) in &usage_rows {
            body.push_str(&format!("| {l} | {m} | {:.0} | {:.0} | {:.0} | {:.0} | ${:.4} |\n", u[0], u[3], u[2], u[1], u[4]));
        }
        body.push_str(&format!("| **total** | | **{:.0}** | **{:.0}** | **{:.0}** | **{:.0}** | **${:.4}** |\n", t[0], t[3], t[2], t[1], t[4]));
        body.push_str("\n_Est. API cost = pay-as-you-go list price of these tokens (informational). On a Claude subscription this is **not billed** — it counts against your plan's usage limits instead._\n");
        // the worktree agent fills in its Fix / Ignore triage at the top (see the wt-review skill)
        let triage = format!("## Agent triage — {name}\n\n> _Pending — the worktree agent fills this in after reading the review below: a **Fix** list\n> and an **Ignore** list, each finding one line with `**[SEVERITY]** file:line — rationale`._\n\n---\n\n");
        std::fs::write(&review_md, format!("{triage}{body}"))?;
        println!("tokens: {:.0} in (+{:.0} cache write) / {:.0} out   est. API cost: ${:.4} (not billed on a subscription)", t[0], t[3], t[1], t[4]);
    }
    for (delta, target, title) in [
        (outdir.join("ledger-delta.md"), ledgerfile.clone(), format!("# Conventions ledger — {slug}\n\nMemoized full-repo standards counts established by past reviews. Each entry was counted across\nthe WHOLE repo; future reviews trust it (after a quick evidence spot-check) for the area a diff\ntouches, instead of re-counting. Newest evidence wins. Human-prunable — delete stale entries.\n\n")),
        (outdir.join("lessons-delta.md"), lessons_file.clone(), "# Reviewer blind spots\n\nIssues the adversarial (skeptic) pass caught that a first pass MISSED. Read FIRST by the\nreviewer each run and actively checked. Newest detail wins. Human-prunable — delete stale ones.\n\n".to_string()),
    ] {
        if let Ok(d) = std::fs::read_to_string(&delta) {
            if !d.trim().is_empty() {
                std::fs::create_dir_all(target.parent().unwrap())?;
                let cur = std::fs::read_to_string(&target).ok().filter(|c| !c.trim().is_empty()).unwrap_or(title);
                std::fs::write(&target, merge_sections(&d, &cur))?;
                println!("updated {}", target.display());
            }
        }
    }
    std::fs::create_dir_all(&wtout)?;
    for f in ["review.md", "highlights.md", "ledger-delta.md", "lessons-delta.md"] {
        let src = outdir.join(f);
        if src.is_file() {
            let _ = std::fs::rename(&src, wtout.join(f)).or_else(|_| std::fs::copy(&src, wtout.join(f)).map(|_| ()));
        }
    }
    if wtout.join("review.md").is_file() {
        cancel_retry(&wt);
    }
    let _ = crate::settings::remove_tree(&scratch);
    println!("\n\x1b[1;32m── reports ──\x1b[0m\n  review.md     : {}\n  highlights.md : {}", wtout.join("review.md").display(), wtout.join("highlights.md").display());
    // WTD_REVIEW_NO_OPEN=1: leave the reports on disk (scheduled retries, tests) instead of opening them
    if std::env::var_os("WTD_REVIEW_NO_OPEN").is_none() {
        let _ = crate::win::no_window(Command::new("cmd").args(["/c", "code"]).arg(wtout.join("review.md")).arg(wtout.join("highlights.md")))
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    Ok(0)
}

/// The reviewer account's 5-hour window reset (unix seconds), from the live usage endpoint.
fn reset_epoch(home: &Path) -> Option<i64> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or(home.join(".claude"));
    let mut last = std::collections::HashMap::new();
    let accts = crate::daemon::usage_fetch_dir(&dir, &mut last);
    accts.and_then(|a| a.five_hour).map(|l| l.resets_at).filter(|e| *e > 0)
}

/// Queue a one-shot retry of this exact review with the daemon (it runs it after `at`).
fn schedule_retry(args: &[String], wt: &Path, at: Option<i64>) {
    let Some(at) = at else {
        println!("  couldn't determine when the limit resets — re-run the review later.");
        return;
    };
    match Client::connect() {
        Ok(Some(mut c)) => match c.request(method::JOB_SCHEDULE, json!({ "kind": "review", "args": args, "at": at, "key": s(wt) })) {
            Ok(_) => println!("  \x1b[1;33m⏳ reviewer account out of usage\x1b[0m — the daemon will retry at {} (limit reset + 2m).", crate::daemon::fmt_local(at)),
            Err(e) => println!("  couldn't queue a retry: {e:#}"),
        },
        _ => println!("  the daemon isn't running, so no automatic retry — re-run after the limit resets ({}).", crate::daemon::fmt_local(at)),
    }
}

fn cancel_retry(wt: &Path) {
    if let Ok(Some(mut c)) = Client::connect() {
        let _ = c.request(method::JOB_CANCEL, json!({ "key": s(wt), "kind": "review" }));
    }
}

#[cfg(test)]
mod tests {
    use super::merge_sections;

    #[test]
    fn ledger_upsert() {
        let ledger = "# Ledger\n\nintro\n\n## A\nold a\n\n## B\nkeep b\n";
        let delta = "## A\nnew a\n## C\nnew c\n";
        let m = merge_sections(delta, ledger);
        assert!(m.starts_with("# Ledger\n\nintro\n"), "{m}");
        assert!(m.contains("## A\nnew a") && !m.contains("old a"), "{m}");
        assert!(m.contains("## B\nkeep b"), "{m}");
        assert!(m.find("## C").unwrap() > m.find("## B").unwrap(), "{m}");
    }
}
