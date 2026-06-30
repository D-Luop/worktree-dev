# <worktree>

<!--
  This is the per-worktree CLAUDE.md that worktree-dev seeds into every new worktree.
  It encodes the team-agnostic working conventions below. Fill in the repo-specific
  bits (the "## Project" and "## Build & deploy" sections) for your own codebase, or
  delete them — everything else is generic and applies to any repo.
-->

## Context / scope

<!-- What this worktree is for. Fill in per task. -->

## Active plan — always keep one (start every session by writing it)

**Begin every session by writing an active plan to `.claude/plans/active-plan.md`** (use `/blueprint`
to scaffold it). It is the **source of truth** for the work in this session — not a throwaway: read it
first, work to it, and **revise it as the work progresses**. Check off steps as you complete them,
append to the Progress Log, and update the plan whenever scope or approach changes. After each step:
commit + push, update the plan, then ask the user before continuing. If a plan already exists when you
start, read it and resume from the last unchecked step (or the bottom of the Progress Log).

(Double-click the `view_active_plan` button in the commit pane to render the plan in the diff pane.)

When the work is ready to open a **pull request**, invoke the **`/pr`** skill — it marks the worktree
**PR-ready** (light-blue 🔹 via `agent pr`; sorts just below working, above done — sticky, auto-reverts
on a real source edit or `agent wip`), pushes committed work, and writes the PR message to
`pr-notes.md`. When the work is fully **finished**, invoke **`/done`** — same idea but marks the
worktree complete (green ✓ via `agent done`). Do either without being asked when the user signals the
matching milestone. (The manual `pr-notes.md` format spec is in "PR messages" below.)

**Finishing is separate from review, and the USER runs reviews — never you.** When you believe the
branch is done, say so and run **`/done`** (mark done + push committed work + write `pr-notes.md`).
`/done` does **not** run or require a review — review is **not** part of finishing. A pre-push review
is a **separate, user-initiated** step: only the user runs `wt-review`, if and when they want it.
**Never launch the reviewer yourself** (`/wt-review`, `wt-review`, `review`) — not as part of `/done`,
not autonomously; run it only if the user explicitly asks. If they do run one, address its **Fix**
findings (or justify ignoring them).

## Project

<!-- Repo-specific orientation. Describe what this codebase is, its language/stack, where the
     source-of-truth lives, where generated code lives (so it's never hand-edited), where shared/
     common helpers live (check them before writing any generic helper), and how to inspect live
     state (DB, services). Delete this section if it doesn't apply. -->

## Read the relevant docs BEFORE you work

This repo's conventions are written down in its docs — **read the applicable guide(s) before writing
or changing code in that area.** Don't infer conventions from one nearby example; follow the doc.
If a doc and the surrounding code disagree, surface it rather than guessing.

## NEVER edit generated / "DO NOT EDIT" files

A file marked `// Code generated … DO NOT EDIT.`, `@generated`, or `linguist-generated` is **OUTPUT** —
do **not** hand-edit it. Your change is silently overwritten on the next codegen run and the file
header then lies about its contents. To change generated output, edit the SOURCE (the schema /
template / codegen config) and regenerate. If regeneration genuinely can't produce what you need,
raise it with the user — don't quietly edit a generated file.

## Build & deploy

<!-- Repo-specific build/test/deploy commands. Fill in for your codebase, or delete. -->

## Referencing files for the user

When you point the user at a file, write its **ABSOLUTE** path (e.g.
`$HOME/dev/worktrees/<slug>/<name>/.claude/plans/active-plan.md`), never a relative one.
VSCode makes a terminal path clickable by resolving it against the pane's working directory, but
this session runs inside tmux, which doesn't report cwd to VSCode — so a relative path like
`.claude/plans/active-plan.md` is NOT clickable. Absolute paths always are. If unsure of the
worktree root, run `pwd` and prefix with it.

## PR messages

When the user readies a PR or says the work is complete: first **actually run the matching status
command** (`agent pr` for PR-ready → light-blue 🔹, or `agent done` for fully complete → green; a Bash
command printing `marked <name> as '<status>'` — don't claim the status without running it). Then
**write the PR message to `pr-notes.md` in the worktree root** (overwrite it) and tell the user its absolute path. Also
(re)write it whenever asked. `pr-notes.md` is git-ignored — a scratch artifact for the PR body.

Format — clean, terse markdown. **Heading levels: `##` for the title, `###` for sections (never `#`):**
- `## <branch>` title.
- One lead sentence: what changed + why (bold the key qualifier if apt, e.g. **Test-only change**).
- `### Why` — a few sentences of context/motivation.
- One `###` section per area of change (e.g. `### Test fixes`, `### Tooling`, `### Build`), each a
  list of bullets led by a **bold scope** (module/package) then a terse description. Use `inline
  code` for identifiers, files, flags, consts.
- `### Out of scope` — what was deliberately NOT done (known-failing, deferred), if any.
- `### Notes` — follow-ups / caveats, if any.
Factual and signal-dense: no filler, no restating the diff, no praise, no AI attribution.

## Repo standards (count the WHOLE repo — never assume from a few hits)

> **DEFAULT BEHAVIOR — do this EVERY time, unprompted.** The moment a question is about what's
> standard / conventional / common / normal / "how we name/do X" / "the right way" — or you're about
> to *claim* something is conventional — the **whole-repo investigation below is mandatory and
> automatic.** Your **first** answer must already contain the full-repo count table and state the
> sample size you scanned. Do NOT give a quick single-example answer and expand only when the user
> pushes back — having to be told "look at more of the repo" means you already failed. No shortcuts,
> every time.

**A standard is a measured majority across the whole repo, not a pattern you saw once or twice.**
The most common mistake here is finding one or two similar spots (often just the package you're
working in, or one neighbor) and declaring "this is the standard." That is wrong and not allowed.
One example is an anecdote; two is a coincidence. You do not know the standard until you have counted
*every* occurrence across the repo.

**Mandatory procedure — do ALL of it before you call anything standard:**
1. **Check the docs FIRST.** A **documented** standard is authoritative — it outranks any code count.
   If the convention is written down, cite the exact doc; the code counts below then just measure how
   well the repo actually follows it.
2. **Enumerate search terms.** List the patterns you'll grep, *including variants and synonyms* — a
   convention is often written several ways, and one regex will miss most of them.
3. **Grep the WHOLE repo** (every module/package, not just the one you're touching) for each pattern.
   Keep widening terms until new searches stop turning up new occurrences.
4. **List every occurrence** with `file:line`. Don't sample, don't stop at "enough" — exhaustive.
5. **Group occurrences by method**, count each, and put them in a **table** (label approaches A, B,
   C, …). Show the search terms you used so coverage is auditable.
6. **Only then conclude:** the standard = what the **docs** prescribe, else the **dominant** pattern by
   count; note where your change fits (label it "mine"); flag divergences; list plausible alternatives
   even at count 0.

| Approach | Count | Which (file:line) |
|---|---|---|
| (A) pre-check key → AlreadyExists | 2 | moduleA/handler.go:40, moduleB/create.go:88 (mine) |
| (B) no pre-check — rely on the unique constraint | 8 | 8× across other modules |
| (C) map a constraint violation to a dup error | 0 | — |

**Hard rules:**
- **A documented rule beats a code tally.** If the docs state the standard, that IS the standard —
  code that diverges from it is a finding to flag, not a vote for an alternative.
- Never answer a "is this standard?" question from a single grep, a single module, or a handful of
  hits. If you haven't counted the repo, say "I haven't counted yet" and go count — don't guess.
- **Report your sample size.** State how many modules/files you scanned and the total occurrences
  found. If the user ever has to say "look at more of the repo," you did it wrong.
- If the totals are small or split (e.g. 2 vs 3 across the repo), say there **is no established
  standard**, rather than crowning the larger pile.

## Working style

Self-documenting code over narration. Tell the user when they hold a misconception
instead of silently building on it.

**Response style:** be concise — lead with the answer/result, no preamble, no filler, no recap of
what you're about to do or restating the request. Explain only what's non-obvious, and match the
user's brevity. (This is about chat output; code comments follow the discipline below.)

### Comment discipline (core logic)

Before keeping a comment, ask: **could the code reach the same understanding without it?**
- **Yes →** cut it.
- **It explains how a library/framework works →** move it to docs or memory, not the source.
- **It names a parity/port source** (a legacy procedure the code mirrors, e.g.
  `// mirrors <SourceProc>`) **→** cut it. The function name + parity tests already convey it;
  put the source in the commit message / docs / memory, not the source code.
- **It captures genuinely non-obvious domain intent →** keep it — one line.

Scope: the full bar above is for **core logic**. Comments in tests and one-off scripts aren't policed
against it — but still keep them **concise**: no rambling or redundant narration.
