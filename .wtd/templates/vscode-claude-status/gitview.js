// Worktree Changes — a native Explorer tree of one worktree's uncommitted changes and branch commits,
// opening files in VSCode's own diff editor (the same one Source Control uses).
//
//   Uncommitted Changes          ← tracked edits vs HEAD + untracked files (right side is the live,
//     M  extension.js  src/          editable file, like SCM's working-tree diff)
//   feat: add widget  1a2b3c4 · 2 hours ago
//     A  widget.ts  src/          ← parent ↔ commit, both read-only (wtd-git: content provider)
//
// The tree follows the focused session (and a roster row's diff button). It's refreshed when the
// daemon pushes a git-state change for the shown worktree, so it stays live without polling.

const vscode = require('vscode');
const cp = require('child_process');
const fs = require('fs');
const path = require('path');

const SCHEME = 'wtd-git';          // read-only file content at a git revision
const DECO = 'wtd-change';         // tree-item resource uris: carry the status letter for decorations
const MAX_COMMITS = 200;
const FALLBACK_COMMITS = 30;       // a branch with nothing over its base shows recent history instead
const MAX_BLOB = 8 * 1024 * 1024;

// Test-file globs mirror test_excludes() in .wtd/hooks/generated-filter.sh.
const TEST_RE = /(^|\/)(testdata\/|.*_test\.(go|sql|ts)$|.*\.(test|spec)\.(ts|tsx|js|jsx)$)/i;

const STATUS = {   // letter → [label, decoration colour] (the colours SCM itself uses)
  M: ['Modified', 'gitDecoration.modifiedResourceForeground'],
  A: ['Added', 'gitDecoration.addedResourceForeground'],
  D: ['Deleted', 'gitDecoration.deletedResourceForeground'],
  R: ['Renamed', 'gitDecoration.renamedResourceForeground'],
  C: ['Copied', 'gitDecoration.addedResourceForeground'],
  U: ['Untracked', 'gitDecoration.untrackedResourceForeground'],
  T: ['Type changed', 'gitDecoration.modifiedResourceForeground'],
  '!': ['Conflict', 'gitDecoration.conflictingResourceForeground'],
};

function git(wt, args, opts) {
  return new Promise((resolve) => {
    cp.execFile('git', ['-C', wt].concat(args),
      Object.assign({ maxBuffer: 64 * 1024 * 1024, timeout: 20000, windowsHide: true, encoding: 'buffer' }, opts || {}),
      (e, so) => resolve(e ? null : so));
  });
}
async function gitText(wt, args) { const b = await git(wt, args); return b == null ? null : b.toString('utf8'); }

// the ref this branch forked from: origin/HEAD, else the usual trunk names (local-only repos)
async function baseRef(wt, branch) {
  const sym = ((await gitText(wt, ['symbolic-ref', '-q', 'refs/remotes/origin/HEAD'])) || '').trim();
  if (sym) return sym.replace(/^refs\/remotes\//, '');
  for (const c of ['origin/main', 'origin/master', 'main', 'master']) {
    if (c === branch) continue;   // sitting on trunk: there's no base to compare against
    if (((await gitText(wt, ['rev-parse', '--verify', '--quiet', c + '^{commit}'])) || '').trim()) return c;
  }
  return '';
}

// -z name-status: "M\0path\0", "R100\0old\0new\0"
function parseNameStatus(text) {
  const out = []; const p = (text || '').split('\0');
  for (let i = 0; i < p.length;) {
    const st = p[i++]; if (!st) continue;
    const letter = st[0];
    if (letter === 'R' || letter === 'C') { out.push({ status: letter, from: p[i], path: p[i + 1] }); i += 2; }
    else { out.push({ status: letter, path: p[i] }); i += 1; }
  }
  return out;
}

// -z porcelain v1: "XY path\0" (renames: "R  new\0old\0")
function parsePorcelain(text) {
  const out = []; const p = (text || '').split('\0');
  for (let i = 0; i < p.length; i++) {
    const e = p[i]; if (e.length < 4) continue;
    const x = e[0], y = e[1], file = e.slice(3);
    if (x === '?') { out.push({ status: 'U', path: file, untracked: true }); continue; }
    if (x === '!') continue;
    if (x === 'U' || y === 'U' || (x === 'A' && y === 'A') || (x === 'D' && y === 'D')) { out.push({ status: '!', path: file }); continue; }
    if (x === 'R' || x === 'C') { out.push({ status: x, path: file, from: p[++i] }); continue; }
    const letter = y === 'D' || x === 'D' ? 'D' : x === 'A' ? 'A' : y === 'T' || x === 'T' ? 'T' : 'M';
    out.push({ status: letter, path: file });
  }
  return out;
}

// read-only content of <path> at <ref> in <wt>. Missing (added/deleted side, root commit) → empty.
class GitContentProvider {
  async provideTextDocumentContent(uri) {
    let q; try { q = JSON.parse(uri.query); } catch { return ''; }
    if (!q.ref) return '';
    const size = parseInt(((await gitText(q.wt, ['cat-file', '-s', q.ref + ':' + q.path])) || '0').trim(), 10) || 0;
    if (size > MAX_BLOB) return `(${(size / 1048576).toFixed(1)} MB — too large to show)`;
    const b = await git(q.wt, ['show', q.ref + ':' + q.path]);
    if (b == null) return '';
    if (b.subarray(0, 8000).includes(0)) return '(binary file)';
    return b.toString('utf8');
  }
}
function revUri(wt, rel, ref) {
  return vscode.Uri.from({ scheme: SCHEME, path: '/' + rel, query: JSON.stringify({ wt, path: rel, ref: ref || '' }) });
}

class Node {
  constructor(kind, props) { this.kind = kind; Object.assign(this, props); }
}

class GitView {
  // dev: the roster provider (focus tracking, worktree paths); testsFlag: path of the exclude-tests flag
  constructor(context, dev, testsFlag) {
    this.dev = dev; this.testsFlag = testsFlag;
    this.target = null;           // { slug, name, wt }
    this.state = null;            // last load: { branch, base, fallback, changes, commits, hidden }
    this._files = new Map();      // commit sha → [file]
    this._emitter = new vscode.EventEmitter();
    this.onDidChangeTreeData = this._emitter.event;
    this._decoEmitter = new vscode.EventEmitter();

    const subs = context.subscriptions;
    subs.push(vscode.workspace.registerTextDocumentContentProvider(SCHEME, new GitContentProvider()));
    subs.push(vscode.window.registerFileDecorationProvider({
      onDidChangeFileDecorations: this._decoEmitter.event,
      provideFileDecoration: (uri) => {
        if (uri.scheme !== DECO) return undefined;
        const s = STATUS[uri.query]; if (!s) return undefined;
        return new vscode.FileDecoration(uri.query === '!' ? '!' : uri.query, s[0], new vscode.ThemeColor(s[1]));
      },
    }));
    this.view = vscode.window.createTreeView('claudeStatus.changes', { treeDataProvider: this, showCollapseAll: true });
    subs.push(this.view);
    this.view.onDidChangeVisibility(() => { if (this.view.visible && this._stale) this.refresh(); });

    const reg = (id, fn) => subs.push(vscode.commands.registerCommand(id, fn));
    reg('claudeStatus.changes.refresh', () => this.refresh());
    reg('claudeStatus.changes.pick', () => this.pick());
    reg('claudeStatus.changes.hideTests', () => this._setTests(true));
    reg('claudeStatus.changes.showTests', () => this._setTests(false));
    reg('claudeStatus.changes.openDiff', (n) => this.openDiff(n));
    reg('claudeStatus.changes.openFile', (n) => this.openFile(n));
    reg('claudeStatus.changes.openAll', (n) => this.openAll(n));
    reg('claudeStatus.changes.copySha', (n) => n && n.sha && vscode.env.clipboard.writeText(n.sha));
    this._syncTestsContext();

    // clickable SHAs in a session's terminal → that commit's changes in the multi-diff editor
    subs.push(vscode.window.registerTerminalLinkProvider({
      provideTerminalLinks: (ctx) => {
        if (!this._wtForTerminal(ctx.terminal)) return [];
        const links = []; const re = /\b[0-9a-f]{7,40}\b/g; let m;
        while ((m = re.exec(ctx.line)) !== null) {
          if (!/[a-f]/.test(m[0])) continue;   // all digits: a number, not a SHA
          links.push({ startIndex: m.index, length: m[0].length, tooltip: 'Show commit changes', sha: m[0], terminal: ctx.terminal });
        }
        return links;
      },
      handleTerminalLink: (link) => this.showCommit(this._wtForTerminal(link.terminal), link.sha),
    }));
  }

  // ---- targeting ----
  _wtForTerminal(t) {
    const key = this.dev._terminalKey(t);
    if (!key) return null;
    const i = key.indexOf('\x01');
    return { slug: key.slice(0, i), name: key.slice(i + 1), wt: this.dev._wtPath(key.slice(0, i), key.slice(i + 1)) };
  }
  // follow the focused session (cheap: a hidden view just marks itself stale)
  follow(slug, name) {
    if (this.target && this.target.slug === slug && this.target.name === name) return;
    const wt = this.dev._wtPath(slug, name);
    if (!fs.existsSync(path.join(wt, '.git'))) return;
    this.target = { slug, name, wt };
    this.state = null; this._files.clear();
    this.refresh();
  }
  // a roster row's diff button: target it and bring the view up
  async show(slug, name) {
    this.follow(slug, name);
    try { await vscode.commands.executeCommand('claudeStatus.changes.focus'); } catch {}
  }
  async pick() {
    const wts = [...((this.dev.daemon && this.dev.daemon.wts) || new Map()).values()]
      .filter((w) => w.kind !== 'dev' && w.id !== '_dev' && w.slug && w.name)
      .sort((a, b) => a.id.localeCompare(b.id));
    const it = await vscode.window.showQuickPick(wts.map((w) => ({
      label: w.name, description: w.slug, detail: w.git && w.git.branch ? '$(git-branch) ' + w.git.branch : undefined, w,
    })), { placeHolder: 'Show changes for which worktree?', matchOnDescription: true });
    if (it) this.show(it.w.slug, it.w.name);
  }
  // the daemon pushed an update for a worktree; reload if it's the one shown and its git state moved
  onWorktree(w, prev) {
    if (!this.target || !w || w.slug !== this.target.slug || w.name !== this.target.name) return;
    if (prev && JSON.stringify(prev.git) === JSON.stringify(w.git) && prev.status === w.status) return;
    this.refresh();
  }

  // ---- loading ----
  refresh() {
    if (!this.view.visible) { this._stale = true; return; }
    this._stale = false;
    clearTimeout(this._deb);
    this._deb = setTimeout(() => this._load().catch(() => {}), 200);
  }
  _hideTests() { try { return fs.existsSync(this.testsFlag); } catch { return false; } }
  _filter(files) { return this._hideTests() ? files.filter((f) => !TEST_RE.test(f.path)) : files; }
  async _load() {
    const t = this.target;
    if (!t) { this.view.message = 'Focus a session, or pick a worktree (↻ menu), to see its changes.'; this.view.description = ''; this._emitter.fire(); return; }
    const gen = (this._gen = (this._gen || 0) + 1);
    const branch = ((await gitText(t.wt, ['rev-parse', '--abbrev-ref', 'HEAD'])) || 'HEAD').trim();
    const base = await baseRef(t.wt, branch);
    const fmt = '%H%x1f%h%x1f%an%x1f%ar%x1f%ad%x1f%s%x1e';
    const log = async (extra) => ((await gitText(t.wt, ['log', '--no-color', '--date=format:%Y-%m-%d %H:%M', '--format=' + fmt].concat(extra))) || '')
      .split('\x1e').map((s) => s.replace(/^\r?\n/, '')).filter((s) => s.trim()).map((s) => {
        const p = s.split('\x1f');
        return { sha: p[0], short: p[1], author: p[2], when: p[3], date: p[4], subject: p[5] || '' };
      });
    let commits = base ? await log(['--max-count=' + MAX_COMMITS, base + '..HEAD']) : [];
    const fallback = !commits.length;
    if (fallback) commits = await log(['--max-count=' + FALLBACK_COMMITS, 'HEAD']);
    const changes = parsePorcelain(await gitText(t.wt, ['status', '--porcelain=v1', '-z', '--untracked-files=all']));
    if (gen !== this._gen) return;   // a newer load (or target switch) superseded this one
    this._files.clear();
    this.state = { branch, base, fallback, changes, commits };
    this.view.message = undefined;
    this.view.title = 'Changes';
    this.view.description = t.slug + '/' + t.name;
    this._emitter.fire();
    this._decoEmitter.fire(undefined);
  }

  // ---- TreeDataProvider ----
  async getChildren(n) {
    const t = this.target, st = this.state;
    if (!t) return [];
    if (!n) {
      if (!st) return [];
      const out = [];
      const changes = this._filter(st.changes);
      if (changes.length) out.push(new Node('changes', { files: changes }));
      if (st.fallback && st.base) out.push(new Node('note', { text: 'No commits ahead of ' + st.base + ' — recent history' }));
      else if (st.fallback) out.push(new Node('note', { text: 'Recent history' }));
      for (const c of st.commits) out.push(new Node('commit', c));
      if (!out.length) out.push(new Node('note', { text: 'No changes' }));
      return out;
    }
    if (n.kind === 'changes') return n.files.map((f) => new Node('file', Object.assign({ ref: null }, f)));
    if (n.kind === 'commit') {
      let files = this._files.get(n.sha);
      if (!files) {
        files = parseNameStatus(await gitText(t.wt, ['diff-tree', '-r', '-z', '--no-commit-id', '--name-status',
          '--find-renames', '--root', '-m', '--first-parent', n.sha]));
        this._files.set(n.sha, files);
      }
      const shown = this._filter(files);
      const kids = shown.map((f) => new Node('file', Object.assign({ ref: n.sha, short: n.short }, f)));
      if (shown.length < files.length) kids.push(new Node('note', { text: (files.length - shown.length) + ' test file(s) hidden' }));
      return kids;
    }
    return [];
  }

  getTreeItem(n) {
    const C = vscode.TreeItemCollapsibleState;
    if (n.kind === 'changes') {
      const it = new vscode.TreeItem('Uncommitted Changes', C.Expanded);
      it.description = String(n.files.length);
      it.iconPath = new vscode.ThemeIcon('diff');
      it.contextValue = 'changes';
      return it;
    }
    if (n.kind === 'note') {
      const it = new vscode.TreeItem(n.text, C.None);
      it.iconPath = new vscode.ThemeIcon('info');
      return it;
    }
    if (n.kind === 'commit') {
      const it = new vscode.TreeItem(n.subject, C.Collapsed);
      it.id = 'c:' + n.sha;
      it.description = n.short + ' · ' + n.when;
      it.iconPath = new vscode.ThemeIcon('git-commit');
      it.contextValue = 'commit';
      const md = new vscode.MarkdownString();
      md.appendMarkdown('**' + n.subject.replace(/[\\`*_{}[\]()#+\-.!|<>]/g, '\\$&') + '**\n\n');
      md.appendMarkdown('$(git-commit) `' + n.short + '` · ' + n.author + ' · ' + n.date + ' (' + n.when + ')');
      md.supportThemeIcons = true;
      it.tooltip = md;
      return it;
    }
    // file
    const dir = path.posix.dirname(n.path);
    const it = new vscode.TreeItem(path.posix.basename(n.path), C.None);
    it.id = (n.ref || 'wt') + ':' + n.path;
    it.resourceUri = vscode.Uri.from({ scheme: DECO, path: '/' + n.path, query: n.status });
    it.iconPath = vscode.ThemeIcon.File;
    it.description = (dir === '.' ? '' : dir) + (n.from ? ' ← ' + n.from : '');
    it.tooltip = n.path + ' • ' + ((STATUS[n.status] || ['Changed'])[0]) + (n.from ? ' from ' + n.from : '');
    it.contextValue = n.ref ? 'commitFile' : 'workFile';
    it.command = { command: 'claudeStatus.changes.openDiff', title: 'Open Changes', arguments: [n] };
    return it;
  }

  // ---- opening ----
  _sides(n) {
    const wt = this.target.wt;
    const oldPath = n.from || n.path;
    if (!n.ref) {   // working tree: HEAD ↔ live file
      const left = n.status === 'U' || n.status === 'A' ? revUri(wt, oldPath, '') : revUri(wt, oldPath, 'HEAD');
      const right = n.status === 'D' ? revUri(wt, n.path, '') : vscode.Uri.file(path.join(wt, n.path));
      return { left, right, title: path.posix.basename(n.path) + ' (Working Tree)' };
    }
    const parent = n.ref + '^';
    const left = n.status === 'A' ? revUri(wt, oldPath, '') : revUri(wt, oldPath, parent);
    const right = n.status === 'D' ? revUri(wt, n.path, '') : revUri(wt, n.path, n.ref);
    return { left, right, title: path.posix.basename(n.path) + ' (' + n.short + ')' };
  }
  async openDiff(n) {
    if (!n || !this.target) return;
    const { left, right, title } = this._sides(n);
    await vscode.commands.executeCommand('vscode.diff', left, right, title, { preview: true });
  }
  async openFile(n) {
    if (!n || !this.target) return;
    const fp = path.join(this.target.wt, n.path);
    if (!fs.existsSync(fp)) { vscode.window.showInformationMessage('That file no longer exists in the worktree.'); return; }
    await vscode.window.showTextDocument(vscode.Uri.file(fp), { preview: false });
  }
  // every file of a commit (or of the working tree) in VSCode's multi-file diff editor
  async openAll(n) {
    if (!n || !this.target) return;
    const files = n.kind === 'changes' ? n.files : this._filter((await this.getChildren(n)).filter((k) => k.kind === 'file'));
    if (!files.length) return;
    const nodes = files.map((f) => (f instanceof Node ? f : new Node('file', Object.assign({ ref: null }, f))));
    const title = n.kind === 'changes' ? this.target.name + ' — Uncommitted Changes' : n.short + ' — ' + n.subject;
    const resources = nodes.map((f) => { const s = this._sides(f); return [vscode.Uri.file(path.join(this.target.wt, f.path)), s.left, s.right]; });
    try { await vscode.commands.executeCommand('vscode.changes', title, resources); }
    catch { await this.openDiff(nodes[0]); }   // VSCode older than the multi-diff editor
  }
  // a SHA clicked in a session's terminal: target that worktree, reveal the commit, open its changes
  async showCommit(t, sha) {
    if (!t) return;
    const full = ((await gitText(t.wt, ['rev-parse', '--verify', '--quiet', sha + '^{commit}'])) || '').trim();
    if (!full) { vscode.window.showInformationMessage(sha + ' is not a commit in ' + t.name + '.'); return; }
    const meta = ((await gitText(t.wt, ['show', '--no-patch', '--format=%h%x1f%s%x1f%an%x1f%ar%x1f%ad', '--date=format:%Y-%m-%d %H:%M', full])) || '').trim().split('\x1f');
    await this.show(t.slug, t.name);
    const node = new Node('commit', { sha: full, short: meta[0], subject: meta[1] || '', author: meta[2] || '', when: meta[3] || '', date: meta[4] || '' });
    await this.openAll(node);
  }

  _setTests(hide) {
    try {
      if (hide) { fs.mkdirSync(path.dirname(this.testsFlag), { recursive: true }); fs.writeFileSync(this.testsFlag, ''); }
      else if (fs.existsSync(this.testsFlag)) fs.unlinkSync(this.testsFlag);
    } catch (e) { vscode.window.showErrorMessage('Toggling test files failed: ' + e.message); }
    this.testsToggled();
    if (this.dev._postTests) this.dev._postTests();
  }
  // the flag flipped (here or from the roster toolbar)
  testsToggled() { this._syncTestsContext(); this._emitter.fire(); }
  _syncTestsContext() { vscode.commands.executeCommand('setContext', 'claudeStatus.testsHidden', this._hideTests()); }
}

module.exports = { GitView, parsePorcelain, parseNameStatus, TEST_RE };
