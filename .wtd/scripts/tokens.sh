#!/usr/bin/env bash
# Per-worktree token usage + estimated API cost from Claude Code session logs, grouped by current
# worktree status (working/input/done/not-running).
# Usage: tokens.sh [filter-substring] [--since YYYY-MM-DD] [--sort out|in|cacheR|cacheW|total|cost] [--all]
# Default: only worktrees that currently exist (git worktree list, across all registered repos).
# --all also includes historical/deleted worktrees.
set -euo pipefail

WTD="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"   # ~/dev/.wtd
DEV="$(dirname "$WTD")"                                                   # ~/dev
REG="$WTD/repos.tsv"

# Map: project-dir prefix -> repo slug.  Project dirs are the worktree abs path with '/' -> '-'.
# New layout: <DEV>/worktrees/<slug>/<name>.
prefixes() {
  local dashed_dev="${DEV//\//-}"        # e.g. -home-dev-user-dev
  while IFS=$'\t' read -r slug url; do
    case "$slug" in ''|'#'*) continue;; esac
    printf '%s-worktrees-%s-\t%s\n' "$dashed_dev" "$slug" "$slug"
  done < "$REG"
}

# Live worktree project-dir names across every registered repo.
existing() {
  while IFS=$'\t' read -r slug url; do
    case "$slug" in ''|'#'*) continue;; esac
    local bare="$DEV/repos/$slug/.bare"
    [ -d "$bare" ] || continue
    git -c safe.bareRepository=all -C "$bare" worktree list --porcelain 2>/dev/null \
      | awk '/^worktree /{print $2}' | sed 's#/#-#g'
  done < "$REG"
}

# Current .claude-status per live worktree:  <dashed-abs-path>\t<status>  (working|input|done|none).
statuses() {
  while IFS=$'\t' read -r slug url; do
    case "$slug" in ''|'#'*) continue;; esac
    local bare="$DEV/repos/$slug/.bare"; [ -d "$bare" ] || continue
    git -c safe.bareRepository=all -C "$bare" worktree list --porcelain 2>/dev/null | awk '/^worktree /{print $2}' | while read -r p; do
      local st="none"; [ -s "$p/.claude-status" ] && st="$(cat "$p/.claude-status" 2>/dev/null)"; [ -z "$st" ] && st="none"
      printf '%s\t%s\n' "$(printf '%s' "$p" | sed 's#/#-#g')" "$st"
    done
  done < "$REG"
}

EXISTING="$(existing | paste -sd, -)"; export EXISTING
PREFIXES="$(prefixes)"; export PREFIXES
STATUSES="$(statuses)"; export STATUSES

exec python3 - "$@" <<'PY'
import os, json, glob, sys
from datetime import datetime

BASE = os.path.expanduser("~/.claude/projects")
argv = sys.argv[1:]

def opt(name, default=None):
    return argv[argv.index(name) + 1] if name in argv else default

flt = next((a for a in argv if not a.startswith("--")
            and (argv.index(a) == 0 or argv[argv.index(a) - 1] not in ("--since", "--sort"))), None)
since = opt("--since")
since_ts = datetime.fromisoformat(since).timestamp() if since else 0
sortkey = opt("--sort", "out")
show_all = "--all" in argv

# Estimated API list price, USD per token: (input, output, cache-write 5m, cache-read).
PRICES = {
    "opus":   (15e-6, 75e-6, 18.75e-6, 1.5e-6),
    "sonnet": (3e-6,  15e-6, 3.75e-6,  0.3e-6),
    "haiku":  (1e-6,  5e-6,  1.25e-6,  0.1e-6),
}
def price(model):
    m = (model or "").lower()
    for k in PRICES:
        if k in m:
            return PRICES[k]
    return PRICES["opus"]   # unknown model → assume opus (conservative)
existing = set(x for x in os.environ.get("EXISTING", "").split(",") if x)

# (prefix, slug) pairs, longest prefix first so hyphenated slugs disambiguate.
pairs = []
for ln in os.environ.get("PREFIXES", "").splitlines():
    if "\t" in ln:
        p, s = ln.split("\t", 1)
        pairs.append((p, s))
pairs.sort(key=lambda ps: -len(ps[0]))

def classify(name):
    """Return (slug, short_name) if dir belongs to a tracked worktree, else (None, None)."""
    for p, s in pairs:
        if name.startswith(p):
            return s, (name[len(p):] or "(root)")
    return None, None

# project-dir name -> current .claude-status (working|input|done|none); absent => historical/none.
status_map = {}
for ln in os.environ.get("STATUSES", "").splitlines():
    if "\t" in ln:
        k, v = ln.split("\t", 1)
        status_map[k] = v or "none"

rows = []
for d in sorted(glob.glob(os.path.join(BASE, "*"))):
    name = os.path.basename(d)
    slug, short = classify(name)
    if slug is None:
        continue
    if not show_all and existing and name not in existing:
        continue
    if flt and flt not in name:
        continue
    inp = co = cr = out = msgs = 0
    cost = 0.0
    for f in glob.glob(os.path.join(d, "*.jsonl")):
        for line in open(f, errors="ignore"):
            if '"usage"' not in line:
                continue
            try:
                o = json.loads(line)
            except Exception:
                continue
            if since_ts:
                t = o.get("timestamp")
                if t:
                    try:
                        if datetime.fromisoformat(t.replace("Z", "+00:00")).timestamp() < since_ts:
                            continue
                    except Exception:
                        pass
            u = (o.get("message") or {}).get("usage") or o.get("usage")
            if not u:
                continue
            i = u.get("input_tokens", 0); o2 = u.get("output_tokens", 0)
            r = u.get("cache_read_input_tokens", 0); w = u.get("cache_creation_input_tokens", 0)
            inp += i; out += o2; cr += r; co += w
            p = price((o.get("message") or {}).get("model") or o.get("model"))
            cost += i*p[0] + o2*p[1] + w*p[2] + r*p[3]
            msgs += 1
    if msgs:
        rows.append({"slug": slug, "name": short, "status": status_map.get(name, "none"),
                     "in": inp, "cacheW": co, "cacheR": cr,
                     "out": out, "msgs": msgs, "cost": cost, "total": inp + co + cr + out})

def h(n):
    n = float(n)
    for u in ["", "K", "M", "B"]:
        if abs(n) < 1000:
            return f"{n:.0f}{u}" if u == "" else f"{n:.1f}{u}"
        n /= 1000
    return f"{n:.1f}T"

def money(c):
    return f"${c:,.2f}"

W = 38
SEP = W + 41 + 12
print(f"{'repo / worktree':<{W}}{'in':>8}{'cacheW':>9}{'cacheR':>9}{'out':>8}{'msgs':>7}{'est $':>12}")
print("-" * SEP)

grand = {k: 0 for k in ("in", "cacheW", "cacheR", "out", "msgs", "cost")}

# Group by current worktree status. Order + labels:
ORDER = ["working", "input", "pr", "done", "none"]
LABEL = {"working": "● working", "input": "● input (your turn)", "pr": "◆ pr (PR-ready)", "done": "✓ done", "none": "· not running"}
by_status = {}
for r in rows:
    by_status.setdefault(r["status"] if r["status"] in ORDER else "none", []).append(r)

for st in ORDER + sorted(k for k in by_status if k not in ORDER):
    if not by_status.get(st):
        continue
    sub = {k: 0 for k in ("in", "cacheW", "cacheR", "out", "msgs", "cost")}
    print(LABEL.get(st, st))
    for r in sorted(by_status[st], key=lambda r: -r[sortkey]):
        label = f"{r['slug']}/{r['name']}"
        print(f"  {label[:W-2]:<{W-2}}{h(r['in']):>8}{h(r['cacheW']):>9}{h(r['cacheR']):>9}{h(r['out']):>8}{r['msgs']:>7}{money(r['cost']):>12}")
        for k in sub:
            sub[k] += r[k]
    print(f"{'  └ subtotal':<{W}}{h(sub['in']):>8}{h(sub['cacheW']):>9}{h(sub['cacheR']):>9}{h(sub['out']):>8}{sub['msgs']:>7}{money(sub['cost']):>12}")
    print("-" * SEP)
    for k in grand:
        grand[k] += sub[k]

print(f"{'TOTAL':<{W}}{h(grand['in']):>8}{h(grand['cacheW']):>9}{h(grand['cacheR']):>9}{h(grand['out']):>8}{grand['msgs']:>7}{money(grand['cost']):>12}")
PY
