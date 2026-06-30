# ledger-merge.awk — merge a reviewer's ledger-delta into the per-repo conventions ledger.
# Usage:  awk -f ledger-merge.awk <delta.md> <ledger.md>   (delta FIRST, ledger SECOND)
# Upserts by the exact "## <heading>" line: a delta section REPLACES the same-headed ledger section
# in place (keeping its position); delta-only sections are appended; ledger-only sections are kept.
# Lines before the first "## " are preamble and pass through. Result goes to stdout.
FNR==NR {                                   # delta file
  if ($0 ~ /^## /) { cur=$0; if (!(cur in D)) order[++n]=cur; D[cur]="" }
  if (cur != "") D[cur] = D[cur] $0 "\n"
  next
}
{                                           # ledger file
  if ($0 ~ /^## /) {
    h=$0
    if (h in D) { repl=1; if (!emitted[h]) { printf "%s", D[h]; emitted[h]=1 } }
    else { repl=0; print }
  } else if (!repl) print
}
END { for (i=1;i<=n;i++) { h=order[i]; if (!emitted[h]) { printf "%s", D[h]; emitted[h]=1 } } }
