#!/usr/bin/env bash
# Package the worktree-dev ENGINE into a portable tarball a teammate can drop into ~/dev and install.
# Ships the tooling + docs + example repo registry; EXCLUDES local, heavy, and SECRET data
# (worktrees, bare clones, refs, any .env with creds, old vsix builds, runtime state). Shipped
# templates carry __DEV__/__USER__/__DISTRO__ placeholders that the recipient's install.sh renders to
# their machine. (When the engine lives in its own git repo, just clone it — this is for offline copies.)
#
# Usage:  ship [OUTFILE.tgz]      (default: ~/worktree-dev-<date>.tgz)
set -euo pipefail
WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
OUT="${1:-$HOME/worktree-dev-$(date +%Y%m%d).tgz}"

stageroot="$(mktemp -d)"; trap 'rm -rf "$stageroot"' EXIT
stage="$stageroot/worktree-dev"; mkdir -p "$stage"

# --- copy only the engine pieces (never worktrees/ repos/ refs/ .claude/ .claude-status) ---
cp -a "$WTD" "$stage/.wtd"
for f in Makefile README.md; do [ -e "$DEV/$f" ] && cp -a "$DEV/$f" "$stage/"; done
[ -d "$DEV/.vscode" ] && cp -a "$DEV/.vscode" "$stage/.vscode"
# NB: dev-root docs/ is project scratch (not engine docs — the README's docs/ means the *repos'* docs),
# and worktrees/ repos/ refs/ .claude/ are local/heavy/runtime — none are copied above.

# --- prune from the staged copy ---
rm -rf "$stage/.wtd/env"                        # SECRETS: any local .env + personal config
find "$stage" -name '*.bak' -delete            # editor/install backups, anywhere in the tree
# keep only the newest vsix (drop the build history)
vd="$stage/.wtd/templates/vscode-claude-status"
if [ -d "$vd" ]; then
  newest="$(ls -1 "$vd"/claude-status-*.vsix 2>/dev/null | sort -V | tail -1 || true)"
  [ -n "$newest" ] && find "$vd" -name 'claude-status-*.vsix' ! -name "$(basename "$newest")" -delete
fi

# --- safety: never ship secrets/credentials ---
if find "$stage" \( -name '.env' -o -name '*.env' \) -print | grep -q .; then
  echo "ABORT: a .env file reached the staged tree:" >&2
  find "$stage" \( -name '.env' -o -name '*.env' \) >&2; exit 1
fi
secrets="$(grep -rIlE '(PASSWORD|SECRET|_PW|API[_-]?KEY|TOKEN)=' "$stage" --exclude='ship.sh' 2>/dev/null || true)"
if [ -n "$secrets" ]; then
  echo "ABORT: possible credential(s) in the staged tree:" >&2; printf '  %s\n' $secrets >&2; exit 1
fi
# --- warn (non-fatal): personal strings the recipient must override ---
leftover="$(grep -rIlE '/home/dev-user' "$stage" --exclude='ship.sh' 2>/dev/null || true)"
if [ -n "$leftover" ]; then
  echo "NOTE: these shipped files still mention the placeholder home stub (comments or overridable defaults):"
  printf '      %s\n' $leftover
  echo "      → harmless; install.sh renders __DEV__ to the recipient's real path."
fi

tar czf "$OUT" -C "$stageroot" worktree-dev
size="$(du -h "$OUT" | cut -f1)"

cat <<EOF

==> shipped: $OUT  ($size)

   Give this to the teammate. On their Unix box (or Git Bash on Windows):

     mkdir -p ~/dev && tar xzf $(basename "$OUT") -C ~/dev --strip-components=1
     ~/dev/.wtd/scripts/install.sh        # renders paths, links commands, installs the extension
     # then, once: log in to Claude ('claude') and ensure SSH access to the repos they'll work on.

   Register repos with 'add-repo <slug> <git-url>'; bare clones are created on first 'agent <slug> <name>'.
EOF
