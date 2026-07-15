#!/usr/bin/env python3
"""Render a blueprint markdown plan (.claude/plans/active-plan.md) into the LIVING-PLAN HTML the roster
preview panel shows in its `plan` tab.

The panel only renders a staged .html; the documented source of truth is the markdown active-plan.md
(written by /blueprint). This bridges the two: it emits the same visual structure the /plan template
uses — a header, a progress counter, one `.item` per checklist step (checkbox = done, ▶ Start button
wired by the panel host), a Progress-log list, plus any other prose sections — so the markdown plan
renders and stays interactive without the agent hand-authoring HTML.

Self-contained — stdlib only (keeps the engine shippable). Reads the markdown, writes HTML to stdout.

Usage:  plan-md-to-html.py <active-plan.md> [out.html]
        (writes to out.html if given, else stdout — always UTF-8, whatever the platform locale)
"""
import html
import re
import sys

# --- inline markdown -> HTML (escape first so real </>&  in the plan can't inject tags) ---
def inline(s):
    s = html.escape(s, quote=False)
    s = re.sub(r"`([^`]+)`", lambda m: "<code>" + m.group(1) + "</code>", s)
    s = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", lambda m: '<a href="' + m.group(2) + '">' + m.group(1) + "</a>", s)
    s = re.sub(r"\*\*([^*]+)\*\*", lambda m: "<b>" + m.group(1) + "</b>", s)
    s = re.sub(r"__([^_]+)__", lambda m: "<b>" + m.group(1) + "</b>", s)
    s = re.sub(r"(?<![\w*])\*([^*\n]+)\*(?![\w*])", lambda m: "<i>" + m.group(1) + "</i>", s)
    s = re.sub(r"(?<!\w)_([^_\n]+)_(?!\w)", lambda m: "<i>" + m.group(1) + "</i>", s)
    return s

TASK_RE = re.compile(r"^(\s*)[-*+]\s*\[([ xX])\]\s*(.*)$")
BULLET_RE = re.compile(r"^(\s*)[-*+]\s+(.*)$")
HEAD_RE = re.compile(r"^(#{1,6})\s+(.*)$")


def parse_sections(lines):
    """Split into an ordered list of sections: {name, level, body:[lines]}. Text before the first
    heading (and the top-level title) is captured separately."""
    title = None
    intro = []
    sections = []
    cur = None
    for ln in lines:
        m = HEAD_RE.match(ln)
        if m:
            lvl, txt = len(m.group(1)), m.group(2).strip()
            if lvl == 1 and title is None:
                title = txt
                continue
            cur = {"name": txt, "level": lvl, "body": []}
            sections.append(cur)
            continue
        (cur["body"] if cur else intro).append(ln)
    return title, intro, sections


def collect_items(body):
    """Pull checklist items out of a section body. Each item = its task line plus any indented
    continuation lines. Returns (items, leftover_lines) where leftover are non-item lines."""
    items, leftover = [], []
    i, n = 0, len(body)
    while i < n:
        m = TASK_RE.match(body[i])
        if not m:
            leftover.append(body[i]); i += 1; continue
        indent = len(m.group(1).expandtabs())
        done = m.group(2).lower() == "x"
        text = [m.group(3)]
        i += 1
        while i < n:
            nxt = body[i]
            if not nxt.strip():
                break
            if TASK_RE.match(nxt):
                break
            if len(nxt) - len(nxt.lstrip()) <= indent and BULLET_RE.match(nxt):
                break
            text.append(nxt.strip()); i += 1
        items.append({"done": done, "text": text})
    return items, leftover


_next_seq = [0]


def _badge_from(tok):
    """Derive a short id badge from a leading token; return (badge, remainder) or (None, tok)."""
    m = re.match(r"^step\s+(\w+)\s*[—:-]?\s*", tok, re.I)
    if m:
        return "S" + m.group(1), tok[m.end():]
    m = re.match(r"^([A-Za-z])[.)]\s*", tok)
    if m:
        return m.group(1).upper(), tok[m.end():]
    m = re.match(r"^(\d+)[.)]\s*", tok)
    if m:
        return "P" + m.group(1), tok[m.end():]
    return None, tok


def build_item(it):
    """Split an item into (badge, title, desc). Handles bold-prefixed names (`**Step 1 — Name:** …`),
    numbered/lettered leads (`1. …`, `A. …`), and plain items."""
    first = it["text"][0].strip()
    cont = " ".join(x.strip() for x in it["text"][1:] if x.strip())
    badge, ttl, desc = None, first, cont
    m = re.match(r"^\*\*(.+?)\*\*\s*(.*)$", first)   # bold-prefixed step name
    if m:
        name, rest = m.group(1).strip(), m.group(2).strip()
        desc = (rest + (" " + cont if cont else "")).strip()
        badge, name = _badge_from(name)
        ttl = re.sub(r"^\s*[—:-]\s*", "", name).rstrip(":").strip() or first
    else:
        badge, rest = _badge_from(first)
        if badge:
            ttl = rest.strip()
    if not badge:
        _next_seq[0] += 1
        badge = "P" + str(_next_seq[0])
    return badge, ttl, desc


def item_html(it):
    badge, ttl, desc = build_item(it)
    checked = " checked" if it["done"] else ""
    parts = ['<div class="item">',
             '<input type="checkbox"' + checked + ">",
             '<span class="id">' + html.escape(badge) + "</span>",
             '<span class="body"><span class="row1"><span class="ttl">' + inline(ttl) + "</span></span>"]
    if desc:
        parts.append('<span class="desc">' + inline(desc) + "</span>")
    parts.append("</span>")
    parts.append('<button class="go" type="button" title="Tell this worktree\'s session to start this step">&#9654; Start</button>')
    parts.append("</div>")
    return "".join(parts)


def prose_html(body):
    """Render a non-checklist section body: paragraphs, bullets (nested by indent), bold/code/links."""
    out = []
    i, n = 0, len(body)
    para = []
    def flush():
        if para:
            out.append("<p>" + inline(" ".join(x.strip() for x in para)) + "</p>")
            para.clear()
    in_list = False
    def close_list():
        nonlocal in_list
        if in_list:
            out.append("</ul>"); in_list = False
    while i < n:
        ln = body[i]
        b = BULLET_RE.match(ln)
        if b:
            flush()
            if not in_list:
                out.append("<ul>"); in_list = True
            out.append("<li>" + inline(b.group(2)) + "</li>")
            i += 1; continue
        if not ln.strip():
            flush(); close_list(); i += 1; continue
        close_list()
        para.append(ln); i += 1
    flush(); close_list()
    return "\n".join(out)


CSS = """
:root{--bg:#0b0e14;--panel:#121924;--bd:#232e3d;--bd2:#2c3849;--tx:#c9d3e0;--mut:#7d8899;--dim:#5b6675;
  --blue:#4aa3ff;--green:#3fb950;--grey:#6e7681;}
*{box-sizing:border-box}
body{margin:0;padding:26px 30px 60px;background:var(--bg);color:var(--tx);
  font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif;}
code{font-family:"SF Mono",Consolas,"Liberation Mono",monospace;font-size:.86em;background:#1c2430;
  border:1px solid var(--bd);border-radius:4px;padding:1px 5px;color:#d7e0ec;}
a{color:var(--blue)}
.h1{font-size:24px;font-weight:700;letter-spacing:-.4px;margin:0 0 6px;color:#eaf1fb}
.lead{font-size:14.5px;color:#aab6c6;margin:0 0 4px}
.prog{margin:18px 0 8px;display:flex;align-items:center;gap:12px}
.track{flex:1;height:8px;border-radius:5px;background:#1a2230;overflow:hidden;border:1px solid var(--bd)}
.fill{height:100%;width:0;background:linear-gradient(90deg,var(--blue),var(--green));transition:width .35s ease}
.count{font-size:13px;color:var(--mut);font-variant-numeric:tabular-nums;white-space:nowrap}
.count b{color:var(--green)}
.legend{margin:12px 0 20px;font-size:12px;color:var(--mut)}
.phase{margin:24px 0 6px}
.phead{display:flex;align-items:baseline;gap:10px;font-size:12px;font-weight:700;letter-spacing:1px;
  text-transform:uppercase;color:var(--mut);border-bottom:1px solid var(--bd);padding-bottom:7px;margin-bottom:12px}
.item{display:flex;gap:13px;align-items:flex-start;background:var(--panel);border:1px solid var(--bd);
  border-radius:10px;padding:13px 15px;margin:9px 0;transition:opacity .2s,background .2s}
.item input[type=checkbox]{margin:2px 0 0;width:17px;height:17px;flex:none;accent-color:var(--green);cursor:pointer}
.id{flex:none;font-size:11px;font-weight:700;color:#9fb0c4;background:#1a2331;border:1px solid var(--bd2);
  border-radius:6px;padding:3px 8px;letter-spacing:.5px;min-width:42px;text-align:center}
.body{flex:1;min-width:0}
.row1{display:flex;align-items:center;gap:9px;flex-wrap:wrap;margin-bottom:3px}
.ttl{font-weight:600;color:#e7eefa;font-size:14.5px}
.desc{color:#93a1b4;font-size:13px}
.desc b{color:#c3cede;font-weight:600}
.go{flex:none;align-self:center;cursor:pointer;font:600 11.5px system-ui;color:#9cc7ff;background:#132131;
  border:1px solid #27405c;border-radius:7px;padding:5px 11px;white-space:nowrap;transition:.15s}
.go:hover{background:#1b3350;color:#cfe4ff;border-color:#3a5c82}
.go:active,.go.go-fired{background:#1f4d31;border-color:#2f7d4e;color:#9ff0b8}
.item.is-done{opacity:.5}.item.is-done .go{opacity:.4}
.item.is-done .ttl{text-decoration:line-through;text-decoration-color:var(--dim)}
.notes{margin:22px 0 6px}
.notes .phead{margin-bottom:8px}
.notes p{color:#93a1b4;font-size:13px;margin:8px 0}
.notes ul{margin:8px 0;padding-left:20px}
.notes li{color:#93a1b4;font-size:13px;margin:4px 0}
.notes b{color:#c3cede}
.log{margin-top:28px}
.log ul{margin:10px 0 0;padding-left:0;list-style:none}
.log li{padding:7px 0 7px 16px;border-left:2px solid var(--bd2);margin-left:3px;color:#9aa7b8;font-size:13px}
.log li b{color:#c3cede}
.log .empty{color:var(--dim);font-style:italic;border:none;padding-left:0}
.foot{margin-top:30px;color:var(--dim);font-size:11.5px;border-top:1px solid var(--bd);padding-top:12px}
"""

SCRIPT = """
function sync(){
  var items=[].slice.call(document.querySelectorAll('.item'));var done=0;
  items.forEach(function(it){var on=it.querySelector('input[type=checkbox]').checked;
    it.classList.toggle('is-done',on);if(on)done++;});
  document.getElementById('ndone').textContent=done;
  document.getElementById('ntot').textContent=items.length;
  document.getElementById('fill').style.width=items.length?(done/items.length*100)+'%':'0%';
}
document.addEventListener('change',function(e){if(e.target.matches('input[type=checkbox]'))sync();});
sync();
"""


def main():
    if len(sys.argv) < 2:
        print("usage: plan-md-to-html.py <active-plan.md>", file=sys.stderr); sys.exit(2)
    try:
        with open(sys.argv[1], encoding="utf-8", errors="replace") as f:
            lines = f.read().split("\n")
    except OSError as e:
        print("cannot read " + sys.argv[1] + ": " + str(e), file=sys.stderr); sys.exit(1)

    title, intro, sections = parse_sections(lines)
    title = title or "Living plan"
    # lead = first Goal bullet, else the first non-blank intro line
    lead = ""
    for sec in [{"body": intro}] + sections:
        m = re.search(r"\*\*\s*goal\b[^*]*\*\*\s*[:—-]?\s*(.+)", "\n".join(sec["body"]), re.I)
        if m:
            lead = m.group(1).strip(); break
    if not lead:
        for ln in intro:
            if ln.strip():
                lead = ln.strip(); break

    body_html = []
    for sec in sections:
        is_log = re.search(r"progress\s*log", sec["name"], re.I) is not None
        items, leftover = ([], sec["body"]) if is_log else collect_items(sec["body"])
        if is_log:
            body_html.append('<section class="log"><div class="phead"><span>' + inline(sec["name"])
                             + '</span></div><ul>')
            entries = [b.group(2) for b in (BULLET_RE.match(x) for x in sec["body"]) if b]
            if entries:
                for e in entries:
                    body_html.append("<li>" + inline(e) + "</li>")
            else:
                body_html.append('<li class="empty">No steps completed yet.</li>')
            body_html.append("</ul></section>")
        elif items:
            body_html.append('<section class="phase"><div class="phead"><span>' + inline(sec["name"])
                             + "</span></div>")
            body_html.extend(item_html(it) for it in items)
            body_html.append("</section>")
        elif "".join(sec["body"]).strip():
            body_html.append('<section class="notes"><div class="phead"><span>' + inline(sec["name"])
                             + "</span></div>" + prose_html(sec["body"]) + "</section>")

    doc = (
        "<!doctype html>\n"
        "<!-- auto-generated from .claude/plans/active-plan.md by plan-md-to-html.py -->\n"
        '<html lang="en"><head><meta charset="utf-8"><title>PLAN — ' + html.escape(title) + "</title>\n"
        "<style>" + CSS + "</style></head><body>\n"
        '<h1 class="h1">' + inline(title) + "</h1>\n"
        + ('<p class="lead">' + inline(lead) + "</p>\n" if lead else "")
        + '<div class="prog"><div class="track"><div class="fill" id="fill"></div></div>'
          '<div class="count"><b id="ndone">0</b> / <span id="ntot">0</span> done</div></div>\n'
        '<div class="legend">tick a box to complete · &#9654; Start hands a step to this worktree\'s session</div>\n'
        + "\n".join(body_html) + "\n"
        '<div class="foot">Living plan · rendered from <code>.claude/plans/active-plan.md</code></div>\n'
        "<script>" + SCRIPT + "</script></body></html>\n"
    )
    if len(sys.argv) > 2:
        with open(sys.argv[2], "w", encoding="utf-8") as f:
            f.write(doc)
    else:
        sys.stdout.buffer.write(doc.encode("utf-8"))


if __name__ == "__main__":
    main()
