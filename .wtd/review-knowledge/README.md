# review-knowledge

The reviewer's **per-repo conventions ledger**, one file per slug (`<slug>.md`). The reviewer
memoizes conventions it has counted across the whole repo here (rule + evidence `file:line` + sample
size + dates), then trusts them (with a spot-check) on later runs instead of re-counting — fewer
tokens per review.

These ledgers are written by the engine and are local to your machine / git-ignored (this README is
the only tracked file here). They populate themselves as you run reviews.
