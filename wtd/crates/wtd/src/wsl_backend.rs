//! The WSL side of a worktree: its clone inside the distro and its hosted session. The boundary
//! `agent --platform wsl:<distro>` calls through; `feat/wsl-backend` implements it (contract:
//! docs/wsl-platform-contract.md). Until then every call fails with [`NOT_BUILT`].

use std::path::Path;

use anyhow::{bail, Result};

pub const NOT_BUILT: &str = "WSL backend not built yet — WSL worktrees arrive with feat/wsl-backend; use --platform windows for now";

pub struct CreateWorktree<'a> {
    pub distro: &'a str,
    pub slug: &'a str,
    /// Branch = worktree name (`feat/x`).
    pub name: &'a str,
    /// The repo's origin URL: the distro gets its own bare clone of it.
    pub url: &'a str,
    pub from: Option<&'a str>,
    /// Markdown to seed into the worktree's CLAUDE.md (Context / scope), if any.
    pub issue: Option<&'a Path>,
}

pub struct Created {
    /// The worktree's path inside the distro (`/home/<user>/dev/worktree-dev/worktrees/<slug>/<name>`).
    pub wsl_path: String,
}

pub struct Host<'a> {
    pub distro: &'a str,
    pub wsl_path: &'a str,
    /// Worktree id (`<slug>/<name>`).
    pub id: &'a str,
    /// `claude` | `codex`
    pub program: &'a str,
    /// Account label inside the distro (`default` = its ~/.claude).
    pub account: &'a str,
    pub permission_mode: &'a str,
    /// First prompt for a new session.
    pub prompt: Option<&'a str>,
    /// Start it in the background instead of attaching this terminal.
    pub detach: bool,
}

/// Clone-if-needed and add the worktree inside the distro.
pub fn create_worktree(_dev: &Path, _req: &CreateWorktree) -> Result<Created> {
    bail!(NOT_BUILT)
}

/// Open-or-attach the worktree's daemon-hosted session (`wsl.exe -d <distro> --cd <wsl_path> -- <program> …`).
/// Returns the process exit code, like `attach::host_main`.
pub fn host(_dev: &Path, _req: &Host) -> Result<i32> {
    bail!(NOT_BUILT)
}
