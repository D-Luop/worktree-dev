---
name: pr
description: >
  Mark a worktree PR-ready and write its PR message. Use when the user is ready to open a pull
  request for this worktree's work, says "/pr", "pr notes", "ready for PR", or "write the PR".
  Marks the worktree `pr` (light-blue 🔹 — sorts just below working, above done) by running
  `agent pr`, pushes already-committed local work to origin, then writes a concise PR message to
  pr-notes.md in the worktree root. Does NOT mark the worktree done (that's /done) and does NOT run a
  review (only the user runs `wt-review`).
allowed-tools: Read, Grep, Glob, Bash, Write
---

# Mark a worktree PR-ready

`/pr` is the **PR-ready milestone** — distinct from `/done`. It flips the worktree to the light-blue
`pr` status (one step below working, above the green done worktrees) and produces the PR message,
without claiming the work is fully finished. Run the steps **in order**, for real — actually call the
tools; never just say you did.

## 1. Mark the worktree PR-ready
Run this as a Bash command:
```
agent pr
```
It prints `marked <name> as 'pr'` and flips the Explorer folder + roster glyph to light-blue 🔹
(sticky: survives Stop/SessionEnd; a real source edit or `agent wip` reverts it to working). If it
doesn't print that line, it didn't work — fix it before continuing.

## 2. Push committed work to origin
Push the branch's already-committed commits (a PR needs them on the remote):
```
git push -u origin HEAD
```
This pushes **only what's already committed** — it does NOT commit or stage anything. If the tree has
uncommitted changes, leave them (mention it in the report); don't commit on the user's behalf unless
asked. "Everything up-to-date" is fine. If the push fails (no remote, rejected/non-fast-forward),
report the error and continue — never force-push.

## 3. Write the PR message to `pr-notes.md`
Write (overwrite) `pr-notes.md` in the **worktree root** (`git rev-parse --show-toplevel`). Base it on
the ACTUAL changes — inspect the unpushed diff if needed:
`git diff "$(git merge-base HEAD origin/HEAD 2>/dev/null || echo origin/main)"...` and `git log`.

Format — clean, terse markdown. **Heading levels: `##` for the title, `###` for every section**
(never `#`):
- `## <branch>` title (`git branch --show-current`).
- One lead sentence: what changed + why (bold the key qualifier if apt, e.g. **Test-only change**).
- `### Why` — a few sentences of context/motivation.
- One `###` section per change area (e.g. `### Test fixes`, `### Tooling`, `### Build`), each a list
  of bullets led by a **bold scope** (module/package) + a terse description. Use `inline code` for
  identifiers, files, flags, consts, SQL.
- `### Out of scope` — deliberately-not-done / known-failing, if any.
- `### Notes` — follow-ups / caveats, if any.

Factual and signal-dense: no filler, no restating the diff, no praise, no AI attribution.
`pr-notes.md` is git-ignored — a scratch artifact for the PR body. (Double-click the `view_pr_notes`
button in the commit pane to render it; ctrl-click to open the source.)

## 4. Report
Tell the user: whether the push succeeded (and the branch pushed to, e.g. `origin/<branch>`, or
"nothing to push" / any error + leftover uncommitted changes), the **absolute** path to `pr-notes.md`
(absolute so it's clickable), and a one-line summary. The worktree now shows light-blue `pr`. Done.
