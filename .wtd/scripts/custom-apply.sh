#!/usr/bin/env bash
# custom-apply.sh <slug> <worktree> [--fresh] — apply the presets in .wtd/custom to a worktree on
# Linux / WSL / macOS (wtd.exe, which does this on Windows, doesn't run there). Same layers and output
# as `wtd custom apply`; prints one line per change. Needs bash 3.2+, jq and git.
#
#   tool dirs for PATH: `custom-apply.sh --tool-dirs <slug>` prints them, one per line.
set -euo pipefail

WTD="$(cd "$(dirname "$0")/.." && pwd)"
DEV="$(dirname "$WTD")"
. "$WTD/scripts/platform-lib.sh"
OS="${WTD_PLATFORM:-$(wtd_os)}"
C="$WTD/custom"
BEGIN='<!-- BEGIN custom (worktree-dev: from .wtd/custom — edit there, not here) -->'
END='<!-- END custom -->'
MANIFEST=".claude/.wtd-custom.json"
OVERRIDE="AGENTS.override.md"

layers() {   # lowest first
  local slug="$1"
  printf '%s\n' "$C" "$C/platforms/$OS"
  [ -n "$slug" ] && printf '%s\n' "$C/repos/$slug" "$C/repos/$slug/platforms/$OS"
  return 0
}

if [ "${1:-}" = "--tool-dirs" ]; then
  layers "${2:-}" | while read -r l; do if [ -d "$l/tools" ]; then echo "$l/tools"; fi; done
  exit 0
fi

slug="$1"; wt="$2"; fresh="${3:-}"
targets() {
  case "$1" in
    skills) echo ".claude/skills .agents/skills" ;;
    agents) echo ".claude/agents" ;;
    commands) echo ".claude/commands" ;;
    rules) echo ".claude/rules" ;;
  esac
}

disabled="$(layers "$slug" | while read -r l; do [ -f "$l/disabled" ] && sed -e 's/[[:space:]]*$//' -e 's#/$##' "$l/disabled"; done | grep -v '^#' | grep -v '^$' || true)"
is_disabled() { printf '%s\n' "$disabled" | grep -qxF -e "$1" -e "${1%.md}"; }

# --- directory items: merge by name (later layer wins), copy, track in the manifest ---
items="$(mktemp)"; trap 'rm -f "$items"' EXIT
for l in "$WTD/templates/.claude" $(layers "$slug"); do
  for kind in skills agents commands rules; do
    [ "$l" = "$WTD/templates/.claude" ] && [ "$kind" != skills ] && continue
    [ -d "$l/$kind" ] || continue
    for e in "$l/$kind"/*; do
      [ -e "$e" ] || continue
      n="$(basename "$e")"; case "$n" in .*) continue ;; esac
      printf '%s\t%s\n' "$kind/$n" "$e" >> "$items"
    done
  done
done
# the last (highest) layer per item wins
merged="$(awk -F'\t' '{ src[$1] = $2 } END { for (k in src) print k "\t" src[k] }' "$items" | sort)"

tracked="$(git -C "$wt" ls-files -- .claude .agents "$OVERRIDE" 2>/dev/null || true)"
is_tracked() { printf '%s\n' "$tracked" | grep -qE "^$(printf '%s' "$1" | sed 's/[.[\*^$/]/\\&/g')(/|$)"; }

now=()
while IFS=$'\t' read -r key from; do
  [ -n "$key" ] || continue
  is_disabled "$key" && continue
  kind="${key%%/*}"; n="${key#*/}"
  for t in $(targets "$kind"); do
    rel="$t/$n"
    if is_tracked "$rel"; then echo "kept the repo's own $rel"; continue; fi
    mkdir -p "$wt/$t"
    if [ -d "$from" ]; then mkdir -p "$wt/$rel"; cp -r "$from/." "$wt/$rel/"; else cp "$from" "$wt/$rel"; fi
    now+=("$rel")
  done
done <<<"$merged"

before="$( [ -f "$wt/$MANIFEST" ] && jq -r '.items[]?' "$wt/$MANIFEST" 2>/dev/null || true)"
{ printf '%s\n' "$before"
  printf '%s\n' "$disabled" | while read -r k; do
    [ -n "$k" ] || continue
    for t in $(targets "${k%%/*}"); do echo "$t/${k#*/}"; done
  done; } | sort -u | while read -r p; do
  [ -n "$p" ] || continue
  printf '%s\n' "${now[@]:-}" | grep -qxF "$p" && continue
  is_tracked "$p" && continue
  rm -rf "${wt:?}/$p"
done
mkdir -p "$wt/.claude"
printf '%s\n' "${now[@]:-}" | grep -v '^$' | jq -R . | jq -s '{items: .}' > "$wt/$MANIFEST.tmp" && mv "$wt/$MANIFEST.tmp" "$wt/$MANIFEST"

common="$(git -C "$wt" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
if [ -n "$common" ]; then
  excl="$common/info/exclude"; mkdir -p "$(dirname "$excl")"; touch "$excl"
  for p in "${now[@]:-}" "$MANIFEST" "$OVERRIDE"; do
    [ -n "$p" ] || continue
    grep -qxF "/$p" "$excl" || { [ -s "$excl" ] && [ -n "$(tail -c1 "$excl")" ] && printf '\n' >> "$excl"; printf '/%s\n' "$p" >> "$excl"; }
  done
fi

# --- env files (repo layers): seeded into new worktrees when absent ---
if [ "$fresh" = "--fresh" ] && [ -n "$slug" ]; then
  for l in "$C/repos/$slug" "$C/repos/$slug/platforms/$OS"; do
    if [ -d "$l/env" ]; then cp -rn "$l/env/." "$wt/" 2>/dev/null || true; echo "seeded custom env files from $l/env"; fi
  done
fi

# --- marked blocks ---
layer_text() { layers "$slug" | while read -r l; do if [ -s "$l/$1" ]; then cat "$l/$1"; printf '\n\n'; fi; done | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}'; }
with_block() {   # with_block <file-or-empty> <body> → stdout
  local base=""; [ -f "$1" ] && base="$(awk -v b="$BEGIN" -v e="$END" 'index($0,b){skip=1} !skip{print} index($0,e){skip=0}' "$1" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')"
  if [ -z "$2" ]; then [ -n "$base" ] && printf '%s\n' "$base"; return 0; fi
  [ -n "$base" ] && printf '%s\n\n' "$base"
  printf '%s\n%s\n%s\n' "$BEGIN" "$2" "$END"
}
if [ -f "$wt/CLAUDE.md" ]; then
  new="$(with_block "$wt/CLAUDE.md" "$(layer_text CLAUDE.md)")"
  [ "$new" = "$(cat "$wt/CLAUDE.md")" ] || { printf '%s\n' "$new" > "$wt/CLAUDE.md"; echo "CLAUDE.md custom section updated"; }
fi
if [ ! -f "$wt/$OVERRIDE" ] || grep -qF "$BEGIN" "$wt/$OVERRIDE"; then
  body=""
  [ -f "$wt/CLAUDE.md" ] && body='Read and follow `CLAUDE.md` in this worktree: its conventions (plan, commits, PRs, reviews) apply to you too.'
  extra="$(layer_text AGENTS.md)"; [ -n "$extra" ] && body="${body:+$body

}$extra"
  if [ -n "$body" ]; then
    new="$(with_block "$wt/AGENTS.md" "$body")"
    [ -f "$wt/$OVERRIDE" ] && [ "$new" = "$(cat "$wt/$OVERRIDE")" ] || { printf '%s\n' "$new" > "$wt/$OVERRIDE"; echo "$OVERRIDE (Codex) updated"; }
  elif [ -f "$wt/$OVERRIDE" ]; then rm -f "$wt/$OVERRIDE"; echo "$OVERRIDE removed"; fi
fi

# --- hooks.json layers merged into .claude/settings.json, replacing last time's (_wtdCustomHooks) ---
add='{}'
while read -r l; do
  [ -f "$l/hooks.json" ] || continue
  add="$(jq -cn --argjson a "$add" --argjson f "$(sed "s#__DEV__#$DEV#g" "$l/hooks.json")" \
    '($f.hooks // {}) as $h | reduce ($h | to_entries[]) as $e ($a; .[$e.key] = ((.[$e.key] // []) + $e.value))')"
done < <(layers "$slug")
proj="$wt/.claude/settings.json"
if [ -f "$proj" ] || [ "$add" != '{}' ]; then
  cur="$( [ -f "$proj" ] && cat "$proj" || echo '{}')"
  next="$(jq --argjson add "$add" '
    (._wtdCustomHooks // {}) as $prev
    | .hooks = ((.hooks // {}) | with_entries(.value -= ($prev[.key] // [])) | with_entries(select(.value | length > 0)))
    | reduce ($add | to_entries[]) as $e (.; .hooks[$e.key] = ((.hooks[$e.key] // []) + $e.value))
    | if ($add | length) == 0 then del(._wtdCustomHooks) | (if (.hooks | length) == 0 then del(.hooks) else . end)
      else ._wtdCustomHooks = $add end' <<<"$cur")"
  if [ "$(jq -S . <<<"$next")" != "$(jq -S . <<<"$cur")" ]; then
    printf '%s\n' "$next" > "$proj"; echo "custom hooks merged into .claude/settings.json"
  fi
fi
