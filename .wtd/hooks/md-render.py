#!/usr/bin/env python3
"""Render Markdown to ANSI for a terminal pane (used by the diff pane to show plans nicely).

Self-contained — no third-party deps (keeps the engine shippable). Handles the elements that show up
in plan docs: ATX headings, bullet/numbered lists, task checkboxes, fenced code blocks, blockquotes,
horizontal rules, tables, and inline **bold** / *italic* / `code` / [links](url). Paragraphs wrap to
the pane width.

Usage:  md-render.py <file.md> [width]      (width defaults to $COLUMNS or 100)
"""
import os, re, sys, textwrap

# Absolute worktree root for resolving relative file paths in the doc (set in main()). Repo-relative
# paths like `pkg/x/y.go` appear identically in every worktree + ref, so a bare click does a
# workspace-wide search and matches them all; anchoring each to THIS worktree's absolute file (via an
# OSC 8 hyperlink) makes the click open exactly one file while the visible text stays short/relative.
WT_ROOT = None
# path-like token: 2+ slash-separated segments ending in a .ext. The (?<!/) guard skips the tail of an
# already-absolute path (e.g. the "home/..." inside "/home/..."), which the isfile check drops anyway.
PATH_RE = re.compile(r"(?<!/)[\w@.+-]+(?:/[\w@.+-]+)+\.[A-Za-z0-9]+")

# --- ANSI ---
R = "\033[0m"; B = "\033[1m"; DIM = "\033[2m"; IT = "\033[3m"; UL = "\033[4m"
def c(n): return f"\033[38;5;{n}m"
H = [c(213), c(81), c(150), c(180), c(245), c(245)]   # heading colors by level 1..6
CODE = c(180); QUOTE = c(109); RULE = c(240); BULLET = c(81); LINK = c(75); NUM = c(214)

def main():
    args = sys.argv[1:]
    if not args:
        print("usage: md-render.py <file.md> [width]", file=sys.stderr); sys.exit(2)
    path = args[0]
    # Derive the worktree root from the doc's own location so relative paths resolve to THIS worktree:
    # docs live at <wt>/.claude/... (plans, reviews); other docs (e.g. <wt>/pr-notes.md) sit at the root.
    global WT_ROOT
    ap = os.path.abspath(path)
    WT_ROOT = ap.split("/.claude/")[0] if "/.claude/" in ap else os.path.dirname(ap)
    try:
        width = int(args[1]) if len(args) > 1 else int(os.environ.get("COLUMNS", "100"))
    except ValueError:
        width = 100
    width = max(40, min(width, 200))
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.read().split("\n")
    except OSError as e:
        print(f"{c(203)}cannot read {path}: {e}{R}", file=sys.stderr); sys.exit(1)
    out = []
    render(lines, width, out)
    sys.stdout.write("\n".join(out) + "\n")

def vislen(s):  # printable length ignoring ANSI SGR colors AND OSC 8 hyperlink markers
    s = re.sub(r"\033\]8;;.*?\033\\", "", s)        # strip OSC 8 start/end (URI is not displayed)
    return len(re.sub(r"\033\[[0-9;]*m", "", s))

def inline(s):
    # Shield file paths FIRST: stash each existing repo-relative path as a NUL placeholder so the
    # markdown styling below can't inject ANSI into the absolute file:// URI we attach (and so an ANSI
    # escape can't merge with a path digit). Restore them as OSC 8 hyperlinks at the very end.
    links = []
    if WT_ROOT:
        def stash(m):
            rel = m.group(0); target = os.path.join(WT_ROOT, rel)
            if os.path.isfile(target):
                links.append((rel, target)); return f"\x00{len(links)-1}\x00"
            return rel
        s = PATH_RE.sub(stash, s)
    # links FIRST — they match [..](..); doing them before any ANSI escape (which contains '[') is
    # injected avoids the link regex mis-matching the '[' inside a code/bold escape sequence.
    s = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", lambda m: f"{UL}{LINK}{m.group(1)}{R} {DIM}({m.group(2)}){R}", s)
    s = re.sub(r"`([^`]+)`", lambda m: f"{CODE}{m.group(1)}{R}", s)
    s = re.sub(r"\*\*([^*]+)\*\*", lambda m: f"{B}{m.group(1)}{R}", s)
    s = re.sub(r"__([^_]+)__", lambda m: f"{B}{m.group(1)}{R}", s)
    s = re.sub(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])", lambda m: f"{IT}{m.group(1)}{R}", s)
    if links:
        s = re.sub(r"\x00(\d+)\x00",
                   lambda m: (lambda rel, tgt: f"\033]8;;file://{tgt}\033\\{rel}\033]8;;\033\\")(*links[int(m.group(1))]), s)
    return s

def render(lines, width, out):
    i, n = 0, len(lines)
    para = []
    def flush_para():
        if not para:
            return
        text = " ".join(x.strip() for x in para)
        for w in textwrap.wrap(text, width=width, break_long_words=False, break_on_hyphens=False) or [""]:
            out.append(inline(w))   # wrap PLAIN, then style — never split an ANSI escape
        para.clear()
    while i < n:
        ln = lines[i]
        # fenced code block
        m = re.match(r"^\s*```(.*)$", ln)
        if m:
            flush_para()
            lang = m.group(1).strip()
            i += 1
            code = []
            while i < n and not re.match(r"^\s*```", lines[i]):
                code.append(lines[i]); i += 1
            i += 1  # closing fence
            bar = "│ "
            if lang:
                out.append(f"{DIM}{RULE}┌─ {lang} {'─'*max(0,width-len(lang)-4)}{R}")
            for cl in code:
                out.append(f"{RULE}{bar}{R}{CODE}{cl}{R}")
            out.append(f"{DIM}{RULE}{'─'*width}{R}")
            continue
        # heading
        m = re.match(r"^(#{1,6})\s+(.*)$", ln)
        if m:
            flush_para()
            lvl = len(m.group(1)); col = H[lvl-1]
            txt = inline(m.group(2).strip())
            out.append("")
            out.append(f"{B}{col}{txt}{R}")
            if lvl <= 2:
                out.append(f"{col}{'─'*min(width, vislen(txt))}{R}")
            i += 1; continue
        # horizontal rule
        if re.match(r"^\s*([-*_])\s*(\1\s*){2,}$", ln):
            flush_para(); out.append(f"{RULE}{'─'*width}{R}"); i += 1; continue
        # table row
        if "|" in ln and ln.strip().startswith("|"):
            flush_para()
            block = []
            while i < n and "|" in lines[i] and lines[i].strip().startswith("|"):
                block.append(lines[i]); i += 1
            render_table(block, width, out)
            continue
        # blockquote
        m = re.match(r"^\s*>\s?(.*)$", ln)
        if m:
            flush_para()
            out.append(f"{QUOTE}▏ {R}{DIM}{inline(m.group(1))}{R}"); i += 1; continue
        # list item (bullet / numbered / checkbox)
        m = re.match(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$", ln)
        if m:
            flush_para()
            indent = len(m.group(1).replace("\t", "  "))
            pad = " " * indent
            marker, body = m.group(2), m.group(3)
            cb = re.match(r"^\[([ xX])\]\s+(.*)$", body)
            if cb:
                box = f"{c(150)}☑{R}" if cb.group(1).lower() == "x" else f"{c(245)}☐{R}"
                body = cb.group(2); lead = box + " "
            elif marker[0].isdigit():
                lead = f"{NUM}{marker}{R} "
            else:
                lead = f"{BULLET}•{R} "
            wrapped = textwrap.wrap(body, width=max(10, width-indent-2),
                                    break_long_words=False, break_on_hyphens=False) or [""]
            out.append(f"{pad}{lead}{inline(wrapped[0])}")
            for cont in wrapped[1:]:
                out.append(f"{pad}  {inline(cont)}")
            i += 1; continue
        # blank line ends a paragraph
        if ln.strip() == "":
            flush_para(); out.append(""); i += 1; continue
        # plain paragraph text (accumulate)
        para.append(ln); i += 1
    flush_para()

def render_table(block, width, out):
    rows = []
    for ln in block:
        cells = [x.strip() for x in ln.strip().strip("|").split("|")]
        if cells and all(re.match(r"^:?-{3,}:?$", x) for x in cells if x):  # separator row
            continue
        rows.append(cells)
    if not rows:
        return
    ncol = max(len(r) for r in rows)
    for r in rows:                              # pad short rows so every row has ncol cells
        r += [""] * (ncol - len(r))

    sep_w = 3 * (ncol - 1)                       # " │ " between columns
    budget = max(ncol * 4, width - 1)           # keep each line within the pane (avoids less-wrap)
    nat = [0] * ncol                            # natural (unwrapped) visible width per column
    for r in rows:
        for j in range(ncol):
            nat[j] = max(nat[j], vislen(inline(r[j])))
    # shrink the widest columns until the row fits the pane; wrapping (below) absorbs the rest
    widths = list(nat)
    MINW = 6
    while sum(widths) + sep_w > budget and max(widths) > MINW:
        widths[widths.index(max(widths))] -= 1
    while sum(widths) + sep_w > budget and max(widths) > 1:   # everything's at the floor — keep going
        widths[widths.index(max(widths))] -= 1

    sepstr = f"{RULE} │ {R}"
    for ri, r in enumerate(rows):
        # wrap each cell's PLAIN text to its column width, THEN style (never split an ANSI escape)
        wrapped = []
        for j in range(ncol):
            lines = textwrap.wrap(r[j], width=max(1, widths[j]),
                                  break_long_words=True, break_on_hyphens=False) or [""]
            wrapped.append([inline(x) for x in lines])
        for k in range(max(len(c) for c in wrapped)):          # emit aligned continuation lines
            parts = []
            for j in range(ncol):
                seg = wrapped[j][k] if k < len(wrapped[j]) else ""
                parts.append(seg + " " * max(0, widths[j] - vislen(seg)))
            out.append((B if ri == 0 else "") + sepstr.join(parts) + R)
        if ri == 0:
            out.append(f"{RULE}{'─' * min(width, sum(widths) + sep_w)}{R}")

if __name__ == "__main__":
    main()
