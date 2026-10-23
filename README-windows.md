# WorkTreeDev on Windows

This is native Windows with no WSL and no tmux. The engine is **`wtd.exe`**, a small Rust daemon
plus CLI. It hosts every agent session and pushes fleet state to the VS Code extension.

## 1. Prerequisites

```powershell
winget install Git.Git                  # Git + Git Bash (Claude Code needs it; so does the installer)
winget install Rustlang.Rustup          # Rust toolchain, to build wtd.exe
winget install Microsoft.VisualStudio.2022.BuildTools   # pick "Desktop development with C++"
winget install jqlang.jq                # the installer merges JSON config with jq
winget install Python.Python.3.12       # builds the VS Code extension's .vsix at install time
winget install OpenJS.NodeJS.LTS        # checks the extension's webview scripts before packaging
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
- builds the VS Code extension from source (`build-vsix.py`) and installs it, plus the control-window settings (`.vscode/settings.json` in the
  dev root)
- starts the tray icon and sets it to start at logon

**The daemon does not start at logon.** Start it yourself (step 3).

Then open the dev root in VS Code (`code /d/dev/worktree-dev`) and run *Developer: Reload Window*.

### Program dir and dev root

An installed build keeps its program files in `%LOCALAPPDATA%\WorkTreeDev\current\` (`bin\wtd.exe`,
`bin\wtd-tray.exe`, `templates\`, `hooks\`, `scripts\`, `claude-status.vsix`, `version.txt`). The dev
root then holds only your data (worktrees, repos, refs, `.wtd\state`, `.wtd\custom`,
`.wtd\config.json`, `.wtd\repos.tsv`) and doesn't have to be a git checkout.

A checkout installed with `install.sh` runs in **dev mode**: binaries in `.wtd\bin`, templates from
`.wtd\templates`. It stays in dev mode until a program dir is installed; set `WTD_DEV_SOURCE=1` to keep
using the checkout even then. `wtd version` shows which one is active (`dev_mode`, `program_dir`).

To move a checkout install to the program dir:

```bash
wtd setup --migrate
```

It copies the checkout's program files into `versions\<ver>\`, points `current\` at it, writes
`~/.config/wtd/program-dir`, and repoints hooks, shims, MCP registrations and repo git hooks from
`.wtd\bin|hooks|scripts` to `current\`. Worktrees, repos and `.wtd\state` aren't touched. Then restart
the daemon and tray from the new `wtd.exe` and reload VS Code.

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

- Your sessions, needs-you first. Each row opens in VS Code, Zed or a terminal (`wtd tui`, with the
  session and its diffs) — the default one is bold; a live row can also be stopped.
- New session ▸ in VS Code (the New Session wizard) / Zed (`wtd new`, then the worktree opens in Zed)
  / Terminal (the TUI's New session form)
- Open sessions in ▸ VS Code / Zed / Terminal: the default (same as `wtd frontend <name>`)
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
- **The extension looks stale**: re-run `install.sh` (it rebuilds the vsix from source), run
  *Developer: Reload Window*, and check the version with
  `code --list-extensions --show-versions | grep claude-status`.
- **Logs**: the daemon's log is `.wtd/state/daemon.log`. `wtd ls` prints the fleet as the daemon
  sees it.

## Releases

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which publishes a GitHub Release with:

| Asset | What it is |
|---|---|
| `WorkTreeDev-Setup-X.Y.Z.exe` | Per-user installer (no admin). Asks for the dev root, offers `winget install` for missing Git / VS Code / Claude Code, lays the payload down at `%LOCALAPPDATA%\WorkTreeDev\versions\X.Y.Z`, points `current` at it, then runs `wtd setup`. |
| `worktree-dev-X.Y.Z-windows-x64.zip` | The payload in `current\` shape (`bin\`, `templates\`, `hooks\`, `scripts\`, `claude-status.vsix`, `version.txt`), used by auto-update. |
| `latest.json` | Updater manifest: version, payload/Setup URLs and sha256. A tag with a prerelease suffix (`v1.2.0-beta.1`) publishes `latest-beta.json` instead and marks the release as a prerelease. |

To cut a release:

```bash
git tag v0.2.0 && git push origin v0.2.0      # or v0.2.0-beta.1 for the beta channel
```

The version comes from the tag only. The workflow stamps it into the crate and the vsix at build time;
don't bump `Cargo.toml` or `package.json` by hand. Re-running the workflow on an existing release
replaces its assets.

`.github/workflows/ci.yml` runs `cargo test` and the webview parse check on every PR.

### Building the release locally

`installer/build-local.ps1` runs the same scripts as the workflow, without signing or upload. It
needs cargo, python, node and Inno Setup 6 (`winget install JRSoftware.InnoSetup`):

```powershell
powershell -File installer\build-local.ps1 -Version 0.2.0    # artifacts in dist\
```

To try the installer without touching your real install, install into a throwaway program dir and
dev root, then uninstall:

```powershell
dist\WorkTreeDev-Setup-0.2.0.exe /VERYSILENT /DIR=$env:TEMP\wtd-prog /DEVROOT=$env:TEMP\wtd-dev /LOG=$env:TEMP\wtd-setup.log
& $env:TEMP\wtd-prog\unins000.exe /VERYSILENT
```

`wtd setup` writes its output to `setup.log` in the program dir. Uninstalling runs
`wtd setup --uninstall --keep-data`, so the dev root stays.

### Code signing

Without signing, SmartScreen warns on Setup and Smart App Control can block `wtd.exe` on other
machines. The workflow signs `wtd.exe`, `wtd-tray.exe` and Setup with
[Azure Trusted Signing](https://learn.microsoft.com/azure/trusted-signing/) when all six repository
secrets below are set. If any is missing, it ships unsigned and logs a warning.

| Secret | Value |
|---|---|
| `AZURE_TENANT_ID` | Tenant of the app registration |
| `AZURE_CLIENT_ID` | App registration (service principal) with the *Trusted Signing Certificate Profile Signer* role on the account |
| `AZURE_CLIENT_SECRET` | A client secret for that app registration |
| `TRUSTED_SIGNING_ENDPOINT` | The account's region endpoint, e.g. `https://wus2.codesigning.azure.net/` |
| `TRUSTED_SIGNING_ACCOUNT` | Trusted Signing account name |
| `TRUSTED_SIGNING_PROFILE` | Certificate profile name |

Set them under *Settings → Secrets and variables → Actions*, or with
`gh secret set AZURE_TENANT_ID` (and so on).

## Environment variables

| Variable | Effect |
|---|---|
| `WTD_DEV` | Use this dev root instead of the one `wtd.exe` lives in. |
| `WTD_PIPE` | Use this pipe name instead of `wtd-<user>`, for running a test daemon beside the real one. The extension honours it too. |
| `WTD_REVIEW_NO_OPEN=1` | `review` leaves its reports on disk instead of opening them in VS Code. |
