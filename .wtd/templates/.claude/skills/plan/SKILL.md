---
name: plan
description: >
  Create and maintain a LIVING visual plan for this worktree — a self-contained HTML checklist that
  opens in the fleet roster's preview panel and is kept in sync as work progresses. Use when the user
  wants a visual plan, a plan they can watch, a living checklist, or invokes /plan. Builds the plan at
  .claude/plans/active-plan.html and stages it with the `preview` skill so a 🖼 appears on this
  worktree's row; re-stage after each step to tick items off. Pairs with /blueprint (which owns the
  markdown active-plan.md) — this is the human-facing, always-current view.
allowed-tools: Read, Grep, Glob, Bash, Write, Edit
---

# Living plan (visual checklist)

Turn the work for this worktree into a **self-contained HTML checklist** that lives in the roster
preview panel and stays current as you execute. The agent owns it: you tick items off and log
progress here as you go, so the user can watch the plan fill in without reading the terminal.

The state of record is this HTML file — a step is **done** when its checkbox has `checked`. Ticking a
box in the panel is ephemeral (the webview is sandboxed); real progress is you editing the file and
re-running `preview`.

## 1. Gather the plan
- If `.claude/plans/active-plan.md` already exists (e.g. from `/blueprint`), **use its steps** —
  this HTML is the visual mirror of that plan; keep them consistent.
- Otherwise scope it yourself: read the repo's standards/docs for this kind of change, find the target
  code, and break the work into **sequential, commit-sized steps** (err toward fewer, larger steps —
  merge anything that would always be reviewed or reverted together). Don't ask the user what the
  codebase already answers.

## 2. Write the HTML plan
Copy the template and fill it in:
```
mkdir -p .claude/plans
cp .claude/skills/plan/references/plan.html .claude/plans/active-plan.html
```
Then edit `.claude/plans/active-plan.html` (the template's top comment documents every slot):
- **Header** — title, one-sentence goal, and a context line a fresh (context-wiped) agent would need.
- **Phases** — one `<section class="phase">` per phase; inside, one `.item` per step with a short
  `id` badge (P1, P2, …), an imperative **title**, and a **desc** naming the files/dirs it touches
  (use `<code>` for identifiers). Each step's desc should remind: **commit + push, log progress, then
  ask before the next step.**
- **Keep the fixed tail last** — `Tests` and `Green build`. Reviews are **not** a step (the user runs
  `wt-review`, never you).
- Leave every checkbox unchecked at first (unless a step is genuinely already done).

Keep it **self-contained** — all CSS/JS inline, no network (it renders sandboxed). Don't add external
images or fonts.

Every item carries a **▶ Start** button. When the plan is open in the preview panel, clicking it tells
**this worktree's session** to begin that step (it sends the step's **title** as the instruction — so
titles must be self-contained and imperative). The button is inert if the file is opened outside the
panel. You don't wire anything for this; the panel host does. Just write clear titles.

## 3. Stage it in the panel
From the worktree root:
```
preview .claude/plans/active-plan.html plan
```
A 🖼 appears on this worktree's roster row. Tell the user: **"Living plan staged — click the 🖼 on
this worktree's row to watch it; hit ▶ Start on any step to kick it off."**

## 4. Keep it living — update it whenever you finish work
This is the whole point of the skill. **Every time you complete a step (or any meaningful chunk of
work), before you hand back to the user, update the plan:**
- Add `checked` to that item's `<input>` — the row dims + strikes through and the `done / total`
  counter advances automatically.
- Append a `<li>` to the **Progress log**: what changed, the pushed commit, what's next.
- Adjust steps if scope changed — add / split / reword items so the plan always matches reality.

You do **not** need to re-run `preview` — editing `.claude/plans/active-plan.html` auto-re-stages it
(a hook) and the open panel refreshes in place. Just keep the file honest.

A fresh agent (or the user glancing at the panel) should be able to see exactly where the work stands
from this file alone — **never finish a step and leave the plan stale.**
