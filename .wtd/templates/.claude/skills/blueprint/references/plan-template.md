> **Source of truth across context resets.** Context may be wiped between steps. A fresh agent
> recovers all state from this file: read it top-to-bottom, resume from the first unchecked step
> (or the bottom of the Progress Log), and append a Progress Log entry after every step.

# Plan: <task name>

<!-- The STEPS CHECKLIST goes first (right under the title) so it's the first thing you see — context,
     decisions, and the log live below it. -->

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
