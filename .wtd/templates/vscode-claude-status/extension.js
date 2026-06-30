const vscode = require('vscode');
const fs = require('fs');
const os = require('os');
const path = require('path');
const cp = require('child_process');
const https = require('https');

const STATUS_FILE = '.claude-status';
const HOME = os.homedir();
const RATE_FILE = path.join(HOME, '.claude', 'rate-limits.json');   // statusline-written fallback
const CREDS = path.join(HOME, '.claude', '.credentials.json');      // OAuth token for the live fetch
const CLAUDE_JSON = path.join(HOME, '.claude.json');                // account email
const TESTS_FLAG = path.join(HOME, '.config', 'wtd', 'exclude-tests');  // present = exclude test files from diffs
// The dev root (the dir holding .wtd/ + worktrees/ + repos/) is normally ~/dev, but the tree is
// RELOCATABLE — install.sh renders __DEV__ into the hooks for wherever it actually lives. This
// extension ships as a prebuilt .vsix, so it can't be render-substituted; instead it discovers the
// root at runtime (see resolveDevRoot, called from activate). Initialized to the canonical ~/dev so
// these are always defined; activate() overwrites them once the workspace is known.
let DEV = path.join(HOME, 'dev');
let WTD = path.join(DEV, '.wtd');
let SESS_DIR = path.join(WTD, 'state', 'sessions');   // vscode-backend session registry (no tmux)

// A dir is a worktree-dev base iff it contains a .wtd/ dir.
const DEV_ROOT_PIN = path.join(HOME, '.config', 'wtd', 'dev-root');  // install-written absolute path
function isDevRoot(d) { try { return !!d && fs.existsSync(path.join(d, '.wtd')); } catch { return false; } }

// Resolve the dev base. The tree is relocatable and the shipped .vsix can't be render-substituted,
// so we find it at runtime, most-authoritative first:
//   1. the install-written pin file (location-independent — works even with no relevant folder open);
//   2. a WTD_DEV env override;
//   3. discovery from the open folders — each folder, its ANCESTORS (opened on a worktree) and its
//      immediate CHILDREN (opened on the base's parent, e.g. D:\Projects\Dev);
//   4. the canonical ~/dev.
function resolveDevRoot() {
  try { const p = fs.readFileSync(DEV_ROOT_PIN, 'utf8').trim(); if (isDevRoot(p)) return p; } catch {}
  if (isDevRoot(process.env.WTD_DEV)) return process.env.WTD_DEV;
  for (const start of (vscode.workspace.workspaceFolders || []).map((f) => f.uri.fsPath)) {
    let d = start;
    for (let i = 0; i < 8; i++) {                       // climb toward the filesystem root
      if (isDevRoot(d)) return d;
      const up = path.dirname(d);
      if (up === d) break;                              // reached the root
      d = up;
    }
    try {                                               // opened one level above the base
      for (const e of fs.readdirSync(start, { withFileTypes: true }))
        if (e.isDirectory() && isDevRoot(path.join(start, e.name))) return path.join(start, e.name);
    } catch {}
  }
  return path.join(HOME, 'dev');
}

// (re)compute DEV/WTD/SESS_DIR from the current workspace
function refreshDevRoot() {
  DEV = resolveDevRoot();
  WTD = path.join(DEV, '.wtd');
  SESS_DIR = path.join(WTD, 'state', 'sessions');
}
const IS_WIN = process.platform === 'win32';
// The shell each worktree terminal runs. On Windows that's Git Bash (so the .wtd bash scripts run);
// elsewhere /bin/bash. On Windows `agent` runs claude directly in this terminal (no tmux); on Unix it
// runs `tmux attach`, so the terminal hosts the tmux session either way.
function bashShell() {
  if (!IS_WIN) return '/bin/bash';
  for (const c of ['C:\\Program Files\\Git\\bin\\bash.exe', 'C:\\Program Files (x86)\\Git\\bin\\bash.exe',
                   path.join(HOME, 'scoop', 'apps', 'git', 'current', 'bin', 'bash.exe')]) {
    try { if (fs.existsSync(c)) return c; } catch {}
  }
  return 'bash.exe';
}

// Run a .wtd shell script or a shebang wrapper (archive/agent/refresh-diffs/monitor-stats). On Windows
// these aren't directly spawnable — cp.execFile throws EFTYPE — so route them through Git Bash (with
// forward-slash paths it can stat); elsewhere exec them directly. Crucially this NEVER throws
// synchronously: a spawn failure is delivered to cb, so e.g. _postMonitor can't take down the webview.
function execScript(file, args, opts, cb) {
  const done = typeof cb === 'function' ? cb : () => {};
  try {
    if (IS_WIN) {
      const line = [file].concat(args || []).map((a) => shq(String(a).replace(/\\/g, '/'))).join(' ');
      return cp.execFile(bashShell(), ['-lc', line], opts, done);
    }
    return cp.execFile(file, args || [], opts, done);
  } catch (e) { done(e); }
}

class ClaudeStatusProvider {
  constructor() {
    this._onDidChange = new vscode.EventEmitter();
    this.onDidChangeFileDecorations = this._onDidChange.event;
  }

  provideFileDecoration(uri) {
    let stat;
    try { stat = fs.statSync(uri.fsPath); } catch { return; }
    if (!stat.isDirectory()) return;

    let content;
    try { content = fs.readFileSync(path.join(uri.fsPath, STATUS_FILE), 'utf8').trim(); }
    catch { return; }

    if (content === 'working')   return new vscode.FileDecoration('◐', 'Claude working',       new vscode.ThemeColor('claudeStatus.working'));
    if (content === 'input')     return new vscode.FileDecoration('!', 'Your turn',             new vscode.ThemeColor('claudeStatus.input'));
    if (content === 'reviewing') return new vscode.FileDecoration('⋯', 'Waiting on review',     new vscode.ThemeColor('claudeStatus.reviewing'));
    if (content === 'pr')        return new vscode.FileDecoration('◆', 'PR ready',              new vscode.ThemeColor('claudeStatus.pr'));
    if (content === 'done')      return new vscode.FileDecoration('✓', 'Done',                  new vscode.ThemeColor('claudeStatus.done'));
    if (content === 'stopped')   return new vscode.FileDecoration('○', 'Stopped (no session)',  new vscode.ThemeColor('claudeStatus.stopped'));
    return;
  }

  refresh(uri) { this._onDidChange.fire(uri); }
}

function shq(s) { return "'" + String(s).replace(/'/g, "'\\''") + "'"; }

// "Dev workflow summary" Explorer panel: the Claude session-limit bars (+ active account email) on
// top, then a live fleet roster of worktrees (status glyph + git state) you can click to open, plus
// a "+ agent" launcher.
class DevSummaryProvider {
  constructor() {
    this.view = null; this.limTimer = null; this.rosTimer = null; this._tick = null; this._lastUsage = {};
    this._terms = new Map();      // worktree key -> Terminal we opened (to focus instead of duplicate)
    this._lastStatus = {};        // worktree key -> last status seen (to detect transitions)
    this._unread = {};            // worktree key -> true when it flipped to "your turn" and not yet opened
    this._current = null;         // the Terminal of the worktree session currently focused (marked in roster)
  }
  _key(slug, name) { return slug + '' + name; }
  clearUnread(key) { if (this._unread[key]) { this._unread[key] = false; this._postRoster(); } }
  markUnread(key) { if (!this._unread[key]) { this._unread[key] = true; this._postRoster(); } }
  // a roster row was opened: focus the existing terminal if we have one (incl. a reload-revived tab
  // matched by name), otherwise launch a new one.
  openOrFocus(slug, name, glyph) {
    const key = this._key(slug, name);
    let t = this._terms.get(key);
    if (!t || t.exitStatus !== undefined) {
      t = vscode.window.terminals.find((x) => x.name === name);
    }
    if (t) { t.show(); }
    else {
      const nm = name;   // tab = worktree name only (no slug, no status glyph — status shows in the roster)
      t = vscode.window.createTerminal({ name: nm, location: vscode.TerminalLocation.Editor,
        shellPath: bashShell(), shellArgs: ['-lc', 'agent ' + shq(slug) + ' ' + shq(name)] });
      t.show();
    }
    this._terms.set(key, t);
    this._current = t;            // the just-opened session is now the selected one
    this.clearUnread(key);
    setTimeout(() => this._postRoster(), 1500);
  }
  onTermClosed(t) { if (this._current === t) { this._current = null; this._postRoster(); } for (const [k, v] of this._terms) if (v === t) { this._terms.delete(k); break; } }
  onTermActive(t) {
    if (!t) return;
    this._current = t; setTimeout(() => this._postRoster(), 0);   // mark the focused session as selected
    for (const [k, v] of this._terms) if (v === t) { this.clearUnread(k); return; }
    // also match a reload-revived terminal by name
    for (const k of Object.keys(this._unread)) { const name = k.split('')[1]; if (this._unread[k] && t.name && t.name === name) { this.clearUnread(k); return; } }
  }

  resolveWebviewView(view) {
    this.view = view;
    view.webview.options = { enableScripts: true };
    view.webview.html = this._html();

    view.webview.onDidReceiveMessage((m) => {
      if (!m) return;
      if (m.cmd === 'open' && m.slug && m.name) {
        this.openOrFocus(m.slug, m.name, m.glyph);   // focus an existing tab instead of duplicating
      } else if (m.cmd === 'markunread' && m.slug && m.name) {
        this.markUnread(this._key(m.slug, m.name));   // flag the row yellow to revisit (clears on open/focus)
      } else if (m.cmd === 'archive' && m.slug && m.name) {
        vscode.window.showWarningMessage(
          'Archive ' + m.slug + ' ' + m.name + '?  Ends its session and moves it to worktrees/' + m.slug + '/archive/ (reopen later from the roster).',
          { modal: true }, 'Archive'
        ).then((ch) => {
          if (ch !== 'Archive') return;
          execScript(path.join(HOME, '.local', 'bin', 'archive'), [m.slug, m.name], { timeout: 30000 }, (e, so, se) => {
            if (e) vscode.window.showErrorMessage('archive failed: ' + ((se || '').trim() || e.message));
            else vscode.window.showInformationMessage('Archived ' + m.slug + ' ' + m.name);
            this._postRoster();
          });
        });
      } else if (m.cmd === 'delete' && m.slug && m.name) {
        // Two-option modal: remove the worktree (keep the branch) or also force-delete the branch.
        // We never pass --force up front (it would silently discard uncommitted/untracked work). A
        // clean delete is tried first; only if `agent rm` reports a dirty worktree do we ask, in a
        // SECOND modal, whether to force-delete and discard — so destroying work is always an explicit
        // extra confirmation, never agent-inferred.
        vscode.window.showWarningMessage(
          'Delete ' + m.slug + ' ' + m.name + '?  Ends its session and removes the worktree. "+ branch" also force-deletes the git branch — any unpushed commits on it are lost.',
          { modal: true }, 'Delete worktree', 'Delete worktree + branch'
        ).then((ch) => {
          if (ch !== 'Delete worktree' && ch !== 'Delete worktree + branch') return;
          const withBranch = ch === 'Delete worktree + branch';
          const run = (force) => {
            const args = ['rm', m.slug, m.name, '-y'];
            if (force) args.push('--force');
            if (withBranch) args.push('--branch');
            execScript(path.join(HOME, '.local', 'bin', 'agent'), args, { timeout: 30000 }, (e, so, se) => {
              const out = ((se || '') + (so || '')).trim();
              if (!e) {
                vscode.window.showInformationMessage('Deleted ' + m.slug + ' ' + m.name + (withBranch ? ' (+ branch)' : ''));
                setTimeout(() => this._postRoster(), 600);
                return;
              }
              if (!force && /--force|modified or untracked/i.test(out)) {   // dirty worktree → ask before discarding
                vscode.window.showWarningMessage(
                  m.slug + ' ' + m.name + ' has uncommitted or untracked changes. Force-delete and DISCARD them?',
                  { modal: true }, 'Force delete'
                ).then((c) => { if (c === 'Force delete') run(true); });
                return;
              }
              vscode.window.showErrorMessage('delete failed: ' + (out || e.message));
            });
          };
          run(false);
        });
      } else if (m.cmd === 'terminate' && m.slug && m.name) {
        vscode.window.showWarningMessage(
          'End the session for ' + m.slug + ' ' + m.name + '?  The worktree (branch, changes, reviews) stays — only the live Claude session ends. Reopen it from the roster.',
          { modal: true }, 'End session'
        ).then((ch) => {
          if (ch !== 'End session') return;
          execScript(path.join(HOME, '.local', 'bin', 'agent'), ['stop', m.slug, m.name], { timeout: 15000 }, (e, so, se) => {
            if (e) vscode.window.showErrorMessage('end session failed: ' + ((se || '').trim() || e.message));
            setTimeout(() => this._postRoster(), 800);
          });
        });
      } else if (m.cmd === 'newAgent') {
        vscode.window.showInputBox({
          prompt: 'New agent — enter: <slug> <name> [ref-tokens…]',
          placeHolder: 'mod feat/my-thing',
        }).then((v) => {
          if (v && v.trim()) {
            const nm = (v.trim().split(/\s+/)[1] || v.trim().split(/\s+/)[0] || 'agent');   // tab = the <name> token (no slug)
            const t = vscode.window.createTerminal({ name: nm, location: vscode.TerminalLocation.Editor,
              shellPath: bashShell(), shellArgs: ['-lc', 'agent ' + v.trim()] });
            t.show();
            setTimeout(() => this._postRoster(), 2500);
          }
        });
      } else if (m.cmd === 'toggleTests') {
        // flip the global "exclude test files from diffs" flag, then re-seed live diffs so it shows now
        try {
          if (fs.existsSync(TESTS_FLAG)) fs.unlinkSync(TESTS_FLAG);
          else { fs.mkdirSync(path.dirname(TESTS_FLAG), { recursive: true }); fs.writeFileSync(TESTS_FLAG, ''); }
        } catch (e) { vscode.window.showErrorMessage('toggle tests failed: ' + e.message); }
        this._postTests();
        execScript(path.join(DEV, '.wtd', 'hooks', 'refresh-diffs.sh'), [], { timeout: 30000 }, () => {});
      }
    });

    const tick = () => this._postLimits();
    tick(); this._postRoster(); this._postMonitor(); this._postTests();
    // refresh accounts' usage every 60s. Active accounts come from their (free) statusline file; only
    // idle accounts hit the endpoint — 60s keeps API calls low enough to avoid 429. Roster every 12s,
    // system monitor every 5s.
    this.limTimer = setInterval(tick, 60000);
    this.rosTimer = setInterval(() => this._postRoster(), 12000);
    this.monTimer = setInterval(() => this._postMonitor(), 5000);
    view.onDidChangeVisibility(() => { if (view.visible) { tick(); this._postRoster(); this._postMonitor(); this._postTests(); } });
    view.onDidDispose(() => {
      if (this.limTimer) clearInterval(this.limTimer);
      if (this.rosTimer) clearInterval(this.rosTimer);
      if (this.monTimer) clearInterval(this.monTimer);
      this.limTimer = this.rosTimer = this.monTimer = this.view = null;
    });
  }

  // tell the webview whether test files are currently excluded from the diffs (flag-file presence)
  _postTests() {
    if (!this.view || !this.view.visible) return;
    let excluded = false; try { excluded = fs.existsSync(TESTS_FLAG); } catch {}
    this.view.webview.postMessage({ type: 'teststate', excluded });
  }

  // every configured account: the default (~/.claude) + each ~/.claude-accounts/<name>
  _accounts() {
    const list = [{ name: 'default', dir: path.join(HOME, '.claude'), json: path.join(HOME, '.claude.json') }];
    try {
      for (const d of fs.readdirSync(path.join(HOME, '.claude-accounts'), { withFileTypes: true }))
        if (d.isDirectory()) { const dir = path.join(HOME, '.claude-accounts', d.name); list.push({ name: d.name, dir, json: path.join(dir, '.claude.json') }); }
    } catch {}
    return list;
  }

  // gather usage for ALL accounts and post them together so the panel shows each (no overwrite)
  _postLimits() {
    if (!this.view || !this.view.visible) return;
    const accts = this._accounts();
    if (!accts.length) { this.view.webview.postMessage({ type: 'limits', accounts: [] }); return; }
    const out = new Array(accts.length); let pending = accts.length;
    const done = () => { if (--pending === 0 && this.view && this.view.visible) this.view.webview.postMessage({ type: 'limits', accounts: out.filter(Boolean) }); };
    accts.forEach((a, i) => this._usageFor(a, (u) => { out[i] = u; done(); }));
  }

  // usage for one account: LIVE-FETCH FIRST from the same endpoint the /usage panel uses (authoritative,
  // zero token cost) so the numbers always match the official panel. The statusline file is an
  // unreliable cache (its rate_limits schema drifted to null), so it's only a fallback, and any
  // fallback is returned with its ORIGINAL (old) ts so the webview clearly marks it stale — we never
  // show a stale value as if it were current.
  _usageFor(a, cb) {
    let email = '';
    try { email = (JSON.parse(fs.readFileSync(a.json, 'utf8')).oauthAccount || {}).emailAddress || ''; } catch {}
    let file = null;
    try { file = JSON.parse(fs.readFileSync(path.join(a.dir, 'rate-limits.json'), 'utf8')); } catch {}
    const now = Math.floor(Date.now() / 1000);
    const hasNums = (d) => d && (typeof (d.five_hour || {}).used === 'number' || typeof (d.seven_day || {}).used === 'number');
    const ofFile = (f) => ({ name: a.name, email: email || (f && f.email) || '', five_hour: f && f.five_hour, seven_day: f && f.seven_day, ts: f && f.ts });
    // fallback when the live fetch can't run/succeeds: file (if it has real numbers) else last-known
    // cache — both keep their OLD ts so the webview dims them + shows "⟳ Nm old". Never blank.
    const fallback = () => {
      if (hasNums(ofFile(file))) return cb(ofFile(file));
      if (this._lastUsage[a.name]) return cb({ ...this._lastUsage[a.name], email: email || this._lastUsage[a.name].email });
      return cb({ name: a.name, email, nologin: !email });
    };
    let tok = '';
    try { tok = (JSON.parse(fs.readFileSync(path.join(a.dir, '.credentials.json'), 'utf8')).claudeAiOauth || {}).accessToken || ''; } catch {}
    if (!tok) return fallback();
    this._fetchUsage(tok, (u) => {
      if (u && hasNums(u)) {
        const fresh = { name: a.name, email, five_hour: u.five_hour, seven_day: u.seven_day, ts: now };
        this._lastUsage[a.name] = fresh;       // keep the cache current for offline fallback
        return cb(fresh);
      }
      fallback();   // fetch failed (429/expired/offline) → stale file / last-known, marked stale, never blank
    });
  }

  // Live usage straight from the same endpoint the /usage panel uses. Zero token cost. Returns the
  // webview's data shape, or null on any failure (token missing/expired, offline) so we fall back.
  // live usage for a given token: { five_hour:{used,resets_at}, seven_day:{...} } or null on failure
  _fetchUsage(tok, cb) {
    const num = (x) => (typeof x === 'number' && isFinite(x)) ? Math.round(x) : null;
    const epoch = (s) => { const t = Date.parse(s); return isNaN(t) ? 0 : Math.floor(t / 1000); };
    const req = https.get({
      host: 'api.anthropic.com', path: '/api/oauth/usage', timeout: 10000,
      headers: { 'Authorization': 'Bearer ' + tok, 'anthropic-beta': 'oauth-2025-04-20', 'Content-Type': 'application/json' },
    }, (res) => {
      if (res.statusCode !== 200) { res.resume(); return cb(null); }
      let b = ''; res.on('data', (d) => b += d);
      res.on('end', () => {
        try {
          const j = JSON.parse(b), f = j.five_hour || {}, s = j.seven_day || {};
          cb({ five_hour: { used: num(f.utilization), resets_at: epoch(f.resets_at) },
               seven_day: { used: num(s.utilization), resets_at: epoch(s.resets_at) } });
        } catch { cb(null); }
      });
    });
    req.on('error', () => cb(null));
    req.on('timeout', () => { req.destroy(); cb(null); });
  }

  async _postRoster() {
    if (!this.view || !this.view.visible) return;
    const rows = await this._roster();
    const live = await this._liveSessions();   // session names with a live tmux session
    // resolve which row is the currently-focused session: by exact terminal if we opened it, else
    // (e.g. after a window reload, when _terms is empty) fall back to matching the terminal's name.
    const cur = this._current; let curKey = null;
    if (cur) for (const [k, v] of this._terms) if (v === cur) { curKey = k; break; }
    // mark a row "unread" when it flips to 'input' (agent finished → your turn); cleared on open/focus
    for (const w of rows) {
      const key = this._key(w.slug, w.name);
      const prev = this._lastStatus[key];
      if (w.status === 'input' && prev !== undefined && prev !== 'input') this._unread[key] = true;
      this._lastStatus[key] = w.status;
      w.unread = !!this._unread[key];
      w.active = live.has(w.slug + '-' + w.name);   // has a live tmux session (vs. closed/inactive)
      w.current = !!cur && (curKey ? key === curKey : (!!cur.name && cur.name === w.name));   // focused session
    }
    if (this.view && this.view.visible) this.view.webview.postMessage({ type: 'roster', rows });
  }

  // names of worktrees with a live session (session name = "<slug>-<name>"). On Windows there's no
  // tmux: the on-disk registry (written by agent.sh) is the source of truth — filenames encode '/'
  // as '__', so decode them back. On Unix, ask tmux.
  _liveSessions() {
    return new Promise((res) => {
      if (IS_WIN) {
        const s = new Set();
        try { for (const f of fs.readdirSync(SESS_DIR)) s.add(f.replace(/__/g, '/')); } catch {}
        return res(s);
      }
      cp.execFile('tmux', ['list-sessions', '-F', '#{session_name}'], { timeout: 3000 }, (e, out) => {
        const s = new Set();
        if (!e && out) for (const ln of out.split('\n')) { const n = ln.trim(); if (n) s.add(n); }
        res(s);
      });
    });
  }

  // system monitor: tmux sessions, claude procs + RSS, reviews running, WSL mem, CPU load
  _postMonitor() {
    if (!this.view || !this.view.visible) return;
    execScript(path.join(DEV, '.wtd', 'hooks', 'monitor-stats.sh'), [], { timeout: 8000 }, (e, out) => {
      if (e || !this.view || !this.view.visible) return;
      const p = (out || '').trim().split('|');   // sess|nag|rev|acpu|amem|mt|msys|ncpu|load
      if (p.length < 9) return;
      this.view.webview.postMessage({ type: 'monitor', m: {
        sess: +p[0], nag: +p[1], rev: +p[2], acpu: +p[3], amem: +p[4], mt: +p[5], msys: +p[6], ncpu: +p[7], load: p[8] } });
    });
  }

  _roster() {
    const wts = [];
    const walk = (dir, slug, rel, depth) => {
      if (depth > 4) return;
      let isWt = false;
      try { isWt = fs.existsSync(path.join(dir, '.git')); } catch {}
      if (isWt) { wts.push({ slug, name: rel, dir }); return; }
      let es = [];
      try { es = fs.readdirSync(dir, { withFileTypes: true }).filter((d) => d.isDirectory()); } catch {}
      for (const e of es) {
        if (rel === '' && e.name === 'archive') continue;   // reserved: archived worktrees
        walk(path.join(dir, e.name), slug, rel ? rel + '/' + e.name : e.name, depth + 1);
      }
    };
    let slugs = [];
    try { slugs = fs.readdirSync(path.join(DEV, 'worktrees'), { withFileTypes: true }).filter((d) => d.isDirectory()).map((d) => d.name); } catch {}
    for (const slug of slugs) walk(path.join(DEV, 'worktrees', slug), slug, '', 0);

    return Promise.all(wts.map((w) => new Promise((res) => {
      let status = '';
      try { status = fs.readFileSync(path.join(w.dir, STATUS_FILE), 'utf8').trim(); } catch {}
      cp.execFile('git', ['-C', w.dir, 'status', '--porcelain', '--branch'], { timeout: 3000 }, (e, out) => {
        let dirty = false, ahead = 0;
        if (!e && out) {
          const lines = out.split('\n');
          dirty = lines.slice(1).some((l) => l.trim().length > 0);
          const m = out.match(/ahead (\d+)/);
          if (m) ahead = parseInt(m[1], 10) || 0;
        }
        res({ slug: w.slug, name: w.name, status, dirty, ahead });
      });
    })));
  }

  _html() {
    return `<!DOCTYPE html><html><head><meta charset="utf-8">
<style>
  html{height:100%;}
  /* full-height flex column so the monitor block can be pinned to the bottom of the panel */
  body{padding:3px 8px 5px;margin:0;font:12px var(--vscode-font-family);color:var(--vscode-foreground);display:flex;flex-direction:column;min-height:100vh;box-sizing:border-box;}
  #monwrap{margin-top:auto;}   /* push the monitor section to the bottom of the available space */
  .acct{opacity:.6;font-size:12px;margin-bottom:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
  .acctblk{margin-bottom:5px;}
  .row{display:flex;align-items:center;gap:6px;height:15px;}
  .lbl{width:15px;opacity:.65;}
  .track{flex:1;height:5px;border-radius:3px;background:var(--vscode-input-background,rgba(127,127,127,.18));overflow:hidden;}
  .fill{display:block;height:100%;width:0;border-radius:3px;transition:width .3s ease;}
  .pct{width:30px;text-align:right;font-variant-numeric:tabular-nums;}
  .meta{min-width:34px;opacity:.55;font-size:12px;}
  .none{opacity:.5;font-size:12px;padding:2px 0;}
  .stale{opacity:.45;} .staleNote{font-size:10px;opacity:.6;font-style:italic;color:var(--vscode-charts-yellow,#d2a000);margin-top:1px;}
  hr{border:none;border-top:1px solid var(--vscode-panel-border,rgba(127,127,127,.2));margin:5px 0 4px;}
  .head{display:flex;align-items:center;justify-content:space-between;gap:6px;font-size:12px;margin-bottom:3px;}
  .counts{opacity:.8;}
  .btn{cursor:pointer;border:1px solid var(--vscode-button-border,transparent);background:var(--vscode-button-secondaryBackground,rgba(127,127,127,.18));color:var(--vscode-button-secondaryForeground,inherit);border-radius:3px;padding:0 6px;line-height:16px;font-size:12px;}
  .btn:hover{background:var(--vscode-button-secondaryHoverBackground,rgba(127,127,127,.3));}
  .wt{display:flex;align-items:center;gap:5px;height:21px;cursor:pointer;border-radius:3px;padding:0 3px;font-size:13px;}
  .wt:hover{background:var(--vscode-list-hoverBackground,rgba(127,127,127,.12));}
  .wt .nm{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
  .wt .git{opacity:.6;font-size:12px;font-variant-numeric:tabular-nums;}
  .ahead{color:var(--vscode-charts-blue,#4aa3ff);} .dirty{color:var(--vscode-charts-yellow,#d2a000);}
  .repo{font-size:10px;text-transform:uppercase;letter-spacing:.6px;opacity:.5;margin:5px 0 1px;}
  .repo:first-child{margin-top:1px;}
  .wt .arch,.wt .term,.wt .unr,.wt .del{opacity:0;cursor:pointer;padding:0 2px;font-size:12px;}
  .wt:hover .arch,.wt:hover .term,.wt:hover .unr,.wt:hover .del{opacity:.55;} .wt .arch:hover,.wt .term:hover,.wt .unr:hover,.wt .del:hover{opacity:1;}
  .wt .term:hover,.wt .del:hover{color:var(--vscode-charts-red,#e5534b);}
  .wt .unr:hover{color:var(--vscode-charts-yellow,#d2a000);}
  .wt.sep{margin-top:7px;}   /* gap between status groups */
  .wt.active{background:rgba(127,127,127,.13);}   /* live tmux session — noticeably lighter than normal */
  .wt.unread{background:rgba(255,216,61,.16);box-shadow:inset 2px 0 0 var(--vscode-charts-yellow,#d2a000);}  /* your-turn, not yet opened — yellow */
  /* the session you currently have focused — VSCode's "selected list item" look (accent bar + bg).
     Declared last so its background wins over .active/.unread; pairs with .unread's yellow bar fine. */
  .wt.current{background:var(--vscode-list-activeSelectionBackground,rgba(9,71,113,.55));box-shadow:inset 3px 0 0 var(--vscode-focusBorder,#2f81f7);}
  .wt.current .nm{font-weight:600;}
  .statline{display:flex;flex-wrap:wrap;gap:3px 14px;margin:3px 0 6px;}
  .stat{display:flex;gap:5px;align-items:baseline;}
  .sk{opacity:.5;} .sv{font-variant-numeric:tabular-nums;}
  .mlbl{width:26px;flex:none;opacity:.65;}
  #mon .row{height:16px;margin:1px 0;}
  #mon .meta{min-width:0;text-align:right;white-space:nowrap;}
</style></head><body>
<div id="lim"><div class="none">waiting for a session…</div></div>
<hr>
<div class="head"><span class="counts" id="counts"></span><span class="btn" id="tests" title="Include/exclude test files in the diff panes">tests ✓</span><span class="btn" id="add" title="Launch a new agent">+ agent</span></div>
<div id="roster"></div>
<div id="monwrap">
<hr>
<div class="repo">monitor</div>
<div id="mon"><div class="none">…</div></div>
</div>
<script>
  const vsc = acquireVsCodeApi();
  let accts=[], ros=[], mon=null;
  const GLYPH={working:'🔵',input:'🟡',reviewing:'🟣',pr:'🔹',done:'🟢',stopped:'🔴'};   // emoji -> editor tab name
  const GLYPHD={working:'◐',input:'!',reviewing:'⋯',pr:'◆',done:'✓',stopped:'○'};        // explorer-style glyph for the roster
  const COL={working:'#4aa3ff',input:'#ffd83d',reviewing:'#c586f0',pr:'#5cc8ff',done:'#3fd35f',stopped:'#ff5c57'};
  const PRIO={input:0,reviewing:1,working:2,pr:3,done:4,stopped:5,'':6};   // pr sorts below working, above done
  function esc(s){ return String(s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }
  function col(p){ return p>=90?'var(--vscode-charts-red,#e5534b)':p>=70?'var(--vscode-charts-yellow,#d2a000)':'var(--vscode-charts-green,#3fb950)'; }
  function rel(ts){ if(!ts) return ''; const s=ts-Math.floor(Date.now()/1000); if(s<=0) return 'resetting'; const h=Math.floor(s/3600),m=Math.floor((s%3600)/60); return h>0?(h+'h'+m+'m'):(m+'m'); }
  function ago(ts){ if(!ts) return 0; return Math.max(0, Math.floor(Date.now()/1000)-ts); }
  function agoTxt(s){ const m=Math.floor(s/60); return m>=60?(Math.floor(m/60)+'h'+(m%60)+'m'):(m>0?(m+'m'):(s+'s')); }
  function bar(lbl,p,resets){ const has=typeof p==='number'; const w=has?Math.min(100,Math.max(0,p)):0;
    return '<div class="row" title="'+lbl+' limit '+(has?w+'%':'unknown')+(resets?(' · resets in '+rel(resets)):'')+'">'
      +'<span class="lbl">'+lbl+'</span><div class="track"><div class="fill" style="width:'+w+'%;background:'+col(w)+'"></div></div>'
      +'<span class="pct">'+(has?w+'%':'--')+'</span><span class="meta">'+(resets?rel(resets):'')+'</span></div>';
  }
  function renderLim(){
    const el=document.getElementById('lim');
    if(!accts || !accts.length){ el.innerHTML='<div class="none">waiting for a session…</div>'; return; }
    el.innerHTML = accts.map(a=>{
      const label = a.email || a.name;
      const head = '<div class="acct" title="'+esc(label)+'">'+esc(label)+'</div>';
      if(a.nologin) return '<div class="acctblk">'+head+'<div class="none">not logged in</div></div>';
      const f=a.five_hour||{}, s=a.seven_day||{};
      const age = ago(a.ts); const stale = age >= 90;
      const bars = '<div class="'+(stale?'stale':'')+'">'+bar('5h', f.used, f.resets_at)+bar('7d', s.used, s.resets_at)+'</div>';
      const note = stale ? '<div class="staleNote" title="Live fetch failing (token expired / offline / rate-limited); showing last known.">⟳ '+agoTxt(age)+' old</div>' : '';
      return '<div class="acctblk">'+head+bars+note+'</div>';
    }).join('');
  }
  function renderRoster(){
    const c={}; ros.forEach(w=>{ const k=w.status||''; c[k]=(c[k]||0)+1; });
    const order=['input','reviewing','working','pr','done','stopped'];
    document.getElementById('counts').innerHTML =
      order.filter(k=>c[k]).map(k=>'<span style="color:'+(COL[k]||'#888')+'">'+GLYPHD[k]+'</span>'+c[k]).join(' · ') || '<span style="opacity:.5">no worktrees</span>';
    // group by repo (slug); 'mod' first, then the rest alphabetically. Within a repo, sort by
    // status priority (actionable on top) then name.
    const groups={}; ros.forEach(w=>{ (groups[w.slug]=groups[w.slug]||[]).push(w); });
    const slugs=Object.keys(groups).sort((a,b)=> a==='mod'?-1 : b==='mod'?1 : a.localeCompare(b));
    const wtRow=(w,sep)=>{
      const g=GLYPH[w.status]||GLYPH.stopped;      // emoji -> editor tab name (data-glyph)
      const gd=GLYPHD[w.status]||'○';              // explorer-style glyph shown in the row
      const gc=COL[w.status]||'#888';
      const git=(w.ahead?'<span class="ahead">↑'+w.ahead+'</span> ':'')+(w.dirty?'<span class="dirty">●</span>':'');
      return '<div class="wt'+(sep?' sep':'')+(w.active?' active':'')+(w.unread?' unread':'')+(w.current?' current':'')+'" data-slug="'+esc(w.slug)+'" data-name="'+esc(w.name)+'" data-glyph="'+g+'" title="'+esc(w.slug+' '+w.name)+(w.status?(' — '+w.status):'')+(w.active?' · active':'')+(w.current?' · selected':'')+(w.unread?' · unread':'')+' · click to open">'
        +'<span style="color:'+gc+'">'+gd+'</span><span class="nm">'+esc(w.name)+'</span><span class="git">'+git+'</span>'
        +(w.active&&!w.unread?'<span class="unr" title="Mark unread (flag it yellow to revisit)">✉</span>':'')
        +(w.active?'<span class="term" title="End the tmux session (worktree stays)">⏹</span>':'')
        +'<span class="arch" title="Archive '+esc(w.name)+'">📦</span>'
        +'<span class="del" title="Delete '+esc(w.name)+' (remove worktree)">🗑</span></div>';
    };
    document.getElementById('roster').innerHTML = slugs.map(slug=>{
      const rows=groups[slug].sort((a,b)=>(PRIO[a.status]??9)-(PRIO[b.status]??9) || a.name.localeCompare(b.name));
      // add a gap whenever the status changes, so each status group is visually separated
      return '<div class="repo">'+esc(slug)+'</div>'+rows.map((w,i)=>wtRow(w, i>0 && rows[i-1].status!==w.status)).join('');
    }).join('');
    document.querySelectorAll('.wt').forEach(el=>el.onclick=()=>vsc.postMessage({cmd:'open',slug:el.dataset.slug,name:el.dataset.name,glyph:el.dataset.glyph}));
    document.querySelectorAll('.arch').forEach(el=>el.onclick=(ev)=>{ ev.stopPropagation(); const p=el.closest('.wt'); vsc.postMessage({cmd:'archive',slug:p.dataset.slug,name:p.dataset.name}); });
    document.querySelectorAll('.del').forEach(el=>el.onclick=(ev)=>{ ev.stopPropagation(); const p=el.closest('.wt'); vsc.postMessage({cmd:'delete',slug:p.dataset.slug,name:p.dataset.name}); });
    document.querySelectorAll('.term').forEach(el=>el.onclick=(ev)=>{ ev.stopPropagation(); const p=el.closest('.wt'); vsc.postMessage({cmd:'terminate',slug:p.dataset.slug,name:p.dataset.name}); });
    document.querySelectorAll('.unr').forEach(el=>el.onclick=(ev)=>{ ev.stopPropagation(); const p=el.closest('.wt'); vsc.postMessage({cmd:'markunread',slug:p.dataset.slug,name:p.dataset.name}); });
  }
  function renderMonitor(){
    const el=document.getElementById('mon'); if(!el) return;
    if(!mon){ el.innerHTML='<div class="none">…</div>'; return; }
    const gb=x=>(x/1024).toFixed(1);
    // bars are AGENT-scoped: summed over every claude process tree, as a share of the box.
    const cpuPct = mon.ncpu>0?Math.min(100,Math.round(mon.acpu/mon.ncpu)):0;   // agent CPU / total cores
    const memPct = mon.mt>0?Math.min(100,Math.round(mon.amem*100/mon.mt)):0;    // agent RAM / total RAM
    const sysPct = mon.mt>0?Math.round(mon.msys*100/mon.mt):0;                  // whole-box RAM (tooltip)
    const cores  = (mon.acpu/100).toFixed(1);
    const stat=(k,v,t)=>'<span class="stat" title="'+t+'"><span class="sk">'+k+'</span><span class="sv">'+v+'</span></span>';
    const mbar=(lbl,pct,meta,title)=>
      '<div class="row" title="'+title+'"><span class="mlbl">'+lbl+'</span>'
      +'<div class="track"><div class="fill" style="width:'+pct+'%;background:'+col(pct)+'"></div></div>'
      +'<span class="pct">'+pct+'%</span><span class="meta">'+meta+'</span></div>';
    el.innerHTML =
      '<div class="statline">'
        + stat('sessions', mon.sess, 'tmux sessions running')
        + stat('agents', mon.nag, 'interactive claude agents')
        + stat('reviews', mon.rev, 'review agents running')
      +'</div>'
      + mbar('cpu', cpuPct, cores+'/'+mon.ncpu+'c', 'Claude agents: '+cores+' of '+mon.ncpu+' cores ('+cpuPct+'% of CPU) · system load '+mon.load)
      + mbar('mem', memPct, gb(mon.amem)+'G', 'Claude agents using '+gb(mon.amem)+' GB RAM ('+memPct+'% of '+gb(mon.mt)+'G) · whole system '+gb(mon.msys)+'/'+gb(mon.mt)+'G used ('+sysPct+'%)');
  }
  document.getElementById('add').onclick=()=>vsc.postMessage({cmd:'newAgent'});
  function renderTests(excluded){ const b=document.getElementById('tests'); if(!b) return;
    b.textContent = excluded ? 'tests ✕' : 'tests ✓';
    b.title = excluded ? 'Test files are EXCLUDED from the diff panes — click to include' : 'Test files are INCLUDED in the diff panes — click to exclude';
    b.style.opacity = excluded ? '.6' : '1'; }
  document.getElementById('tests').onclick=()=>vsc.postMessage({cmd:'toggleTests'});
  window.addEventListener('message', e => {
    const m=e.data; if(!m) return;
    if(m.type==='limits'){ accts=m.accounts||[]; renderLim(); }
    else if(m.type==='roster'){ ros=m.rows||[]; renderRoster(); }
    else if(m.type==='monitor'){ mon=m.m; renderMonitor(); }
    else if(m.type==='teststate'){ renderTests(m.excluded); }
  });
  setInterval(renderLim, 15000);   // keep the reset countdown ticking
</script></body></html>`;
  }
}

// Map a terminal's shell pid → the tmux session attached on its tty (so clicking a SHA in that
// terminal repaints THAT session's diff pane). The VSCode terminal's shell runs `agent` → `tmux
// attach`, so the tmux client's tty == the terminal's pty.
// the worktree directory backing a session = its commit pane's (@cpane) cwd (-s: search all the
// session's panes, across windows). Used to resolve a footer button's file to an absolute path.
function tmuxCpanePath(session) {
  return new Promise((res) => {
    cp.execFile('tmux', ['list-panes', '-s', '-t', session, '-F', '#{?#{@cpane},#{pane_current_path},}'],
      { timeout: 3000 }, (e, out) => {
        if (e) return res('');
        res((out || '').split('\n').map((s) => s.trim()).find(Boolean) || '');
      });
  });
}

function tmuxSessionForPid(pid) {
  return new Promise((res) => {
    cp.exec('ps -o tty= -p ' + pid, (e, out) => {
      const tty = (out || '').trim();                 // e.g. "pts/5"
      if (!tty) return res('');
      // space-separated (client_tty and session names contain no spaces). NOTE: tmux does NOT expand
      // \t in -F, so a tab separator would come through literal and break parsing.
      cp.exec("tmux list-clients -F '#{client_tty} #{session_name}'", (e2, out2) => {
        for (const ln of (out2 || '').split('\n')) {
          const i = ln.indexOf(' '); if (i < 0) continue;
          const ct = ln.slice(0, i), sn = ln.slice(i + 1);
          if (ct === '/dev/' + tty || ct.endsWith('/' + tty)) return res(sn);
        }
        res('');
      });
    });
  });
}

function activate(context) {
  refreshDevRoot();   // resolve the real dev base (the tree is relocatable) before anything scans it

  const provider = new ClaudeStatusProvider();
  context.subscriptions.push(vscode.window.registerFileDecorationProvider(provider));

  const watcher = vscode.workspace.createFileSystemWatcher('**/' + STATUS_FILE);
  const fire = (uri) => provider.refresh(vscode.Uri.file(path.dirname(uri.fsPath)));
  watcher.onDidCreate(fire);
  watcher.onDidChange(fire);
  watcher.onDidDelete(fire);
  context.subscriptions.push(watcher);

  const dev = new DevSummaryProvider();
  context.subscriptions.push(vscode.window.registerWebviewViewProvider('claudeStatus.limit', dev));
  // the dev base is derived from the open folders — re-resolve it (and repaint) if they change
  context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(() => { refreshDevRoot(); dev._postRoster(); }));
  // keep the roster's terminal map + unread flags in sync with the actual terminals
  context.subscriptions.push(vscode.window.onDidCloseTerminal((t) => dev.onTermClosed(t)));
  context.subscriptions.push(vscode.window.onDidChangeActiveTerminal((t) => dev.onTermActive(t)));
  // refresh the roster THE MOMENT a .claude-status changes (debounced) so unread/status flips
  // near-instantly with the bell, instead of waiting up to the 12s poll.
  let rosBump;
  const bumpRoster = () => { clearTimeout(rosBump); rosBump = setTimeout(() => dev._postRoster(), 200); };
  watcher.onDidCreate(bumpRoster);
  watcher.onDidChange(bumpRoster);
  watcher.onDidDelete(bumpRoster);

  // Clickable commit SHAs in terminal output (e.g. an agent's chat): click → repaint that session's
  // diff pane with the commit, like double-clicking a SHA in the commit pane.
  // footer buttons in the commit pane: ctrl+click opens the backing file as RAW TEXT. We do this in
  // the extension (not via a terminal path link or a tmux mouse binding) because (a) VSCode swallows
  // ctrl+click for its own link handling, so it never reaches tmux, and (b) the user maps `*.md` to
  // the markdown PREVIEW editor — showTextDocument({preview:false}) opens the source, ignoring that.
  const FILE_BUTTONS = [
    ['view_pr_notes', 'pr-notes.md', 'Open PR notes as text'],
    ['view_active_plan', '.claude/plans/active-plan.md', 'Open active plan as text'],
  ];
  // The SHA→diff-pane and footer-button links are a tmux-pane feature (they repaint a tmux diff
  // pane / resolve the commit pane's cwd). There are no tmux panes on Windows, so skip registration
  // there — VSCode's native SCM/diff and the Source Control view cover diffs instead.
  if (!IS_WIN) context.subscriptions.push(vscode.window.registerTerminalLinkProvider({
    provideTerminalLinks(ctx) {
      const links = []; const re = /\b[0-9a-f]{7,40}\b/g; let m;
      while ((m = re.exec(ctx.line)) !== null) {
        links.push({ startIndex: m.index, length: m[0].length, tooltip: 'Show commit diff', data: { sha: m[0], terminal: ctx.terminal } });
      }
      for (const [tok, rel, tip] of FILE_BUTTONS) {
        for (let i = ctx.line.indexOf(tok); i !== -1; i = ctx.line.indexOf(tok, i + tok.length)) {
          links.push({ startIndex: i, length: tok.length, tooltip: tip, data: { file: rel, terminal: ctx.terminal } });
        }
      }
      return links;
    },
    async handleTerminalLink(link) {
      try {
        const pid = await link.data.terminal.processId;
        if (!pid) return;
        const session = await tmuxSessionForPid(pid);
        if (!session) { vscode.window.showInformationMessage('claude-status: no tmux session found for this terminal'); return; }
        if (link.data.file) {
          const wt = await tmuxCpanePath(session);
          if (!wt) { vscode.window.showInformationMessage('claude-status: could not locate the worktree for this session'); return; }
          const fp = path.join(wt, link.data.file);
          if (!fs.existsSync(fp)) { vscode.window.showInformationMessage('claude-status: no file at ' + fp); return; }
          await vscode.window.showTextDocument(vscode.Uri.file(fp), { preview: false });
          return;
        }
        cp.execFile(path.join(DEV, '.wtd', 'hooks', 'diff-commit.sh'), [session, link.data.sha]);
      } catch {}
    },
  }));
}

function deactivate() {}

module.exports = { activate, deactivate };
