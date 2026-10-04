# WorkTreeDev on Windows

This is native Windows with no WSL and no tmux. The engine is **`wtd.exe`**, a small Rust daemon
plus CLI. It hosts every agent session and pushes fleet state to the VS Code extension.

## 1. Prerequisites

```powershell
winget install Git.Git                  # Git + Git Bash (Claude Code needs it; so does the installer)
winget install Rustlang.Rustup          # Rust toolchain, to build wtd.exe
winget install Microsoft.VisualStudio.2022.BuildTools   # pick "Desktop development with C++"
winget install jqlang.jq                # the installer merges JSON config with jq
winget install GitHub.cli               # optional: GitHub issues / Projects in New Session
winget install dandavison.delta         # optional
```

You also need:

- **VS Code**, with the `code` command on PATH (in VS Code: *Shell Command: Install 'code' command
  in PATH*).
- **Claude Code**, installed and logged in: run `claude` once.
- **Codex CLI**, optional: `npm i -g @openai/codex`.

For GitHub issue sources, run `gh auth login`. Project boards also need the `read:project` scope
(`gh auth refresh -s read:project`, or `-s project` to let New Session move cards).

## 2. Install

From **Git Bash**:

```bash
git clone https://github.com/D-Luop/worktree-dev /d/dev/worktree-dev   # this folder becomes the dev root
/d/dev/worktree-dev/.wtd/scripts/install.sh
```

The installer is idempotent. Re-run it any time; it doesn't stop a running daemon. It:

- builds `wtd.exe` and `wtd-tray.exe` into `.wtd/bin/`
- writes `~/.local/bin` shims (`agent`, `archive`, `review`, …) that exec `wtd.exe`, and puts
  `~/.local/bin` on PATH in `~/.bashrc`
- points Claude's status hooks at `wtd.exe hook <event>`, merged into `~/.claude/settings.json`
  without touching your other hooks
- registers the `wtd` MCP server (the fleet tools) for every Claude account
- installs the VS Code extension and the control-window settings (`.vscode/settings.json` in the
  dev root)
- starts the tray icon and sets it to start at logon

**The daemon does not start at logon.** Start it yourself (step 3).

Then open the dev root in VS Code (`code /d/dev/worktree-dev`) and run *Developer: Reload Window*.

## 3. Use it

1. **Start the daemon:** use the ▶ in the fleet panel's header, the tray icon (right-click →
   *Start daemon*), or `wtd daemon start`.
2. **Add a repo:** *Settings* (gear in the panel) → *Repositories*, or
   `add-repo <slug> <git-url>`. Optionally link it to GitHub and choose its issue source.
3. **Start a session:** run *WorkTreeDev: New Session* from the Command Palette, or
   `agent <slug> <branch>`.
4. Click a roster row to open or re-attach its session. Use the **Worktree Changes** view for
   diffs.

### Hosted sessions

While the daemon runs, it hosts each agent in a pseudo-console, and VS Code terminals attach to it.
That means:

- Reloading or closing the VS Code window **does not** kill agents. Reopen the row to re-attach.
  Scrollback from before you re-attach isn't replayed; the screen is repainted.
- **Stop daemon** asks before ending live sessions (`wtd daemon stop --force` ends them). Every
  session resumes its conversation when you reopen it.
- With the daemon stopped, `agent` falls back to running the session directly in the terminal.

### The tray icon

Left-click opens the VS Code window on the dev root. Right-click gives:

- Start / Stop daemon
- Open VS Code
- Start tray at logon
- Quit tray (the daemon keeps running)

The icon sits in the taskbar's `^` overflow; drag it onto the taskbar to keep it visible.

## Troubleshooting

- **"`agent`: command not found"**: `~/.local/bin` isn't on PATH in this shell. Open a new Git Bash
  (the installer added it to `~/.bashrc`), or re-run `install.sh`.
- **"wtd.exe is not built yet"**: the shims need `wtd.exe`. Install Rust and the C++ build tools,
  then re-run `install.sh`.
- **Windows blocked `wtd.exe`** (Smart App Control): Windows 11 can block an unsigned local build
  by reputation. Rebuild (`cd wtd && cargo build --release`) and re-run the installer, or turn Smart
  App Control off. Distributing to other machines needs code signing.
- **The panel says the daemon is stopped**: start it from the panel or the tray. If starting fails,
  check `wtd daemon status` and `.wtd/state/daemon.log`. Only one daemon per user can own the pipe.
- **An updated `wtd.exe` isn't picked up**: a running daemon keeps the version it started with,
  because it hosts your sessions. Restart it from the tray when convenient. Old copies
  (`*.exe.old-*`) are cleaned up on the next install.
- **The extension looks stale**: run *Developer: Reload Window*, and check that it's installed with
  `code --list-extensions | grep claude-status`.
- **Logs**: the daemon's log is `.wtd/state/daemon.log`. `wtd ls` prints the fleet as the daemon
  sees it.

## Environment variables

| Variable | Effect |
|---|---|
| `WTD_DEV` | Use this dev root instead of the one `wtd.exe` lives in. |
| `WTD_PIPE` | Use this pipe name instead of `wtd-<user>`, for running a test daemon beside the real one. The extension honours it too. |
| `WTD_REVIEW_NO_OPEN=1` | `review` leaves its reports on disk instead of opening them in VS Code. |
