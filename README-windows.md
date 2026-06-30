# WorkTreeDev on Windows (native — no WSL)

WorkTreeDev runs on two session backends:

| Platform | Backend | Each worktree session is… |
|---|---|---|
| Linux / WSL / macOS | **tmux** | one tmux session with the 4-pane layout (claude · command · commit · diff) |
| **native Windows** | **vscode** | one **VSCode integrated terminal** running `claude` (no tmux) |

Everything else is the same: the **Dev workflow summary** panel (account usage + fleet
roster), the colored worktree folders in the Explorer, `agent` / `review` / `ask` / `archive`,
the separate reviewer + skeptic, and per-repo Q&A. On Windows the bash scripts run under **Git
Bash**, and diffs come from VSCode's built-in Source Control / diff views instead of a tmux diff pane.

The backend is auto-detected (`uname` → `windows` ⇒ `vscode`). Override it any time with
`WTD_SESSION_BACKEND=tmux|vscode`.

## 1. Prerequisites

Install these (via `winget`, or `scoop` if you prefer):

```powershell
winget install Git.Git              # provides Git Bash — the shell everything runs in
winget install OpenJS.NodeJS.LTS    # node
winget install jqlang.jq            # jq
winget install dandavison.delta     # git-delta (optional — nicer diffs; falls back to git colors)
```

You also need:

- **VSCode** with the `code` CLI on your PATH (in VSCode: *Shell Command: Install 'code' command in PATH*).
- **Claude Code** installed and logged in (run `claude` once and sign in).

> All worktree-dev commands run from **Git Bash**, not PowerShell or CMD.

## 2. Install

From **Git Bash**:

```bash
# put the engine at its native home (it manages many repos from here)
git clone https://github.com/D-Luop/worktree-dev ~/dev/.wtd-repo
mv ~/dev/.wtd-repo/.wtd ~/dev/.wtd && cp -r ~/dev/.wtd-repo/Makefile ~/dev/.wtd-repo/README.md ~/dev/ 2>/dev/null
# (or just clone wherever and point at .wtd/scripts/install.sh)

~/dev/.wtd/scripts/install.sh        # or:  make -C ~/dev install
```

The installer detects Windows and:

- writes **exec-shims** into `~/.local/bin` (Git Bash symlinks need Developer Mode; shims don't),
- skips tmux and all tmux config,
- installs the `claude-status` VSCode extension,
- writes VSCode terminal prefs to `%APPDATA%\Code\User\settings.json`,
- creates the session registry at `~/dev/.wtd/state/sessions/`.

Make sure `~/.local/bin` is on your PATH (add to `~/.bashrc` if needed):

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

Then **reload the VSCode window** (Developer: Reload Window) so the extension + settings apply.

## 3. Use it

```bash
add-repo app git@github.com:you/your-app.git   # register + bare-clone a repo
agent app feat/login                            # open a worktree + a VSCode terminal running claude
```

- The **Dev workflow summary** panel (Explorer) shows your account usage bars and the fleet roster.
  Click a row to open/focus its terminal; **⏹** ends a session (keeps the worktree); **📦** archives;
  **+ agent** launches a new one.
- Worktree folders are colored by status (working / your-turn / reviewing / PR-ready / done / stopped).
- `agent ls` lists live sessions (from the registry); `agent stop <slug> <name>` ends one; `close`
  (inside a session) ends the current one.

## How Windows differs from Linux/WSL

- **No tmux, no 4-pane layout.** Each session is a single VSCode terminal. Use VSCode's Source
  Control view and `git diff` for diffs; the panel still hosts review reports.
- **Liveness** is tracked by an on-disk registry (`~/dev/.wtd/state/sessions/`) that `agent` writes on
  launch and removes when claude exits — the extension reads it instead of `tmux list-sessions`.
- **The SHA→diff-pane click** (a tmux-pane feature) isn't wired on Windows.

## Troubleshooting

- **`agent: command not found`** — `~/.local/bin` isn't on PATH in this shell. Add the export above and
  open a new Git Bash, or re-run `install.sh`.
- **`code: command not found`** — install the `code` CLI from VSCode's command palette, then re-run.
- **The panel/roster is empty** — reload the VSCode window; confirm the extension installed
  (`code --list-extensions | grep claude-status`).
- **Force a backend** — `export WTD_SESSION_BACKEND=vscode` (or `tmux`) before running `agent`.
- **A session shows live after a crash** — delete its stale file in `~/dev/.wtd/state/sessions/`.
