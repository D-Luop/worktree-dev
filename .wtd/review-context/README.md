# review-context

Optional **per-repo orientation** for the reviewer, one file per slug (`<slug>.md`). It tells the
reviewer what's generated (never suggest editing it), the codegen flow, the source-of-truth, and
where shared helpers/docs live — so reviews judge against *your* repo, not generic assumptions.

These files are local to your machine and git-ignored (this README is the only tracked file here).
Create `<slug>.md` for a repo when you want richer, repo-aware reviews; it's read automatically.
