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
# On Linux/WSL/mac without tmux on PATH, this falls back to the vscode backend — which was built
# and only ever exercised on native Windows Git Bash. That fallback is otherwise silent (the
# caller just sees "vscode" and has no idea tmux was the reason), so warn once per process.
wtd_session_backend() {
  if [ -n "${WTD_SESSION_BACKEND:-}" ]; then echo "$WTD_SESSION_BACKEND"; return; fi
  case "$(wtd_os)" in
    windows) echo vscode ;;
    *)
      if command -v tmux >/dev/null 2>&1; then
        echo tmux
      else
        if [ -z "${_WTD_TMUX_WARNED:-}" ]; then
          echo "WARN: tmux not found on PATH — falling back to the vscode session backend" \
               "(built for native Windows; untested on Linux/mac). Install tmux and re-run" \
               "'make install' to use the intended backend." >&2
          export _WTD_TMUX_WARNED=1
        fi
        echo vscode
      fi ;;
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

# wtd_git_perf_config <bare>  → cheaper `git status` in every worktree of this bare (config is shared):
# untrackedCache + manyFiles (index v4). Deliberately NOT core.fsmonitor: on a ~3k-file repo it measured
# no gain (status is ~200ms either way — dominated by process startup), and it costs an idle daemon per
# worktree whose directory handle blocks `git worktree move/remove` on Windows.
wtd_git_perf_config() {
  local bare="$1" g=(git -c safe.bareRepository=all -C "$1")
  [ -d "$bare" ] || return 0
  "${g[@]}" config core.untrackedCache true
  "${g[@]}" config feature.manyFiles true
  return 0
}

# Let git drive worktree-dev's own bare repos (repos/<slug>/.bare) even when the environment forces
# `safe.bareRepository=explicit`. VSCode injects exactly that via GIT_CONFIG_PARAMETERS, so any wtd
# script launched from VSCode (an extension button or an integrated terminal) would otherwise fail
# every `git -C <bare> worktree …` with "cannot use bare repository … safe.bareRepository is
# 'explicit'". We append our own `=all` last (GIT_CONFIG_PARAMETERS is last-wins, and -c/env beats
# global config), scoped to this process tree only — the user's global setting is untouched. Runs at
# source time so every script that sources platform-lib.sh is covered; idempotent.
case " ${GIT_CONFIG_PARAMETERS:-} " in
  *"'safe.bareRepository=all'"*) ;;
  *) export GIT_CONFIG_PARAMETERS="${GIT_CONFIG_PARAMETERS:+$GIT_CONFIG_PARAMETERS }'safe.bareRepository=all'" ;;
esac
