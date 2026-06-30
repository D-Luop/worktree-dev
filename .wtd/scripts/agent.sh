#!/usr/bin/env bash
# Launch (or re-attach) an agent worktree+tmux session for a given repo.
# Usage: agent <repo-slug> <name> [--from <ref>] [ref-token ...]
#   <name>        : if it matches an existing branch (local or remote) that branch is
#                   checked out; otherwise a new branch <name> is created.
#   --from <ref>  : when creating a NEW branch, base it off <ref> (a premade branch/tag/sha,
#                   e.g. --from release-2.0). Ignored if <name> already exists.
#   ref-token     : read-only cross-repo context: <slug> or <slug>@<branch>. Each is a
#                   detached checkout at ~/dev/refs/<slug>/<branch>, wired into the worktree's
#                   .claude/settings.local.json (additionalDirectories + Edit/Write deny) and
#                   announced in CLAUDE.md.
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
# shipped repo-hooks use __DEV__ placeholders (portability); render to this machine's dev root.
wtd_render_dev() { sed "s#__DEV__#$DEV#g" "$1"; }
REG="$WTD/repos.tsv"
# shared ref-checkout helpers: registered, default_branch, parse_ref_token, ensure_ref
# shellcheck source=refs-lib.sh
. "$WTD/scripts/refs-lib.sh"
# shellcheck source=account-lib.sh
. "$WTD/scripts/account-lib.sh"          # account_dir_for_role/name, account_name_for_role
# platform detection + backend-agnostic session ops (tmux on Linux/WSL/mac, VSCode terminals on Windows)
# shellcheck source=platform-lib.sh
. "$WTD/scripts/platform-lib.sh"
# shellcheck source=session-lib.sh
. "$WTD/scripts/session-lib.sh"

# --- teardown subcommand:  agent rm <slug> <name> [--branch] [--force] [-y] ---
# Kill the tmux session, remove the worktree, and (with --branch) delete the branch.
if [ "${1:-}" = "rm" ]; then
  shift
  rmrepo="${1:-}"; rmname="${2:-}"
  [ "$#" -ge 2 ] && shift 2 || shift "$#"
  del_branch=0; force=""; assume_yes=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --branch|-b) del_branch=1; shift;;
      --force|-f)  force="--force"; shift;;
      --yes|-y)    assume_yes=1; shift;;
      *) echo "agent rm: unknown option '$1'"; exit 1;;
    esac
  done
  if [ -z "$rmrepo" ] || [ -z "$rmname" ]; then
    echo "usage: agent rm <slug> <name> [--branch] [--force] [-y]"; exit 1
  fi
  registered "$rmrepo" || { echo "error: repo slug '$rmrepo' is not registered."; exit 1; }
  rmbare="$DEV/repos/$rmrepo/.bare"
  rmwt="$DEV/worktrees/$rmrepo/$rmname"
  rmsession="${rmrepo}-${rmname}"; rmsession="${rmsession//[.:]/-}"

  echo "About to remove:"
  printf '  session      : %-32s %s\n' "$rmsession" "$(wtd_session_exists "$rmsession" && echo '(running)' || echo '(none)')"
  printf '  worktree     : %-32s %s\n' "$rmwt" "$([ -d "$rmwt" ] && echo '' || echo '(missing)')"
  [ "$del_branch" = 1 ] && printf '  branch       : %-32s %s\n' "$rmname" "(force-deleted)"
  [ -n "$force" ] && echo "  forcing: any uncommitted changes in the worktree will be discarded"
  if [ "$assume_yes" != 1 ]; then
    printf 'Proceed? [y/N] '; read -r ans || ans=""
    case "$ans" in y|Y|yes|YES) ;; *) echo "aborted"; exit 1;; esac
  fi

  wtd_session_kill "$rmsession" "$rmwt" && echo "killed session $rmsession" || echo "no running session $rmsession"
  if [ -d "$rmwt" ]; then
    # A session we just killed can keep file handles open for a moment on Windows, so `git worktree
    # remove` fails with "Permission denied"/"used by another process". Retry a few times to let the
    # OS release the handles; bail immediately on any other error (e.g. a real dirty-tree refusal).
    out=""; removed=0
    for _attempt in 1 2 3 4 5; do
      if out="$(git -C "$rmbare" worktree remove $force "$rmwt" 2>&1)"; then removed=1; break; fi
      case "$out" in *"Permission denied"*|*"used by another process"*) sleep 1 ;; *) break ;; esac
    done
    if [ "$removed" = 1 ]; then
      echo "removed worktree $rmwt"
    else
      echo "worktree remove failed: $out"
      case "$out" in
        *"Permission denied"*|*"used by another process"*)
          echo "  (a process is still using the worktree — close its VSCode terminal/editor tab, then retry)" ;;
        *) echo "  (uncommitted changes? re-run with --force to discard, or commit/push first)" ;;
      esac
      exit 1
    fi
  else
    git -C "$rmbare" worktree prune 2>/dev/null || true
    echo "worktree not present; pruned stale entries"
  fi
  wtd_session_id_forget "$rmsession"   # only now (worktree actually gone) drop the durable resume id
  if [ "$del_branch" = 1 ]; then
    if git -C "$rmbare" show-ref --verify --quiet "refs/heads/$rmname"; then
      git -C "$rmbare" branch -D "$rmname" && echo "deleted branch $rmname"
    else
      echo "no local branch '$rmname' to delete"
    fi
  fi
  echo "done."
  exit 0
fi

# --- status subcommand:  agent done | agent pr | agent wip   (run from INSIDE a worktree) ---
# Sets the .claude-status sentinel the VSCode claude-status extension reads:
#   done -> green ✓ (sticky: survives Stop/SessionEnd; cleared when you submit new work)
#   pr   -> light-blue 🔹 PR-ready (sticky like done; sorts just below working, above done)
#   wip  -> working/blue (manual revert; new prompts also revert automatically)
# This is what the agent runs at the matching milestone (work finished / ready for PR).
if [ "${1:-}" = "done" ] || [ "${1:-}" = "pr" ] || [ "${1:-}" = "wip" ] || [ "${1:-}" = "working" ]; then
  ev="$1"; [ "$ev" = "working" ] && ev="wip"
  "$WTD/hooks/wt-status.sh" "$ev" </dev/null      # writes .claude-status + updates tmux tab glyph
  root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
  echo "marked $(basename "$root") as '$(cat "$root/.claude-status" 2>/dev/null)'"
  exit 0
fi

# --- list subcommand:  agent ls   (show active worktree tmux sessions) ---
if [ "${1:-}" = "ls" ] || [ "${1:-}" = "sessions" ] || [ "${1:-}" = "ps" ]; then
  printf '%-34s %-9s %s\n' "SESSION" "ATTACHED" "STATUS"
  printf '%-34s %-9s %s\n' "----------------------------------" "--------" "------"
  rows="$(wtd_session_list)"
  if [ -n "$rows" ]; then
    printf '%s\n' "$rows" | while IFS=$'\t' read -r s att st; do
      printf '%-34s %-9s %s\n' "$s" "$att" "${st:-·}"
    done
  else
    echo "(no live sessions)"
  fi
  echo
  echo "end one with:  agent stop <slug> <name>   (kills the session, keeps the worktree)"
  exit 0
fi

# --- stop subcommand:  agent stop <slug> <name>   (kill the tmux session, KEEP the worktree) ---
if [ "${1:-}" = "stop" ] || [ "${1:-}" = "kill" ]; then
  shift
  sslug="${1:-}"; sname="${2:-}"
  if [ -z "$sslug" ] || [ -z "$sname" ]; then
    # no args: end the CURRENT session (run from inside the session you want to close)
    cur="$(wtd_session_current)"
    if [ -n "$cur" ]; then
      sroot="$(git rev-parse --show-toplevel 2>/dev/null)"
      [ -n "$sroot" ] && CLAUDE_PROJECT_DIR="$sroot" "$WTD/hooks/wt-status.sh" sessionend </dev/null
      echo "ending current session '$cur' (worktree kept → red 'stopped' unless done)"
      wtd_session_kill "$cur" "$sroot"; exit 0
    fi
    echo "usage: agent stop [<slug> <name>]   (no args = end the current session, from inside it)"
    exit 1
  fi
  ssession="${sslug}-${sname}"; ssession="${ssession//[.:]/-}"
  swt="$DEV/worktrees/$sslug/$sname"
  [ -d "$swt" ] && CLAUDE_PROJECT_DIR="$swt" "$WTD/hooks/wt-status.sh" sessionend </dev/null
  if wtd_session_exists "$ssession"; then
    wtd_session_kill "$ssession" "$swt"
    echo "stopped session '$ssession' (worktree kept → red 'stopped' unless done). Re-open: agent $sslug $sname"
  else
    echo "no running session '$ssession'."
  fi
  exit 0
fi

repo="${1:-}"
name="${2:-}"
[ "$#" -ge 2 ] && shift 2 || shift "$#"
from=""
account=""         # --account <name>: run this session under a different Claude login (its own
                   # CLAUDE_CONFIG_DIR) so ALL of the session's usage/cost bills to that account.
refs=()            # reference tokens (cross-repo context)
# Auto-launch claude in the session's main pane on creation; opt out with --no-claude or
# AGENT_NO_CLAUDE=1 (e.g. when you just want a shell in the worktree).
launch_claude=1; [ -n "${AGENT_NO_CLAUDE:-}" ] && launch_claude=0
# Launch claude in "auto" permission mode by default (auto-accept edits). Opt out per-call with
# --no-auto, or globally with AGENT_PERMISSION_MODE=default (or plan / acceptEdits / bypassPermissions).
pmode="${AGENT_PERMISSION_MODE:-auto}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --from)   [ "$#" -ge 2 ] || { echo "error: --from requires a <ref>"; exit 1; }; from="$2"; shift 2;;
    --from=*) from="${1#*=}"; shift;;
    --account|-a) [ "$#" -ge 2 ] || { echo "error: --account requires a <name>"; exit 1; }; account="$2"; shift 2;;
    --account=*)  account="${1#*=}"; shift;;
    --no-auto)   pmode=default; shift;;          # start claude in normal (ask) permission mode
    --mode)   [ "$#" -ge 2 ] || { echo "error: --mode requires a value"; exit 1; }; pmode="$2"; shift 2;;
    --no-claude) launch_claude=0; shift;;
    *)        refs+=("$1"); shift;;
  esac
done

# Resolve the account's config dir (a separate Claude login). Precedence: --account flag, else the
# configured 'dev' role (account use dev <name>), else empty = the default ~/.claude account.
ccdir=""; account_label="$account"
if [ -n "$account" ]; then
  ccdir="$(account_dir_for_name "$account")"
  [ -n "$ccdir" ] || { echo "error: no Claude account '$account'."; echo "       create it with:  account add $account"; exit 1; }
else
  ccdir="$(account_dir_for_role dev)"        # configured default for dev sessions (empty = ~/.claude)
  [ -n "$ccdir" ] && account_label="$(account_name_for_role dev)"
fi

list_repos() {
  echo "registered repos:"
  awk -F'\t' '!/^#/ && NF>=1 && $1!="" {printf "  %s\t%s\n", $1, $2}' "$REG" 2>/dev/null \
    || echo "  (none — add one with: add-repo <slug> <git-url>)"
}

if [ -z "$repo" ] || [ -z "$name" ]; then
  echo "usage: agent <repo-slug> <name> [ref-slug ...]"
  echo "       agent ls                                               (list active sessions)"
  echo "       agent stop [<slug> <name>]                              (end session, keep worktree; no args = current)"
  echo "       agent done | agent pr | agent wip                      (mark worktree status; from inside it)"
  echo "       agent rm <slug> <name> [--branch] [--force] [-y]       (tear down worktree)"
  echo
  list_repos
  exit 1
fi

# repo must be registered + cloned
if ! registered "$repo"; then
  echo "error: repo slug '$repo' is not registered."; echo
  list_repos
  exit 1
fi
bare="$DEV/repos/$repo/.bare"
if [ ! -d "$bare" ]; then
  url="$(awk -F'\t' -v s="$repo" '!/^#/ && $1==s{print $2; exit}' "$REG")"
  echo "error: repo '$repo' is registered but not cloned yet."
  echo "       run: add-repo $repo ${url:-<git-url>}"
  exit 1
fi

# tmux session names can't contain '.' or ':'; namespace by repo so names can repeat across repos
session="${repo}-${name}"
session="${session//[.:]/-}"
wt="$DEV/worktrees/$repo/$name"

# --- if this name is ARCHIVED, offer to reopen it instead of creating a new worktree ---
arc="$DEV/worktrees/$repo/archive/$name"
if [ ! -d "$wt" ] && [ -d "$arc" ]; then
  echo "An archived worktree for '$name' exists at: $arc"
  printf 'Reopen it (move it back into the active worktrees)? [Y/n] '
  read -r ans </dev/tty || ans=""
  case "$ans" in
    n|N|no|NO)
      echo "left it archived. (Use a different name, or 'archive' again later.)"; exit 1;;
    *)
      mkdir -p "$(dirname "$wt")"
      git -c safe.bareRepository=all -C "$bare" worktree move "$arc" "$wt" && echo "reopened from archive → $wt"
      # tidy now-empty worktrees/<slug>/archive parent dirs
      rmdir -p "$(dirname "$arc")" 2>/dev/null || true ;;
  esac
fi

# --- create worktree on first call ---
if [ ! -d "$wt" ]; then
  git -c safe.bareRepository=all -C "$bare" fetch origin
  if git -c safe.bareRepository=all -C "$bare" show-ref --verify --quiet "refs/heads/$name"; then
    [ -n "$from" ] && echo "note: local branch '$name' already exists; ignoring --from"
    git -c safe.bareRepository=all -C "$bare" worktree add "$wt" "$name"                        # existing local branch
  elif git -c safe.bareRepository=all -C "$bare" show-ref --verify --quiet "refs/remotes/origin/$name"; then
    [ -n "$from" ] && echo "note: remote branch 'origin/$name' already exists; ignoring --from"
    git -c safe.bareRepository=all -C "$bare" worktree add --track -b "$name" "$wt" "origin/$name"   # premade remote branch
  else
    # brand-new branch: base off --from if given, else the remote's default branch
    if [ -n "$from" ]; then
      if   git -c safe.bareRepository=all -C "$bare" rev-parse --verify --quiet "refs/remotes/origin/$from" >/dev/null; then base="origin/$from"
      elif git -c safe.bareRepository=all -C "$bare" rev-parse --verify --quiet "$from" >/dev/null;                      then base="$from"
      else echo "error: --from ref '$from' not found in '$repo' (tried origin/$from and $from)"; exit 1; fi
    else
      base="$(git -c safe.bareRepository=all -C "$bare" symbolic-ref --quiet refs/remotes/origin/HEAD 2>/dev/null | sed 's@^refs/remotes/@@')"
      base="${base:-origin/main}"
    fi
    git -c safe.bareRepository=all -C "$bare" worktree add "$wt" -b "$name" "$base"            # new branch off "$base"
  fi
  # seed: stub CLAUDE.md + .claude scaffolding (template skills + plans dir); rely on global hooks
  mkdir -p "$wt/.claude/skills" "$wt/.claude/plans"
  [ -f "$wt/CLAUDE.md" ] || cp "$WTD/templates/CLAUDE.md" "$wt/CLAUDE.md"
  cp -r "$WTD/templates/.claude/skills/." "$wt/.claude/skills/" 2>/dev/null || true

  # seed gitignored host-local env files from .wtd/env/<slug>/ (e.g. .env, .devcontainer/.env).
  # The stash mirrors the worktree layout: each file is copied to the same relative path, but
  # only when the destination doesn't already exist (never clobber an edited env). Generated
  # files like .devcontainer/.env.hash are intentionally NOT stashed — dc.sh rewrites them.
  envsrc="$WTD/env/$repo"
  if [ -d "$envsrc" ]; then
    while IFS= read -r -d '' f; do
      rel="${f#"$envsrc"/}"
      dest="$wt/$rel"
      [ -e "$dest" ] && continue
      mkdir -p "$(dirname "$dest")"
      cp "$f" "$dest" && echo "seeded env: $rel"
    done < <(find "$envsrc" -type f -print0)
  fi

  # never commit worktree-dev scratch artifacts: add them to this repo's bare exclude (idempotent)
  exclude="$bare/info/exclude"
  if [ -f "$exclude" ]; then
    for ign in CLAUDE.md pr-notes.md .claude-status .claude-status.resume; do
      grep -qxF "$ign" "$exclude" && continue
      [ -s "$exclude" ] && [ -n "$(tail -c1 "$exclude")" ] && printf '\n' >> "$exclude"
      printf '%s\n' "$ign" >> "$exclude"
    done
  fi
fi

# --- per-repo hooks: merge .wtd/repo-hooks/<slug>.json into the worktree (idempotent) ---
hookfrag="$WTD/repo-hooks/$repo.json"
if [ -f "$hookfrag" ]; then
  hookfrag="$(mktemp)"; wtd_render_dev "$WTD/repo-hooks/$repo.json" > "$hookfrag"
  proj="$wt/.claude/settings.json"
  mkdir -p "$wt/.claude"
  [ -f "$proj" ] || echo '{}' > "$proj"
  hookcmd="$(jq -r '[.. | objects | .command? // empty] | .[0] // empty' "$hookfrag")"
  if [ -z "$hookcmd" ] || ! grep -qF "$hookcmd" "$proj" 2>/dev/null; then
    tmp=$(mktemp)
    jq --slurpfile frag "$hookfrag" '
      .hooks = (.hooks // {})
      | reduce ($frag[0].hooks | to_entries[]) as $e (.; .hooks[$e.key] = ((.hooks[$e.key] // []) + $e.value))
    ' "$proj" > "$tmp" && mv "$tmp" "$proj"
    echo "wired repo hooks for '$repo' into .claude/settings.json"
  fi
fi

# --- reference repos/branches: read-only cross-repo context ---
# Each ref token is  <slug>  (default branch)  or  <slug>@<branch>.
# Access is GLOBAL (install.sh grants read + Edit/Write deny on the whole ~/dev/refs tree in
# ~/.claude/settings.json), so here we only (a) ensure the shared checkout exists via the same
# `ref add` lifecycle, and (b) ANNOUNCE these refs in this worktree's CLAUDE.md so the agent is
# aware of them. The CLAUDE.md block is ADDITIVE across runs — re-running `agent` with a new ref
# token appends it rather than dropping the previously announced ones.
if [ "${#refs[@]}" -gt 0 ]; then
  refpaths=(); reflabels=()
  for token in "${refs[@]}"; do
    parse_ref_token "$token"   # -> REF_SLUG / REF_BRANCH
    if ! registered "$REF_SLUG" || [ ! -d "$DEV/repos/$REF_SLUG/.bare" ]; then
      echo "warning: skipping reference '$token' (repo '$REF_SLUG' not registered/cloned)"; continue
    fi
    if p="$(ensure_ref "$REF_SLUG" "$REF_BRANCH")"; then
      refpaths+=("$p"); reflabels+=("$REF_SLUG@$REF_BRANCH")
    fi
  done
  if [ "${#refpaths[@]}" -gt 0 ]; then
    cmd="$wt/CLAUDE.md"
    [ -f "$cmd" ] || cp "$WTD/templates/CLAUDE.md" "$cmd"
    # Gather already-announced entries (label -> path) so the block accumulates across runs.
    declare -A REFMAP=()
    while IFS= read -r line; do
      if [[ "$line" =~ ^-\ \*\*(.+)\*\*\ →\ \`(.+)\`$ ]]; then
        REFMAP["${BASH_REMATCH[1]}"]="${BASH_REMATCH[2]}"
      fi
    done < <(sed -n '/<!-- BEGIN agent-references -->/,/<!-- END agent-references -->/p' "$cmd" 2>/dev/null)
    for i in "${!refpaths[@]}"; do REFMAP["${reflabels[$i]}"]="${refpaths[$i]}"; done

    tmp=$(mktemp)
    sed '/<!-- BEGIN agent-references -->/,/<!-- END agent-references -->/d' "$cmd" > "$tmp" && mv "$tmp" "$cmd"
    {
      printf '\n<!-- BEGIN agent-references -->\n'
      printf '## Reference repos/branches (read-only context)\n\n'
      printf 'The repo@branch checkouts below are available READ-ONLY for cross-repo context. '
      printf 'Read / grep / glob them freely; do NOT edit them (Write/Edit are denied):\n\n'
      for label in $(printf '%s\n' "${!REFMAP[@]}" | sort); do
        printf -- '- **%s** → `%s`\n' "$label" "${REFMAP[$label]}"
      done
      printf '<!-- END agent-references -->\n'
    } >> "$cmd"

    echo "references announced in CLAUDE.md (read access is global via ~/.claude/settings.json):"
    for i in "${!refpaths[@]}"; do printf '  %-24s %s\n' "${reflabels[$i]}" "${refpaths[$i]}"; done
  fi
fi

# --- vscode backend (Windows / no tmux): run claude directly in the terminal the extension opened.
# There are no tmux panes; the commit/diff surfaces come from VSCode's native SCM/diff + the panel.
if [ "$(wtd_session_backend)" != tmux ]; then
  if [ -n "$ccdir" ]; then echo "session '$session' → Claude account '$account_label' ($ccdir)"; fi
  # wtd_session_run_claude registers the session, exports WTD_SESSION, cd's to the worktree, and
  # exec's claude (replacing this shell). The trap it sets deregisters on exit so liveness is accurate.
  wtd_session_run_claude "$session" "$repo" "$name" "$wt" "$ccdir" "$pmode" "$launch_claude"
  exit 0   # safety net: wtd_session_run_claude exec's, so we never get here
fi

# --- tmux: one session per worktree ---
# Layout:  left column (~52%) = [command pane (top ~16%)] + [claude (below)];
#          right column (~48%, repos with a diff hook) = [commit pane (top ~16%)] + [diff pane].
# The command pane (left) and commit pane (right) are the SAME height so their tops align into one band.
# Pane IDs (%N) are used throughout — adding the top command pane renumbers index .0, so never
# reference panes by index here.
if ! tmux has-session -t "=$session" 2>/dev/null; then
  main_pane=$(tmux new-session -d -s "$session" -c "$wt" -P -F '#{pane_id}')   # becomes the claude pane
  # repos with a hook fragment get a right column that live-tails their colored diff log.
  hookfrag="$WTD/repo-hooks/$repo.json"
  if [ -f "$hookfrag" ]; then
    hookfrag="$(mktemp)"; wtd_render_dev "$WTD/repo-hooks/$repo.json" > "$hookfrag"
    rel_log="$(jq -r '._diffLog // ".claude/diffs.log"' "$hookfrag")"
    diffs_log="$wt/$rel_log"
    diffcmd="$(jq -r '[.. | objects | .command? // empty] | .[0] // empty' "$hookfrag")"
    mkdir -p "$(dirname "$diffs_log")"; : > "$diffs_log"
    [ -n "$diffcmd" ] && [ -x "$diffcmd" ] && "$diffcmd" --seed "$wt" >/dev/null 2>&1 || true
    # diff pane (right column): live-tails the colored diff log. @dpane/@dlog let the commit-click
    # helper repaint it with a specific commit's diff (and exec back here on q).
    diff_pane=$(tmux split-window -d -h -l 48% -P -F '#{pane_id}' -t "$main_pane" -c "$wt" "$WTD/hooks/diff-pane.sh '$wt' '$rel_log'")
    tmux set -p -t "$diff_pane" @dpane 1
    tmux set -p -t "$diff_pane" @dlog "$rel_log"
    # commit-history pane stacked ABOVE the diff pane (~20% tall, aligned with the command pane).
    commit_pane=$(tmux split-window -d -v -b -l 20% -P -F '#{pane_id}' -t "$diff_pane" -c "$wt" "$WTD/hooks/commit-pane.sh '$wt'")
    tmux set -p -t "$commit_pane" @cpane 1
  fi
  # COMMAND pane above the claude pane (left column, ~20% height — aligned with the commit pane): a
  # plain shell for running close / wt-review / archive / build / agent ls directly, without claude.
  cmd_pane=$(tmux split-window -d -v -b -l 20% -P -F '#{pane_id}' -t "$main_pane" -c "$wt")
  tmux set -p -t "$cmd_pane" @cmdpane 1
  tmux send-keys -t "$cmd_pane" "clear; printf '\\033[1;90mcommands: close · wt-review [--main] · archive <slug> <name> · agent ls\\033[0m\\n'" C-m
  tmux set -p -t "$main_pane" @claudepane 1   # so double-click-a-SHA-to-diff works in the chat too
  tmux select-pane -t "$main_pane"
  # If a non-default account was requested, point this whole session at its CLAUDE_CONFIG_DIR so the
  # session (and all its usage/cost) runs under that login. set-environment covers future panes/shells;
  # the launch below also sets it inline so the very first claude picks it up.
  if [ -n "$ccdir" ]; then
    tmux set-environment -t "$session" CLAUDE_CONFIG_DIR "$ccdir"
    tmux set -t "$session" @wt_account "$account_label" 2>/dev/null || true
    echo "session '$session' → Claude account '$account_label' ($ccdir)"
  fi
  # auto-launch claude in the main pane (only on first creation), in "$pmode" permission mode
  # (default: auto). Opt out of launch: --no-claude; of auto: --no-auto.
  if [ "$launch_claude" = 1 ]; then
    if command -v claude >/dev/null 2>&1; then
      cli="claude --permission-mode $(printf %q "$pmode")"
      if [ -n "$ccdir" ]; then tmux send-keys -t "$main_pane" "CLAUDE_CONFIG_DIR=$(printf %q "$ccdir") $cli" C-m
      else                     tmux send-keys -t "$main_pane" "$cli" C-m; fi
    else
      echo "note: 'claude' not on PATH; left a shell in the worktree"
    fi
  fi
fi
# Label the session for the terminal tab title. set-titles-string (wired by install.sh) reads
# this @wt_label and falls back to the session name; VSCode shows it when its tab title uses
# ${sequence}. Use just the worktree name (no repo slug) so the tab reads e.g. "feat/job-setup".
tmux set -t "$session" @wt_label "$name" 2>/dev/null || true
# Sync the tab glyph from the worktree's current .claude-status (e.g. a reopened 'done' worktree
# should show 🟢, not nothing) — a no-op on the file, just refreshes @wt_status.
CLAUDE_PROJECT_DIR="$wt" "$WTD/hooks/wt-status.sh" sync </dev/null 2>/dev/null || true

if [ -n "${TMUX:-}" ]; then
  tmux switch-client -t "$session"      # already inside tmux: can't nest attach
else
  tmux attach -t "$session"
fi
