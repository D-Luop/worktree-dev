//! Repo and worktree operations inside a distro. Each is one POSIX `sh` script run through a single
//! `wsl.exe --exec sh -c …` (one round trip instead of one per git call); paths and names travel as
//! positional args, never spliced into the script.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{path, Entry, Layout};

/// Where a script runs. `Local` (tests) runs it with this machine's `sh`.
pub enum Sh {
    Wsl(String),
    #[cfg(test)]
    Local,
}

impl Sh {
    fn command(&self, script: &str, args: &[&str]) -> Command {
        match self {
            Sh::Wsl(d) => {
                let mut c = Command::new(super::wsl_exe());
                c.args(super::sh_args(d, None, script, args)).env_remove("WSLENV");
                crate::win::no_window(&mut c);
                c
            }
            #[cfg(test)]
            Sh::Local => {
                let mut c = Command::new("sh");
                // Git Bash would rewrite path-like args (`#!/bin/sh: …`); WSL passes them verbatim
                c.args(["-c", script, "sh"]).args(args).env("MSYS2_ARG_CONV_EXCL", "*").env("MSYS_NO_PATHCONV", "1");
                c
            }
        }
    }

    /// Trimmed stdout, or an error carrying stderr.
    pub fn out(&self, script: &str, args: &[&str]) -> Result<String> {
        let o = self.command(script, args).stdin(Stdio::null()).output().context("running wsl.exe")?;
        if !o.status.success() {
            bail!("{}", super::decode(&o.stderr).trim());
        }
        Ok(super::decode(&o.stdout).trim_end().to_string())
    }

    /// Progress on this terminal (fetches, worktree add).
    pub fn visible(&self, script: &str, args: &[&str]) -> Result<()> {
        let o = self.command(script, args).stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::piped()).output().context("running wsl.exe")?;
        let err = super::decode(&o.stderr);
        if !o.status.success() {
            bail!("{}", err.trim());
        }
        eprint!("{err}");
        Ok(())
    }
}

/// args: bare, url, exclude entries (space-separated), commit-msg hook body, pre-push hook body
pub const ENSURE_REPO: &str = r#"set -e
bare=$1 url=$2
if [ ! -d "$bare" ]; then
  mkdir -p "$bare"
  git init --bare --quiet "$bare"
  git -C "$bare" remote add origin "$url"
  git -C "$bare" config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
  echo "cloning $url into $bare ..."
  if ! git -C "$bare" fetch --prune --progress origin; then
    rm -rf "$bare"; echo "clone of $url failed (can git inside the distro reach and authenticate to it?)" >&2; exit 2
  fi
  git -C "$bare" remote set-head origin -a >/dev/null 2>&1 || true
fi
git -C "$bare" config push.autoSetupRemote >/dev/null || git -C "$bare" config push.autoSetupRemote true
mkdir -p "$bare/info" "$bare/hooks"
ex="$bare/info/exclude"; touch "$ex"
[ ! -s "$ex" ] || [ -z "$(tail -c1 "$ex")" ] || echo >> "$ex"
set -f; for l in $3; do grep -qxF -- "$l" "$ex" || printf '%s\n' "$l" >> "$ex"; done; set +f
hook() { f="$bare/hooks/$1"; if [ -f "$f" ] && ! grep -q worktree-dev "$f"; then return 0; fi; printf '%s' "$2" > "$f"; chmod +x "$f"; }
[ -z "$4" ] || hook commit-msg "$4"
[ -z "$5" ] || hook pre-push "$5"
"#;

/// args: bare, worktree, branch, base ("" = the default branch). Prints `exists` or `created`.
pub const CREATE_WORKTREE: &str = r#"set -e
bare=$1 wt=$2 name=$3 from=$4
if [ -e "$wt/.git" ]; then echo exists; exit 0; fi
has() { git -C "$bare" rev-parse --verify --quiet "$1" >/dev/null; }
git -C "$bare" fetch --prune --quiet origin >&2 || true
mkdir -p "$(dirname "$wt")"
if has "refs/heads/$name"; then
  [ -z "$from" ] || echo "note: local branch '$name' already exists; ignoring the base" >&2
  git -C "$bare" worktree add "$wt" "$name" >&2
elif has "refs/remotes/origin/$name"; then
  [ -z "$from" ] || echo "note: origin/$name already exists; ignoring the base" >&2
  git -C "$bare" worktree add --track -b "$name" "$wt" "origin/$name" >&2
else
  if [ -n "$from" ]; then
    if has "refs/remotes/origin/$from"; then base="origin/$from"
    elif has "$from"; then base=$from
    else echo "base ref '$from' not found (tried origin/$from and $from)" >&2; exit 3; fi
  else
    h=$(git -C "$bare" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null || true)
    if [ -n "$h" ]; then base="origin/${h#refs/remotes/origin/}"
    elif has refs/remotes/origin/main; then base=origin/main
    else base=$(git -C "$bare" symbolic-ref --quiet --short HEAD 2>/dev/null || echo main); fi
  fi
  git -C "$bare" worktree add --no-track "$wt" -b "$name" "$base" >&2
fi
echo created
"#;

/// args: bare, worktree, force (1|0), worktrees root (stop pruning there), branch to delete ("" = keep)
pub const REMOVE: &str = r#"set -e
bare=$1 wt=$2
if [ -d "$wt" ]; then
  if [ "$3" = 1 ]; then git -C "$bare" worktree remove --force "$wt"; else git -C "$bare" worktree remove "$wt"; fi
  echo "removed worktree $wt"
  d=$(dirname "$wt"); while [ "$d" != "$4" ] && [ "${d#"$4"/}" != "$d" ] && rmdir "$d" 2>/dev/null; do d=$(dirname "$d"); done
else
  git -C "$bare" worktree prune; echo "worktree not present; pruned stale entries"
fi
if [ -n "$5" ]; then
  if git -C "$bare" rev-parse --verify --quiet "refs/heads/$5" >/dev/null; then git -C "$bare" branch -D "$5" >/dev/null; echo "deleted branch $5"
  else echo "no local branch '$5' to delete"; fi
fi
"#;

/// args: bare, from, to, prune-stop dir (empty parents of `from` are removed up to it)
pub const MOVE: &str = r#"set -e
[ -d "$2" ] || { echo "no worktree at $2" >&2; exit 2; }
[ ! -e "$3" ] || { echo "target already exists: $3" >&2; exit 2; }
mkdir -p "$(dirname "$3")"
git -C "$1" worktree move "$2" "$3"
d=$(dirname "$2"); while [ "$d" != "$4" ] && [ "${d#"$4"/}" != "$d" ] && rmdir "$d" 2>/dev/null; do d=$(dirname "$d"); done
"#;

/// args: bare, ref checkout path, branch
pub const ENSURE_REF: &str = r#"set -e
git -C "$1" fetch --quiet origin 2>/dev/null || true
if git -C "$1" rev-parse --verify --quiet "refs/remotes/origin/$3" >/dev/null; then r="origin/$3"
elif git -C "$1" rev-parse --verify --quiet "refs/heads/$3" >/dev/null; then r=$3
else echo "branch '$3' not found (origin or local)" >&2; exit 2; fi
if [ -d "$2" ]; then git -C "$2" checkout --quiet --detach --force "$r"
else mkdir -p "$(dirname "$2")"; git -C "$1" worktree add --quiet --detach "$2" "$r"; fi
"#;

/// args: bare. origin/HEAD's branch, else the bare's HEAD branch, else main.
pub const DEFAULT_BRANCH: &str = r#"h=$(git -C "$1" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null) && { echo "${h#refs/remotes/origin/}"; exit 0; }
h=$(git -C "$1" symbolic-ref --quiet HEAD 2>/dev/null) && { echo "${h#refs/heads/}"; exit 0; }
echo main
"#;

/// args: worktree. Files git tracks under the preset targets (`custom::apply` never overwrites them).
pub const TRACKED: &str = r#"git -C "$1" ls-files -- .claude .agents AGENTS.override.md 2>/dev/null || true"#;

/// The remote a distro clone should use: the repo's origin (a Windows path becomes its `/mnt` form).
pub fn remote_for(url: &str) -> Result<String> {
    let u = url.trim();
    if u.is_empty() || u == "(local)" {
        bail!("a WSL worktree needs the repo to have a remote (it gets its own clone inside the distro)");
    }
    Ok(path::to_mnt(Path::new(u)).unwrap_or_else(|| u.to_string()))
}

fn hook_bodies(dev: &Path) -> (String, String) {
    let strip = path::to_mnt(&crate::paths::asset_in(dev, "hooks/strip-claude-attribution.sh"));
    let exe = path::to_mnt(&crate::paths::wtd_exe());
    let commit_msg = strip.map(|s| format!("#!/bin/sh\n# worktree-dev: strip AI attribution lines from commit messages\nexec \"{s}\" \"$@\"\n")).unwrap_or_default();
    let pre_push = exe.map(|e| format!("#!/bin/sh\n# worktree-dev: PR guardrails (blocked push branches) — configure in Settings → Guardrails\nexec \"{e}\" guard pre-push \"$@\"\n")).unwrap_or_default();
    (commit_msg, pre_push)
}

/// Bare clone of `remote` at `<root>/repos/<slug>/.bare` (fetch config as `wtd repo add` sets it), plus
/// the per-repo setup `repos::prepare` does on Windows. Idempotent.
pub fn ensure_repo(sh: &Sh, dev: &Path, layout: &Layout, slug: &str, remote: &str) -> Result<()> {
    let (commit_msg, pre_push) = hook_bodies(dev);
    let exclude = crate::repos::WORKTREE_LOCAL.join(" ");
    sh.visible(ENSURE_REPO, &[&layout.bare(slug), remote, &exclude, &commit_msg, &pre_push])
}

/// Check out `branch` at `<root>/worktrees/<slug>/<branch>`: the local branch, else origin's, else a new
/// branch off `base` (or the default branch) that doesn't track it (#54). Returns whether it was created.
pub fn create_worktree(sh: &Sh, layout: &Layout, slug: &str, branch: &str, base: Option<&str>) -> Result<bool> {
    let out = sh.out(CREATE_WORKTREE, &[&layout.bare(slug), &layout.worktree(slug, branch), branch, base.unwrap_or("")])?;
    Ok(out.lines().last() == Some("created"))
}

pub fn remove(sh: &Sh, layout: &Layout, e: &Entry, force: bool, delete_branch: bool) -> Result<Vec<String>> {
    let stop = path::join(&layout.root, "worktrees");
    let out = sh.out(REMOVE, &[&layout.bare(&e.slug), &e.wsl_path, if force { "1" } else { "0" }, &stop, if delete_branch { &e.name } else { "" }])
        .map_err(|err| anyhow::anyhow!("{err:#}\n(uncommitted changes? remove with --force to discard them, or commit/push first)"))?;
    Ok(out.lines().map(String::from).collect())
}

pub fn move_worktree(sh: &Sh, layout: &Layout, slug: &str, from: &str, to: &str) -> Result<()> {
    let stop = path::join(&layout.root, "worktrees");
    sh.out(MOVE, &[&layout.bare(slug), from, to, &stop]).map(|_| ())
}

pub fn ensure_ref(sh: &Sh, layout: &Layout, slug: &str, branch: &str) -> Result<String> {
    let p = layout.reference(slug, branch);
    sh.out(ENSURE_REF, &[&layout.bare(slug), &p, branch])?;
    Ok(p)
}

pub fn default_branch(sh: &Sh, layout: &Layout, slug: &str) -> String {
    sh.out(DEFAULT_BRANCH, &[&layout.bare(slug)]).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "main".into())
}

pub fn tracked(sh: &Sh, wt: &str) -> std::collections::BTreeSet<String> {
    sh.out(TRACKED, &[wt]).map(|o| o.lines().map(String::from).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitx::{self, s};

    fn have_sh() -> bool {
        Command::new("sh").args(["-c", "exit 0"]).status().is_ok_and(|s| s.success())
    }

    fn fwd(p: &Path) -> String {
        s(p).replace('\\', "/")
    }

    #[test]
    fn remote_translation() {
        assert_eq!(remote_for("git@github.com:o/n.git").unwrap(), "git@github.com:o/n.git");
        assert_eq!(remote_for(r"D:\src\repo").unwrap(), "/mnt/d/src/repo");
        assert!(remote_for("(local)").is_err() && remote_for("").is_err());
    }

    // The scripts run unchanged under this machine's sh (Git Bash) against a real origin, so the git
    // logic is covered without a distro; only the wsl.exe wrapping differs.
    #[test]
    fn scripts_clone_create_archive_remove() {
        if !have_sh() {
            eprintln!("skipping: no sh on PATH");
            return;
        }
        let t = std::env::temp_dir().join(format!("wtd-wslrepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let src = t.join("src");
        let g = |args: &[&str]| gitx::run(args).unwrap();
        g(&["init", "-q", "-b", "main", &s(&src)]);
        g(&["-C", &s(&src), "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        g(&["-C", &s(&src), "branch", "feat/remote"]);
        let layout = Layout { root: fwd(&t.join("root")) };
        let sh = Sh::Local;
        let url = fwd(&src);

        let body = "#!/bin/sh\n# worktree-dev: test\nexit 0\n";
        for _ in 0..2 {
            sh.out(ENSURE_REPO, &[&layout.bare("r"), &url, "CLAUDE.md .claude/plans/", body, ""]).unwrap();
        }
        let bare = t.join("root/repos/r/.bare");
        let exclude = std::fs::read_to_string(bare.join("info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == "CLAUDE.md").count(), 1, "{exclude}");
        assert_eq!(gitx::out(&["-C", &s(&bare), "config", "remote.origin.fetch"]).unwrap(), "+refs/heads/*:refs/remotes/origin/*");
        assert_eq!(gitx::out(&["-C", &s(&bare), "config", "push.autoSetupRemote"]).unwrap(), "true");
        let hook = std::fs::read_to_string(bare.join("hooks/commit-msg")).unwrap();
        assert!(hook.contains("worktree-dev"), "{hook:?}");
        assert_eq!(default_branch(&sh, &layout, "r"), "main");

        assert!(create_worktree(&sh, &layout, "r", "feat/new", None).unwrap());
        assert!(!create_worktree(&sh, &layout, "r", "feat/new", None).unwrap(), "second call finds it");
        assert!(gitx::out(&["-C", &s(&bare), "config", "branch.feat/new.merge"]).is_err(), "new branch must not track its base");
        assert!(create_worktree(&sh, &layout, "r", "feat/remote", None).unwrap());
        assert_eq!(gitx::out(&["-C", &s(&bare), "config", "branch.feat/remote.merge"]).unwrap(), "refs/heads/feat/remote");
        assert!(create_worktree(&sh, &layout, "r", "x", Some("nope")).unwrap_err().to_string().contains("not found"));

        let e = Entry { id: "r/feat/new".into(), slug: "r".into(), name: "feat/new".into(), distro: "U".into(), wsl_path: layout.worktree("r", "feat/new"), archived: false };
        move_worktree(&sh, &layout, "r", &e.wsl_path, &layout.archived("r", "feat/new")).unwrap();
        assert!(t.join("root/worktrees/r/archive/feat/new/.git").exists());
        assert!(!t.join("root/worktrees/r/feat/new").exists());
        move_worktree(&sh, &layout, "r", &layout.archived("r", "feat/new"), &e.wsl_path).unwrap();

        let r = ensure_ref(&sh, &layout, "r", "main").unwrap();
        assert!(Path::new(&r).join(".git").exists());

        let log = remove(&sh, &layout, &e, false, true).unwrap();
        assert!(log.iter().any(|l| l.starts_with("removed worktree")) && log.iter().any(|l| l == "deleted branch feat/new"), "{log:?}");
        assert!(!t.join("root/worktrees/r/feat/new").exists());
        assert!(t.join("root/worktrees/r/feat").exists(), "feat/remote's folder stays");
        std::fs::remove_dir_all(&t).ok();
    }
}
