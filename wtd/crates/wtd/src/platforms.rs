//! Where a worktree can run: `windows`, plus each WSL distro with the tools a session needs.
//!
//!   wtd platforms [--refresh]   JSON list (from the daemon's 60s cache when it's up)

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use wtd_core::model::Platform;
use wtd_core::protocol::method;

use crate::win;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Tools {
    pub git: bool,
    pub claude: bool,
    pub codex: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlatformInfo {
    pub id: String,
    /// `windows` | `wsl`
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_user: Option<String>,
    pub tools: Tools,
    /// git and claude are both there: a session can start.
    pub ok: bool,
    pub hints: Vec<String>,
}

/// Text from wsl.exe: its own messages (`-l -q`) are UTF-16LE, a Linux command's output is UTF-8.
pub fn decode_wsl_output(b: &[u8]) -> String {
    let b = b.strip_prefix(&[0xFF, 0xFE]).unwrap_or(b);
    let looks_utf16 = b.len() >= 2 && b.iter().skip(1).step_by(2).filter(|&&c| c == 0).count() * 2 >= b.len() / 2;
    if looks_utf16 {
        let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
    .replace('\0', "")
}

/// Distro names from `wsl.exe -l -q`. Docker Desktop's internal distros can't host a session.
pub fn parse_distros(raw: &[u8]) -> Vec<String> {
    decode_wsl_output(raw)
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}').trim())
        .filter(|l| !l.is_empty() && !l.starts_with("docker-desktop"))
        .map(String::from)
        .collect()
}

/// `user=<name>` then one `command -v` line per tool found.
fn parse_probe(out: &str) -> (Option<String>, Tools) {
    let mut user = None;
    let mut t = Tools::default();
    for l in out.lines().map(str::trim) {
        if let Some(u) = l.strip_prefix("user=") {
            user = Some(u.to_string()).filter(|u| !u.is_empty());
            continue;
        }
        match l.rsplit('/').next().unwrap_or(l) {
            "git" => t.git = true,
            "claude" => t.claude = true,
            "codex" => t.codex = true,
            _ => {}
        }
    }
    (user, t)
}

fn hints(t: &Tools) -> Vec<String> {
    let mut h = vec![];
    if !t.git {
        h.push("git is missing — install it in the distro: sudo apt install -y git".into());
    }
    if !t.claude {
        h.push("claude is missing — install it in the distro: curl -fsSL https://claude.ai/install.sh | bash".into());
    }
    if !t.codex {
        h.push("codex (optional) is missing — npm install -g @openai/codex".into());
    }
    h
}

/// Run to completion, killing it after `timeout` (a distro that hangs on boot mustn't hang us).
fn output_within(mut cmd: Command, timeout: Duration) -> Result<(bool, Vec<u8>)> {
    win::no_window(&mut cmd);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = stdout.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("no answer within {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok((status.success(), reader.join().unwrap_or_default()))
}

fn wsl_exe() -> Command {
    Command::new("wsl.exe")
}

pub fn list_distros() -> Vec<String> {
    let mut c = wsl_exe();
    c.args(["-l", "-q"]);
    match output_within(c, PROBE_TIMEOUT) {
        Ok((true, out)) => parse_distros(&out),
        _ => vec![], // no WSL, or no distros installed (wsl.exe exits non-zero)
    }
}

fn probe_distro(distro: &str) -> PlatformInfo {
    let mut c = wsl_exe();
    c.args(["-d", distro, "--", "sh", "-lc", "echo \"user=$(id -un)\"; command -v git claude codex"]);
    let mut info = PlatformInfo {
        id: Platform::Wsl(distro.to_string()).to_string(),
        kind: "wsl",
        distro: Some(distro.to_string()),
        default_user: None,
        tools: Tools::default(),
        ok: false,
        hints: vec![],
    };
    // `command -v` exits non-zero when any tool is missing, so the exit code says nothing here
    match output_within(c, PROBE_TIMEOUT) {
        Ok((_, out)) => {
            let (user, tools) = parse_probe(&decode_wsl_output(&out));
            if user.is_none() {
                info.hints.push(format!("couldn't run a shell in {distro} — open it once with: wsl -d {distro}"));
            }
            info.hints.extend(hints(&tools));
            info.ok = user.is_some() && tools.git && tools.claude;
            info.default_user = user;
            info.tools = tools;
        }
        Err(e) => info.hints.push(format!("{distro} didn't respond ({e:#}) — check it with: wsl -d {distro}")),
    }
    info
}

fn windows() -> PlatformInfo {
    let tools = Tools {
        git: crate::settings::which("git").is_some(),
        claude: crate::settings::which("claude").is_some(),
        codex: crate::settings::which("codex").is_some(),
    };
    let mut h = vec![];
    if !tools.git {
        h.push("git is missing — install Git for Windows: winget install Git.Git".into());
    }
    if !tools.claude {
        h.push("claude is missing — install it: irm https://claude.ai/install.ps1 | iex".into());
    }
    PlatformInfo { id: "windows".into(), kind: "windows", distro: None, default_user: None, ok: tools.git && tools.claude, tools, hints: h }
}

/// Every platform, probing distros in parallel (each probe may boot its distro).
pub fn detect() -> Vec<PlatformInfo> {
    let probes: Vec<_> = list_distros().into_iter().map(|d| std::thread::spawn(move || probe_distro(&d))).collect();
    let mut out = vec![windows()];
    out.extend(probes.into_iter().filter_map(|h| h.join().ok()));
    out
}

/// The daemon's cache of [`detect`].
#[derive(Default)]
pub struct Cache {
    at: Option<Instant>,
    value: Value,
}

pub const CACHE_TTL: Duration = Duration::from_secs(60);

impl Cache {
    pub fn fresh(&self) -> Option<Value> {
        self.at.filter(|t| t.elapsed() < CACHE_TTL).map(|_| self.value.clone())
    }

    pub fn store(&mut self, v: Value) {
        self.at = Some(Instant::now());
        self.value = v;
    }
}

pub fn main(args: &[String]) -> Result<i32> {
    let refresh = args.iter().any(|a| a == "--refresh");
    if let Some(a) = args.iter().find(|a| *a != "--refresh" && *a != "--json") {
        anyhow::bail!("unknown argument '{a}' (usage: wtd platforms [--refresh])");
    }
    let from_daemon = crate::client::Client::connect().ok().flatten().and_then(|mut c| c.request(method::PLATFORMS_LIST, json!({ "refresh": refresh })).ok());
    let v = match from_daemon {
        Some(v) => v,
        None => serde_json::to_value(detect())?,
    };
    crate::settings::out(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut b = if bom { vec![0xFF, 0xFE] } else { vec![] };
        b.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        b
    }

    #[test]
    fn distros_from_utf16_output() {
        // what `wsl.exe -l -q` really prints: UTF-16LE, CRLF, sometimes a BOM, a trailing NUL
        let fixtures = [
            utf16le("Ubuntu\r\nDebian\r\n", false),
            utf16le("Ubuntu\r\nDebian\r\n", true),
            utf16le("Ubuntu\r\ndocker-desktop\r\ndocker-desktop-data\r\nDebian\r\n\0", false),
            utf16le("\u{feff}Ubuntu\r\n\r\n  Debian  \r\n", false),
        ];
        for f in fixtures {
            assert_eq!(parse_distros(&f), ["Ubuntu", "Debian"]);
        }
        assert_eq!(parse_distros(&utf16le("Ubuntu-24.04\r\n", false)), ["Ubuntu-24.04"]);
        assert!(parse_distros(&[]).is_empty());
    }

    #[test]
    fn utf8_passthrough() {
        assert_eq!(decode_wsl_output(b"user=dev\n/usr/bin/git\n"), "user=dev\n/usr/bin/git\n");
        assert_eq!(decode_wsl_output(&utf16le("hi", false)), "hi");
    }

    #[test]
    fn probe_output() {
        let (u, t) = parse_probe("user=dev\n/usr/bin/git\n/home/dev/.local/bin/claude\n");
        assert_eq!(u.as_deref(), Some("dev"));
        assert_eq!(t, Tools { git: true, claude: true, codex: false });
        assert_eq!(hints(&t).len(), 1);
        let (u, t) = parse_probe("user=\n");
        assert_eq!(u, None);
        assert_eq!(t, Tools::default());
        assert_eq!(hints(&t).len(), 3);
    }

    #[test]
    fn cache_expires() {
        let mut c = Cache::default();
        assert!(c.fresh().is_none());
        c.store(json!([1]));
        assert_eq!(c.fresh(), Some(json!([1])));
        c.at = Some(Instant::now() - CACHE_TTL - Duration::from_secs(1));
        assert!(c.fresh().is_none());
    }
}
