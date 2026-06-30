---
name: close
description: >
  Close THIS worktree's tmux session (end all its panes), keeping the worktree on disk. Use when the
  user says close / stop / "end this session" / "close the terminal" or invokes /close. Does NOT
  remove the worktree or branch — that's `agent rm`.
allowed-tools: Bash
---

# Close this tmux session

Run the command:
```
close
```
It ends the current session (all panes) but keeps the worktree, branch, and uncommitted changes on
disk — reopen later with `agent <slug> <name>`. The session (and this Claude process) terminates
immediately, so don't expect to report anything afterward. Do NOT use `agent rm` (that deletes the
worktree).
