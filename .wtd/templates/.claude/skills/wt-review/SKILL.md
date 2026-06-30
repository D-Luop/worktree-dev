---
name: wt-review
description: >
  Run the separate pre-push reviewer on THIS worktree's changes, then triage its findings. Use when
  the user wants their changes reviewed before pushing, says "review my changes", or invokes /wt-review.
  Runs the external `review` command (independent reviewer + adversarial skeptic), then you — the
  agent who wrote the code — sort every finding into Fix vs Ignore with reasons.
allowed-tools: Read, Grep, Glob, Bash, Edit, Write
---

# Review this worktree, then triage the findings

> **USER-TRIGGERED ONLY.** Run this skill **only when the user explicitly asks for a review in their
> current message** (they typed `/wt-review`, said "review my changes" / "review this", or ran
> `wt-review`). **NEVER start a review on your own** — not as a step in finishing work, not before
> `/done`, not autonomously after committing. If you got here while wrapping up, STOP and just tell
> the user the branch is ready for them to review.

## 1. Run the reviewer — IN THE BACKGROUND
The reviewer runs two passes (Sonnet review + Opus skeptic) and takes **~5–7 minutes — longer than
the 2-minute Bash timeout**, so DON'T run it in the foreground (it'll be killed at 2 min and you'll
have to retry). Run it as a **background** Bash command and let it finish:
```
wt-review            # add --main to review the WHOLE branch vs the default branch
```
- If `wt-review` reports **"nothing to review"** (the branch is fully committed+pushed, so there's no
  diff vs upstream), re-run it with **`--main`** to review the whole branch vs the default branch.
- Run it with `run_in_background: true`. You'll be notified when it completes; don't poll it with
  short-timeout foreground waits.

The reviewer writes its reports INTO this worktree at `.claude/reviews/<YYYY-MM-DD__h.mmAMPM>/`
(git-ignored). When the background command finishes, proceed to step 2.

## 2. Read the report
Open the newest report for this worktree and read ALL of it, including the `## Adversarial pass`:
```
ls -dt "$root"/.claude/reviews/*/ | head -1
```
Read `review.md` in that dir (and `highlights.md` if useful).

## 3. Triage every finding — Fix vs Ignore
You wrote this code and know the intent, so judge each finding honestly using the actual code:
- **Fix** — real issues worth addressing now (correctness/security first, then the rest).
- **Ignore** — false positives, deliberate choices, out-of-scope, or not-worth-it — each with a
  concrete reason (don't dismiss a real bug just to shrink the list, and don't pad Fix with trivia).
Open the cited `file:line`s to verify before deciding; a finding the reviewer got wrong goes to
Ignore with why.

## 4. Write the triage to the TOP of the review file
The review's `review.md` opens with an `## Agent triage — <name>` placeholder (a "_Pending…_" line).
**Replace that placeholder** with your triage — two lists, terse, each finding one line as
`**[SEVERITY]** file:line — rationale`. Edit that same `review.md` (the newest review dir from step 2);
leave everything below the `---` (the reviewer's report) intact:
```
## Agent triage — <name>

### Fix
- **[MAJOR]** pkg/.../x.go:42 — <why it's real> → <the fix>

### Ignore
- **[NIT]** pkg/.../y.go:10 — <why it's safe to ignore: intentional / false positive / out of scope>

---
```
This is the canonical home for the triage — **not chat** — so it's there whenever anyone re-renders
the report via the `view_review_N` button in the commit pane.

## 5. Report briefly, then ask
In chat, give just a one-line summary (e.g. "2 Fix, 5 Ignore — triage written to the top of the
review") and ask whether to apply the Fix list. **Don't change code until they say so.**
