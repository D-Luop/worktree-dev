---
name: done
description: >
  Wrap up a finished worktree. Use when the user says the work in this worktree is complete /
  finished / done / "ship it" / "ready to push", or invokes /done. Marks the worktree done (green)
  by running `agent done`, pushes already-committed local work to origin, then writes a concise PR
  message to pr-notes.md in the worktree root.
allowed-tools: Read, Grep, Glob, Bash, Write
---

# Wrap up a finished worktree

Run these steps **in order**. Do them for real — actually call the tools; never just say you did.

## 1. Mark the worktree done
Run this as a Bash command:
```
agent done
```
It prints `marked <name> as 'done'` and flips the Explorer folder + terminal tab to green (sticky).
If it doesn't print that line, it didn't work — fix it before continuing. Do not claim the worktree
is green unless you ran this and saw the output.

## 2. Push committed work to origin
Push the branch's already-committed commits:
```
git push -u origin HEAD
```
This pushes **only what's already committed** — it does NOT commit or stage anything. If the tree has
uncommitted changes, leave them (mention it in the report); don't commit on the user's behalf unless
they asked. If there's nothing to push ("Everything up-to-date") that's fine. If the push fails
(e.g. no remote, rejected/non-fast-forward), report the error and continue — don't force-push.

## 3. Write the PR message to `pr-notes.md`
Write (overwrite) `pr-notes.md` in the **worktree root** (`git rev-parse --show-toplevel`).
Base it on the ACTUAL changes — inspect the unpushed diff if needed:
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
`pr-notes.md` is git-ignored — it's a scratch artifact for the PR body.

## 4. Report
Tell the user: whether the push succeeded (and the branch pushed to, e.g. `origin/<branch>`, or
"nothing to push" / any error + leftover uncommitted changes), the **absolute** path to `pr-notes.md`
(absolute so it's clickable), and a one-line summary. Done.
