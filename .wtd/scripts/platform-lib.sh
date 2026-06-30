#!/usr/bin/env bash
# Platform detection + a session-backend resolver, shared by all worktree-dev scripts.
#
# worktree-dev runs on two session backends:
#   - tmux   : Linux / WSL / macOS — one tmux session per worktree (the original model).
#   - vscode : native Windows (Git Bash, no WSL, no tmux) — the claude-status VSCode
#              extension owns one integrated terminal per worktree; liveness is tracked
#              via a small on-disk session registry instead of `tmux list-sessions`.
#
# Everything platform-specific funnels through here + session-lib.sh, so the rest of the
# tooling stays backend-agnostic.

# wtd_os → windows | wsl | linux | mac  (echoed)
wtd_os() {
  case "$(uname -s 2>/dev/null)" in
    MINGW*|MSYS*|CYGWIN*) echo windows ;;
    Linux)
      # WSL reports Linux but exposes Microsoft in /proc/version
      if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null; then echo wsl; else echo linux; fi ;;
    Darwin) echo mac ;;
    *) echo linux ;;
  esac
}

# wtd_session_backend → tmux | vscode  (echoed)
# Default: vscode on native Windows, tmux everywhere else. Override with WTD_SESSION_BACKEND.
wtd_session_backend() {
  if [ -n "${WTD_SESSION_BACKEND:-}" ]; then echo "$WTD_SESSION_BACKEND"; return; fi
  case "$(wtd_os)" in
    windows) echo vscode ;;
    *)       command -v tmux >/dev/null 2>&1 && echo tmux || echo vscode ;;
  esac
}

# On-disk session registry (used by the vscode backend; also written on every platform so the
# extension has a tmux-independent source of truth). One file per live session.
#   $WTD/state/sessions/<session>   (TSV: slug \t name \t wt \t pid \t started_epoch)
wtd_state_dir()    { printf '%s/state' "${WTD:?WTD must be set}"; }
wtd_sessions_dir() { printf '%s/sessions' "$(wtd_state_dir)"; }

# Path to a VSCode user-settings.json for this OS (where machine-wide terminal prefs go).
# Windows native: %APPDATA%\Code\User\settings.json ; WSL/Linux server: ~/.vscode-server/data/Machine.
wtd_vscode_settings_path() {
  case "$(wtd_os)" in
    windows)
      local appdata="${APPDATA:-$HOME/AppData/Roaming}"
      printf '%s/Code/User/settings.json' "$appdata" ;;
    wsl)
      printf '%s/.vscode-server/data/Machine/settings.json' "$HOME" ;;
    *)
      printf '%s/.config/Code/User/settings.json' "$HOME" ;;
  esac
}

# Best-effort path to the Git-Bash executable (for the extension's terminal shellPath on Windows).
wtd_git_bash_path() {
  local c
  for c in "/c/Program Files/Git/bin/bash.exe" "/c/Program Files (x86)/Git/bin/bash.exe" \
           "$HOME/scoop/apps/git/current/bin/bash.exe"; do
    [ -x "$c" ] && { printf '%s' "$c"; return 0; }
  done
  command -v bash 2>/dev/null
}
