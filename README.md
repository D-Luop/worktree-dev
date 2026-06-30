# WorkTreeDev

Run many parallel Claude Code sessions across repos — **one git worktree + one session per
branch**, each repo stored once as a bare clone. Commands are on your `PATH` after `make install`.

```
~/dev/
├── repos/<slug>/.bare                # one bare repo per slug
├── worktrees/<slug>/<name>           # working copies (one per branch)
├── worktrees/<slug>/archive/<name>   # archived (shelved) worktrees
├── refs/<slug>/<branch>              # read-only reference checkouts
└── .wtd/                             # the tooling
```

**Cross-platform.** The session backend is auto-detected: a **tmux** session per worktree on
Linux / WSL / macOS, or a **VSCode integrated terminal** per worktree on **native Windows** (Git
Bash, no WSL). Override with `WTD_SESSION_BACKEND=tmux|vscode`. Windows setup:
[**README-windows.md**](README-windows.md).

## Purpose & uses

WorkTreeDev is a control center for driving **many Claude Code agents in parallel** across several
repos at once, without them stepping on each other. Each unit of work is a branch; each branch gets
its own git worktree (isolated files), its own tmux session (isolated terminal + agent), and its own
status. One command spins all of that up and drops you into a running agent.

It exists to make parallel, multi-repo agent work practical and safe:

- **Isolation** — every task is a separate worktree + branch + session, so several agents edit
  different things simultaneously with zero file or branch collisions. Repos are stored once as a
  bare clone and shared by all their worktrees (cheap to spin up many).
- **At-a-glance state** — each worktree shows its status as a colored folder in the VSCode Explorer
  and as a colored glyph in the **Dev workflow summary** roster (working / your-turn / waiting-on-
  review / PR-ready / done / stopped), so you can run a fleet and instantly see which agents need you.
- **Quality gates before push** — a separate, read-only reviewer agent (Sonnet review + adversarial
  Opus skeptic) checks a worktree's changes against repo docs/standards and writes reports into the
  worktree. **You** trigger it (`wt-review` / `/wt-review`) — agents never self-start a review — and it
  triages findings. The reviewer memoizes confirmed repo conventions to a per-repo ledger so later
  reviews reuse them instead of re-deriving (fewer tokens).
- **Grounded answers** — an `ask` expert agent answers questions about any repo strictly from its
  real code/docs (cites `file:line`, won't guess), for understanding a codebase without editing it.
- **Cross-repo context** — read-only reference checkouts let an agent consult other repos/branches
  without being able to modify them.
- **Convention enforcement** — seeded per-worktree rules push agents to start each session by writing
  an **active plan** (the session's source of truth, revised as work proceeds), read the relevant
  `docs/` first, never hand-edit generated files, count the whole repo before calling anything
  "standard," and write terse PR notes — encoding the team's standards into every session.
- **Cost visibility** — `make tokens` reports per-worktree token usage and estimated cost, grouped
  by status.

**Typical flow:** `agent <slug> <name>` to start a task → the agent writes an active plan and works in
its isolated worktree → when it’s ready for a PR, `/pr` writes the PR notes and marks it light-blue
(`/done` marks it green when fully finished).
A pre-push review is **separate and on you** — run `wt-review` whenever you want it (agents never start
one, and it's not part of `/done`). Meanwhile other agents run the same loop on other branches/repos,
and `ask` answers questions on the side.

**Good for:** running a fleet of coding agents across several repos, pre-push review,
codebase Q&A, and keeping parallel branches organized. **Not for:** a single repo with one task at a
time (plain Claude Code is simpler), or non-git workflows.

## Setup

| Command | Does |
|---|---|
| `make install` | Symlink commands, merge hooks/statusline, install the VSCode status extension, set tmux/VSCode prefs. Re-run any time; **reload the VSCode window** after. |
| `add-repo <slug> <git-url>` | Register + bare-clone a repo (default branch auto-detected). |
| `make repos` | List registered repos. |
| `ship [out.tgz]` | Package the engine (placeholders rendered at install; **no** secrets/worktrees/repos/refs) into a tarball a teammate untars to `~/dev` and runs `install.sh`. (Or just clone this repo.) |

## Sessions

| Command | Does |
|---|---|
| `agent <slug> <name> [--from <ref>] [--account <a>] [--no-claude] [ref…]` | Open/create a worktree + tmux + running `claude`. New name → new branch (off `--from` or default). Archived name → prompts to reopen. `--account <a>` runs the session under another Claude login (all its cost bills there). `ref` tokens (`dv@develop`) add read-only context. |
| `agent ls` | List active sessions (name, attached, status glyph). |
| `agent stop <slug> <name>` | End a session, keep the worktree. |
| `agent rm <slug> <name> [--branch] [--force] [-y]` | Tear down: kill session + remove worktree (`--branch` deletes branch too). |
| `agent done` / `agent pr` / `agent wip` | (Inside a worktree) mark complete (sticky green) / PR-ready (sticky light-blue) / back to working. |
| `assistant [--account <a>] [--mode <m>]` | Open/resume the **fleet assistant** — a Claude session in the dev base for managing worktrees (launch/stop/archive/review/teardown). Pinned at the top of the roster. |
| `close` | (Inside a session) end the current session, keep the worktree. |
| `archive <slug> <name>` | Shelve a worktree → `worktrees/<slug>/archive/`. Reopen via `agent` (prompts to restore). |

Session layout: small **command pane** (top-left, aligned with the commit pane) over the **claude**
pane; **commit history** (top-right) over the live **diff** pane. Panes are **locked** — mouse-drag on
borders and the pane swap/rotate/relayout keys are disabled, so the layout can't be scrambled by accident.

## Claude accounts (multi-login / billing)

Run sessions and reviews under different Claude logins — each its own `CLAUDE_CONFIG_DIR`, so all of
that work's usage/cost bills to that account. Default (nothing configured) = your normal `~/.claude`.

| Command | Does |
|---|---|
| `account add <name>` | Create an account and log in to it (opens Claude — run `/login`, then exit). |
| `account ls` | List accounts + their email + the role mappings. |
| `account usage [name]` | Live 5h/7d/Sonnet usage for an account (default if omitted). |
| `account use dev <name>` / `account use review <name>` | Default account for **dev sessions** / **reviews** (`<name>` or `default`). |
| `agent <slug> <name> --account <name>` | Run one session under `<name>` (overrides the `dev` role). |

So you can set `account use review work` (all `wt-review`/`review` cost → "work") while dev sessions
stay on default, **and/or** override any single session with `agent … --account <name>`.

## References (read-only cross-repo context)

| Command | Does |
|---|---|
| `ref add <slug>[@<branch>] …` | Create/refresh read-only checkout(s) under `refs/`. |
| `ref ls` / `ref sync` / `ref rm <token>` | List / re-fetch / remove. |

## Review · expert

| Command | Does |
|---|---|
| `review <slug> <name> [-i] [--main] [--base <ref>] [--model <m>] [--deep] [--account <a>]` | Separate read-only reviewer (Sonnet review + Opus skeptic). Reports → `<wt>/.claude/reviews/<ts>/`. `--main` = whole branch vs default. Cost bills to `--account`, else `$REVIEW_ACCOUNT`, else the configured `review` account (`account use review …`). |
| `wt-review [--main]` | (Inside a worktree) `review` for the current worktree (slug/name inferred); same account routing. |
| `ask <slug>[@<branch>] [question]` | Per-repo **expert** Q&A — read-only, cites `file:line`, won't guess. No question → interactive. |

## Tokens

| Command | Does |
|---|---|
| `make tokens [ARGS="…"]` | Per-worktree token usage + est. cost, grouped by status. Flags: filter substring, `--since`, `--sort`, `--all`. |

## In-session skills

`/pr` (mark **PR-ready** + push + write `pr-notes.md`) · `/done` (mark done + **push committed work** +
write `pr-notes.md`) · `/wt-review` (review + triage) · `/close` · `/push` · `/blueprint` (plan).

## Status colors

🔵 working · 🟡 your turn · 🟣 waiting on review · 🔹 PR-ready (light blue) · 🟢 done · 🔴 stopped (no session).
Shown as the Explorer folder color and the roster glyph. On the VSCode backend the editor **tab bar is
hidden** (`workbench.editor.showTabs: none`) so the roster is the single place you switch sessions —
**click a roster row** to focus its session. No AI attribution is added to commits/PRs.

A **Dev workflow summary** panel in the Explorer (between the folder tree and Outline) shows:
- every configured account's **email** + **live** 5h + 7d session-limit bars + reset countdown
  (fetched from Anthropic's usage endpoint every ~60s; falls back to the per-account
  `rate-limits.json` if offline);
- a pinned **🤖 assistant** row at the top (separated from the worktrees, no status glyph) — a single
  durable Claude session that runs in the dev base and helps you **manage the fleet** (launch / stop /
  archive / review / tear down worktrees via the wtd commands). Always present — **click it** to open
  (or focus the one running); it resumes across reopen/reboot like a worktree session. There's only
  ever one. Route its cost with `--account` /
  `$ASSISTANT_ACCOUNT` (defaults to your normal login);
- a **fleet roster** grouped by repo (alphabetically) — each worktree's status glyph (`◐ ! ⋯ ◆ ✓ ○`,
  colored) + `↑n` unpushed / `●` dirty; live sessions get a faint gray row, and a row that just became
  *your turn* is highlighted until opened. **Click a row** to open it, **⏹** (live sessions only) to
  end the session keeping the worktree, **📦** to archive, **🗑** to delete the worktree (a modal offers
  *worktree only* or *worktree + branch*; if it's dirty, a second prompt asks before force-discarding),
  **+ agent** to launch one, **📷** to add an image to the focused session (pastes a clipboard
  screenshot — the workaround for native-Windows terminal paste not loading clipboard images — else
  pick a file; it stages the image and inserts its path, which Claude Code auto-loads on submit), or
  **tests ✓/✕** to include/exclude test files from every diff pane;
- a **monitor** — agent-scoped CPU/memory (summed over the claude process trees), tmux session count,
  and running review count.

The commit pane colors **unpushed** SHAs cyan (vs yellow once pushed) and marks commits **not on the
default branch** with a magenta `┃` (where your branch diverges from main). **Past reviews appear
inline** as pink `⟳` rows interleaved with the commits in the order they ran — double-click one to
render that review. Below an **`── actions ──`** separator the footer has double-click buttons —
**view_uncommitted_diff**, **view_branch_diff** (the whole branch vs the default branch),
**view_active_plan**, **view_pr_notes**, and (during a live review) **view_review_status** — that
render in the diff pane; the reviewer's findings render there automatically when it finishes.

---

## Tools & underlying technology

A map of every moving part and what it's built on.

### Orchestration — git + tmux
- **git (bare clones + worktrees).** Each repo is cloned once as a **bare** repo at
  `repos/<slug>/.bare`; every branch is a **`git worktree`** at `worktrees/<slug>/<name>` sharing
  that object store (cheap to spin up dozens). `archive`/restore and reopen are `git worktree move`.
  Unpushed/dirty state in the roster comes from `git status --porcelain --branch`. Per-worktree
  gitignore (CLAUDE.md, pr-notes.md, .claude-status) is the bare's `info/exclude`.
- **tmux.** One session per worktree (`<slug>-<name>`). The layout is built with `split-window`
  (command pane, claude pane, commit + diff panes), addressed by **pane IDs**. Session options
  (`@wt_label`) drive the tab title (worktree name, no status dot) via `set-titles`; `close`
  and `agent stop` use `kill-session`. Mouse scrollback + a visual (silent) bell are set in
  `~/.tmux.conf`, which also **locks the layout** (unbinds mouse border-drag + the pane
  swap/rotate/relayout keys).

### Agents — Claude Code
- **Claude Code CLI.** Interactive in each session; **headless** (`claude -p --output-format json`,
  parsed with `jq` for `.result` / `.usage` / `.total_cost_usd`) for the reviewer, skeptic, and expert.
- **Skills** (`.claude/skills/<name>/SKILL.md`) — model- or user-invoked rituals (`/pr`, `/done`,
  `/wt-review`, `/close`, `/push`, `/blueprint`); thin wrappers over the PATH commands.
- **Hooks** (`~/.claude/settings.json`) — deterministic handlers on harness events (UserPromptSubmit /
  PreToolUse / **PostToolUse** / Stop / Notification / SessionEnd): drive the `.claude-status` sentinel
  via `hooks/wt-status.sh`, refresh the live sql/proto diff on edits, and emit a double-clickable
  `view_plan` token when an agent writes a plan.
- **Subagents** (`--agent <name>`, defs in `.claude/agents/*.md`) — the `reviewer`, `skeptic`, and
  `expert` run as isolated agents with their own system prompts, **model tiers** (Sonnet review →
  Opus skeptic; Opus expert), read-only scoping via `--add-dir` + `--disallowedTools`. The reviewer
  memoizes confirmed repo conventions to a per-repo ledger (`review-knowledge/<slug>.md`) and trusts
  it (with a spot-check) on later runs instead of re-counting.
- **Statusline** — `claude-pace.sh` (bash + jq), reads the per-render JSON Claude Code passes on
  stdin (model, context %, cost, **rate_limits**); also persists the limit + account email to
  `~/.claude/rate-limits.json` for the Explorer panel.

### Editor integration — VSCode
- **`claude-status` extension** (Node, VSCode Extension API). Uses a **`FileDecorationProvider`**
  (worktree folder colors from `.claude-status`), **`contributes.colors`** (the 5 bright status
  hues), a **`WebviewViewProvider`** + **`contributes.views.explorer`** (the Dev workflow summary
  webview: HTML/CSS bars + roster, `fs.watchFile` for live updates, `child_process` to launch
  `agent`), and a **`FileSystemWatcher`** for status changes. Packaged into a `.vsix` (a zip) with
  Python's `zipfile` (no `vsce`).
- **Machine settings** (`~/.vscode-server/data/Machine/settings.json`) — terminal tab title
  `${sequence}`, silent visual bell (`accessibility.signals.terminalBell.sound=off`), and
  `workbench.editorAssociations` to open `.md` reports as preview.

### Diff & commit panes
- **delta** (`git-delta`) renders syntax-highlighted diffs in the diff pane (falls back to
  `git --color=always` if absent); filenames get a yellow overline + line-number gutter. The
  commit-history pane is `git log` formatted by an **awk** script (relative ages, dated rules, wrapped
  subjects, cyan unpushed SHAs, a magenta `┃` on commits that diverge from the default branch, and
  past reviews as pink `⟳ view_review_<N>` rows interleaved by run time). **Double-click** a SHA, a
  review row, or a footer button under the `── actions ──` rule (`view_uncommitted_diff` /
  `view_branch_diff` (whole branch vs default) / `view_active_plan` / `view_pr_notes` /
  `view_review_status` during a live one) — to repaint the diff pane (tmux `DoubleClick1Pane` →
  `commit-diff-show.sh`). Markdown (plans, PR notes,
  review findings) is rendered to ANSI by a self-contained `md-render.py` (no external deps). A panel
  toggle drops test files from every diff via `:(exclude)` pathspecs.

### Shell tooling & packaging
- **bash** scripts (`agent`, `review`, `wt-review`, `build`, `archive`, `close`, `ask`, `account`,
  `ship`, `ref`, `add-repo`, `tokens`) symlinked onto `PATH`; **jq** for all JSON; **bash-completion**
  for slug/branch/worktree tab-completion. `install.sh` is idempotent and **merges** into existing
  config with `jq` rather than overwriting; shipped templates carry `__DEV__`/`__USER__`/`__DISTRO__`
  placeholders rendered to real values at install (so the tree is portable — see `ship`).
- **State files:** `repos.tsv` (slug→URL registry), `.claude-status` (per-worktree sentinel),
  `rate-limits.json` (limit + email feed), `pr-notes.md` (scratch PR body),
  `.claude/plans/active-plan.md` (per-session plan), `review-knowledge/<slug>.md` (conventions ledger),
  `~/.config/wtd/exclude-tests` (test-toggle flag), `~/.config/wtd/dev-root` (absolute path of this
  tree, written at install so the prebuilt VSCode extension finds it wherever it lives),
  `.wtd/state/sessions/<session>` (liveness registry, deleted on close) and
  `.wtd/state/session-ids/<session>` (durable Claude session id — see resume below).

### Session continuity — resume across reopen/reboot
A live Claude process never survives a reboot (on the vscode backend it runs in the VSCode terminal;
even tmux dies on reboot), but Claude persists every session transcript to disk. So each worktree gets
a **durable session id** (`.wtd/state/session-ids/<session>`, kept out of the worktree so it never
shows as git-dirty): the first launch mints a UUID and starts `claude --session-id <uuid>`; reopening
the worktree (from the roster or `agent <slug> <name>`) **resumes** that conversation with
`claude --resume <uuid>` once its transcript is on disk — including after a restart. `agent rm` forgets
the id so a future same-named worktree starts clean.

> **Repo-agnostic.** worktree-dev makes no assumptions about the language or stack of the repos it
> drives — it orchestrates git worktrees, tmux sessions, and Claude Code agents. Repo-specific build,
> test, and deploy steps belong in each repo's own tooling and its seeded `CLAUDE.md`.
