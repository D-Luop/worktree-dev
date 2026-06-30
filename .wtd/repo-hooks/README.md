# repo-hooks

Optional **per-repo hook config**, one file per slug (`<slug>.json`). Lets a repo extend the
review/diff pipeline with repo-specific behavior (e.g. an extra diff view for a particular file type).

These files are local to your machine and git-ignored (this README is the only tracked file here).
Add `<slug>.json` only if a repo needs custom hook behavior; the engine works fine without one.
