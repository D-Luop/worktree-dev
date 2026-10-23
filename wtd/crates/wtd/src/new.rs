//! `wtd new [<slug>] [--in terminal|zed]`: a console New Session wizard (repo → branch name), for
//! frontends without the VS Code quick-pick — the tray's terminal/Zed entries, a plain shell, the TUI.

use std::io::{BufRead, Write};

use anyhow::{anyhow, bail, Result};

use crate::frontend::{self, Frontend};
use crate::{agent, paths, repos};

pub fn main(args: &[String]) -> Result<i32> {
    let dev = paths::dev_root()?;
    let f = match args.iter().position(|a| a == "--in") {
        Some(i) => args.get(i + 1).and_then(|s| Frontend::parse(s)).ok_or_else(|| anyhow!("--in takes terminal|zed"))?,
        None => Frontend::Terminal,
    };
    if f == Frontend::VsCode {
        bail!("in VS Code use the roster's + New (or the tray's New session ▸ VS Code)");
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ask = |q: &str| -> Result<String> {
        print!("{q}");
        std::io::stdout().flush()?;
        Ok(lines.next().transpose()?.unwrap_or_default().trim().to_string())
    };

    let slug = match args.first().filter(|a| !a.starts_with("--")) {
        Some(s) => s.clone(),
        None => {
            let all = repos::all(&dev);
            if all.is_empty() {
                bail!("no repos yet — add one with `wtd repo add <slug> <url>` or in Settings → Repositories");
            }
            println!("New session — which repository?");
            for (i, r) in all.iter().enumerate() {
                println!("  {:>2}) {}", i + 1, r.slug);
            }
            loop {
                let a = ask("repo [1]: ")?;
                let pick = if a.is_empty() { Some(0) } else { a.parse::<usize>().ok().and_then(|n| n.checked_sub(1)) };
                match pick.and_then(|i| all.get(i)).or_else(|| all.iter().find(|r| r.slug == a)) {
                    Some(r) => break r.slug.clone(),
                    None => println!("  pick 1-{} or type a repo name", all.len()),
                }
            }
        }
    };
    let name = loop {
        let n = ask("branch (e.g. feat/thing): ")?;
        if n.is_empty() {
            return Ok(1);
        }
        match branch_error(&n) {
            None => break n,
            Some(e) => println!("  {e}"),
        }
    };

    match f {
        // the session lives in this console
        Frontend::Terminal => agent::main(&[slug, name]),
        // Zed can't start it on open: create the worktree, open Zed, point at the task key
        _ => {
            let code = agent::main(&[slug.clone(), name.clone(), "--no-claude".into()])?;
            if code != 0 {
                return Ok(code);
            }
            frontend::open(&dev, &format!("{slug}/{name}"), Frontend::Zed)?;
            println!("Opened in Zed — press {} there to start the session.", frontend::ZED_KEY);
            Ok(0)
        }
    }
}

/// The New Session wizard's branch-name rules.
pub fn branch_error(v: &str) -> Option<&'static str> {
    if v.is_empty() {
        return Some("enter a branch name");
    }
    if !v.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c)) {
        return Some("use letters, digits, . _ - and /");
    }
    if v.starts_with(['/', '.']) || v.ends_with('/') || v.contains("..") || v.contains("//") || v.ends_with(".lock") {
        return Some("not a valid git branch name");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::branch_error;

    #[test]
    fn branch_rules() {
        assert_eq!(branch_error("feat/thing-2"), None);
        assert!(branch_error("feat thing").is_some());
        assert!(branch_error("/feat").is_some());
        assert!(branch_error("feat/").is_some());
        assert!(branch_error("a..b").is_some());
        assert!(branch_error("x.lock").is_some());
    }
}
