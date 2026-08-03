#!/usr/bin/env bash
# Pre-push reviewer: review a worktree's unpushed changes with a SEPARATE Claude agent that
# runs OUTSIDE the worktree (read-only access to it). Produces two markdown reports:
#   review.md     — findings on documentation, repo standards, efficient/minimal code (+ precedents)
#   highlights.md — repo-aware "changes worth noting" (e.g. a repo's SQL + proto + integral code)
#
# Headless runs do TWO passes: (1) the `reviewer` agent writes the reports, then (2) an adversarial
# `skeptic` agent re-checks the same diff for what pass 1 missed/blessed and appends an
# "## Adversarial pass" section to review.md. Skip pass 2 with REVIEW_NO_ADVERSARIAL=1.
# Interactive (-i) is a single session you drive yourself.
#
# Usage:
#   review <repo-slug> <name> [-i|--interactive] [--main] [--base <ref>] [--model <m>] [ref-token ...]
#     <name>        : the worktree at ~/dev/worktrees/<slug>/<name> to review.
#     -i, --interactive : open an interactive Claude session (own tmux session) seeded with the
#                         review prompt, instead of a one-shot headless print.
#     --main/--full : review the WHOLE branch vs the repo default branch (e.g. main or dev,
#                     per repo), not just unpushed work. Aliases: --vs-main.
#     --base <ref>  : diff against <ref> instead of the auto-detected upstream/merge-base.
#     --model <m>   : model for the reviewer (default: opus).
#     ref-token ... : <slug> or <slug>@<branch> read-only cross-repo context for finding precedents
#                     (shares the ~/dev/refs checkout lifecycle, added to the reviewer via --add-dir).
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
REG="$WTD/repos.tsv"
. "$WTD/scripts/platform-lib.sh"   # lets git use our bare repos under safe.bareRepository=explicit
REVDIR="$WTD/reviews"                 # neutral workspace = the reviewer's cwd (OUTSIDE worktrees)
FOCUSDIR="$WTD/review-focus"          # optional per-repo highlight focus
CTXDIR="$WTD/review-context"          # optional per-repo orientation (generated paths, codegen flow, conventions)
KNOWDIR="$WTD/review-knowledge"       # per-repo conventions LEDGER (memoized full-repo standards counts)
LESSONS="$WTD/reviews/reviewer-lessons.md"   # blind spots: what the skeptic caught that pass 1 missed
# shellcheck source=refs-lib.sh
. "$WTD/scripts/refs-lib.sh"          # registered, default_branch, parse_ref_token, ensure_ref
# shellcheck source=account-lib.sh
. "$WTD/scripts/account-lib.sh"       # account_dir_for_role/name (route the reviewer's cost)

slug="${1:-}"; name="${2:-}"
ORIG_ARGS=("$@")                 # full argv, captured before any shift — replayed verbatim by an auto-retry
[ "$#" -ge 2 ] && shift 2 || shift "$#"
interactive=0
base_override=""
# Cost-tiered models: pass 1 (broad review) on the cheaper model, pass 2 (adversarial skeptic,
# where depth matters most) on the stronger one. --model X overrides BOTH; --deep bumps pass 1 to opus.
review_model="sonnet"
skeptic_model="opus"
account_flag=""
refs=()
vs_default=0
USAGE_LIMIT_HIT=0; USAGE_RESET_EPOCH=""   # set by run_pass when a claude pass dies on a usage/rate limit
while [ "$#" -gt 0 ]; do
  case "$1" in
    -i|--interactive) interactive=1; shift;;
    --main|--vs-main|--full) vs_default=1; shift;;   # review the WHOLE branch vs the repo default branch
    --deep) review_model="opus"; shift;;             # both passes on opus (max depth, max cost)
    --account)  [ "$#" -ge 2 ] || { echo "error: --account requires a name"; exit 1; }; account_flag="$2"; shift 2;;
    --account=*) account_flag="${1#*=}"; shift;;
    --base)   [ "$#" -ge 2 ] || { echo "error: --base requires a <ref>"; exit 1; }; base_override="$2"; shift 2;;
    --base=*) base_override="${1#*=}"; shift;;
    --model)  [ "$#" -ge 2 ] || { echo "error: --model requires a value"; exit 1; }; review_model="$2"; skeptic_model="$2"; shift 2;;
    --model=*) review_model="${1#*=}"; skeptic_model="${1#*=}"; shift;;
    *) refs+=("$1"); shift;;
  esac
done

# Route the reviewer's Claude cost: --account flag > $REVIEW_ACCOUNT env > configured 'review' role
# (account use review <name>) > default ~/.claude. Sets CLAUDE_CONFIG_DIR for the claude -p passes.
racc=""
if   [ -n "$account_flag" ];          then racc="$(account_dir_for_name "$account_flag")"; [ -n "$racc" ] || { echo "error: no Claude account '$account_flag' (account add $account_flag)"; exit 1; }
elif [ -n "${REVIEW_ACCOUNT:-}" ];    then racc="$(account_dir_for_name "$REVIEW_ACCOUNT")"
else                                       racc="$(account_dir_for_role review)"; fi
[ -n "$racc" ] && export CLAUDE_CONFIG_DIR="$racc"

# Soonest 5-hour-window reset epoch for the reviewer's account (CLAUDE_CONFIG_DIR, else default
# ~/.claude). Authoritative source is the live OAuth usage API (same call `account usage` makes);
# falls back to the account's statusline-written rate-limits.json, then the default account's.
# Prints an epoch (seconds) or nothing.
review_reset_epoch() {
  local cfg="${CLAUDE_CONFIG_DIR:-$HOME/.claude}" tok body code json r=""
  tok="$(jq -r '.claudeAiOauth.accessToken // empty' "$cfg/.credentials.json" 2>/dev/null || true)"
  if [ -n "$tok" ]; then
    body="$(curl -s -m 15 -w $'\n%{http_code}' -H "Authorization: Bearer $tok" \
            -H "anthropic-beta: oauth-2025-04-20" https://api.anthropic.com/api/oauth/usage 2>/dev/null || true)"
    code="${body##*$'\n'}"; json="${body%$'\n'*}"
    [ "$code" = 200 ] && r="$(printf '%s' "$json" | jq -r '.five_hour.resets_at // empty' 2>/dev/null || true)"
  fi
  [ -n "$r" ] || r="$(jq -r '.five_hour.resets_at // empty' "$cfg/rate-limits.json" 2>/dev/null || true)"
  [ -n "$r" ] || r="$(jq -r '.five_hour.resets_at // empty' "$HOME/.claude/rate-limits.json" 2>/dev/null || true)"
  case "$r" in
    '')          : ;;                                            # unknown
    *[!0-9]*)    date -d "$r" +%s 2>/dev/null || true ;;         # ISO-8601 → epoch
    *)           printf '%s\n' "$r" ;;                           # already epoch
  esac
}

# Schedule a ONE-SHOT retry of this exact review at (reset epoch + 2 min) via `at` (atd preserves the
# current environment, so PATH/claude/CLAUDE_CONFIG_DIR carry over). Deduped per-worktree by a marker
# file holding "<epoch>\t<at-job-id>"; a later successful review cancels the pending job and clears it.
schedule_review_retry() {
  local epoch="$1" at_epoch buffer=120 marker="$wt/.claude/.review-retry" prev now atstamp atout jid cmd q='' a
  if [ -z "$epoch" ]; then
    echo "  could not determine the reset time — no automatic retry scheduled (re-run manually later)."
    return 0
  fi
  now="$(date +%s)"; at_epoch=$((epoch + buffer))
  [ "$at_epoch" -gt "$now" ] || at_epoch=$((now + buffer))      # never schedule in the past
  if [ -f "$marker" ]; then                                     # don't stack duplicates
    prev="$(cut -f1 "$marker" 2>/dev/null || true)"
    if [ -n "$prev" ] && [ "$prev" -gt "$now" ] 2>/dev/null; then
      echo "  retry already queued for $(date -d "@$prev" '+%a %H:%M') — not scheduling another."
      return 0
    fi
  fi
  for a in ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"}; do q="$q $(printf '%q' "$a")"; done
  cmd="\"$WTD/scripts/review.sh\"$q >> \"$wt/.claude/.review-retry.log\" 2>&1"
  atstamp="$(date -d "@$at_epoch" +%Y%m%d%H%M.%S)"
  atout="$(printf '%s\n' "$cmd" | at -t "$atstamp" 2>&1 || true)"
  jid="$(printf '%s' "$atout" | grep -oiE 'job[[:space:]]+[0-9]+' | grep -oE '[0-9]+' | head -1 || true)"
  if [ -n "$jid" ]; then
    printf '%s\t%s\n' "$at_epoch" "$jid" > "$marker"
    printf '  \033[1;33m⏳ reviewer account out of usage\033[0m — retry scheduled for %s (5-hour limit reset + 2m).\n' \
      "$(date -d "@$at_epoch" '+%a %b %d %H:%M')"
    echo "     job $jid · queue: atq · cancel: atrm $jid · log: $wt/.claude/.review-retry.log"
  else
    echo "  failed to schedule retry via 'at': ${atout:-unknown error}"
  fi
}

list_repos() {
  echo "registered repos:"
  awk -F'\t' '!/^#/ && NF>=1 && $1!="" {printf "  %s\t%s\n", $1, $2}' "$REG" 2>/dev/null \
    || echo "  (none)"
}
if [ -z "$slug" ] || [ -z "$name" ]; then
  echo "usage: review <repo-slug> <name> [-i] [--main] [--base <ref>] [--model <m>] [ref-token ...]"
  echo "  --main / --full : review the WHOLE branch vs the repo default branch (not just unpushed work)"; echo
  list_repos; exit 1
fi
registered "$slug" || { echo "error: repo slug '$slug' is not registered."; echo; list_repos; exit 1; }

bare="$DEV/repos/$slug/.bare"
wt="$DEV/worktrees/$slug/$name"
[ -d "$wt" ]   || { echo "error: no worktree at $wt"; echo "       create one with: agent $slug $name"; exit 1; }
[ -d "$bare" ] || { echo "error: repo '$slug' not cloned ($bare missing)"; exit 1; }

# --- determine the base to diff against ---
# Default: everything not yet pushed (vs @{upstream}, else the repo default branch).
# --main / --full: the WHOLE branch vs the repo default branch (everything the branch introduced,
# committed + uncommitted), regardless of what's been pushed.
git -c safe.bareRepository=all -C "$bare" fetch --quiet origin 2>/dev/null || true
if [ -n "$base_override" ]; then
  base_ref="$base_override"
elif [ "$vs_default" = 1 ]; then
  base_ref="origin/$(default_branch "$slug")"      # --main: full branch vs default branch
elif up="$(git -C "$wt" rev-parse --abbrev-ref --symbolic-full-name '@{upstream}' 2>/dev/null)"; then
  base_ref="$up"                                   # branch is pushed: diff vs its upstream
else
  base_ref="origin/$(default_branch "$slug")"      # never pushed: diff vs the repo default branch
fi
# merge-base keeps the diff to OUR work, excluding changes the base picked up since we branched.
base="$(git -C "$wt" merge-base HEAD "$base_ref" 2>/dev/null || echo "$base_ref")"
branch="$(git -C "$wt" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '(detached)')"

# --- per-run dirs. Reports land IN the worktree at .claude/reviews/<ts>/ (git-ignored via .claude),
# so each review lives with its worktree. The reviewer is read-only on the worktree, so it writes to
# a scratch staging dir and review.sh MOVES the finished reports into the worktree at the end.
# PDT, 24-hour timestamp (e.g. 2026-06-23__14.25) so directory names sort chronologically;
# -2,-3… on same-minute collision.
ts="$(TZ='America/Los_Angeles' date '+%Y-%m-%d__%H.%M')"
revdest="$wt/.claude/reviews"; cand="$ts"; k=2
while [ -e "$revdest/$cand" ]; do cand="${ts}-$k"; k=$((k + 1)); done
ts="$cand"
wtout="$revdest/$ts"                                   # FINAL destination (inside the worktree)
scratch="$REVDIR/.scratch/${slug}-${name//\//-}-$ts"   # staging (reviewer writes here; cleaned up)
indir="$scratch/in"
outdir="$scratch/out"
mkdir -p "$indir" "$outdir"

{
  echo "repo_slug:      $slug"
  echo "worktree_name:  $name"
  echo "branch:         $branch"
  echo "worktree_path:  $wt"
  echo "default_branch: $(default_branch "$slug")"
  echo "base_ref:       $base_ref"
  echo "base_commit:    $base"
} > "$indir/meta.txt"
git -C "$wt" diff --stat "$base"     > "$indir/stat.txt"    2>/dev/null || true
git -C "$wt" log --oneline "$base"..HEAD > "$indir/commits.txt" 2>/dev/null || true
git -C "$wt" ls-files --others --exclude-standard > "$indir/untracked.txt" 2>/dev/null || true

# diff.patch EXCLUDES generated files (.pb.go, generated TS, swagger, anything linguist-generated /
# "DO NOT EDIT") to save tokens — they aren't reviewed, only noted. stat.txt above keeps the FULL
# list so the report still sees that generated artifacts changed. gen_excludes from generated-filter.sh.
# shellcheck source=../hooks/generated-filter.sh
. "$WTD/hooks/generated-filter.sh"
mapfile -t gen_ex < <(git -C "$wt" diff --name-only "$base" 2>/dev/null | gen_excludes "$wt" "-")
git -C "$wt" diff "$base" -- . "${gen_ex[@]}" > "$indir/diff.patch" 2>/dev/null || true
if [ "${#gen_ex[@]}" -gt 0 ]; then
  printf '%s\n' "${gen_ex[@]}" | sed 's/^:(exclude)//' > "$indir/generated-excluded.txt"
fi

focus=""
if [ -f "$FOCUSDIR/$slug.md" ]; then
  cp "$FOCUSDIR/$slug.md" "$indir/focus.md"
  focus="$indir/focus.md"
fi

# Per-repo orientation read FIRST by both passes: generated paths (DO NOT EDIT), codegen flow,
# source-of-truth, where helpers/docs live — so the reviewer doesn't e.g. suggest editing a
# generated file or reinvent a pkg/common helper.
ctx=""
if [ -f "$CTXDIR/$slug.md" ]; then
  cp "$CTXDIR/$slug.md" "$indir/repo-context.md"
  ctx="$indir/repo-context.md"
fi

# Per-repo conventions LEDGER: memoized full-repo standards counts established by past reviews. Read
# after repo-context; lets the reviewer TRUST an already-counted convention (after a quick evidence
# spot-check) instead of re-counting the whole repo. Reviewer appends new/confirmed entries via
# ledger-delta.md, which review.sh merges back below.
mkdir -p "$KNOWDIR"
ledgerfile="$KNOWDIR/$slug.md"
ledger=""
if [ -s "$ledgerfile" ]; then
  cp "$ledgerfile" "$indir/conventions-ledger.md"
  ledger="$indir/conventions-ledger.md"
fi

# reviewer blind spots: issues a past adversarial (skeptic) pass caught that the first pass missed.
# Injected so this first pass checks them up front; the skeptic appends new ones via lessons-delta.md.
lessons=""
if [ -s "$LESSONS" ]; then
  cp "$LESSONS" "$indir/reviewer-lessons.md"
  lessons="$indir/reviewer-lessons.md"
fi

if [ ! -s "$indir/diff.patch" ] && [ ! -s "$indir/untracked.txt" ]; then
  echo "nothing to review: no changes in $wt vs $base_ref ($base)"
  rm -rf "$indir" "$outdir"
  exit 0
fi

# --- read-only cross-repo reference checkouts for precedent-hunting (optional) ---
add_dirs=( --add-dir "$wt" )
if [ "${#refs[@]}" -gt 0 ]; then
  for token in "${refs[@]}"; do
    parse_ref_token "$token"
    if ! registered "$REF_SLUG" || [ ! -d "$DEV/repos/$REF_SLUG/.bare" ]; then
      echo "warning: skipping reference '$token' (repo '$REF_SLUG' not registered/cloned)"; continue
    fi
    if p="$(ensure_ref "$REF_SLUG" "$REF_BRANCH")"; then add_dirs+=( --add-dir "$p" ); fi
  done
fi

# --- the task prompt handed to the reviewer agent (rubric lives in the agent definition) ---
read -r -d '' prompt <<EOF || true
Review the unpushed changes for worktree '$name' of repo '$slug' before it is pushed.

$( [ -n "$ctx" ] && printf 'READ FIRST — repo orientation (generated paths, codegen flow, conventions): %s\n' "$ctx" )
$( [ -n "$ledger" ] && printf 'CONVENTIONS LEDGER (already-counted standards, trust + spot-check per your rubric): %s\n' "$ledger" )
$( [ -n "$lessons" ] && printf 'BLIND SPOTS — issues a past adversarial pass caught that first passes MISSED; actively CHECK every one this run: %s\n' "$lessons" )
Inputs are in:        $indir
  - meta.txt, diff.patch, stat.txt, commits.txt, untracked.txt$( [ -n "$focus" ] && printf ', focus.md' )$( [ -n "$ctx" ] && printf ', repo-context.md' )$( [ -n "$ledger" ] && printf ', conventions-ledger.md' )$( [ -n "$lessons" ] && printf ', reviewer-lessons.md' )
  - diff.patch EXCLUDES generated files to save tokens$( [ -f "$indir/generated-excluded.txt" ] && printf ' (listed in generated-excluded.txt). Treat them as regenerated artifacts — note that they changed, do not review line-by-line. EXCEPTION: a few (e.g. internal/service/service.go) carry a codegen marker but get hand-edited handlers; if an excluded file looks hand-edited, pull its diff yourself with `git -C '"$wt"' diff '"$base"' -- <file>`' ).
Target worktree (READ-ONLY, already granted): $wt
$( [ "${#add_dirs[@]}" -gt 2 ] && printf 'Read-only reference checkouts for precedent-hunting are also granted (see --add-dir).\n' )

Do a full review on all dimensions (correctness, performance, security, documentation, repo
standards, efficient & minimal code) per your rubric — one line per finding, severity-tagged, no
praise — cite in-repo precedents (same-way / different-way) with file:line, and write your two reports:
  $outdir/review.md
  $outdir/highlights.md
$( [ -n "$focus" ] && printf 'Organize highlights.md under the headings in focus.md and emphasize exactly what it asks for.\n' )
CONVENTIONS LEDGER: per your rubric, for any repo convention you established (full-repo count) or
confirmed this run, also write $outdir/ledger-delta.md — one '## <convention>' section each (Rule,
Evidence file:line + sample size, Source, dates). Skip the file if you established/confirmed none.
Then print a short summary with the verdict, finding counts, and the report paths.
EOF

# read-only on the code under review: deny edits/writes into the worktree and refs; the reviewer
# may still write its two reports (under $outdir, inside the cwd workspace) — acceptEdits auto-OKs
# those so the headless run never blocks on a permission prompt.
deny=( --disallowedTools "Write($wt/**)" "Edit($wt/**)" "Write($DEV/refs/**)" "Edit($DEV/refs/**)" )

if [ "$interactive" = 1 ]; then
  command -v tmux >/dev/null 2>&1 || { echo "error: tmux not found (needed for -i)"; exit 1; }
  session="review-${slug}-${name}"; session="${session//[.:\/]/-}"
  # build the command as a single string for tmux send-keys
  cmd=( claude --agent reviewer --model "$review_model" "${add_dirs[@]}" --permission-mode acceptEdits "${deny[@]}" )
  printf -v cmdstr '%q ' "${cmd[@]}"
  # initial message via a heredoc-safe single arg
  printf -v promptq '%q' "$prompt"
  tmux kill-session -t "=$session" 2>/dev/null || true
  tmux new-session -d -s "$session" -c "$REVDIR"
  tmux send-keys -t "$session" "${cmdstr}${promptq}" C-m
  echo "interactive reviewer started in tmux session: $session"
  echo "  reports will land in: $outdir"
  if [ -n "${TMUX:-}" ]; then tmux switch-client -t "$session"; else tmux attach -t "$session"; fi
  exit 0
fi

echo "reviewing $slug/$name  (base: $base_ref @ ${base:0:12})  models: review=$review_model skeptic=$skeptic_model"
echo "  inputs : $indir"
echo "  reports: $outdir"
echo "  (a spinner shows each pass running; the full report prints when it finishes)"
echo

# --- live feedback during the review (all restored on exit) ---
# (1) worktree status 'reviewing' (yellow); (2) an animated progress view in the worktree's diff
# pane (if the repo has one). Skipped status if it's already 'done' (green stays).
REVIEWING=0; prog_pid=""; PASS_PID=""; rhb_pid=""; progress_log=""; phase_file="$indir/phase"; diffcmd=""; FINDINGS_MD=""
# the worktree's tmux session (for the in-window reviewing indicator); same derivation as wt-status.sh
rsess=""; case "$wt" in */worktrees/*) rrel="${wt#*/worktrees/}"; rsess="${rrel/\//-}";; esac
if [ "$(cat "$wt/.claude-status" 2>/dev/null)" != done ]; then
  REVIEWING=1
  CLAUDE_PROJECT_DIR="$wt" "$WTD/hooks/wt-status.sh" reviewing </dev/null 2>/dev/null || true
  # LOUD, can't-miss indicator IN the tmux window: paint this session's status bar purple + label it
  # while the (separate) reviewer runs. Reverted in cleanup_review (incl. on kill/timeout via the trap).
  if [ -n "$rsess" ] && tmux has-session -t "$rsess" 2>/dev/null; then
    tmux set -t "$rsess" status-style 'bg=#c586f0,fg=#101010' 2>/dev/null || true
    tmux set -t "$rsess" status-left '#[bold] ⟳ REVIEWING #[default]' 2>/dev/null || true
  fi
fi
hookfrag="$WTD/repo-hooks/$slug.json"
if [ -f "$hookfrag" ]; then
  rel_log="$(jq -r '._diffLog // empty' "$hookfrag" 2>/dev/null)"
  diffcmd="$(jq -r '[.. | objects | .command? // empty] | .[0] // empty' "$hookfrag" 2>/dev/null)"
  [ -n "$rel_log" ] && progress_log="$wt/$rel_log"
fi
set_phase() { [ -n "$progress_log" ] && printf '%s' "$1" > "$phase_file" 2>/dev/null || true; }
cleanup_review() {
  rm -f "$indir/.reviewing" 2>/dev/null || true
  [ -n "$prog_pid" ] && kill "$prog_pid" 2>/dev/null || true
  [ -n "${rhb_pid:-}" ] && kill "$rhb_pid" 2>/dev/null || true   # stop the purple-glyph heartbeat
  # kill the in-flight claude pass (+ its children) so a killed/Ctrl-C'd/timed-out review never leaves
  # an orphaned `claude -p` running and burning tokens.
  if [ -n "${PASS_PID:-}" ]; then pkill -P "$PASS_PID" 2>/dev/null || true; kill "$PASS_PID" 2>/dev/null || true; fi
  [ "$REVIEWING" = 1 ] && CLAUDE_PROJECT_DIR="$wt" "$WTD/hooks/wt-status.sh" reviewed </dev/null 2>/dev/null || true
  # revert the purple "REVIEWING" status bar to the global default
  if [ -n "$rsess" ] && tmux has-session -t "$rsess" 2>/dev/null; then
    tmux set -u -t "$rsess" status-style 2>/dev/null || true
    tmux set -u -t "$rsess" status-left 2>/dev/null || true
  fi
  # diff pane: on a COMPLETED review, render the FINDINGS (review.md) there so they replace the
  # spinner; otherwise (killed / timed out / no report) restore the live working diff.
  if [ -n "$progress_log" ] && [ -s "${FINDINGS_MD:-}" ]; then
    W="$(tmux list-panes -t "$rsess" -F '#{?#{@dpane},#{pane_width},}' 2>/dev/null | grep -m1 .)"; case "$W" in ''|*[!0-9]*) W=88;; esac
    { "$WTD/hooks/md-render.py" "$FINDINGS_MD" "$((W-2))" 2>/dev/null | sed 's/^/  /'; } > "$progress_log" 2>/dev/null || true
  elif [ -n "$progress_log" ] && [ -n "$diffcmd" ] && [ -x "$diffcmd" ]; then
    "$diffcmd" --seed "$wt" >/dev/null 2>&1 || true
  fi
}
trap cleanup_review EXIT
# Keep the tab CIRCLE purple for the whole review: re-assert @wt_status (the tmux glyph the VSCode tab
# shows via ${sequence}) + re-emit the title every 2s, so the agent's ongoing working/tool events can't
# drift it back to blue. Stops when cleanup removes the .reviewing flag.
if [ "$REVIEWING" = 1 ] && [ -n "$rsess" ]; then
  : > "$indir/.reviewing"
  ( while [ -f "$indir/.reviewing" ] && tmux has-session -t "$rsess" 2>/dev/null; do
      tmux set -t "$rsess" @wt_status '🟣' 2>/dev/null
      for c in $(tmux list-clients -t "$rsess" -F '#{client_name}' 2>/dev/null); do tmux refresh-client -t "$c" 2>/dev/null; done
      sleep 2
    done ) & rhb_pid=$!
fi
# The reviewer pass streams its steps (stream-json) into $STREAM; the progress pane tails a live,
# human-readable feed of them. run_pass truncates $STREAM at the start of each pass.
STREAM="$indir/activity.jsonl"; : > "$STREAM"
# jq: turn one stream-json event into a one-line activity ("$ <cmd>", "read <file>", "» <text>", …).
ACTJQ='if .type=="assistant" then (.message.content[]? |
  if .type=="tool_use" then (.name) as $n | (.input) as $in |
    ( if   $n=="Bash"  then "$ " + (($in.command // "")|gsub("\n";" "))
      elif $n=="Read"  then "read " + ($in.file_path // "")
      elif $n=="Grep"  then "grep " + (($in.pattern // "")|tostring) + (if ($in.path//"")!="" then " ("+$in.path+")" else "" end)
      elif $n=="Glob"  then "glob " + ($in.pattern // "")
      elif $n=="Edit"  then "edit " + ($in.file_path // "")
      elif $n=="Write" then "write " + ($in.file_path // "")
      elif $n=="TodoWrite" then "· updated its checklist"
      else $n end )
  elif (.type=="text" and (.text|test("\\S"))) then "» " + (.text|gsub("\n";" "))
  else empty end)
else empty end'
if [ -n "$progress_log" ]; then
  set_phase "pass 1 — review ($review_model)…"
  : > "$indir/.reviewing"
  ppid=$$
  # width of the diff pane (so activity lines don't wrap); fall back if it can't be read
  awidth="$(tmux list-panes -t "$rsess" -F '#{?#{@dpane},#{pane_width},}' 2>/dev/null | grep -m1 .)"
  case "$awidth" in ''|*[!0-9]*) awidth=120;; esac; awidth=$(( awidth > 14 ? awidth - 6 : 90 ))
  ( # self-heal: if review.sh dies abnormally (killed / session closed → cleanup_review never runs),
    # restore the live diff instead of leaving the spinner frozen in the pane. Only fires when
    # ORPHANED (parent gone); on normal exit cleanup_review renders the findings.
    trap 'kill -0 "$ppid" 2>/dev/null || { [ -n "$diffcmd" ] && [ -x "$diffcmd" ] && "$diffcmd" --seed "$wt" >/dev/null 2>&1; rm -f "$indir/.reviewing"; }' EXIT
    frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'; i=0; start=$SECONDS
    while [ -f "$indir/.reviewing" ] && kill -0 "$ppid" 2>/dev/null; do
      el=$((SECONDS - start))
      { printf '\033[1;33m⟳ review in progress — %s/%s\033[0m\n\n' "$slug" "$name"
        printf '   \033[1;33m%s\033[0m  %s   \033[2m%dm%02ds elapsed\033[0m\n\n' "${frames:i++%10:1}" "$(cat "$phase_file" 2>/dev/null)" $((el / 60)) $((el % 60))
        printf '   \033[90m── live activity (newest at bottom) ──────────────────────\033[0m\n'
        if [ -s "$STREAM" ]; then
          jq -r "$ACTJQ" "$STREAM" 2>/dev/null | tail -n 18 | cut -c1-"$awidth" \
            | awk 'NF{a[++n]=$0} END{ if(n==0){print "   \033[2m(thinking…)\033[0m"} for(j=1;j<=n;j++){ c=(j==n)?"\033[36m":"\033[2m"; printf "   %s%s\033[0m\n",c,a[j] } }'
        else
          printf '   \033[2m(reviewer starting up…)\033[0m\n'
        fi
      } > "$progress_log" 2>/dev/null
      sleep 1
    done ) & prog_pid=$!
fi

cd "$REVDIR"
USAGE_TSV="$indir/usage.tsv"; : > "$USAGE_TSV"

# Lightweight markdown -> ANSI for the terminal (no external deps). Plain passthrough when stdout
# isn't a TTY (piped, background, or the /review skill) so logs/agents get clean text.
render_md() {
  if [ -t 1 ] && command -v perl >/dev/null 2>&1; then
    perl -pe '
      s/^\s*#{1,6}\s*(.*)$/\e[1;36m$1\e[0m/;          # headings -> bold cyan
      s/\*\*(.+?)\*\*/\e[1m$1\e[0m/g;                  # **bold**
      s/`([^`]+)`/\e[33m$1\e[0m/g;                     # `code` -> yellow
      s/\b(BLOCKER|MAJOR)\b/\e[1;31m$1\e[0m/g;         # severities
      s/\b(MINOR)\b/\e[1;33m$1\e[0m/g;
      s/\b(NIT)\b/\e[2mNIT\e[0m/g;
      s/^(\s*)[-*]\s/$1\e[36m•\e[0m /;                 # bullets -> cyan •
    '
  else
    cat
  fi
}

# Animated spinner + elapsed timer while a backgrounded pass runs. TTY only — when output isn't a
# terminal (piped, background run) it just waits silently so logs stay clean.
spin() {  # pid "label"
  local pid="$1" label="$2"
  [ -t 1 ] || return 0   # not a TTY (headless / piped): no spinner. MUST be `return 0` — a bare
                         # `return` propagates the failing `[ -t 1 ]` status and `set -e` aborts the run.
  local frames='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏' i=0 start=$SECONDS el
  printf '\033[?25l'                                            # hide cursor
  while kill -0 "$pid" 2>/dev/null; do
    el=$((SECONDS - start))
    printf '\r  \033[36m%s\033[0m %s  \033[2m%dm%02ds\033[0m' \
      "${frames:i++%${#frames}:1}" "$label" $((el / 60)) $((el % 60))
    sleep 0.1
  done
  printf '\033[?25h\r\033[K'                                    # restore cursor + clear the line
}

# Run one claude pass with a live spinner; print its result text and record token usage
# (label, model, input, output, cache-read, cache-creation, cost) into $USAGE_TSV for the footer.
run_pass() {  # label model agent prompt
  local label="$1" mdl="$2" ag="$3" pr="$4" pid usage sig e
  USAGE_LIMIT_HIT=0                               # reset per pass; checked right after the call returns
  : > "$STREAM"                                   # fresh activity stream for this pass
  # stream-json + --verbose emits every step (tool calls, text) as it happens, so the progress pane
  # can tail $STREAM for a live "what it's doing" feed. The final {"type":"result"} event carries the
  # result text + token usage — the same fields the old --output-format json gave at top level.
  claude -p "$pr" --agent "$ag" --model "$mdl" "${add_dirs[@]}" \
    --permission-mode acceptEdits --allowedTools Read Grep Glob Bash TodoWrite Write Edit \
    "${deny[@]}" --output-format stream-json --verbose >"$STREAM" 2>"$indir/pass-stderr.log" &
  pid=$!; PASS_PID=$pid
  spin "$pid" "$label · $mdl …"
  wait "$pid" 2>/dev/null || true
  PASS_PID=""
  # usage/rate-limit detection: inspect ONLY the final result event + stderr (NOT tool output, which
  # may quote "rate limit" from the diff) to avoid false positives. Capture an inline reset epoch if
  # the message carries one ("…reached|<epoch>"); otherwise the caller falls back to the usage API.
  sig="$( { jq -r 'select(.type=="result") | [.subtype, (.is_error|tostring), (.result // ""), (.error // "" | tostring)] | @tsv' "$STREAM" 2>/dev/null
           cat "$indir/pass-stderr.log" 2>/dev/null; } | grep -iE 'usage limit|limit reached|rate.?limit|too many requests|\b429\b' | head -1 || true )"
  if [ -n "$sig" ]; then
    USAGE_LIMIT_HIT=1
    e="$(printf '%s' "$sig" | grep -oE '\|[0-9]{10}' | tr -d '|' | head -1 || true)"
    [ -n "$e" ] && USAGE_RESET_EPOCH="$e"
  fi
  jq -r 'select(.type=="result") | .result // empty' "$STREAM" 2>/dev/null | render_md
  usage="$(jq -r 'select(.type=="result") | [.usage.input_tokens//0, .usage.output_tokens//0, .usage.cache_read_input_tokens//0, .usage.cache_creation_input_tokens//0, .total_cost_usd//0] | @tsv' "$STREAM" 2>/dev/null | tail -1)"
  [ -n "$usage" ] || usage="$(printf '0\t0\t0\t0\t0')"
  printf '%s\t%s\t%s\n' "$label" "$mdl" "$usage" >> "$USAGE_TSV"
}

# Pass 1 — the reviewer (cheaper model) produces review.md + highlights.md.
printf '\033[1;36m── pass 1: review (%s) ────────────────────────\033[0m\n' "$review_model"
echo
run_pass "pass 1 · review" "$review_model" reviewer "$prompt"
echo

# Reviewer account ran out of usage and produced no report → auto-schedule a retry at the limit reset
# and stop now, WITHOUT leaving an empty .claude/reviews/<ts>/ dir behind.
if [ "$USAGE_LIMIT_HIT" = 1 ] && [ ! -s "$outdir/review.md" ]; then
  echo
  printf '\033[1;33m── review incomplete: reviewer account out of usage ─────\033[0m\n'
  [ -n "$USAGE_RESET_EPOCH" ] || USAGE_RESET_EPOCH="$(review_reset_epoch)"
  schedule_review_retry "$USAGE_RESET_EPOCH"
  rm -rf "$scratch" 2>/dev/null || true
  exit 75   # EX_TEMPFAIL — nothing was wrong with the work; try again later
fi

# Pass 2 — adversarial skeptic (stronger model) re-checks the SAME diff for what pass 1
# missed/blessed and appends a "## Adversarial pass" section. Skip with REVIEW_NO_ADVERSARIAL=1.
if [ -z "${REVIEW_NO_ADVERSARIAL:-}" ] && [ -f "$outdir/review.md" ]; then
  set_phase "pass 2 — adversarial skeptic ($skeptic_model)…"
  echo
  printf '\033[1;35m── pass 2: adversarial (%s) ──────────────────\033[0m\n' "$skeptic_model"
  echo
  read -r -d '' skprompt <<EOF || true
A first-pass review of worktree '$name' (repo '$slug') is at:  $outdir/review.md  (read it first).
Run inputs (diff, stat, commits, untracked$( [ -n "$focus" ] && printf ', focus' )$( [ -n "$ctx" ] && printf ', repo-context' )) are in: $indir
$( [ -n "$ctx" ] && printf 'Read %s for repo orientation (generated paths, codegen flow, conventions) before judging.\n' "$ctx" )
$( [ -n "$ledger" ] && printf 'Conventions ledger: %s — if you find an entry pass 1 relied on is STALE or wrong (evidence gone / re-count disagrees), add a corrected "## <convention>" section to %s/ledger-delta.md.\n' "$ledger" "$outdir" )
Target worktree (READ-ONLY, granted): $wt

Adversarially re-check the SAME diff: try to BREAK the logic and find what pass 1 missed or wrongly
blessed — edge/branch combinations in ported logic (enumerate the fall-throughs), per-column
NULL/COALESCE inconsistencies, error/empty/boundary paths, performance and security, and any pass-1
finding called "intentional"/"safe" that deserves a second look. Then EDIT $outdir/review.md: append
a section '## Adversarial pass' with the NEW findings (file:line + fix) and a '### Corrections' list
for any pass-1 finding you think is wrong. If pass 1 missed nothing material, say so in one line.

For every MATERIAL issue you found that pass 1 MISSED, also append a generalized lesson to
$outdir/lessons-delta.md — one '## <short blind-spot name>' section each: the CATEGORY of issue to
check next time + a terse pointer to this example (file:line). These feed future first-pass reviewers
so the same class of miss isn't repeated. Skip NITs/trivia; only real, recurring-risk misses. Omit
the file if pass 1 missed nothing material.
EOF
  run_pass "pass 2 · adversarial" "$skeptic_model" skeptic "$skprompt"
  echo
fi

# --- token-usage footer appended to review.md (and printed) ---
if [ -s "$USAGE_TSV" ] && [ -f "$outdir/review.md" ]; then
  {
    printf '\n---\n\n## Token usage\n\n'
    printf '| pass | model | input | cache write | cache read | output | est. API cost |\n'
    printf '|---|---|--:|--:|--:|--:|--:|\n'
    awk -F'\t' '{ti+=$3;to+=$4;tcr+=$5;tcw+=$6;tc+=$7; printf "| %s | %s | %s | %s | %s | %s | $%.4f |\n",$1,$2,$3,$6,$5,$4,$7}
      END{printf "| **total** | | **%d** | **%d** | **%d** | **%d** | **$%.4f** |\n",ti,tcw,tcr,to,tc}' "$USAGE_TSV"
    printf '\n_Est. API cost = pay-as-you-go list price of these tokens (informational). On a Claude subscription this is **not billed** — it counts against your plan'"'"'s usage limits instead._\n'
  } >> "$outdir/review.md"
  echo
  awk -F'\t' '{ti+=$3;to+=$4;tcw+=$6;tc+=$7} END{printf "tokens: %d in (+%d cache write) / %d out   est. API cost: $%.4f (not billed on a subscription)\n",ti,tcw,to,tc}' "$USAGE_TSV"
fi

# --- triage placeholder at the TOP of review.md. The in-worktree agent (who wrote the code) fills
# this in after reading the review, so its Fix/Ignore triage lives at the top of the report — not
# only in chat. (See the wt-review skill, step 4.) ---
if [ -f "$outdir/review.md" ]; then
  tri="$(mktemp)"
  {
    printf '## Agent triage — %s\n\n' "$name"
    printf '> _Pending — the worktree agent fills this in after reading the review below: a **Fix** list\n'
    printf '> and an **Ignore** list, each finding one line with `**[SEVERITY]** file:line — rationale`._\n\n'
    printf -- '---\n\n'
    cat "$outdir/review.md"
  } > "$tri" && mv "$tri" "$outdir/review.md"
fi

# --- merge newly established/confirmed conventions into the per-repo ledger (upsert by ## heading) ---
if [ -s "$outdir/ledger-delta.md" ]; then
  if [ ! -s "$ledgerfile" ]; then
    {
      printf '# Conventions ledger — %s\n\n' "$slug"
      printf 'Memoized full-repo standards counts established by past reviews. Each entry was counted across\n'
      printf 'the WHOLE repo; future reviews trust it (after a quick evidence spot-check) for the area a diff\n'
      printf 'touches, instead of re-counting. Newest evidence wins. Human-prunable — delete stale entries.\n\n'
    } > "$ledgerfile"
  fi
  merged="$(mktemp)"
  if awk -f "$WTD/hooks/ledger-merge.awk" "$outdir/ledger-delta.md" "$ledgerfile" > "$merged"; then
    sed 's/^## /\n## /' "$merged" | cat -s > "$ledgerfile"; rm -f "$merged"   # one blank line per heading
    echo "updated conventions ledger: $ledgerfile"
  else rm -f "$merged"; fi
fi

# --- merge skeptic-found blind spots into the reviewer-lessons file (upsert by ## heading) ---
if [ -s "$outdir/lessons-delta.md" ]; then
  if [ ! -s "$LESSONS" ]; then
    {
      printf '# Reviewer blind spots\n\n'
      printf 'Issues the adversarial (skeptic) pass caught that a first pass MISSED. Read FIRST by the\n'
      printf 'reviewer each run and actively checked. Newest detail wins. Human-prunable — delete stale ones.\n\n'
    } > "$LESSONS"
  fi
  merged="$(mktemp)"
  if awk -f "$WTD/hooks/ledger-merge.awk" "$outdir/lessons-delta.md" "$LESSONS" > "$merged"; then
    sed 's/^## /\n## /' "$merged" | cat -s > "$LESSONS"; rm -f "$merged"
    echo "updated reviewer blind spots: $LESSONS"
  else rm -f "$merged"; fi
fi

# --- move the finished reports INTO the worktree (.claude/reviews/<ts>/), then drop the scratch ---
mkdir -p "$wtout"
mv -f "$outdir/review.md" "$outdir/highlights.md" "$wtout/" 2>/dev/null || true
[ -s "$outdir/ledger-delta.md" ] && mv -f "$outdir/ledger-delta.md" "$wtout/" 2>/dev/null || true
[ -s "$outdir/lessons-delta.md" ] && mv -f "$outdir/lessons-delta.md" "$wtout/" 2>/dev/null || true
if [ -s "$wtout/review.md" ]; then
  FINDINGS_MD="$wtout/review.md"                              # cleanup_review renders this into the diff pane
  if [ -f "$wt/.claude/.review-retry" ]; then                # a queued usage-limit retry is now moot — cancel it
    jid="$(cut -f2 "$wt/.claude/.review-retry" 2>/dev/null || true)"
    [ -n "$jid" ] && atrm "$jid" 2>/dev/null || true
    rm -f "$wt/.claude/.review-retry"
  fi
fi
rm -rf "$scratch" 2>/dev/null || true

echo
printf '\033[1;32m── reports ───────────────────────────────────────────\033[0m\n'
echo "  review.md     : $wtout/review.md"
echo "  highlights.md : $wtout/highlights.md"
command -v code >/dev/null 2>&1 && code "$wtout/review.md" "$wtout/highlights.md" 2>/dev/null || true
