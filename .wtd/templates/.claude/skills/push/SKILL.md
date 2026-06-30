---
name: push
description: >
  Push this worktree branch's commits to origin. Use when the user says "push", "push it", "push to
  remote", or invokes /push. Just pushes committed work to the remote branch — it does not commit,
  review, or build.
allowed-tools: Bash
---

# Push this branch to its remote

1. Confirm you're on a branch (not detached): `git branch --show-current`. If empty (detached HEAD),
   stop and tell the user — there's nothing to push to.
2. Push the current branch to origin, setting upstream on the first push (idempotent afterwards):
   ```
   git push -u origin HEAD
   ```
3. Report the result concisely: the branch name and that it's pushed (or "everything up to date").
   - If `git status --porcelain` shows **uncommitted** changes, note that they were NOT pushed (only
     committed work is) — don't auto-commit.
   - PR hint by host: for GitHub repos suggest `gh pr create`; for other hosts (e.g. Bitbucket,
     GitLab) open the PR via the web/API — `gh pr create` won't work there.

That's it — just the push. Don't run a review or build unless the user asks.
