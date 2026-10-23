//! `wtd setup`: install, repair or uninstall WorkTreeDev for this user — everything
//! `.wtd/scripts/install.sh` did, without bash, jq or cargo. Steps keep install.sh's letters.

mod merge;
mod shell;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::{codex, paths, repos, settings, tray, win};

const USAGE: &str = "usage: wtd setup [--dev-root <dir>] [--repair] [--uninstall [--keep-data]] [--from <payload dir>] [--yes] [--skip <step,…>]
  steps: a (shims, PATH, completion)  a2 (binaries)  b (dependency check)  c (Claude status hooks)
         d (VS Code extension)  d2 (VS Code user settings)  e (statusline)  f (attribution)
         g (refs read grant)  i (repo hooks + git perf config)  j (dev-root VS Code settings)
         mcp (MCP server for Claude/Codex)  tray (tray + start at logon)";

const EXTENSION_ID: &str = "local.claude-status";

#[derive(Default, Debug)]
struct Opts {
    dev_root: Option<PathBuf>,
    repair: bool,
    uninstall: bool,
    keep_data: bool,
    from: Option<PathBuf>,
    yes: bool,
    skip: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().with_context(|| format!("{name} needs a value\n{USAGE}"));
        match a.as_str() {
            "--dev-root" => o.dev_root = Some(paths::normalize(&val("--dev-root")?)),
            "--from" => o.from = Some(paths::normalize(&val("--from")?)),
            "--skip" => o.skip.extend(val("--skip")?.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty())),
            "--repair" => o.repair = true,
            "--uninstall" => o.uninstall = true,
            "--keep-data" => o.keep_data = true,
            "--yes" | "-y" => o.yes = true,
            "-h" | "--help" => bail!("{USAGE}"),
            other => bail!("unknown option '{other}'\n{USAGE}"),
        }
    }
    if o.keep_data && !o.uninstall {
        bail!("--keep-data only goes with --uninstall");
    }
    if o.uninstall && (o.repair || o.from.is_some()) {
        bail!("--uninstall can't be combined with --repair or --from");
    }
    Ok(o)
}

/// Where this install's program files live.
#[derive(Debug, Clone)]
struct Layout {
    /// `<dev>/.wtd` in dev mode, the program dir's `current\` otherwise: holds `templates\`, `hooks\`, `scripts\`.
    assets: PathBuf,
    bin: PathBuf,
    dev_mode: bool,
}

impl Layout {
    fn exe(&self) -> PathBuf {
        self.bin.join("wtd.exe")
    }
    fn tray(&self) -> PathBuf {
        self.bin.join("wtd-tray.exe")
    }
    fn template(&self, name: &str) -> PathBuf {
        self.assets.join("templates").join(name)
    }
}

/// `%LOCALAPPDATA%\WorkTreeDev` (stand-in for `paths::program_dir()` from the program/dev-root split).
fn program_dir(home: &Path) -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Local")).join("WorkTreeDev")
}

fn is_payload(dir: &Path) -> bool {
    dir.join("templates").is_dir() && dir.join("bin").join("wtd.exe").is_file()
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (dunce(a), dunce(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

fn dunce(p: &Path) -> Option<String> {
    let c = std::fs::canonicalize(p).ok()?.to_string_lossy().to_string();
    Some(c.strip_prefix(r"\\?\").unwrap_or(&c).trim_end_matches('\\').to_string())
}

struct Setup {
    opts: Opts,
    home: PathBuf,
    dev: PathBuf,
    layout: Layout,
    /// the running exe's folder (where a fresh build's binaries come from in dev mode)
    exe_dir: PathBuf,
    failed: Vec<&'static str>,
}

pub fn main(args: &[String]) -> Result<i32> {
    let opts = parse(args)?;
    let home = paths::home_dir()?;
    let dev = match &opts.dev_root {
        Some(d) => d.clone(),
        None => paths::dev_root().or_else(|_| default_dev_root(&home))?,
    };
    let exe = std::env::current_exe()?;
    let exe_dir = exe.parent().context("exe has no folder")?.to_path_buf();
    let layout = resolve_layout(&opts, &home, &dev, &exe_dir)?;
    let mut s = Setup { opts, home, dev, layout, exe_dir, failed: vec![] };
    if s.opts.uninstall {
        s.uninstall()?;
    } else {
        s.install()?;
    }
    if s.failed.is_empty() {
        println!("==> done");
        Ok(0)
    } else {
        println!("==> done, with errors in step(s): {}", s.failed.join(", "));
        Ok(1)
    }
}

fn default_dev_root(home: &Path) -> Result<PathBuf> {
    Ok(home.join("dev").join("worktree-dev"))
}

fn resolve_layout(opts: &Opts, home: &Path, dev: &Path, exe_dir: &Path) -> Result<Layout> {
    let prog = program_dir(home);
    if let Some(from) = &opts.from {
        if !is_payload(from) {
            bail!("{} isn't a payload (needs templates\\ and bin\\wtd.exe)", from.display());
        }
        let current = if opts.uninstall { prog.join("current") } else { install_payload(from, &prog)? };
        return Ok(Layout { bin: current.join("bin"), assets: current, dev_mode: false });
    }
    // an installed payload's own exe: <current>\bin\wtd.exe
    if let Some(own) = exe_dir.parent().filter(|d| is_payload(d) && d.file_name().is_none_or(|n| n != ".wtd")) {
        return Ok(Layout { bin: own.join("bin"), assets: own.to_path_buf(), dev_mode: false });
    }
    let source = dev.join(".wtd").join("templates").is_dir();
    let installed = is_payload(&prog.join("current"));
    let force_dev = std::env::var("WTD_DEV_SOURCE").is_ok_and(|v| v == "1");
    if source && (force_dev || !installed) {
        let wtd = dev.join(".wtd");
        return Ok(Layout { bin: wtd.join("bin"), assets: wtd, dev_mode: true });
    }
    if installed {
        let current = prog.join("current");
        return Ok(Layout { bin: current.join("bin"), assets: current, dev_mode: false });
    }
    bail!("nothing to install from: {} has no .wtd\\templates and {} isn't installed (use --from <payload>)", dev.display(), prog.display())
}

/// Copy a payload into `versions\<ver>` and make `current` point at it (a junction, or a copy).
fn install_payload(from: &Path, prog: &Path) -> Result<PathBuf> {
    let ver = std::fs::read_to_string(from.join("version.txt")).map(|s| s.trim().to_string()).unwrap_or_default();
    let ver = if ver.is_empty() { "local".to_string() } else { ver };
    let versions = prog.join("versions");
    let target = versions.join(&ver);
    let current = prog.join("current");
    if !same_path(from, &target) && !same_path(from, &current) {
        copy_tree(from, &target)?;
    }
    if !same_path(&current, &target) {
        replace_current(&current, &target)?;
    }
    println!("==> payload {ver} installed to {}", target.display());
    Ok(current)
}

fn replace_current(current: &Path, target: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(current) {
        // a junction: remove the link only; a real folder (copy fallback): move it aside first
        if meta.file_type().is_symlink() || std::fs::remove_dir(current).is_ok() {
            let _ = std::fs::remove_dir(current);
        } else {
            let aside = current.with_file_name(format!("current.old-{}", stamp()));
            std::fs::rename(current, &aside).with_context(|| format!("moving {} aside", current.display()))?;
            let _ = settings::remove_tree(&aside);
        }
    }
    let mut c = Command::new("cmd");
    c.arg("/c").arg("mklink").arg("/J").arg(current).arg(target).stdout(Stdio::null()).stderr(Stdio::null());
    if !win::no_window(&mut c).status().is_ok_and(|s| s.success()) {
        copy_tree(target, current)?;
    }
    Ok(())
}

fn stamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Copy a folder over another. A file Windows won't overwrite (a running exe) is renamed aside first.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let to = dst.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            place_file(&e.path(), &to)?;
        }
    }
    Ok(())
}

fn place_file(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() && std::fs::remove_file(dst).is_err() {
        let aside = PathBuf::from(format!("{}.old-{}", dst.display(), stamp()));
        std::fs::rename(dst, &aside).with_context(|| format!("{} is in use and can't be moved aside", dst.display()))?;
    }
    std::fs::copy(src, dst).with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    Ok(())
}

/// Leftover `*.old-*` copies from earlier swaps, deleted now that nothing runs them.
fn clean_old(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().contains(".old-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `D:\dev\x` → `D:/dev/x`
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// A program on PATH, including `.cmd` launchers (VS Code's `code` is one).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|d| ["exe", "cmd", "bat"].iter().map(|x| d.join(format!("{name}.{x}"))).find(|p| p.is_file()))
}

fn quiet(cmd: &mut Command) -> &mut Command {
    win::no_window(cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()))
}

fn read_text(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

fn write_text(p: &Path, text: &str) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = PathBuf::from(format!("{}.wtd-tmp", p.display()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p).with_context(|| format!("writing {}", p.display()))
}

impl Setup {
    fn skipped(&self, step: &str) -> bool {
        self.opts.skip.iter().any(|s| s == step)
    }

    /// Run one step; a failure is reported and the rest still run.
    fn step(&mut self, id: &'static str, title: &str, f: impl FnOnce(&mut Self) -> Result<()>) {
        if self.skipped(id) {
            println!("==> {}. {title} — skipped (--skip)", id.to_uppercase());
            return;
        }
        self.run_step(id, title, f);
    }

    fn run_step(&mut self, id: &'static str, title: &str, f: impl FnOnce(&mut Self) -> Result<()>) {
        println!("==> {}. {title}", id.to_uppercase());
        if let Err(e) = f(self) {
            println!("    ERROR: {e:#}");
            self.failed.push(id);
        }
    }

    fn claude_settings_path(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }

    fn refs_root(&self) -> PathBuf {
        self.dev.join("refs")
    }

    fn install(&mut self) -> Result<()> {
        println!("==> {} mode: program files {}, dev root {}", if self.layout.dev_mode { "dev" } else { "installed" }, self.layout.assets.display(), self.dev.display());
        std::fs::create_dir_all(paths::state_dir(&self.dev).join("sessions"))?;
        self.pin_dev_root()?;
        self.step("a2", "binaries", Self::binaries);
        self.step("a", "commands in ~/.local/bin, PATH, bash completion", Self::commands);
        self.step("b", "dependency check", |s| {
            s.deps();
            Ok(())
        });
        // C, E, F, G edit the same file: one read, one backup, one write
        if !["c", "e", "f", "g"].iter().all(|s| self.skipped(s)) {
            self.run_step("c", "Claude status hooks, statusline, attribution, refs grant (~/.claude/settings.json)", Self::claude_settings);
        }
        self.step("d", "VS Code claude-status extension", Self::extension);
        self.step("d2", "VS Code terminal tab title + bell prefs", Self::vscode_user);
        self.step("i", "commit-msg / pre-push hooks + git perf config on registered repos", Self::repo_hooks);
        self.step("j", "dev-root VS Code settings", Self::dev_root_vscode);
        self.step("mcp", "wtd MCP server for every Claude account (+ Codex)", Self::mcp);
        self.step("tray", "tray icon + start at logon", Self::tray);
        Ok(())
    }

    // --- A2: binaries ----------------------------------------------------------------------------------

    /// Dev mode: a fresh build (the exe running this) is copied into `<dev>/.wtd/bin`. Live sessions,
    /// MCP servers and the daemon keep the old exe running, so it's renamed aside, not overwritten.
    fn binaries(&mut self) -> Result<()> {
        let bin = self.layout.bin.clone();
        if same_path(&self.exe_dir, &bin) {
            println!("    running from {}: nothing to copy", bin.display());
            clean_old(&bin);
            return Ok(());
        }
        if !self.layout.dev_mode {
            // a payload's binaries were placed with the payload
            clean_old(&bin);
            return Ok(());
        }
        std::fs::create_dir_all(&bin)?;
        if self.layout.exe().is_file() {
            let _ = quiet(Command::new(self.layout.exe()).args(["tray", "--quit"])).status();
        }
        for exe in ["wtd.exe", "wtd-tray.exe"] {
            let src = self.exe_dir.join(exe);
            if src.is_file() {
                place_file(&src, &bin.join(exe))?;
                println!("    installed {}", bin.join(exe).display());
            }
        }
        clean_old(&bin);
        if Command::new(self.layout.exe()).args(["daemon", "status"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
            println!("    the running daemon keeps the previous version (it hosts your sessions) — restart it from the tray when convenient to update");
        }
        Ok(())
    }

    // --- A: shims, PATH, completion ----------------------------------------------------------------------

    fn commands(&mut self) -> Result<()> {
        let bin = self.home.join(".local").join("bin");
        std::fs::create_dir_all(&bin)?;
        let exe = shell::msys(&self.layout.exe());
        for (name, sub) in shell::SHIMS {
            write_text(&bin.join(name), &shell::shim(&exe, sub))?;
        }
        write_text(&bin.join("wtd"), &shell::wtd_shim(&exe))?;
        println!("    shimmed {} commands in {} -> {}", shell::SHIMS.len() + 1, bin.display(), self.layout.exe().display());

        let bashrc = self.home.join(".bashrc");
        let text = read_text(&bashrc).unwrap_or_default();
        let (text, removed) = shell::strip_obsolete_function(&text);
        if removed {
            println!("    removed obsolete ~/.bashrc agent() function");
        }
        match shell::add_path_block(&text) {
            Some(t) => {
                write_text(&bashrc, &t)?;
                println!("    added ~/.local/bin to PATH in ~/.bashrc");
            }
            None if removed => write_text(&bashrc, &text)?,
            None => {}
        }
        let profile = self.home.join(".bash_profile");
        let ptext = read_text(&profile).unwrap_or_default();
        if !ptext.contains("bashrc") {
            write_text(&profile, &(ptext + shell::PROFILE_SOURCE))?;
            println!("    ~/.bash_profile now sources ~/.bashrc");
        }

        let completion = self.layout.assets.join("completions").join("wtd-completion.bash");
        if completion.is_file() {
            let cdir = self.home.join(".local").join("share").join("bash-completion").join("completions");
            for c in shell::COMPLETED {
                write_text(&cdir.join(c), &shell::completion_loader(&shell::msys(&completion)))?;
            }
            println!("    installed bash completion for {} (open a new shell to use)", shell::COMPLETED.join("/"));
        } else {
            println!("    no completions\\wtd-completion.bash in {}: bash completion not installed", self.layout.assets.display());
        }
        Ok(())
    }

    // --- B: dependencies -----------------------------------------------------------------------------------

    fn deps(&mut self) {
        let mut missing = vec![];
        for (name, hint, required) in [
            ("git", "winget install Git.Git", true),
            ("code", "winget install Microsoft.VisualStudioCode", true),
            ("claude", "irm https://claude.ai/install.ps1 | iex   (PowerShell)", true),
            ("jq", "winget install jqlang.jq   (the statusline uses it)", false),
            ("node", "winget install OpenJS.NodeJS.LTS", false),
            ("delta", "winget install dandavison.delta   (nicer diffs)", false),
        ] {
            match (find_on_path(name), required) {
                (Some(_), _) => println!("    ok: {name}"),
                (None, true) => {
                    println!("    MISSING: {name}    {hint}");
                    missing.push(name);
                }
                (None, false) => println!("    optional: {name}    {hint}"),
            }
        }
        if !missing.is_empty() {
            println!("    install the missing ones, then run `wtd setup --repair`");
        }
    }

    // --- C, E, F, G: ~/.claude/settings.json ---------------------------------------------------------------

    fn load_claude_settings(&self) -> Result<Value> {
        let path = self.claude_settings_path();
        let Some(text) = read_text(&path) else { return Ok(serde_json::json!({})) };
        match merge::parse_object(&text) {
            Ok(v) => Ok(v),
            Err(e) => {
                let bak = path.with_extension("json.bak");
                let restored = read_text(&bak).and_then(|t| merge::parse_object(&t).ok());
                match restored {
                    Some(v) if self.opts.repair => {
                        println!("    {} is not valid JSON ({e}); restored from {}", path.display(), bak.display());
                        Ok(v)
                    }
                    _ => bail!("{} is not valid JSON ({e}); fix it, or run `wtd setup --repair` to restore the backup", path.display()),
                }
            }
        }
    }

    fn save_claude_settings(&self, v: &Value) -> Result<()> {
        let path = self.claude_settings_path();
        let text = merge::to_text(v, 2);
        if read_text(&path).as_deref() == Some(text.as_str()) {
            println!("    {} already up to date", path.display());
            return Ok(());
        }
        if path.is_file() {
            std::fs::copy(&path, path.with_extension("json.bak"))?;
            println!("    backed up to {}", path.with_extension("json.bak").display());
        }
        write_text(&path, &text)?;
        println!("    wrote {}", path.display());
        Ok(())
    }

    fn claude_settings(&mut self) -> Result<()> {
        let mut v = self.load_claude_settings()?;
        let template = std::fs::read_to_string(self.layout.template("claude-hooks.json")).context("reading templates\\claude-hooks.json")?;
        if !self.skipped("c") {
            let hooks_dir = self.layout.assets.join("hooks");
            let rendered = merge::render_hooks(&template, &fwd(&self.dev), &fwd(&hooks_dir), &self.layout.exe().to_string_lossy())?;
            merge::merge_hooks(&mut v, &rendered);
            println!("    status hooks → {} hook <event> (other hooks preserved)", fwd(&self.layout.exe()));
        }
        if !self.skipped("e") {
            let line = self.home.join(".claude").join("statusline.sh");
            std::fs::create_dir_all(line.parent().unwrap())?;
            std::fs::copy(self.layout.template("claude-pace.sh"), &line).context("installing the claude-pace statusline")?;
            merge::set_statusline(&mut v);
            println!("    E. statusLine → {}", line.display());
        }
        if !self.skipped("f") {
            merge::set_attribution(&mut v);
            println!("    F. attribution.commit = \"\", attribution.pr = \"\"");
        }
        if !self.skipped("g") {
            std::fs::create_dir_all(self.refs_root())?;
            merge::grant_refs(&mut v, &fwd(&self.refs_root()));
            println!("    G. read access to {} (edits denied)", fwd(&self.refs_root()));
        }
        self.save_claude_settings(&v)
    }

    // --- D: VS Code extension ------------------------------------------------------------------------------

    fn pin_dev_root(&self) -> Result<()> {
        let pin = self.home.join(".config").join("wtd").join("dev-root");
        let want = format!("{}\n", self.dev.display());
        if read_text(&pin).as_deref() != Some(want.as_str()) {
            write_text(&pin, &want)?;
        }
        println!("    pinned dev root -> {}  ({})", self.dev.display(), pin.display());
        Ok(())
    }

    /// The vsix to install and its version: built from source in dev mode (falling back to a prebuilt
    /// one), the payload's `claude-status.vsix` otherwise.
    fn vsix(&self) -> Option<(PathBuf, String)> {
        if !self.layout.dev_mode {
            let v = self.layout.assets.join("claude-status.vsix");
            let ver = read_text(&self.layout.assets.join("version.txt")).unwrap_or_default().trim().to_string();
            return v.is_file().then_some((v, ver));
        }
        let ext = self.layout.template("vscode-claude-status");
        match python() {
            Some(py) if Command::new(&py[0]).args(&py[1..]).arg(ext.join("build-vsix.py")).stdin(Stdio::null()).status().is_ok_and(|s| s.success()) => {
                println!("    built from source")
            }
            py => println!("    WARN: could not build the vsix ({}); using a prebuilt one if present", if py.is_none() { "no python 3 found" } else { "build failed" }),
        }
        newest_vsix(&ext)
    }

    fn extension(&mut self) -> Result<()> {
        let Some((vsix, ver)) = self.vsix() else {
            println!("    SKIPPED: no vsix{}", if self.layout.dev_mode { " — install python 3 and re-run" } else { " in the payload" });
            return Ok(());
        };
        let Some(code) = find_on_path("code") else {
            println!("    SKIPPED: 'code' not on PATH");
            return Ok(());
        };
        if !self.opts.repair && !ver.is_empty() && installed_extension_version(&code).as_deref() == Some(ver.as_str()) {
            println!("    {EXTENSION_ID} {ver} already installed");
            return Ok(());
        }
        if quiet(Command::new(&code).arg("--install-extension").arg(&vsix).arg("--force")).status().is_ok_and(|s| s.success()) {
            println!("    installed {} — reload VS Code windows to use it", vsix.display());
        } else {
            println!("    WARN: 'code' could not reach a live VS Code. Run from a VS Code terminal, or:");
            println!("          code --install-extension \"{}\" --force", vsix.display());
        }
        Ok(())
    }

    // --- D2, J: VS Code settings -----------------------------------------------------------------------------

    fn merge_file(path: &Path, indent: usize, f: impl FnOnce(&mut Value)) -> Result<bool> {
        let text = read_text(path).unwrap_or_default();
        let mut v = match merge::parse_object(&text) {
            Ok(v) => v,
            Err(_) => {
                println!("    SKIPPED: {} is not plain JSON (comments?) — add the settings by hand", path.display());
                return Ok(false);
            }
        };
        f(&mut v);
        let out = merge::to_text(&v, indent);
        if out != text {
            write_text(path, &out)?;
        }
        Ok(true)
    }

    fn vscode_user(&mut self) -> Result<()> {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| self.home.join("AppData").join("Roaming"));
        let path = appdata.join("Code").join("User").join("settings.json");
        if Self::merge_file(&path, 2, merge::vscode_user)? {
            println!("    set tab title=${{sequence}} + visual terminal bell (sound off) in {} (reload to apply)", path.display());
        }
        Ok(())
    }

    fn dev_root_vscode(&mut self) -> Result<()> {
        let path = self.dev.join(".vscode").join("settings.json");
        if Self::merge_file(&path, 4, merge::dev_root_vscode)? {
            println!("    git autodetect → open editors only; worktrees/repos/refs out of the file watcher; search trimmed");
        }
        Ok(())
    }

    // --- I: repo hooks ------------------------------------------------------------------------------------------

    fn registered_repos(&self) -> Vec<repos::Repo> {
        repos::all(&self.dev).into_iter().filter(|r| r.kind == "registered" && r.admin.is_dir()).collect()
    }

    fn repo_hooks(&mut self) -> Result<()> {
        for r in self.registered_repos() {
            repos::prepare(&self.dev, &r).with_context(|| format!("[{}]", r.slug))?;
            let p = r.path.to_string_lossy().to_string();
            for (k, v) in [("core.untrackedCache", "true"), ("feature.manyFiles", "true")] {
                crate::gitx::run(&["-c", "safe.bareRepository=all", "-C", &p, "config", k, v])?;
            }
            println!("    [{}] commit-msg + pre-push hooks, git perf config (untrackedCache, manyFiles)", r.slug);
        }
        Ok(())
    }

    // --- K: MCP + tray ------------------------------------------------------------------------------------------

    /// Every Claude login: `(label, config dir, its .claude.json)`; the default has no config dir.
    fn claude_accounts(&self) -> Vec<(String, Option<PathBuf>, PathBuf)> {
        let mut v = vec![("default".to_string(), None, self.home.join(".claude.json"))];
        for e in std::fs::read_dir(self.home.join(".claude-accounts")).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                v.push((e.file_name().to_string_lossy().to_string(), Some(e.path()), e.path().join(".claude.json")));
            }
        }
        v
    }

    fn codex_homes(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = vec![self.home.join(".codex")];
        v.extend(std::fs::read_dir(self.home.join(".codex-accounts")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        v.into_iter().filter(|p| p.is_dir()).collect()
    }

    fn claude_cmd(claude: &Path, dir: &Option<PathBuf>) -> Command {
        let mut c = Command::new(claude);
        match dir {
            Some(d) => c.env("CLAUDE_CONFIG_DIR", d),
            None => c.env_remove("CLAUDE_CONFIG_DIR"),
        };
        c
    }

    fn mcp(&mut self) -> Result<()> {
        let exe = self.layout.exe();
        let exe_s = exe.to_string_lossy().to_string();
        match find_on_path("claude") {
            None => println!("    SKIPPED Claude: 'claude' not on PATH"),
            Some(claude) => {
                for (label, dir, cfg) in self.claude_accounts() {
                    if !self.opts.repair && mcp_registered(&cfg).as_deref() == Some(exe_s.as_str()) {
                        println!("    [{label}] MCP server 'wtd' already registered");
                        continue;
                    }
                    let _ = quiet(Self::claude_cmd(&claude, &dir).args(["mcp", "remove", "--scope", "user", "wtd"])).status();
                    let ok = quiet(Self::claude_cmd(&claude, &dir).args(["mcp", "add", "--scope", "user", "wtd", "--"]).arg(&exe).arg("mcp")).status().is_ok_and(|s| s.success());
                    println!("    [{label}] {}", if ok { "MCP server 'wtd' registered (fleet_list / fleet_get / fleet_read_file)" } else { "could not register the MCP server" });
                }
            }
        }
        for h in self.codex_homes() {
            codex::ensure_mcp(&h, &exe)?;
            println!("    [codex {}] MCP server 'wtd' in config.toml", h.display());
        }
        Ok(())
    }

    fn tray(&mut self) -> Result<()> {
        tray::refresh_start_at_logon_in(&self.layout.bin);
        println!("    tray starts at logon unless you turned that off (toggle in its ⚙ menu)");
        let mut cmd = if self.layout.tray().is_file() {
            Command::new(self.layout.tray())
        } else {
            let mut c = Command::new(self.layout.exe());
            c.arg("tray");
            c
        };
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        win::spawn_detached(&mut cmd).context("starting the tray")?;
        println!("    tray icon started — the daemon does NOT auto-start: use the tray icon or the fleet panel's Start button");
        Ok(())
    }

    // --- uninstall -------------------------------------------------------------------------------------------------

    fn uninstall(&mut self) -> Result<()> {
        let delete_data = !self.opts.keep_data && self.confirm_delete_data()?;
        let exe = self.layout.exe();
        self.step("tray", "stop the tray + daemon, remove start at logon", |_| {
            let _ = quiet(Command::new(&exe).args(["tray", "--quit"])).status();
            let _ = quiet(Command::new(&exe).args(["daemon", "stop"])).status();
            tray::remove_start_at_logon();
            println!("    Run key removed; tray asked to quit; daemon stop requested (hosted sessions keep it running)");
            Ok(())
        });
        self.step("mcp", "MCP server registrations (Claude accounts + Codex)", Self::unregister_mcp);
        self.step("c", "Claude status hooks, statusline, refs grant", Self::unmerge_claude_settings);
        self.step("d", "VS Code extension", |_| {
            match find_on_path("code") {
                Some(code) => {
                    let ok = quiet(Command::new(code).args(["--uninstall-extension", EXTENSION_ID])).status().is_ok_and(|s| s.success());
                    println!("    {}", if ok { format!("uninstalled {EXTENSION_ID}") } else { format!("{EXTENSION_ID} was not installed (or VS Code unreachable)") });
                }
                None => println!("    SKIPPED: 'code' not on PATH"),
            }
            Ok(())
        });
        self.step("a", "commands, PATH, bash completion", Self::remove_commands);
        self.step("i", "repo hooks", |s| {
            for r in s.registered_repos() {
                for h in ["commit-msg", "pre-push"] {
                    let p = r.admin.join("hooks").join(h);
                    if read_text(&p).is_some_and(|t| t.contains("worktree-dev")) {
                        std::fs::remove_file(&p)?;
                        println!("    [{}] removed {h}", r.slug);
                    }
                }
            }
            Ok(())
        });
        if delete_data {
            self.step("data", "dev root data", Self::delete_data);
        } else {
            println!("==> kept the dev root data in {}", self.dev.display());
        }
        if !self.layout.dev_mode {
            self.step("program", "program files", Self::remove_program);
        }
        println!("==> left in place: VS Code user settings, attribution settings (your preferences)");
        Ok(())
    }

    fn confirm_delete_data(&self) -> Result<bool> {
        if self.opts.yes {
            return Ok(true);
        }
        print!("Also delete the dev root {} (worktrees, repos, refs, state)? [y/N] ", self.dev.display());
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(matches!(line.trim(), "y" | "Y" | "yes"))
    }

    fn unregister_mcp(&mut self) -> Result<()> {
        if let Some(claude) = find_on_path("claude") {
            for (label, dir, _) in self.claude_accounts() {
                let ok = quiet(Self::claude_cmd(&claude, &dir).args(["mcp", "remove", "--scope", "user", "wtd"])).status().is_ok_and(|s| s.success());
                println!("    [{label}] {}", if ok { "removed MCP server 'wtd'" } else { "no MCP server 'wtd'" });
            }
        } else {
            println!("    SKIPPED Claude: 'claude' not on PATH");
        }
        for h in self.codex_homes() {
            codex::remove_mcp(&h)?;
            codex::remove_hooks(&h)?;
            println!("    [codex {}] removed MCP server + status hooks", h.display());
        }
        Ok(())
    }

    fn unmerge_claude_settings(&mut self) -> Result<()> {
        let mut v = self.load_claude_settings()?;
        merge::unmerge_hooks(&mut v);
        merge::unset_statusline(&mut v);
        merge::revoke_refs(&mut v, &fwd(&self.refs_root()));
        self.save_claude_settings(&v)?;
        let line = self.home.join(".claude").join("statusline.sh");
        if read_text(&line).is_some() && read_text(&line) == read_text(&self.layout.template("claude-pace.sh")) {
            std::fs::remove_file(&line)?;
            println!("    removed {}", line.display());
        }
        Ok(())
    }

    fn remove_commands(&mut self) -> Result<()> {
        let bin = self.home.join(".local").join("bin");
        let mut n = 0;
        for name in shell::SHIMS.iter().map(|(n, _)| *n).chain(["wtd"]) {
            let p = bin.join(name);
            if read_text(&p).is_some_and(|t| shell::is_our_shim(&t)) {
                std::fs::remove_file(&p)?;
                n += 1;
            }
        }
        println!("    removed {n} shims from {}", bin.display());
        let cdir = self.home.join(".local").join("share").join("bash-completion").join("completions");
        for c in shell::COMPLETED {
            let p = cdir.join(c);
            if read_text(&p).is_some_and(|t| t.contains("wtd-completion.bash") || t.contains("_wtd_complete")) {
                std::fs::remove_file(&p)?;
            }
        }
        let bashrc = self.home.join(".bashrc");
        if let Some(t) = read_text(&bashrc).and_then(|t| shell::remove_path_block(&t)) {
            write_text(&bashrc, &t)?;
            println!("    removed the ~/.local/bin PATH line from ~/.bashrc");
        }
        Ok(())
    }

    fn delete_data(&mut self) -> Result<()> {
        if self.dev.join(".git").exists() && self.dev.join("wtd").join("Cargo.toml").is_file() {
            println!("    NOT deleted: {} is a source checkout (dev mode) — remove it by hand if you mean it", self.dev.display());
            return Ok(());
        }
        let cfg = self.home.join(".config").join("wtd");
        if cfg.is_dir() {
            settings::remove_tree(&cfg)?;
        }
        if self.dev.is_dir() {
            settings::remove_tree(&self.dev).with_context(|| format!("deleting {} (is a session or editor still using it?)", self.dev.display()))?;
        }
        println!("    deleted {} and {}", self.dev.display(), cfg.display());
        Ok(())
    }

    /// The program dir; if this exe runs from inside it, the rest is deleted once it exits.
    fn remove_program(&mut self) -> Result<()> {
        let prog = program_dir(&self.home);
        if !prog.is_dir() {
            return Ok(());
        }
        if dunce(&self.exe_dir).zip(dunce(&prog)).is_some_and(|(e, p)| e.to_ascii_lowercase().starts_with(&p.to_ascii_lowercase())) {
            let mut c = Command::new("cmd");
            std::os::windows::process::CommandExt::raw_arg(&mut c, format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", prog.display()));
            c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            win::spawn_detached(&mut c)?;
            println!("    {} will be removed once this exits", prog.display());
        } else {
            settings::remove_tree(&prog)?;
            println!("    removed {}", prog.display());
        }
        Ok(())
    }
}

/// `[program, args…]` for a Python ≥ 3.6, if one runs.
fn python() -> Option<Vec<String>> {
    [vec!["python3"], vec!["python"], vec!["py", "-3"]].into_iter().find_map(|cand| {
        let prog = find_on_path(cand[0])?;
        let mut c = Command::new(&prog);
        c.args(&cand[1..]).args(["-c", "import sys; sys.exit(sys.version_info < (3, 6))"]);
        quiet(&mut c).status().ok()?.success().then(|| std::iter::once(prog.to_string_lossy().to_string()).chain(cand[1..].iter().map(|s| s.to_string())).collect())
    })
}

/// The highest-versioned `claude-status-<ver>.vsix` in `dir`.
fn newest_vsix(dir: &Path) -> Option<(PathBuf, String)> {
    let parse = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            let ver = n.strip_prefix("claude-status-")?.strip_suffix(".vsix")?.to_string();
            Some((e.path(), ver))
        })
        .max_by_key(|(_, v)| parse(v))
}

fn installed_extension_version(code: &Path) -> Option<String> {
    let mut c = Command::new(code);
    c.args(["--list-extensions", "--show-versions"]).stdin(Stdio::null()).stderr(Stdio::null());
    let o = win::no_window(&mut c).output().ok()?;
    let prefix = format!("{EXTENSION_ID}@");
    String::from_utf8_lossy(&o.stdout).lines().find_map(|l| l.trim().strip_prefix(&prefix).map(String::from))
}

/// The `wtd` MCP server's command in a Claude config file, when it's registered as `<exe> mcp`.
fn mcp_registered(claude_json: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(&read_text(claude_json)?).ok()?;
    let s = v.pointer("/mcpServers/wtd")?;
    (s.get("args")? == &serde_json::json!(["mcp"])).then(|| s.get("command")?.as_str().map(String::from))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_options() {
        let o = parse(&args("--uninstall --keep-data --yes --skip D,mcp --dev-root /d/x")).unwrap();
        assert!(o.uninstall && o.keep_data && o.yes && !o.repair);
        assert_eq!(o.skip, ["d", "mcp"]);
        assert_eq!(o.dev_root.unwrap(), PathBuf::from(r"D:\x"));
        assert!(parse(&args("--keep-data")).is_err());
        assert!(parse(&args("--uninstall --repair")).is_err());
        assert!(parse(&args("--from")).is_err());
        assert!(parse(&args("--bogus")).is_err());
    }

    #[test]
    fn picks_newest_vsix() {
        let t = std::env::temp_dir().join(format!("wtd-setup-vsix-{}", std::process::id()));
        std::fs::create_dir_all(&t).unwrap();
        for n in ["claude-status-0.0.9.vsix", "claude-status-0.0.110.vsix", "claude-status-0.0.99.vsix", "other.vsix"] {
            std::fs::write(t.join(n), "").unwrap();
        }
        assert_eq!(newest_vsix(&t).unwrap().1, "0.0.110");
        std::fs::remove_dir_all(&t).ok();
    }

    #[test]
    fn reads_mcp_registration() {
        let t = std::env::temp_dir().join(format!("wtd-setup-mcp-{}.json", std::process::id()));
        std::fs::write(&t, r#"{"mcpServers":{"wtd":{"type":"stdio","command":"D:\\x\\wtd.exe","args":["mcp"]}}}"#).unwrap();
        assert_eq!(mcp_registered(&t).as_deref(), Some(r"D:\x\wtd.exe"));
        std::fs::write(&t, r#"{"mcpServers":{"wtd":{"command":"D:\\x\\wtd.exe","args":["other"]}}}"#).unwrap();
        assert_eq!(mcp_registered(&t), None);
        std::fs::remove_file(&t).ok();
    }

    #[test]
    fn payload_install_and_layout() {
        let t = std::env::temp_dir().join(format!("wtd-setup-payload-{}", std::process::id()));
        let payload = t.join("payload");
        std::fs::create_dir_all(payload.join("bin")).unwrap();
        std::fs::create_dir_all(payload.join("templates")).unwrap();
        std::fs::write(payload.join("bin").join("wtd.exe"), "exe").unwrap();
        std::fs::write(payload.join("version.txt"), "1.2.3\n").unwrap();
        let prog = t.join("WorkTreeDev");
        let current = install_payload(&payload, &prog).unwrap();
        assert!(is_payload(&current));
        assert!(prog.join("versions").join("1.2.3").join("bin").join("wtd.exe").is_file());
        // reinstalling over a live exe moves it aside rather than failing
        std::fs::write(payload.join("version.txt"), "1.2.4\n").unwrap();
        let current = install_payload(&payload, &prog).unwrap();
        assert_eq!(read_text(&current.join("version.txt")).unwrap().trim(), "1.2.4");
        std::fs::remove_dir_all(&t).ok();
    }
}
