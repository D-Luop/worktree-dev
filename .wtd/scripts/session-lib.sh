#!/usr/bin/env bash
# Backend-agnostic session operations. agent.sh / close.sh / archive.sh call these instead of
# touching tmux directly, so the same code drives both backends:
#
#   tmux   (Linux/WSL/mac) — one tmux session per worktree, the original 4-pane layout.
#   vscode (Windows)       — the claude-status VSCode extension owns one integrated terminal per
#                            worktree; this script runs `claude` directly in that terminal (no tmux)
#                            and tracks liveness via the on-disk session registry.
#
# Requires platform-lib.sh to be sourced first (provides wtd_os / wtd_session_backend /
# wtd_sessions_dir) and $WTD set.

# --- session registry (vscode backend's source of truth; harmless on tmux too) ---------------
# Session names can contain '/' (e.g. "<slug>-feat/x") — fine for tmux, not for a filename — so the
# registry file encodes '/' as '__'. The extension decodes it back when reading the registry.
wtd_session_keyfile() { printf '%s/%s' "$(wtd_sessions_dir)" "${1//\//__}"; }

# --- durable per-worktree Claude session id (separate from the liveness registry) ------------
# The registry above is liveness state — deleted on close. The session id below is DURABLE: it lets
# reopening a worktree RESUME the same Claude conversation (the live process never survives a reboot,
# but Claude persists every transcript to disk, so we just relaunch with --resume). Kept in the wtd
# state tree (not the worktree → no git-dirty noise) and keyed exactly like the registry.
wtd_session_idsdir()  { printf '%s/session-ids' "$(wtd_state_dir)"; }
wtd_session_idfile()  { printf '%s/%s' "$(wtd_session_idsdir)" "${1//\//__}"; }
wtd_session_id_forget() { rm -f "$(wtd_session_idfile "$1")" 2>/dev/null || true; }

# wtd_uuid → a fresh UUID, using whatever's available (python on Windows; else uuidgen/kernel/PowerShell)
wtd_uuid() {
  if command -v python  >/dev/null 2>&1; then python  -c 'import uuid;print(uuid.uuid4())' && return; fi
  if command -v python3 >/dev/null 2>&1; then python3 -c 'import uuid;print(uuid.uuid4())' && return; fi
  if [ -r /proc/sys/kernel/random/uuid ]; then cat /proc/sys/kernel/random/uuid && return; fi
  if command -v uuidgen >/dev/null 2>&1; then uuidgen | tr 'A-Z' 'a-z' && return; fi
  command -v powershell >/dev/null 2>&1 && powershell -NoProfile -Command '[guid]::NewGuid().ToString()' | tr -d '\r'
}

# wtd_claude_transcript <wt> <session-id> → where Claude would store that id's transcript for this
# worktree. Claude writes ~/.claude/projects/<cwd, every non-alphanumeric char replaced by '-'>/<id>.jsonl
# (the cwd is the WINDOWS path on the vscode backend, so convert it). Lets us tell resume from start-new.
wtd_claude_transcript() {
  local wt="$1" id="$2" cwd enc
  cwd="$(cygpath -w "$wt" 2>/dev/null || printf '%s' "$wt")"
  enc="$(printf '%s' "$cwd" | sed 's/[^A-Za-z0-9]/-/g')"
  printf '%s/.claude/projects/%s/%s.jsonl' "$HOME" "$enc" "$id"
}

# wtd_session_register <session> <slug> <name> <wt> [pid]
wtd_session_register() {
  local session="$1" slug="$2" name="$3" wt="$4" pid="${5:-$$}"
  mkdir -p "$(wtd_sessions_dir)"
  printf '%s\t%s\t%s\t%s\t%s\n' "$slug" "$name" "$wt" "$pid" "$(date +%s 2>/dev/null || echo 0)" \
    > "$(wtd_session_keyfile "$session")"
}
wtd_session_deregister() { rm -f "$(wtd_session_keyfile "$1")" 2>/dev/null || true; }
wtd_session_registered() { [ -f "$(wtd_session_keyfile "$1")" ]; }

# --- existence ------------------------------------------------------------------------------
# wtd_session_exists <session>  → 0 if a live session by that name exists
wtd_session_exists() {
  case "$(wtd_session_backend)" in
    tmux) tmux has-session -t "=$1" 2>/dev/null ;;
    *)    wtd_session_registered "$1" ;;
  esac
}

# --- listing --------------------------------------------------------------------------------
# wtd_session_list  → prints one line per live session:  <session>\t<attached yes/no>\t<status>
wtd_session_list() {
  case "$(wtd_session_backend)" in
    tmux)
      tmux list-sessions -F '#{session_name}'"$(printf '\t')"'#{?session_attached,yes,no}' 2>/dev/null \
        | sort | while IFS=$'\t' read -r s att; do
            local st; st="$(tmux show -t "$s" -v @wt_status 2>/dev/null)"
            printf '%s\t%s\t%s\n' "$s" "$att" "${st:-·}"
          done
      ;;
    *)
      local dir; dir="$(wtd_sessions_dir)"
      [ -d "$dir" ] || return 0
      for f in "$dir"/*; do
        [ -e "$f" ] || continue
        local session slug name wt st=""
        session="$(basename "$f")"; session="${session//__//}"   # decode the filename → session name
        IFS=$'\t' read -r slug name wt _ _ < "$f"
        [ -n "${wt:-}" ] && st="$(cat "$wt/.claude-status" 2>/dev/null || echo '·')"
        printf '%s\t%s\t%s\n' "$session" "-" "${st:-·}"
      done | sort
      ;;
  esac
}

# wtd_session_live_names  → just the live session names, one per line (used by the extension too)
wtd_session_live_names() {
  case "$(wtd_session_backend)" in
    tmux) tmux list-sessions -F '#{session_name}' 2>/dev/null ;;
    *)    local dir; dir="$(wtd_sessions_dir)"; [ -d "$dir" ] && ls -1 "$dir" 2>/dev/null | sed 's#__#/#g' || true ;;
  esac
}

# --- current session (run from inside one) --------------------------------------------------
# wtd_session_current  → the current session name, or empty
wtd_session_current() {
  case "$(wtd_session_backend)" in
    tmux) [ -n "${TMUX:-}" ] && tmux display-message -p '#S' 2>/dev/null || true ;;
    *)    printf '%s' "${WTD_SESSION:-}" ;;   # set in the worktree terminal's environment
  esac
}

# --- teardown -------------------------------------------------------------------------------
# wtd_session_kill <session> [wt]  → end the session, keep the worktree
wtd_session_kill() {
  local session="$1" wt="${2:-}"
  case "$(wtd_session_backend)" in
    tmux) tmux kill-session -t "=$session" 2>/dev/null ;;
    *)
      local f pid; f="$(wtd_session_keyfile "$session")"
      if [ -f "$f" ]; then
        IFS=$'\t' read -r _ _ _ pid _ < "$f"
        # best-effort stop the claude process tree so the VSCode terminal returns to a shell.
        if [ -n "${pid:-}" ]; then
          if command -v taskkill >/dev/null 2>&1; then taskkill //PID "$pid" //T //F >/dev/null 2>&1 || true
          else kill "$pid" 2>/dev/null || true; fi
        fi
      fi
      wtd_session_deregister "$session"
      ;;
  esac
}

# --- creation / launch ----------------------------------------------------------------------
# On Windows there are no tmux panes: agent.sh calls wtd_session_run_claude to exec claude in the
# current (VSCode-provided) terminal. On tmux, agent.sh keeps its own pane-building code path.
# wtd_session_run_claude <session> <slug> <name> <wt> <ccdir-or-empty> <permission-mode> <launch 0|1>
wtd_session_run_claude() {
  local session="$1" slug="$2" name="$3" wt="$4" ccdir="$5" pmode="$6" launch="$7"
  wtd_session_register "$session" "$slug" "$name" "$wt" "$$"
  # deregister when this shell (the claude process host) exits, so liveness is accurate.
  trap 'wtd_session_deregister "'"$session"'"' EXIT
  export WTD_SESSION="$session"
  cd "$wt" || return 1
  # refresh the worktree's status glyph for the extension roster
  CLAUDE_PROJECT_DIR="$wt" "$WTD/hooks/wt-status.sh" sync </dev/null 2>/dev/null || true
  if [ "$launch" != 1 ]; then
    echo "worktree ready: $wt  (run 'claude' when ready)"
    exec "${SHELL:-bash}" -i
  fi
  if ! command -v claude >/dev/null 2>&1; then
    echo "note: 'claude' not on PATH; leaving a shell in the worktree"
    exec "${SHELL:-bash}" -i
  fi
  [ -n "$ccdir" ] && export CLAUDE_CONFIG_DIR="$ccdir"

  # Per-worktree session continuity: a durable id makes reopening this worktree RESUME its Claude
  # conversation instead of starting cold — including after a reboot (the process dies, the transcript
  # doesn't). First open mints an id and starts with --session-id; later opens pass --resume once that
  # id's transcript is actually on disk, else re-create it under the same id.
  # NB: keep every step here from tripping `set -e` (a failed `cat`/`wtd_uuid` must NOT abort the
  # launch, or the terminal would just close instead of starting claude).
  local idf id=""
  idf="$(wtd_session_idfile "$session")"
  [ -f "$idf" ] && id="$(cat "$idf" 2>/dev/null || true)"
  if [ -n "$id" ]; then
    if [ -f "$(wtd_claude_transcript "$wt" "$id")" ]; then
      exec claude --permission-mode "$pmode" --resume "$id"
    fi
    exec claude --permission-mode "$pmode" --session-id "$id"
  fi
  id="$(wtd_uuid || true)"
  if [ -n "$id" ]; then
    mkdir -p "$(wtd_session_idsdir)"
    printf '%s\n' "$id" > "$idf"
    exec claude --permission-mode "$pmode" --session-id "$id"
  fi
  exec claude --permission-mode "$pmode"   # no uuid tool available → plain session (unchanged behavior)
}
