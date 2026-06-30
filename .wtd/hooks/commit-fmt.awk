# Format git-log records into a hanging-indented, width-aware, color-preserving commit list.
# Input: one record per line, fields separated by \037 (US):  sha \037 epoch(%ct) \037 refs(%D) \037 subject \037 day(%cs)
# Records must arrive oldest-first.
# Vars:  -v cols=<pane width>  -v now=<current unix time>
# Output: "* <sha> <age> <(refs) subject…>" with continuation lines indented to just past the
#         timestamp (i.e. under where the subject/refs begin), and a dim dated rule between
#         calendar days. Manual wrap, so callers should disable terminal autowrap.
BEGIN {
  FS = "\037"; if (cols + 0 < 20) cols = 80
  dashes = ""; for (i = 0; i < 200; i++) dashes = dashes "\342\224\200"   # long run of ─, clipped by the pane
  nu = split(unpushed, ua, "\n"); for (i = 1; i <= nu; i++) if (ua[i] != "") UNP[ua[i]] = 1  # unpushed short SHAs
  nd = split(diverged, da, "\n"); for (i = 1; i <= nd; i++) if (da[i] != "") DIV[da[i]] = 1  # SHAs not on the default branch
}

# visible length (ignoring ANSI SGR escapes)
function vislen(s) { gsub(/\033\[[0-9;]*m/, "", s); return length(s) }

# relative "… ago" from an age in seconds — switches to the next unit AT its boundary
# (e.g. 60 minutes -> "1 hour"), unlike git's %cr which holds minutes until 90.
function reltime(s,   m, h, d, w, mo, y) {
  if (s < 0) s = 0
  if (s < 60)  return s " second" (s == 1 ? "" : "s") " ago"
  m = int(s / 60);            if (m < 60)  return m " minute" (m == 1 ? "" : "s") " ago"
  h = int((s + 1800) / 3600); if (h < 24)  return h " hour"   (h == 1 ? "" : "s") " ago"
  d = int((s + 43200) / 86400)
  if (d < 7)   return d " day"   (d == 1 ? "" : "s") " ago"
  if (d < 30)  { w  = int((d + 3)  / 7);  return w  " week"  (w  == 1 ? "" : "s") " ago" }
  if (d < 365) { mo = int((d + 15) / 30); return mo " month" (mo == 1 ? "" : "s") " ago" }
  y = int((d + 182) / 365);   return y " year" (y == 1 ? "" : "s") " ago"
}

# first n visible chars of s, preserving ANSI escapes; sets global REST to the remainder
function vistake(s, n,   out, vis, i, j) {
  out = ""; vis = 0; i = 1
  while (i <= length(s) && vis < n) {
    if (substr(s, i, 2) == "\033[") {
      j = i + 2
      while (j <= length(s) && substr(s, j, 1) !~ /[a-zA-Z]/) j++
      out = out substr(s, i, j - i + 1); i = j + 1
    } else { out = out substr(s, i, 1); vis++; i++ }
  }
  REST = substr(s, i); return out
}

{
  # two record types, sorted by epoch into one timeline:
  #   C:  epoch \037 C \037 sha \037 refs \037 subj \037 day   (a commit)
  #   R:  epoch \037 R \037 token \037 day                     (a past review, rendered as a pink row)
  epoch = $1 + 0; type = $2; age = reltime(now + 0 - epoch)
  day = (type == "R") ? $4 : $6
  if (day != prevday) {                                # dim dated rule between calendar days
    printf "\033[90m\342\224\200\342\224\200 %s %s\033[0m\n", day, dashes
    prevday = day
  }
  if (type == "R") {                                   # review, in time order among the commits; $3 = clickable token
    printf "\033[35m\342\237\263\033[0m \033[1;35m%s\033[0m \033[90m%s\033[0m \033[2;35m\302\267 review\033[0m\n", $3, age
    next
  }
  sha = $3; refs = $4; subj = $5
  ind = 2 + vislen(sha) + 1 + vislen(age) + 1          # line-1: where the subject starts ("* sha age ")
  contind = 3                                           # small hanging indent for wrapped lines so the
  pad = sprintf("%" contind "s", "")                    # continuation text uses ~the FULL pane width
  avail1 = cols - ind;     if (avail1 < 8) avail1 = 8   # capacity of the first line (after sha+age)
  availc = cols - contind; if (availc < 8) availc = 8   # capacity of each continuation line
  content = (vislen(refs) > 0 ? "(" refs ") " : "") subj

  nlines = 0; cur = ""; curvis = 0; av = avail1
  m = split(content, toks, " ")
  for (k = 1; k <= m; k++) {
    w = toks[k]; if (w == "") continue
    wv = vislen(w)
    if (wv > av) {                                       # word longer than a line: hard-break it
      if (curvis > 0) { lines[++nlines] = cur; cur = ""; curvis = 0; av = availc }
      rem = w
      while (vislen(rem) > av) { lines[++nlines] = vistake(rem, av); rem = REST; av = availc }
      cur = rem; curvis = vislen(rem); continue
    }
    if (curvis == 0)                   { cur = w; curvis = wv }
    else if (curvis + 1 + wv <= av)    { cur = cur " " w; curvis += 1 + wv }
    else                               { lines[++nlines] = cur; av = availc; cur = w; curvis = wv }
  }
  if (curvis > 0) lines[++nlines] = cur
  if (nlines == 0) lines[++nlines] = ""

  shacol = (sha in UNP) ? "\033[36m" : "\033[33m"   # unpushed SHA = cyan; pushed = yellow
  # bullet marks divergence from the default branch: magenta ┃ = this commit is NOT on <base>
  # (unique to this branch); dim * = shared with the default branch.
  bullet = (base != "" && (sha in DIV)) ? "\033[1;35m\342\224\203\033[0m" : "\033[90m*\033[0m"
  printf "%s %s%s\033[0m \033[90m%s\033[0m %s\n", bullet, shacol, sha, age, lines[1]
  for (k = 2; k <= nlines; k++) printf "%s%s\n", pad, lines[k]
  for (k in lines) delete lines[k]
}
