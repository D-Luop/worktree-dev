# WorkTreeDev v2 — native Windows daemon

Status: **draft for review** · Scope: Windows only (a Linux sibling reuses the core later)

## 1. Why

At 5–15 worktrees the current design lags because it **spawns processes constantly**, and process
creation on Windows is expensive. Measured on the target machine:

| Operation | Cost |
|---|---|
| start `git.exe` (nothing else) | ~180 ms |
| start Git Bash (`bash -c true`) | ~100 ms; `bash -lc` (login) 525–765 ms |
| start `powershell.exe` | ~290 ms |
| `git status` on a `luop` worktree (~3k files) | ~200 ms — almost all of it startup |
| status hook `wt-status.sh`, per tool call, per agent | ~580 ms (v1) → ~115 ms (step 0) |
| system monitor, every 5 s | ~6.9 s per run (v1) → resident sampler (step 0) |

Step 0 (branch `perf/step0-quick-wins`) removes the worst loops but keeps the architecture: bash hooks,
a fat extension that scans the disk and shells out, and Claude processes we can't reliably track or
kill (hence the PowerShell PEB-reading reaper in `session-lib.sh`). v2 moves every piece of
background work into one resident process that spawns almost nothing.

**Targets:** hook p95 < 15 ms · roster update < 300 ms after a status change · extension host ~0% CPU
when idle · daemon idle < 0.5% CPU, < 60 MB RSS with 15 worktrees · zero polling in the extension.

## 2. Shape

```
                ┌──────────────── VSCode window (dev root) ────────────────┐
                │  claude-status extension  (thin: UI only, no fs/exec)    │
                │   • Fleet panel webview  • Quick Pick flows  • Settings  │
                │   • File decorations     • Terminals: `wtd attach <id>`  │
                └───────────────┬──────────────────────────────────────────┘
                                │ named pipe  \\.\pipe\wtd-<user-sid>
                                │ JSON-RPC (requests) + pushed state patches
┌───────────────────────────────┴──────────────────────────────────────────┐
│ wtd daemon  (Rust, tokio, single instance per user)                       │
│  State store (in memory; SQLite from Phase 2)                             │
│  Session host ── ConPTY + Job Object per Claude session                   │
│  Git service ─── per-worktree change watch → debounced `git status`       │
│  Hook ingest ─── status state machine (port of wt-status.sh)              │
│  GitHub service ─ issues / Projects v2, ETag-cached                       │
│  Usage service ── Anthropic usage API per account, 60 s                   │
│  Metrics ──────── per-Job CPU / memory (no WMI, no PowerShell)            │
│  Jobs ─────────── reviews / ask / long ops, each in its own Job Object    │
└───────────────────────────────────────────────────────────────────────────┘
        ▲ `wtd hook <event>` (Claude Code hooks, ~ms)    ▲ `wtd <cmd>` (CLI, any shell)
```

**One binary, `wtd.exe`, with subcommands:** `daemon`, `hook`, `attach`, plus the user CLI (`new`,
`ls`, `stop`, `archive`, `rm`, `pr`, `done`, `review`, `ask`, `preview`, `account`, `repo`, …). A
Rust exe starts in a few ms, so even the hook client can be the same binary. The daemon is started and
stopped **explicitly** by the user, from a Start/Stop button or from the tray icon (§3.8). Nothing
auto-starts it.

### Why Rust (vs Node)
- The hook client runs on **every tool call of every agent**; Node's ~50 ms startup is too slow for
  it, so it needs a native exe regardless. Once there is a Rust toolchain, the daemon goes there too.
- Direct Win32 access for the parts that matter: ConPTY, Job Objects, `ReadDirectoryChangesW`, named
  pipes with a per-user ACL (`windows` crate).
- Small, steady memory for a process that runs all day; and the same core crates build for Linux later.

Cost: a Rust toolchain plus VS C++ Build Tools for development, and a prebuilt `wtd.exe` shipped in
the repo or as a release asset (the same way the `.vsix` ships today).

## 3. Components

### 3.1 IPC protocol
- Transport: a named pipe, `\\.\pipe\wtd-<sid>`, with a security descriptor allowing only the current
  user. Newline-delimited JSON. Not WebSockets: a pipe is local-only and has no ports or firewall prompts.
- Requests: `{"id":1,"method":"session.start","params":{…}}` → `{"id":1,"result":…}` / `"error"`.
- Subscriptions: `state.subscribe` → one `snapshot`, then `patch` events (JSON-merge-patch on
  `worktrees/<id>` etc.), each with a monotonically increasing `rev`. On reconnect the client sends
  its last `rev`; the daemon replays patches or sends a fresh snapshot.
- Versioned: `hello {protocol: 1, client: "vscode"|"cli"|"hook"}`. A mismatch tells the extension to
  update.
- Hook fast path (as built): `wtd hook` applies the status machine to the worktree's status files
  itself, then sends one fire-and-forget `hook` notification so the daemon pushes the change at once.
  The files stay authoritative, so status keeps updating while the daemon is stopped and nothing needs
  spooling; the daemon re-reads the files on start and on every rescan. *(The first draft spooled
  hook events to a file instead.)*

### 3.2 State model (in memory; SQLite arrives in Phase 2)

Phase 1 state is all derivable from disk (status files, worktree folders, live `wtd run` connections),
so it isn't persisted. Groups, settings, GitHub links and messages (Phase 2) need durable storage, and
that's when SQLite at `.wtd/state/wtd.db` lands.
| Entity | Key fields |
|---|---|
| `Repo` | slug, bare path, remote URL, default branch, `github` link (§4.3) |
| `Worktree` | id, repo, branch/name, path, archived, **group id**, linked **issue**, created |
| `WorktreeStatus` | status (working/input/reviewing/pr/done/stopped), stashed milestone, unread, last change |
| `GitState` | dirty, ahead, behind, head sha + subject, last checked |
| `Session` | id, worktree, **account**, claude session uuid (resume), job handle, pid, started, live |
| `Account` | name, `CLAUDE_CONFIG_DIR`, email, usage (5 h / 7 d), roles (dev/review default) |
| `Group` | id, name, order, collapsed — user-defined, like the Claude extension's groups |
| `Preview` | worktree, label, path, mtime |
| `Review` | id, worktree, phase, model, cost, report paths |
| `Settings` | branch-name template, intervals, GitHub defaults, keybinding prefs |

The status file (`.claude-status`) and its central mirror are **kept as outputs** during migration so
scripts that read them keep working, then retired.

### 3.3 Session host (ConPTY + Job Objects)
- Each Claude session is a child of the daemon in its own **pseudo console** and **Job Object**,
  launched with `--session-id`/`--resume <uuid>` and the account's `CLAUDE_CONFIG_DIR`.
- **Stop / archive / rm = terminate the job.** That kills the whole process tree exactly, replacing the
  straggler sweep, the PEB-cwd reaper and the retry loops in `session-lib.sh`.
- **Sessions survive VSCode reloads and crashes.** The VSCode terminal runs `wtd attach <id>`, a thin
  client that puts its console into raw/VT mode and relays bytes over the pipe. The daemon keeps a
  scrollback ring (~4 MB per session) and replays it on attach; resize events are forwarded. Terminal
  bytes never pass through the extension host.
- Liveness comes from the job's process-exit notifications, so there's no registry and no stale entries.
- Limitation: if the **daemon** dies, its pseudo consoles close and the sessions end. Conversations
  aren't lost (`--resume` reopens them), but in-flight work is interrupted. So the daemon must stay
  small and stable, and upgrading it should resume every open session automatically.

### 3.4 Status (hook ingest)
- `~/.claude/settings.json` hooks become `wtd hook <event>` (stdin JSON forwarded as-is).
- The `wt-status.sh` state machine (sticky `pr`/`done` milestones, the `reviewing` guard, edit vs
  scratch paths) is ported to Rust **with the step-0 differential test turned into unit tests**.
- The daemon emits a state patch; the extension updates the row and the folder decoration. Nothing is
  written to disk on the hot path except the SQLite write-through.

### 3.5 Git service
**As built in Phase 1:** no file watchers. A worktree's git state is re-checked 2 s after its hook
activity goes quiet (at most 10 s into a burst), after its session ends, and by a slow sweep (60 s if
live, 5 min otherwise), at most 2 at a time, with `git --no-optional-locks` so a background check never
holds `index.lock` against an agent's own git command. Watch handles on worktree folders would block
`git worktree move/remove` while bash still owns archive/rm; the watcher design below waits for the
daemon to own those (Phase 4).

**Later:**
- Per worktree, watch the tree (`ReadDirectoryChangesW`, via the `notify` crate) ignoring `.git/objects`,
  `node_modules`, build dirs; also watch `HEAD`, `index` and `refs` for commits and checkouts.
- On change: debounce 750 ms, then `git status --porcelain=v2 --branch` for **that worktree only**,
  with at most 2 running at once. Session turn-ends (hook `stop`) also trigger a check. No periodic
  full sweep.
- The daemon owns `archive`/`rm`, so it drops a worktree's watch before moving or removing it (watch
  handles otherwise block the move on Windows).
- Later, if spawning git is still visible in profiles: read state in-process with `gix`.

### 3.6 Metrics
- Per session job: CPU time via `QueryInformationJobObject(JobObjectBasicAccountingInformation)`;
  memory by summing `GetProcessMemoryInfo` over `JobObjectBasicProcessIdList`. Microseconds, no WMI.
- System RAM via `GlobalMemoryStatusEx`. Pushed every 5 s only while a client is subscribed to `metrics`.

### 3.7 Usage, reviews, ask
- Usage: the existing endpoint call moves into the daemon, which polls every 60 s per account and pushes
  changes. Tokens are read from each account's credentials file, as today.
- Reviews and `ask`: the daemon spawns `claude -p …` in a Job Object and tracks phase, cost and report
  paths. Phase 1 runs the existing `review.sh` under the daemon; later phases port the orchestration.

### 3.8 Lifecycle: Start/Stop button and tray icon
The daemon runs only when the user starts it. There are two controls, and both reflect the live state.

**Tray icon: `wtd tray`.** A separate, tiny process from the same exe (a hidden window plus
`Shell_NotifyIcon`; no UI framework). It has to be separate: if the daemon owned the icon, stopping the
daemon would remove the only button that can start it again.
- **Icon state:** *running* (colour) · *stopped* (grey) · *needs you* (colour + dot), shown when an
  agent is on your turn or a cross-worktree message (§4.4) is waiting for approval. The tooltip reads
  e.g. `WorkTreeDev — running · 9 sessions · 2 need you`.
- **Left-click:** open or focus the dev-root VSCode window (`code <dev root>`, which focuses an existing
  window rather than opening a second one).
- **Right-click menu:** status line · **Start daemon / Stop daemon** (one item that flips) · **Open
  VSCode** · *Pending approvals (N)…* (opens VSCode on the approval prompt) · *Start tray at logon* ✓ ·
  *Quit tray*.
- It learns the daemon's state by holding a subscription on the pipe. When the daemon is down, it
  retries the connection every few seconds (a cheap failed pipe open, no process spawn).
- *Start tray at logon* (on by default after install) adds an `HKCU\…\Run` entry for the **tray only**.
  The daemon still starts only when you click Start.
- Windows puts new tray icons in the `^` overflow. Drag it onto the taskbar, or turn it on under
  *Settings → Personalization → Taskbar → Other system tray icons*. The installer prints this hint.

**Panel button.** The fleet panel toolbar gets a **Start / Stop** toggle with the same state. While the
daemon is stopped, the panel shows the roster greyed out with a single *Start daemon* call to action,
and the `WorkTreeDev: Start Daemon` / `Stop Daemon` palette commands do the same.

**Stopping is safe:**
- Hooks keep updating the status files while the daemon is down (§3.1), so folder colours stay right
  and the daemon picks the latest state up when it starts.
- Phases 1–2: sessions run in VSCode terminals under `wtd run` and **keep running** when the daemon
  stops. Only fleet features (live status, git state, messaging) pause.
- Phase 3+: the daemon hosts the sessions, so Stop asks for confirmation first: *"Stopping ends 9
  running sessions. Conversations can be resumed."* *Stop* then terminates them cleanly.
- CLI commands that need the daemon say *"daemon not running — start it from the tray or the panel"*.
  They never start it themselves.

## 4. New UX

### 4.1 Fleet panel (modelled on the Claude extension's session list)
- **Toolbar:** filter menu · **Active · N** toggle (live sessions only) · search · **+ New group** ·
  **+ New session**.
- **Search:** matches worktree name, branch, repo, linked issue number and title, group, and account,
  filtering as you type.
- **Filters:** status (working / your turn / reviewing / PR / done / stopped), repo, account,
  has-unread, dirty or unpushed.
- **Groups:** user-defined and collapsible, with counts. They **replace** today's grouping by repo,
  and there is no toggle back. Drag rows between groups, or use *Move to group…*. New sessions land
  in **Ungrouped** (the New Session flow can also pick a group). Rename, reorder and delete groups
  inline; deleting a group moves its rows to Ungrouped. The repo stays visible as a badge on each row
  and as a filter.
- **Rows:** status glyph · name · issue `#123` · account badge · `↑n` / `●` · relative time. Unread rows
  are highlighted, as today. Row actions and the account / usage / monitor sections carry over.
- The webview receives only patches, so a status change re-renders one row, not the whole list.

### 4.2 New-session flow (Quick Pick, keyboard-first)
Entry point: the **Command Palette** (`Ctrl+P` then `>`, or `Ctrl+Shift+P`) → **`WorkTreeDev: New
Session`**, which runs a multi-step Quick Pick. Every fleet action gets a palette entry under the same
`WorkTreeDev:` prefix (Open Session…, Stop Session…, Archive…, Review…, Settings), so the whole fleet is
reachable from the keyboard. Quick Open's file search on plain `Ctrl+P` is untouched.
1. **Account:** each Claude account with its email and live 5 h / 7 d usage. The `dev` role default is
   preselected, and accounts over 90% are flagged.
2. **Repo:** registered repos, most recently used first, plus *Add repo…*.
3. **Issue:** open issues from the repo's linked GitHub source (§4.3), assigned-to-me first, searchable
   by number and title, showing labels and the project status column. Plus *No issue (blank
   session)* and *Existing branch…*.
4. **Confirm the name:** a branch name generated from a template such as `{type}/{number}-{slug}`
   (type from labels: `bug` → `fix`, else `feat`), editable.

Then the daemon creates the worktree and starts the session. The issue's title, body and URL are
seeded into the worktree's `CLAUDE.md` *Context / scope* section, and `/pr` notes get a `Closes #123`.

### 4.3 GitHub linking and the Settings page
*Built ahead of Phase 2:* the Settings tab (gear in the panel toolbar, or `WorkTreeDev: Open
Settings`) with Repositories, Accounts (Claude **and Codex**), Defaults, GitHub and Daemon & tray. It
drives `wtd repo|account|env` (JSON). Storage stays compatible with the bash tools: `.wtd/repos.tsv`,
`~/.claude-accounts/` (+ `roles.conf`), `~/.codex` / `~/.codex-accounts/` (`CODEX_HOME`), and
per-repo GitHub links in `.wtd/config.json` (local, git-ignored). Codex *sessions* come with New Session.

- **Settings** opens as a full editor-tab webview with these sections: **Repos** (add, remove, URL,
  default branch), **GitHub links**, **Accounts** (add or log in, role defaults), **Branch naming**,
  **Groups**, **Performance** (intervals), **Keybindings**.
- **GitHub link per repo:** the GitHub repo that holds the code (`owner/name`, detected from the
  remote), plus an **issue source**, chosen per repo on the Settings page. Both kinds are fully supported:
  - **Repo issues** (the default when nothing is configured): open issues from the code repo, or from
    another repo such as a shared tracker. Optional label and assignee filters.
  - **GitHub Project (v2)**: a user- or org-owned board, identified by its URL, which is parsed into
    owner and number. The Settings page lists the board's single-select fields so you can pick the
    status field and which columns to offer (e.g. *Todo*, *Ready*). An optional toggle moves the card
    to a chosen column (e.g. *In Progress*) when a session starts on it. Board items that are drafts
    rather than real issues are listed but marked, since they have no issue number for `Closes #`.

  ```jsonc
  // stored per repo in the daemon's settings
  "github": {
    "repo": "D-Luop/luop-software-mono-repo",
    "issueSource": { "kind": "repo", "repo": "D-Luop/luop-software-mono-repo", "labels": [], "assignee": "@me" }
    // or
    "issueSource": { "kind": "project", "owner": "D-Luop", "ownerType": "org", "number": 3,
                     "statusField": "Status", "offer": ["Todo", "Ready"], "onStart": "In Progress" }
  }
  ```
  Settings validates a source when it's saved, with a test fetch that shows "N items found" or the
  exact error (e.g. missing `read:project` scope → `gh auth refresh -s read:project`).
- **Auth:** the GitHub token comes from `gh auth token` (so `gh auth login` is the only setup), kept in
  memory and never written to disk.
- **API:** REST for issues, GraphQL for Projects v2, with ETag conditional requests (a `304` doesn't
  count against the rate limit). Results are cached per source and refreshed when the picker opens if
  older than 60 s.

### 4.4 Agent-to-agent communication
Agents can see the rest of the fleet and send prompts to other worktrees. **Every prompt sent to another
worktree needs the user's explicit approval.**

**Interface: a `wtd` MCP server.** `install` registers `wtd mcp` (stdio) as a user-scope MCP server,
so every session gets typed tools with descriptions:

| Tool | Does | Approval |
|---|---|---|
| `fleet_list` | All worktrees: repo, branch, status, group, linked issue, account, live?, dirty/ahead, plan title, last activity | none (read-only) |
| `fleet_get` | One worktree in depth: plan summary (`active-plan.md`), recent commits, diff stat vs default branch, PR notes | none |
| `fleet_read_file` | Read a file from another worktree (path-confined to that worktree; never writes) | none |
| `fleet_send` | Send a prompt to another worktree's agent | **always the user** |
| `fleet_inbox` | Messages this worktree sent and received, with delivery state | none |

The same operations exist as CLI commands (`wtd fleet ls|show|cat|send`) for scripts. Both go through
the daemon, so the approval rule applies equally to them.

**"Always ask" is enforced in two layers:**
1. **The daemon (the guarantee).** `fleet_send` never delivers anything directly. It creates a
   *pending* message and returns `pending_approval` to the sender. The extension raises an approval
   prompt (a notification, plus a badge on the sender's row) showing sender → target, the full text,
   and **Send · Edit & send · Deny**. Only the user's click releases it. This holds in every Claude
   permission mode (`auto`, `bypassPermissions`), and whether the agent uses the MCP tool or the CLI,
   because there is no other path into another session.
2. **The agent (good behaviour).** The seeded `CLAUDE.md` tells agents to propose a cross-worktree
   message to the user in chat and explain why *before* calling `fleet_send`, and never to send
   unprompted. The tool's description says the same thing.

**Delivery** (after approval):
- The target receives it as an ordinary prompt with a provenance header, so it shows in that session's
  transcript and the user sees exactly what arrived:
  ```
  [wtd message from luop/feat-billing (msg 42) — reply with fleet_send to "luop/feat-billing"]
  <body>
  ```
- **Target idle** (your turn): typed into its session right away. Before Phase 3 the extension does
  this with `terminal.sendText`; from Phase 3 the daemon writes it to the pseudo console.
- **Target busy:** queued, then delivered at its next turn end (`stop` hook). It never interrupts a turn
  in progress.
- **Target not running:** the approval prompt offers *Start session & deliver* (resumes its
  conversation) or *Leave in inbox*. The inbox is delivered the next time the session starts.
- Delivery state (pending → approved → delivered / denied) shows in both agents' `fleet_inbox` and on
  the rows.

**Guard rails:** replies are prompts too, so they also need approval, which means two agents can't loop
without the user. Messages are capped (32 KB) and rate-limited per sender. The approval prompt names the
sender clearly, because the receiving agent should treat a message as coming from a peer, not from the
user.

**Fleet awareness without token cost:** sessions aren't fed a fleet summary every turn. The seeded
`CLAUDE.md` says other worktrees exist and points to `fleet_list` / `fleet_get`, so agents look things
up on demand.

## Status (2026-10-04)

All four phases are built. On Windows, every `~/.local/bin` command (`agent`, `archive`, `review`,
`ask`, `account`, `ref`, `preview`, `tokens`, `ship`, `assistant`, `close`, `add-repo`) is a shim that
execs `wtd.exe <sub>`. The extension calls `wtd.exe` directly and runs session terminals as
`wtd.exe agent …`, with no bash in between. The bash scripts are now only the Linux/tmux tooling.

Phase 4 notes:

- **Archive and remove are daemon-owned while it runs** (`worktree.archive` / `worktree.remove`).
  The daemon ends the hosted session and drops its file watch first, because on Windows an open
  watch handle or a live process blocks `git worktree move/remove`.
- **Git state follows file watchers** (`notify`, on the worktree plus its admin dir in the bare
  repo). Sweeps every 300 s (live) / 900 s (idle) are only a backstop.
- **Usage-limit review retries** are daemon-scheduled jobs (`job.schedule`), so they survive the
  terminal closing.
- **Commits and diffs are native:** a *Worktree Changes* Explorer tree shows uncommitted changes
  plus the branch's commits over its base. It follows the focused session. Files open in VSCode's
  own diff editor, with read-only revisions via a `wtd-git:` content provider, and whole commits
  open in the multi-diff editor. SHAs printed in a session terminal link to that commit. The webview
  commits tab and the tmux-era commit/diff panes are gone on Windows.
- **Deleted:** the extension's legacy (pre-daemon) mode, i.e. its own folder scans, `git status`,
  usage fetching and PowerShell monitor; `monitor-stats.*`; and the Windows process-reaping
  PowerShell in `session-lib.sh`.

What changed from the plan while building Phases 1–3:

- **Store:** groups, membership and messages live in a JSON file (`.wtd/state/store.json`, atomic
  writes), not SQLite. The data is tiny, and this keeps the build light.
- **Codex sessions:** Codex has Claude-compatible hooks (`hooks.json` with the same events and stdin
  JSON), so `wtd run` / `wtd host` write the status hooks into the account's `CODEX_HOME`. They also
  register the MCP server and trust the worktree in `config.toml`. Codex asks the user to trust new
  hooks once; that is deliberately not bypassed.
- **Hosted sessions:** `wtd host` opens or attaches. Re-attaching clears the terminal and makes
  conpty repaint the viewport; scrollback from before the re-attach isn't replayed.
- **Message delivery** types an approved message into the target's hosted session as a bracketed
  paste, one per turn, after its turn ends. Targets without a hosted session keep it queued until
  their session starts.
- **Smart App Control** (Windows 11, enforce mode) can block an unsigned `wtd.exe` build by
  reputation, per file hash. One build was blocked; rebuilding with different codegen got through.
  Shipping to other machines needs code signing (e.g. Azure Trusted Signing).

## 5. Migration plan

| Phase | Delivers | Retires |
|---|---|---|
| **0** (done on branch) | Status hook ~5× faster, resident monitor, no login shells, single status-folder watch, row-only roster updates, git index settings, control-window VSCode settings | — |
| **1** Daemon core | `wtd.exe` daemon + hook + pipe protocol + in-memory state; git service; metrics; usage; `wtd mcp` with the read-only fleet tools (`fleet_list` / `fleet_get` / `fleet_read_file`); `wtd tray` icon + panel Start/Stop toggle. Extension switched to the pipe for all state. Sessions still launch in VSCode terminals, wrapped by `wtd run` (puts Claude in a Job Object and reports liveness) | extension disk scans, timers and `exec`s; `monitor-stats.*`; `wt-status.sh`; the session registry and reaper |
| **2** New UX | Fleet panel search / filters / groups; New Session quick pick; Settings page; GitHub linking + issues; agent messaging (`fleet_send` + approval prompt + delivery + inbox) | the `+ agent` input box |
| **3** Session host | ConPTY hosting + `wtd attach`; sessions survive reloads | `wtd run` wrapper; terminal-name tracking hacks |
| **4** CLI parity | `archive`/`rm`/`review`/`ask`/`account`/`preview` in Rust; skills call `wtd …` | Git Bash scripts on Windows (Claude Code itself still needs Git Bash) |

Each phase ships on its own, and the bash tooling keeps working until the phase that retires it.

### Crate layout
```
wtd/                       (cargo workspace, at repo root /wtd)
  crates/wtd-core     state model, status machine, protocol types   (platform-free)
  (Phase 1 ships two crates — wtd-core and wtd, with Windows code in wtd/src/win.rs; the split below
   happens as those areas grow)
  crates/wtd-win      ConPTY, Job Objects, named pipes, dir watch   (cfg(windows))
  crates/wtd-git      status parsing, worktree ops
  crates/wtd-github   issues / Projects v2 client
  crates/wtd          the binary: daemon, hook, attach, CLI
```
`wtd-core` has no OS dependencies, so the Linux sibling adds a `wtd-unix` crate (forkpty or tmux,
process groups or cgroups, Unix sockets) and reuses everything else.

### Testing
- Status machine: unit tests generated from the step-0 differential harness (old bash vs new).
- Protocol: golden JSON fixtures shared by the Rust tests and the extension.
- Session host: an integration test spawning a fake "claude" (prints, sleeps, exits) under ConPTY;
  verifies attach/replay, resize, and job-kill of a grandchild process.
- Performance: a bench that starts 15 fake sessions firing hooks at 10 Hz and asserts the §1 targets.

## 6. Decisions needed

1. ~~Keybinding for New Session.~~ **Decided:** Command Palette entries (`WorkTreeDev: …`), reached via
   `Ctrl+P` → `>`. An optional direct chord can be bound later in Settings.
2. ~~"Project" meaning.~~ **Decided:** both. Each repo's issue source is configured on the Settings
   page as either **repo issues** or a **GitHub Project (v2) board** (§4.3). Repo issues is the default
   when nothing is configured.
3. ~~Groups vs repos.~~ **Decided:** user-defined groups replace grouping by repo, with no toggle. The
   repo shows as a row badge and a filter (§4.1).
4. ~~Daemon lifetime.~~ **Decided:** explicit Start/Stop only, via a toggle button in the panel and
   a tray icon (`wtd tray`) that can start and stop the daemon and open the VSCode window (§3.8). Only
   the tray app starts at logon.
