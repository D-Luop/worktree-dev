---
name: commit
description: >
  Stage and commit this worktree's work with a well-formatted message. Use when the user says
  "commit", "commit this", "commit it", "commit the changes", or invokes /commit. Writes a terse,
  descriptive subject line with the detail in the body — the house commit style. Commits only; does
  not push, review, or merge.
allowed-tools: Bash
---

# Commit this worktree's work

Produce a commit that reads well in the one-line log and stands on its own once it becomes a
squash-merge PR title. Run for real — actually stage and commit; never just describe it.

## 1. See what's changing
- `git status --short` and `git diff` (staged + unstaged) to understand the *actual* change.
- If nothing is staged, stage the intended work: `git add -A` for one coherent change, or add
  specific paths when only part of the tree belongs in this commit. Don't sweep in unrelated edits —
  **one commit = one coherent change**; split unrelated work into separate commits.

## 2. Write the message
The format mirrors the user's merged history — a terse **subject**, then the detail in the **body**:

- **Subject** — one line, ≤~70 chars, no trailing period. Concretely describe the change
  (`tighten README — single-line rows, correct apps/services paths`), never vague (`update`, `fix`,
  `wip`, `changes`, `misc`). Lead with the scope/area when it sharpens it. **No `type:` prefix, no AI
  attribution.**
- **Blank line**, then the **body** (wrap ~72 cols): explain *what changed and why* — the motivation,
  the tradeoff, the thing a reviewer can't infer from the diff. Prose and/or `-` bullets; a few lines
  is good. Don't narrate the diff line-by-line.

Multi-paragraph messages don't survive `-m` cleanly — **write the message to a temp file and commit
with `-F`**:
```
printf '%s\n\n%s\n' "<subject line>" "<body…>" > "$(git rev-parse --git-dir)/WTD_COMMITMSG"
git commit -F "$(git rev-parse --git-dir)/WTD_COMMITMSG"
```

Example (subject + body):
```
untrack dev-session scratch files leaked into repo

CLAUDE.md, pr-notes.md and the .claude-status sentinels were getting
committed from worktree sessions. Add them to .gitignore and git rm
--cached the tracked copies so they stop showing up in diffs.
```

## 3. Report
State the subject line and short hash (`git log --oneline -1`). Note any changes you deliberately left
uncommitted, and that it's committed but **not pushed** (use `/push` or `/pr` for that).
