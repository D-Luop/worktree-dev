// The Changes panel — a webview docked in the right half of the editor area (the session's Claude chat
// is the left half). It follows the focused session and shows that worktree's branch commits plus
// uncommitted changes, with a live, VSCode-styled diff (inline or side-by-side, word-level highlights).
//
// Layout contract (extension.js ensureLayout): the left column is the session's Claude chat (top 80%)
// over a plain shell in the worktree (bottom 20%); this panel is the right column, at an even 50/50.
// Closing the panel only hides it until the next session is focused — it's meant to be always there
// (setting: claudeStatus.changesPanel). Live: one recursive fs.watch on the shown worktree (debounced) plus the
// daemon's git-state pushes for commits.
//
// Its bottom pane also hosts the worktree's staged previews (`preview <file> [label]` →
// .wtd/state/previews/<slug>/<name>/<label>.html) as tabs beside the diff, each in a sandboxed iframe.

const vscode = require('vscode');
const cp = require('child_process');
const fs = require('fs');
const path = require('path');
const gp = require('./gitparse.js');
const { baseRef, revUri } = require('./gitview.js');

const WORKING = '~working';
const MAX_COMMITS = 200;
const FALLBACK_COMMITS = 30;
const PREVIEW_POLL_MS = 1000;   // VSCode's file watcher misses the .wtd/state copy on Windows
const IGNORE = /(^|[\\/])(\.git|node_modules|target|dist|build|out|\.next|\.turbo|\.cache|__pycache__|\.venv)([\\/]|$)|\.claude-status|\.claude-ticket\.md/i;

function git(wt, args) {
  return new Promise((resolve) => {
    cp.execFile('git', ['-C', wt].concat(args), { maxBuffer: 64 * 1024 * 1024, timeout: 20000, windowsHide: true, encoding: 'utf8' },
      (e, so) => resolve(e ? null : so));
  });
}

// A staged preview as an iframe srcdoc. srcdoc inherits the panel's nonce-only script-src, so the preview's
// inline scripts get the nonce (external ones don't: no network). The bridge relays ▶ Start clicks and the
// scroll offset to the panel — the only channel out of the sandbox.
const BRIDGE = '(function(){var P=parent;'
  + 'document.addEventListener("click",function(e){var g=e.target.closest&&e.target.closest(".go");if(!g)return;e.preventDefault();e.stopPropagation();'
  + 'var it=g.closest(".item");if(!it)return;var i=it.querySelector(".id"),t=it.querySelector(".ttl");'
  + 'P.postMessage({__wtpv:"planAction",id:((i&&i.textContent)||"").trim(),title:((t&&t.textContent)||"").trim()},"*");'
  + 'g.classList.add("go-fired");setTimeout(function(){g.classList.remove("go-fired");},1200);});'
  + 'var q;addEventListener("scroll",function(){clearTimeout(q);q=setTimeout(function(){P.postMessage({__wtpv:"scroll",y:scrollY},"*");},120);},{passive:true});'
  + 'addEventListener("message",function(e){var d=e.data;if(e.source===P&&d&&d.__wtpv==="scroll")scrollTo(0,d.y);});})();';
function previewDoc(raw, nonce) {
  const html = String(raw).replace(/<script\b(?![^>]*\b(?:nonce|src)\s*=)/gi, '<script nonce=' + nonce);
  const bridge = '<script nonce=' + nonce + '>' + BRIDGE + '</script>';
  return /<\/body>/i.test(html) ? html.replace(/<\/body>(?![\s\S]*<\/body>)/i, bridge + '</body>') : html + bridge;
}

class ChangesPanel {
  constructor(context, dev, testsFlag) {
    this.context = context; this.dev = dev; this.testsFlag = testsFlag;
    this.panel = null; this.target = null; this.sel = null;
    this._watch = null; this._deb = null; this._gen = 0;
    this._pvLabels = '';   // '|'-joined preview labels last posted
    this._pvLabel = null;  // the preview tab the webview shows (kept fresh by the poll)
    this._pvSent = '';     // label + mtime of the preview html last posted
    context.subscriptions.push({ dispose: () => { this._unwatch(); this._stopPoll(); if (this.panel) this.panel.dispose(); } });
    context.subscriptions.push(vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('claudeStatus.changesPanel') && !this.enabled() && this.panel) this.panel.dispose();
    }));
  }
  enabled() { return vscode.workspace.getConfiguration('claudeStatus').get('changesPanel', true); }
  // the right-hand column (2, or 3 when the bottom-left shell group exists)
  _col() { return (this.dev._cols && this.dev._cols().panel) || vscode.ViewColumn.Two; }

  // ---- docking ----
  async _evenLayout() {
    try {
      await vscode.commands.executeCommand('vscode.setEditorLayout', { orientation: 0, groups: [{ size: 0.5 }, { size: 0.5 }] });
    } catch {}
  }
  // make sure the panel exists, docked in the right column, without taking focus
  // → true when the panel was (re)created
  async ensure() {
    if (!this.enabled()) return false;
    if (this.dev.ensureLayout) await this.dev.ensureLayout(); else if (vscode.window.tabGroups.all.length !== 2) await this._evenLayout();
    if (this.panel) {
      if (!this.panel.visible || this.panel.viewColumn !== this._col()) this.panel.reveal(this._col(), true);
      return false;
    }
    const media = vscode.Uri.joinPath(this.context.extensionUri, 'media');
    const p = vscode.window.createWebviewPanel('claudeStatus.changesPanel', 'Changes',
      { viewColumn: this._col(), preserveFocus: true },
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [media] });
    p.iconPath = new vscode.ThemeIcon('git-compare');
    const nonce = Math.random().toString(36).slice(2) + Date.now().toString(36);
    this._nonce = nonce;
    let html = fs.readFileSync(path.join(this.context.extensionUri.fsPath, 'changes.html'), 'utf8');
    html = html.split('__NONCE__').join(nonce).split('__CSP__').join(p.webview.cspSource)
      .replace('__CODICON_HREF__', p.webview.asWebviewUri(vscode.Uri.joinPath(media, 'codicon.css')).toString());
    p.webview.html = html;
    p.webview.onDidReceiveMessage((m) => this._onMsg(m).catch((e) => vscode.window.showErrorMessage('Changes: ' + e.message)));
    p.onDidDispose(() => { this.panel = null; this._unwatch(); this._stopPoll(); });
    p.onDidChangeViewState(() => { if (p.visible && this._stale) this.refresh(); });
    this.panel = p;
    this._pvTimer = setInterval(() => this._pollPreviews(), PREVIEW_POLL_MS);
    return true;
  }

  // ---- targeting ----
  async follow(slug, name) {
    if (!this.enabled()) return;
    const same = this.target && this.target.slug === slug && this.target.name === name;
    const created = await this.ensure();
    if (same && !created) return;
    const wt = this.dev._wtPath(slug, name);
    if (!fs.existsSync(path.join(wt, '.git'))) return;
    this.target = { slug, name, wt }; this.sel = null; this._shown = null;
    this._pvLabels = ''; this._pvLabel = null; this._pvSent = '';
    this._watchTarget();
    this.refresh(true);
  }
  // a worktree vanished (archived / removed) — drop it if it's the one shown
  forget(slug, name) {
    if (!this.target || this.target.slug !== slug || this.target.name !== name) return;
    this.target = null; this._unwatch();
    this._post({ type: 'clear' });
  }
  // the daemon pushed an update for a worktree; reload if it's ours and its git state moved
  onWorktree(w, prev) {
    if (!this.target || !w || w.slug !== this.target.slug || w.name !== this.target.name) return;
    if (prev && JSON.stringify(prev.git) === JSON.stringify(w.git)) return;
    this.refresh();
  }
  // bring the panel forward on one of a worktree's staged previews (the roster 🖼, ⋯ → Open design preview)
  async showPreview(slug, name, label) {
    if (!this.enabled()) {
      vscode.window.showInformationMessage('Previews open in the Changes panel, which is turned off (setting: claudeStatus.changesPanel).');
      return;
    }
    const labels = this.dev._previewLabels(this.dev._scanPreviews(slug, name));
    if (!labels.length) { vscode.window.showInformationMessage('No staged previews for ' + slug + '/' + name + '.'); return; }
    await this.follow(slug, name);
    if (!this.panel) return;
    this.panel.reveal(this._col(), true);
    this._post({ type: 'tab', target: { slug, name }, tab: 'pv:' + (labels.includes(label) ? label : labels[0]) });
  }
  // select a specific commit (a SHA clicked in a session terminal)
  async showCommit(slug, name, sha) {
    const wt = this.dev._wtPath(slug, name);
    const full = ((await git(wt, ['rev-parse', '--verify', '--quiet', sha + '^{commit}'])) || '').trim();
    if (!full) { vscode.window.showInformationMessage(sha + ' is not a commit in ' + name + '.'); return; }
    await this.follow(slug, name);
    this.sel = full;
    this.refresh(true);
  }

  _watchTarget() {
    this._unwatch();
    const t = this.target; if (!t) return;
    try {
      const w = fs.watch(t.wt, { recursive: true }, (ev, file) => {
        if (file && IGNORE.test(String(file))) return;
        this.refresh();
      });
      w.on('error', () => { try { w.close(); } catch {} if (this._watch === w) this._watch = null; });
      this._watch = w;
    } catch {}
  }
  _unwatch() { if (this._watch) { try { this._watch.close(); } catch {} this._watch = null; } }

  // ---- previews ----
  _previews() { const t = this.target; return t ? this.dev._scanPreviews(t.slug, t.name) : {}; }
  _postPreviews(force) {
    const t = this.target; if (!t) return;
    const labels = this.dev._previewLabels(this._previews());
    if (!force && labels.join('|') === this._pvLabels) return;
    this._pvLabels = labels.join('|');
    this._post({ type: 'previews', target: { slug: t.slug, name: t.name }, labels });
  }
  _sendPreview(label, force) {
    const t = this.target; if (!t || !label) return;
    const fp = this._previews()[label]; if (!fp) return;
    let sig = '', raw;
    try { sig = label + ':' + fs.statSync(fp).mtimeMs; raw = fs.readFileSync(fp, 'utf8'); } catch { return; }
    if (!force && sig === this._pvSent) return;
    this._pvSent = sig;
    this._post({ type: 'preview', target: { slug: t.slug, name: t.name }, label, sig, html: previewDoc(raw, this._nonce) });
  }
  // live-refresh: a new/removed tab, or the shown preview re-staged
  _pollPreviews() {
    if (!this.panel || !this.panel.visible || !this.target) return;
    this._postPreviews();
    if (this._pvLabel) this._sendPreview(this._pvLabel);
  }
  _stopPoll() { if (this._pvTimer) { clearInterval(this._pvTimer); this._pvTimer = null; } }

  // ---- loading ----
  refresh(now) {
    if (!this.panel || !this.target) return;
    if (!this.panel.visible) { this._stale = true; return; }
    this._stale = false;
    clearTimeout(this._deb);
    this._deb = setTimeout(() => this._loadList().catch(() => {}), now ? 0 : 400);
  }
  _post(m) { if (this.panel) this.panel.webview.postMessage(m); }
  _hideTests() { try { return fs.existsSync(this.testsFlag); } catch { return false; } }
  testsToggled() { this._post({ type: 'tests', hide: this._hideTests() }); }

  async _loadList() {
    const t = this.target; if (!t) return;
    if (!fs.existsSync(t.wt)) { this.forget(t.slug, t.name); return; }   // archived / removed
    const gen = ++this._gen;
    const branch = ((await git(t.wt, ['rev-parse', '--abbrev-ref', 'HEAD'])) || 'HEAD').trim();
    const base = await baseRef(t.wt, branch);
    const fmt = '%H%x1f%h%x1f%an%x1f%ar%x1f%ad%x1f%s%x1e';
    const log = async (extra) => ((await git(t.wt, ['log', '--no-color', '--date=format:%Y-%m-%d %H:%M', '--format=' + fmt].concat(extra))) || '')
      .split('\x1e').map((s) => s.replace(/^\r?\n/, '')).filter((s) => s.trim()).map((s) => {
        const p = s.split('\x1f');
        return { sha: p[0], short: p[1], author: p[2], when: p[3], date: p[4], subject: p[5] || '' };
      });
    const [counts, status] = await Promise.all([
      git(t.wt, ['rev-list', '--left-right', '--count', '@{upstream}...HEAD']),
      git(t.wt, ['status', '--porcelain=v1', '-z', '--untracked-files=all']),
    ]);
    let commits = base ? await log(['--max-count=' + MAX_COMMITS, base + '..HEAD']) : [];
    const fallback = !commits.length;
    if (fallback) commits = await log(['--max-count=' + FALLBACK_COMMITS, 'HEAD']);
    if (gen !== this._gen || t !== this.target) return;
    let upstream = null;
    if (counts) { const [behind, ahead] = counts.trim().split(/\s+/).map(Number); upstream = { ahead: ahead || 0, behind: behind || 0 }; }
    // porcelain -z: a rename's second path is its own NUL field, so count entries with an XY prefix
    const changes = (status || '').split('\0').filter((e) => /^[ MADRCU?!]{2} /.test(e) && !e.startsWith('!!')).length;
    this._status = status || '';
    const sel = this.sel; this.sel = null;   // a pending explicit selection (terminal SHA link)
    this._post({ type: 'tests', hide: this._hideTests() });
    this._post({ type: 'list', target: { slug: t.slug, name: t.name }, branch, base, upstream, fallback, commits, changes, sel });
    this._postPreviews(true);
    // live working-tree edits: re-send the shown diff even though the list didn't change
    if (this._shown === WORKING) this._loadDiff(WORKING);
  }

  async _loadDiff(sel) {
    const t = this.target; if (!t) return;
    this._shown = sel;
    if (sel === WORKING) {
      const out = await git(t.wt, ['diff', '--no-color', '--find-renames', 'HEAD']);
      if (t !== this.target) return;
      const { files, omitted } = gp.parsePatch(out || '');
      const status = this._status != null ? this._status : (await git(t.wt, ['status', '--porcelain=v1', '-z', '--untracked-files=all'])) || '';
      const all = files.concat(gp.untrackedFromPorcelain(t.wt, status)).sort((a, b) => a.path.localeCompare(b.path));
      this._post({ type: 'diff', sel, files: all, omitted });
      return;
    }
    const [meta, out] = await Promise.all([
      git(t.wt, ['show', '--no-patch', '--date=format:%Y-%m-%d %H:%M', '--format=%H%x1f%h%x1f%an%x1f%ad%x1f%ar%x1f%s%x1f%b', sel]),
      git(t.wt, ['diff-tree', '-p', '--no-color', '--no-commit-id', '--find-renames', '--root', '-m', '--first-parent', sel]),
    ]);
    if (t !== this.target) return;
    const p = (meta || '').replace(/\s+$/, '').split('\x1f');
    const { files, omitted } = gp.parsePatch(out || '');
    this._post({ type: 'diff', sel, files, omitted,
      meta: { sha: p[0], short: p[1], author: p[2], date: p[3], when: p[4], subject: p[5] || '', body: (p[6] || '').trim() } });
  }

  // ---- webview → host ----
  async _onMsg(m) {
    if (!m) return;
    const t = this.target;
    switch (m.cmd) {
      case 'ready': this.testsToggled(); if (t) { if (!this._watch) this._watchTarget(); this.refresh(true); } break;
      case 'refresh': this.refresh(true); break;
      case 'select': if (t) await this._loadDiff(m.sel); break;
      case 'preview': this._pvLabel = m.label || null; this._sendPreview(this._pvLabel, m.sig == null || m.sig !== this._pvSent); break;
      case 'planAction': if (t && m.title) this.dev.startPlanItem(t.slug, t.name, m.id || '', m.title); break;
      case 'pick': await this.pick(); break;
      case 'copy': await vscode.env.clipboard.writeText(m.text || ''); vscode.window.setStatusBarMessage('$(copy) Copied ' + String(m.text).slice(0, 12), 2000); break;
      case 'toggleTests': {
        try {
          if (this._hideTests()) fs.unlinkSync(this.testsFlag);
          else { fs.mkdirSync(path.dirname(this.testsFlag), { recursive: true }); fs.writeFileSync(this.testsFlag, ''); }
        } catch {}
        this.testsToggled();
        if (this.dev._postTests) this.dev._postTests();
        if (this.dev.gitView) this.dev.gitView.testsToggled();
        break;
      }
      case 'openFile': {
        if (!t || !m.file) return;
        const fp = path.join(t.wt, m.file.path);
        if (fs.existsSync(fp)) await vscode.window.showTextDocument(vscode.Uri.file(fp), { viewColumn: this._col(), preview: true });
        break;
      }
      case 'openNative': {
        if (!t || !m.file) return;
        const s = this._sides(m.sel, m.file);
        await vscode.commands.executeCommand('vscode.diff', s.left, s.right, s.title, { viewColumn: this._col(), preview: true });
        break;
      }
      case 'openAll': {
        if (!t || !this.dev.gitView) return;
        // reuse the tree's multi-diff helper: it needs a node shaped like its own
        const node = m.sel === WORKING ? null : { kind: 'commit', sha: m.sel, short: m.sel.slice(0, 7), subject: '' };
        if (node) { this.dev.gitView.follow(t.slug, t.name); await this.dev.gitView.openAll(node); }
        else { this.dev.gitView.follow(t.slug, t.name); await this.dev.gitView._load(); const roots = await this.dev.gitView.getChildren(); const ch = roots.find((r) => r.kind === 'changes'); if (ch) await this.dev.gitView.openAll(ch); }
        break;
      }
    }
  }
  _sides(sel, f) {
    const wt = this.target.wt, oldPath = f.from || f.path, base = path.posix.basename(f.path);
    if (sel === WORKING) {
      const left = f.untracked || f.status === 'added' ? revUri(wt, oldPath, '') : revUri(wt, oldPath, 'HEAD');
      const right = f.status === 'deleted' ? revUri(wt, f.path, '') : vscode.Uri.file(path.join(wt, f.path));
      return { left, right, title: base + ' (Working Tree)' };
    }
    const left = f.status === 'added' ? revUri(wt, oldPath, '') : revUri(wt, oldPath, sel + '^');
    const right = f.status === 'deleted' ? revUri(wt, f.path, '') : revUri(wt, f.path, sel);
    return { left, right, title: base + ' (' + sel.slice(0, 7) + ')' };
  }
  async pick() {
    const wts = [...((this.dev.daemon && this.dev.daemon.wts) || new Map()).values()]
      .filter((w) => w.id !== '_dev' && w.slug && w.name).sort((a, b) => a.id.localeCompare(b.id));
    const it = await vscode.window.showQuickPick(wts.map((w) => ({ label: w.name, description: w.slug, w })),
      { placeHolder: 'Show changes for which worktree?', matchOnDescription: true });
    if (it) this.follow(it.w.slug, it.w.name);
  }
}

module.exports = { ChangesPanel, WORKING, previewDoc };
