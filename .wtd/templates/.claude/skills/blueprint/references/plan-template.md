> **Source of truth across context resets and handoffs.** Context may be wiped, or the work handed
> to another agent, at any moment. A fresh agent recovers all state from this file: read the
> **Handoff** section first, then resume from the first unchecked step (or the bottom of the Progress
> Log). Update Progress Log + Handoff at every step boundary and before any long-running command.

# Plan: <task name>

<!-- HANDOFF then the STEPS CHECKLIST go first (right under the title) so they're the first thing you
     see — context, decisions, and the log live below them. -->

## Handoff
<!-- Live state for whoever picks this up next. OVERWRITE (don't append) at every step boundary and
     before any long-running command (build, test suite, deploy). Exact enough that another agent
     could continue with no transcript. -->
- **Current step:** <step N — name; done / in progress / blocked>
- **Next action:** <the exact next command or edit, e.g. "add `foo()` to src/bar.rs, then run `cargo test bar`">
- **Open questions:** <unresolved questions for the user, or "none">
- **Decisions made (why):** <choices made during the work + reason; settled ones move to "Decisions locked">
- **Gotchas found:** <surprises, traps, things that didn't work>
- **Files in flux / uncommitted:** <files edited but not committed, half-done changes, or "clean">

## Sequential Steps
<!-- Each step = one meaningful, self-contained, independently-reviewable unit of behavior. Err
     toward FEWER, LARGER steps, not micro-steps: bundle tightly-coupled changes into one step (if
     two would always be reviewed/reverted together, merge them). Each step is exactly one commit.
     After completing a step the agent MUST commit + push, append a Progress Log entry, then ask the
     user before continuing. -->
- [ ] **Step 1 — <name>:** <what to do>. _Commit → push → log → ask._
- [ ] **Step 2 — <name>:** <what to do>. _Commit → push → log → ask._
- [ ] **Step 3 — <name>:** <what to do>. _Commit → push → log → ask._

## Fixed tail steps (always run, in order)
- [ ] **A. Tests:** write tests per the repo's own test conventions for this kind of change.
      _Commit → push → log → ask._
- [ ] **B. Green build:** run whichever of the repo's build/codegen/lint/test commands apply and make
      them pass. Fix every failure UNLESS pre-existing to the branch (check
      `git merge-base HEAD <default-branch>`) — note those, don't fix them. _Commit → push → log → ask._
- [ ] **C. Comment hygiene:** remove unnecessary / redundant comments from the code you added or
      touched, per CLAUDE.md "Comment discipline" — cut any comment the code already conveys, that
      explains how a library/framework works, or that names a parity source; keep only genuinely
      non-obvious domain intent (one line). _Commit → push → log → ask._
- [ ] **D. No stray generic helpers:** confirm no generic (non-domain) helper was created locally; any
      such helper must come from / be added to the shared/common package. _Commit → push → log → ask._

## Context
- **Goal:** <one-line outcome>
- **Type:** <kind of change>
- **Affected modules:** <dirs this touches>
- **Key docs:** <repo docs that govern this work>
- **Reference code:** <sibling file to mirror>

## Decisions locked
<!-- One bullet per resolved interview decision. These are settled — do not relitigate. -->
- <decision> — <rationale>

## Progress Log
<!-- Append-only. One entry per completed step. Format:
     - YYYY-MM-DD — Step N (name): what was done. Commit <sha>, pushed. Follow-ups: … -->
