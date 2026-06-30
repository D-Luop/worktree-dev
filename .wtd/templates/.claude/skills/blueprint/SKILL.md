---
name: blueprint
description: >
  Interactive planning workflow. Researches the codebase and docs, interviews the user one question
  at a time, then writes a sequential, commit-sized implementation plan to
  .claude/plans/active-plan.md. Use when the user wants to create a plan, plan a task, scope a
  feature, start a new task, or invokes /blueprint.
allowed-tools: Read, Grep, Glob, Bash, Edit, Write
---

You are running the **blueprint** planning workflow: turn a fuzzy request into a precise,
sequential, commit-sized plan written to `.claude/plans/active-plan.md`. You produce the plan; a
later agent (possibly with a wiped context) executes it. This skill stays active across turns —
the STEP 2 interview is multi-turn.

## Ground rules
- **Explore before you guess.** If code, docs, or live state can answer a question, read them instead
  of asking. Asking the user something the codebase already answers is a bug.
- **Don't reinvent.** Follow the repo's documented standards and test approach for the kind of work
  you're doing.
- **Generic helpers already exist.** Before planning any non-domain helper (string/time/number/map/
  pagination/comparison utils, test scaffolding, etc.), check the repo's shared/common package.

## STEP 1 — RESEARCH
Identify what's being built. Read the repo's own standards/docs that govern this kind of change
BEFORE designing. Then research the actual code: find the target package/module, read a sibling
implementation of the same kind as a template, inspect existing patterns, and (if relevant) inspect
real live/DB state. Record which dirs the work touches.

## STEP 2 — INTERVIEW
Interview me relentlessly about every aspect of this plan until we reach a shared understanding. Walk down each branch of the design tree, resolving dependencies between decisions one-by-one. For each question, provide your recommended answer.
Ask the questions one at a time.
If a question can be answered by exploring the codebase, explore the codebase instead.

## STEP 3 — WRITE THE PLAN
Write `.claude/plans/active-plan.md` using `references/plan-template.md`. Rules:
- Break work into **sequential steps, each a meaningful, self-contained unit of behavior** worth
  reviewing on its own — and **err toward fewer, larger steps, not micro-steps**. Bundle the
  tightly-coupled changes that only make sense together into ONE step. **Rule of thumb: if two steps
  would always be reviewed or reverted together, merge them.** Each step produces exactly one commit,
  so coarser steps mean fewer, more reviewable commits. (Tests and green build are the separate fixed
  tail steps below — don't pre-split those into the body.)
- **Put the steps checklist at the TOP of the file** (right under the title), per the template —
  Context, Decisions, and the Progress Log go below it. The checklist is the first thing anyone sees.
- After each step the executing agent must: **commit + push** (user reviews in GitHub), **log
  progress in the plan's Progress Log**, then **ask the user before continuing**. State this per step.
- The plan must instruct the agent to **actively log completed work in the Progress Log of this same
  file** — context may be wiped between steps; a fresh agent recovers state only from this file.
- The Steps checklist must END with these fixed tail steps, in order (reviews are NOT a step — the
  user runs `wt-review` separately, never the agent):
  - **A. Tests:** write tests per the repo's own test conventions for this kind of change.
  - **B. Green build:** run whichever of the repo's build/codegen/lint/test commands apply and make
    them pass. Fix every failure UNLESS it is pre-existing to the branch — verify against the
    merge-base (`git merge-base HEAD <default-branch>`); note pre-existing failures, don't fix them.
  - **C. Comment hygiene:** remove unnecessary / redundant comments from the code you added or touched,
    per CLAUDE.md "Comment discipline" — cut any comment the code already conveys, that explains how a
    library/framework works, or that names a parity source; keep only genuinely non-obvious domain
    intent (one line).
  - **D. No stray generics:** confirm no generic (non-domain) helper was created locally; if one was
    needed it must come from / be added to the shared/common package — never duplicated in a domain
    module.

After writing the file, tell the user the plan is ready at `.claude/plans/active-plan.md` and ask
them to review before execution begins.
