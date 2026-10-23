//! Git Bash integration: `~/.local/bin` command shims, the PATH line in `~/.bashrc`, completion loaders.

use std::path::Path;

/// `(command on PATH, wtd subcommand it runs)`
pub const SHIMS: &[(&str, &str)] = &[
    ("agent", "agent"),
    ("add-repo", "repo add"),
    ("ref", "ref"),
    ("review", "review"),
    ("archive", "archive"),
    ("close", "close"),
    ("wt-review", "wt-review"),
    ("ask", "ask"),
    ("account", "account"),
    ("ship", "ship"),
    ("assistant", "assistant"),
    ("preview", "preview"),
    ("tokens", "tokens"),
];

/// Commands bash-completion loads `wtd-completion.bash` for.
pub const COMPLETED: &[&str] = &["agent", "archive", "review", "ask", "account"];

const PATH_MARKER: &str = "# wtd: ~/.local/bin on PATH (worktree-dev commands: agent, assistant, review, …)";
const PATH_LINE: &str = r#"case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac"#;
pub const PROFILE_SOURCE: &str = "# load ~/.bashrc in login shells too\n[ -f ~/.bashrc ] && . ~/.bashrc\n";
const OLD_BLOCK_START: &str = "# >>> agent worktree launcher >>>";
const OLD_BLOCK_END: &str = "# <<< agent worktree launcher <<<";

/// `D:\dev\x y` → `'/d/dev/x y'` (an MSYS path, quoted for bash when needed).
pub fn msys(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let b = s.as_bytes();
    let s = if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        format!("/{}{}", (b[0] as char).to_ascii_lowercase(), &s[2..])
    } else {
        s
    };
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c)) {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn shim(exe: &str, sub: &str) -> String {
    format!("#!/usr/bin/env bash\nw={exe}\n[ -x \"$w\" ] && exec \"$w\" {sub} \"$@\"\necho \"wtd.exe not found at $w: run 'wtd setup --repair' (or reinstall WorkTreeDev)\" >&2; exit 1\n")
}

pub fn wtd_shim(exe: &str) -> String {
    format!("#!/usr/bin/env bash\nexec {exe} \"$@\"\n")
}

pub fn is_our_shim(text: &str) -> bool {
    text.starts_with("#!/usr/bin/env bash") && text.contains("wtd.exe")
}

pub fn completion_loader(script: &str) -> String {
    format!("# worktree-dev completion (written by wtd setup)\n[ -f {script} ] && . {script}\n")
}

/// Remove the bashrc function block old installs wrote (`(text, removed)`).
pub fn strip_obsolete_function(text: &str) -> (String, bool) {
    let (Some(a), Some(b)) = (text.find(OLD_BLOCK_START), text.find(OLD_BLOCK_END)) else { return (text.to_string(), false) };
    if b < a {
        return (text.to_string(), false);
    }
    let end = text[b..].find('\n').map_or(text.len(), |i| b + i + 1);
    (format!("{}{}", &text[..a], &text[end..]), true)
}

/// The PATH block appended, or `None` if it's already there.
pub fn add_path_block(text: &str) -> Option<String> {
    if text.contains("wtd: ~/.local/bin on PATH") {
        return None;
    }
    let mut t = text.to_string();
    if !t.is_empty() && !t.ends_with('\n') {
        t.push('\n');
    }
    Some(format!("{t}{PATH_MARKER}\n{PATH_LINE}\n"))
}

pub fn remove_path_block(text: &str) -> Option<String> {
    if !text.contains("wtd: ~/.local/bin on PATH") {
        return None;
    }
    let mut skip_next = false;
    let kept: Vec<&str> = text
        .split_inclusive('\n')
        .filter(|l| {
            if skip_next && l.trim_end() == PATH_LINE {
                skip_next = false;
                return false;
            }
            skip_next = l.contains("wtd: ~/.local/bin on PATH");
            !skip_next
        })
        .collect();
    Some(kept.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msys_paths() {
        assert_eq!(msys(Path::new(r"D:\dev\worktree-dev\.wtd\bin\wtd.exe")), "/d/dev/worktree-dev/.wtd/bin/wtd.exe");
        assert_eq!(msys(Path::new(r"C:\Users\A B\x")), "'/c/Users/A B/x'");
        assert_eq!(msys(Path::new(r"C:\it's")), r"'/c/it'\''s'");
    }

    #[test]
    fn shims_match_install_sh() {
        let s = shim("/d/dev/.wtd/bin/wtd.exe", "repo add");
        assert!(s.starts_with("#!/usr/bin/env bash\nw=/d/dev/.wtd/bin/wtd.exe\n[ -x \"$w\" ] && exec \"$w\" repo add \"$@\"\n"));
        assert!(is_our_shim(&s) && is_our_shim(&wtd_shim("/d/x/wtd.exe")));
        assert!(!is_our_shim("#!/bin/sh\necho hi\n"));
    }

    #[test]
    fn bashrc_round_trip() {
        let orig = "export A=1";
        let added = add_path_block(orig).unwrap();
        assert!(added.starts_with("export A=1\n# wtd:"));
        assert_eq!(add_path_block(&added), None);
        assert_eq!(remove_path_block(&added).unwrap(), "export A=1\n");
        assert_eq!(remove_path_block(orig), None);
        assert_eq!(add_path_block("").unwrap().lines().count(), 2);
    }

    #[test]
    fn strips_old_function() {
        let t = format!("a\n{OLD_BLOCK_START}\nagent() {{ :; }}\n{OLD_BLOCK_END}\nb\n");
        assert_eq!(strip_obsolete_function(&t), ("a\nb\n".to_string(), true));
        assert!(!strip_obsolete_function("a\n").1);
    }
}
