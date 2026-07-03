---
name: preview
description: >
  Show the user an HTML design mockup in the fleet roster's preview panel. Use when you've produced a
  visual design / page / component mockup and want the user to SEE it (not just read a file path), or
  when the user invokes /preview. Stages a self-contained HTML file so a 🖼 glyph appears on this
  worktree's roster row; the user clicks it to open the mockup in an editor-side panel.
allowed-tools: Bash, Write
---

# Publish a design preview

Use this whenever you build something visual the user should look at — a page layout, component, theme,
diagram, or before/after mockup. Don't hand the user a temp file path and a `start` command; stage it
so it opens with one click from the roster.

1. **Write a single self-contained `.html` file.** It renders in a sandboxed webview with **no network
   access**, so inline everything:
   - CSS in a `<style>` block (no external stylesheets / CDN links).
   - Images as `data:` URIs (no `<img src="http…">`).
   - Any JS inline in a `<script>` block (keep it minimal — these are mockups).
   Put it anywhere convenient (e.g. the scratchpad or the worktree root).

2. **Stage it** from inside the worktree, with a short **label** (the switcher-button name):
   ```
   preview <file.html> <label>
   ```
   e.g. `preview dashboard.html dashboard`. A worktree can hold **many** previews at once — each label
   is its own tab along the top of the preview panel, so staging a new label **adds a tab** rather than
   replacing the plan or another mockup. (`label` defaults to the filename if omitted; the living plan
   uses the reserved label `plan`.) This stages to `.wtd/state/previews/<slug>/<name>/<label>.html`.

3. **Tell the user** it's ready: "Staged a preview — open the panel (🖼) and pick the '<label>' tab."
   Re-run `preview` with the same file+label to refresh; the open panel updates live (no reopen). Use a
   **new label** for an additional preview, the **same label** to update an existing one.
