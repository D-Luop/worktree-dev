# WorkTreeDev

Run a fleet of Claude Code and Codex agents in parallel across several repos, with **one git
worktree and one agent session per branch**. Everything is driven from one VS Code window on the
dev root. Each repo is stored once as a bare clone.

```
<dev>/                                # e.g. D:\dev\worktree-dev (this repo)
├── repos/<slug>/.bare                # one bare clone per repo
├── worktrees/<slug>/<name>           # working copies, one per branch
├── worktrees/<slug>/archive/<name>   # archived (parked) worktrees
├── refs/<slug>/<branch>              # read-only reference checkouts for agents
├── wtd/                              # the Rust daemon + CLI (wtd.exe)
└── .wtd/                             # hooks, skills, VS Code extension, installer, state
```

## Platforms

| Platform | How it runs | Setup |
|---|---|---|
| **Windows** (primary) | `wtd.exe` is a background daemon plus CLI. Sessions are hosted in pseudo-consoles, so they survive VS Code reloads. The VS Code extension talks to the daemon over a named pipe. | [README-windows.md](README-windows.md) |
| Linux / WSL / macOS | The original bash tooling: one tmux session per worktree, with diff and commit panes. The Windows panel features below aren't there yet; a Rust sibling of the daemon is planned. | `make install` |

The command names are the same on both: `agent`, `archive`, `review`, `ask`, `account`, `ref`, and
so on. On Windows each one is a `wtd.exe` subcommand (`wtd agent …`), with `~/.local/bin` shims so
the plain names work too.

## What you get (Windows)

**The fleet panel** (Explorer → *Dev workflow summary*) is a session list modelled on the Claude
extension's:

- **Search** by name, branch, plan or account. Filter by status, or show **Active** (live) sessions
  only.
- **Groups** that you create, rename, reorder and collapse. Drag rows between groups.
- **Rows** show a status glyph, `↑n` for unpushed commits, `●` when there are uncommitted changes,
  the account or Codex badge, and pending agent messages. Click a row to open its session. Hover
  for actions: end session, archive, delete, switch account, *Commits & diffs*, and *Move to
  group…*.
- **Account usage:** 5-hour and 7-day limit bars for every Claude account, plus a pinned
  **assistant** row (a durable session in the dev root that helps manage the fleet).
- **Start/Stop daemon** toggle. A tray icon does the same and can open the VS Code window.

**New Session** is in the Command Palette (`Ctrl+Shift+P`, or `Ctrl+P` then `>`), as
*WorkTreeDev: New Session*:

1. Pick the account, showing live usage for Claude and Codex.
2. Pick the repo.
3. Pick an issue from the repo's GitHub Issues or from its Project board (yours first), or start a
   blank session or an existing branch.
4. Name the branch.

The chosen issue is seeded into the worktree's `CLAUDE.md`. If the repo's Project source names a
"started" column, the card moves there.

**The Changes panel** splits the editor area in half. The focused session's Claude chat is on the
left, and its commits and diff are on the right:

- Uncommitted changes come first, then the branch's commits; pick any one to see its diff.
- Diffs show inline or side-by-side, with word-level highlights, line numbers and collapsible files.
- It updates live as the agent edits files, and follows whichever session you focus.
- Commit SHAs printed in a session's terminal open that commit here.
- You can filter files by path, hide test files, or open any file in VS Code's own diff editor.

If you close it, it comes back the next time you focus a session. To turn it off, set
`claudeStatus.changesPanel` to `false`.

**Worktree Changes** (Explorer, collapsed by default) is a native tree of the same uncommitted
changes and commits:

- Files open in VS Code's own diff editor.
- *Open All Changes* shows a whole commit in the multi-diff editor.
- A beaker toggle hides test files.

**Settings** (gear icon, or *WorkTreeDev: Open Settings*):

- add, clone or remove repos
- link each repo to GitHub and choose its issue source, either **repo issues** or a **Project (v2)
  board**
- log Claude and Codex accounts in and out
- set which account each role uses (dev sessions, reviews, assistant)
- check installed tools and GitHub scopes, and the daemon and tray

**Agent messaging:** agents can see the rest of the fleet through the `wtd` MCP server
(`fleet_list`, `fleet_get`, `fleet_read_file`). They can also *propose* a message to another
worktree (`fleet_send`). Every message needs two approvals before delivery:

1. The sending agent asks you in its chat.
2. You **Review → Send / Edit / Deny** in VS Code.

Once approved, the message is typed into the target session after its current turn ends.

**Status colours** show on the worktree folders in the Explorer and on the roster glyphs:
🔵 working · 🟡 your turn · 🟣 waiting on review · 🔹 PR ready · 🟢 done · 🔴 stopped.

## Commands

On Windows: `wtd help`. Each command below also works as a bare name through its shim.

| Command | Does |
|---|---|
| `agent <slug> <name> [--from <ref>] [--account <a>] [--issue-file <md>] [--no-claude] [ref…]` | Create or open a worktree and its session. A new name creates a new branch, off `--from` or the default branch. An archived name offers to restore it. `ref` tokens (`api@develop`) add read-only context. |
| `agent ls` · `agent stop <slug> <name>` · `agent rm <slug> <name> [--branch] [--force] [-y]` | List · end a session but keep the worktree · remove the worktree (and the branch with `--branch`). |
| `agent done` · `agent pr` · `agent wip` | Run inside a worktree: mark it done, PR-ready, or back to working. |
| `archive <slug> <name>` | Park a worktree under `worktrees/<slug>/archive/`. The branch and changes are kept. |
| `assistant` · `close` | Open the fleet assistant · end the session this terminal belongs to. |
| `review <slug> <name> [--main] [--base <ref>] [--model <m>] [--deep] [--account <a>]` · `wt-review` | A separate read-only reviewer (a review pass plus a skeptic pass) writes reports to `<wt>/.claude/reviews/<ts>/`. **You** start reviews; agents never do. If it hits a usage limit, it is rescheduled automatically. |
| `ask <slug>[@<branch>] [question]` | Ask an expert about a repo. It is read-only, cites `file:line`, and won't guess. With no question it is interactive. |
| `ref add\|sync\|rm\|ls …` | Read-only reference checkouts under `refs/`. |
| `account ls\|add\|login\|rm\|use\|usage\|switch …` | Claude and Codex logins, role defaults, live usage, moving a session to another account. |
| `repo ls\|add\|rm\|fetch …` · `add-repo <slug> <url>` | Register and bare-clone repos (`repo add --new <slug>` creates a local-only repo). |
| `preview <file.html> [label]` | Stage an HTML design mockup for the preview panel. |
| `tokens` | Token usage and estimated cost per worktree. |
| `ship [out]` | Package the toolkit (no secrets, worktrees or repos) for another machine. |
| `wtd daemon start\|stop\|status` · `wtd tray` · `wtd ls` | Daemon control, tray icon, and a fleet listing. |

**In-session skills:**

- `/pr`: mark PR-ready, push, and write `pr-notes.md`
- `/done`: mark done and push
- `/wt-review`: review the worktree and triage the findings
- `/close`
- `/push`
- `/blueprint`: plan

## Conventions every session gets

Each worktree's `CLAUDE.md` is seeded with the team's working rules:

- start by writing an **active plan** (`.claude/plans/active-plan.md`), the session's source of
  truth
- read the relevant `docs/` first
- never hand-edit generated files
- count the whole repo before calling something "standard"
- keep PR notes terse

No AI attribution is added to commits or PRs: a `commit-msg` hook strips it.

## Session continuity

Each worktree has a durable session id (`.wtd/state/session-ids/`). Reopening a worktree resumes
the same Claude conversation (`claude --resume`), even after a reboot. Codex sessions reopen with
`codex resume --last`. On Windows, a session also keeps running across VS Code window reloads while
the daemon is up, and reopening the row re-attaches to it.

## How it's built

- **`wtd.exe`** (Rust): one daemon per user, reached over the named pipe `\\.\pipe\wtd-<user>`. It
  provides:
  - the status machine, fed by Claude and Codex hooks (`wtd hook <event>`)
  - git state, driven by file watchers
  - usage polling and metrics
  - ConPTY session hosting inside kill-on-close Job Objects
  - the store for groups, messages and scheduled jobs (`.wtd/state/store.json`)
  - the MCP server
  - the CLI
- **The VS Code extension** (`.wtd/templates/vscode-claude-status`) provides:
  - the fleet panel (a webview) and the Settings page
  - the New Session quick pick
  - the Worktree Changes tree and its diff content provider
  - folder decorations

  It renders what the daemon pushes and calls `wtd.exe` for actions; it does no polling of its
  own.
- **Claude Code / Codex:** run interactively in each session, and headless for review, skeptic and
  ask. Status hooks come from `~/.claude/settings.json` (and the account's `CODEX_HOME/hooks.json`),
  skills from `.wtd/templates/.claude/skills`, and subagent definitions for the reviewer, skeptic
  and expert.
- **Linux / WSL / macOS:** bash and tmux (`.wtd/scripts`, `.wtd/hooks`), with delta-rendered diff
  and commit panes.

Design notes and rationale: [docs/design/v2-windows-daemon.md](docs/design/v2-windows-daemon.md).

> **Repo-agnostic.** WorkTreeDev orchestrates worktrees, sessions and agents. It makes no
> assumptions about a repo's language or stack, so build, test and deploy steps belong in each
> repo's own tooling and its seeded `CLAUDE.md`.
