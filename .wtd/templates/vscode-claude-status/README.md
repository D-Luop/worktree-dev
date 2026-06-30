# Claude Status

Recolors worktree folders in the VS Code Explorer based on a `.claude-status`
sentinel file written by Claude Code hooks.

- `input` → red ● (agent waiting on you)
- `working` → yellow … (agent running)
- `done` → green ✓ (agent finished turn)
- no file → no decoration

Watches `**/.claude-status` live, so the folder color updates the moment a hook
writes the file.
