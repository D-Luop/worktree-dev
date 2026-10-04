const vscode = require('vscode');
const fs = require('fs');
const os = require('os');
const path = require('path');
const cp = require('child_process');
const https = require('https');
const dv = require('./diffview.js');
const { SettingsPanel } = require('./settings.js');   // the WorkTreeDev Settings editor tab   // the `commits` tab: branch commits + filterable diffs

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
const MON_SECS = 15;   // system-monitor sample interval
const GIT_SECS = 30;   // dirty/ahead refresh interval (status flips don't wait for this — they're watched)
const GIT_CONCURRENCY = 3;   // max simultaneous `git status` (15 at once saturates disk on a monorepo)
const ASST_NAME = 'assistant';   // the reserved terminal name of the pinned fleet-management session
const TERM_NAME = 'terminal';    // the reserved name of the pinned plain shell row (above assistant)
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

// TEMP diagnostic: append a line to .wtd/state/open-debug.log so we can see exactly what the open path
// does at click time (which branch, terminal name/liveness, the full terminal list). Remove once fixed.
function _dbg(msg) {
  try { fs.appendFileSync(path.join(WTD, 'state', 'open-debug.log'), new Date().toISOString() + ' ' + msg + '\n'); } catch {}
}
function _termDump() {
  try { return vscode.window.terminals.map((x) => x.name + ':' + (x.exitStatus === undefined ? 'live' : 'dead')).join(' | '); }
  catch (e) { return 'ERR ' + (e && e.message); }
}

// The name a terminal was CREATED with. Terminal.name tracks the tab title, and Claude Code rewrites
// that title as it works ("✳ Thinking…"), so matching a session by t.name silently stops working the
// moment its agent starts — the roster then can't find, focus, reap, or highlight it. creationOptions
// is fixed at creation and survives a window reload, so it's the stable identity. Falls back to
// t.name for terminals we didn't create (a user's own shell).
function termName(t) {
  try { const n = t && t.creationOptions && t.creationOptions.name; if (n) return n; } catch {}
  return (t && t.name) || '';
}

// Dispose any terminals with this name whose process has already exited (exitStatus set). A window
// reload revives editor terminal tabs but not their agent/claude process, leaving dead tabs behind;
// reaping them before re-launching keeps a stale dead tab from shadowing (or being focused instead of)
// a fresh live session — the "clicking a stopped worktree does nothing" symptom.
function disposeDeadTerminals(name) {
  for (const x of vscode.window.terminals)
    if (termName(x) === name && x.exitStatus !== undefined) { try { x.dispose(); } catch {} }
}

// Slugs registered in .wtd/repos.tsv (first tab-separated field of each non-comment line). Lets the
// roster tell a known repo from a brand-new one when you launch an agent with a slug it's never seen.
function registeredSlugs() {
  try {
    return fs.readFileSync(path.join(WTD, 'repos.tsv'), 'utf8')
      .split('\n').map((l) => l.trim()).filter((l) => l && l[0] !== '#')
      .map((l) => l.split('\t')[0]).filter(Boolean);
  } catch { return []; }
}

// Run a .wtd shell script or a shebang wrapper (archive/agent/refresh-diffs/monitor-stats). On Windows
// these aren't directly spawnable — cp.execFile throws EFTYPE — so route them through Git Bash (with
// forward-slash paths it can stat); elsewhere exec them directly. Crucially this NEVER throws
// synchronously: a spawn failure is delivered to cb, so e.g. _postMonitor can't take down the webview.
// Non-login `bash -c`: a login shell re-sources the whole profile on every call (~0.5-0.8s on Windows).
// Git's bin\bash.exe launcher already puts /usr/bin + /mingw64/bin on PATH; we add ~/.local/bin.
const LOCAL_BIN = path.join(HOME, '.local', 'bin');
// env for the bash terminals we open (sessions, assistant, dev shell): the wtd commands live in
// ~/.local/bin, which a fresh Git Bash login shell doesn't have on PATH unless the user's profile adds it.
function wtdTermEnv() {
  const pk = Object.keys(process.env).find((k) => k.toUpperCase() === 'PATH') || 'PATH';
  return { [pk]: LOCAL_BIN + path.delimiter + (process.env[pk] || '') };
}
function execScript(file, args, opts, cb) {
  const done = typeof cb === 'function' ? cb : () => {};
  try {
    if (IS_WIN) {
      const line = [file].concat(args || []).map((a) => shq(String(a).replace(/\\/g, '/'))).join(' ');
      const pk = Object.keys(process.env).find((k) => k.toUpperCase() === 'PATH') || 'PATH';   // Windows: usually "Path"
      const env = { ...process.env, [pk]: LOCAL_BIN + path.delimiter + (process.env[pk] || '') };
      return cp.execFile(bashShell(), ['-c', line], { ...opts, env }, done);
    }
    return cp.execFile(file, args || [], opts, done);
  } catch (e) { done(e); }
}

// --- wtd daemon (v2) -------------------------------------------------------------------------------
// When wtd.exe is installed, the daemon owns all background work (status, git state, usage, metrics)
// and pushes changes over a named pipe; the panel just renders them. Without it, the legacy
// in-extension scanning below is used.
const net = require('net');
function wtdExe() { return path.join(WTD, 'bin', 'wtd.exe'); }
function daemonInstalled() { try { return IS_WIN && fs.existsSync(wtdExe()); } catch { return false; } }
function pipePath() {
  const u = (process.env.USERNAME || process.env.USER || 'user').replace(/[^A-Za-z0-9_-]/g, '_');
  return '\\\\.\\pipe\\wtd-' + u;
}

class DaemonClient {
  constructor(onChange) {
    this.onChange = onChange;   // (kind) => void   kind: 'daemon' | 'fleet' | 'accounts' | 'metrics'
    this.running = false;
    this.wts = new Map();       // id → Worktree (see wtd-core model.rs)
    this.accounts = [];
    this.metrics = null;
    this.groups = [];     // user-defined roster groups, in display order
    this.messages = [];   // agent messages still in flight (pending approval / queued)
    this._sock = null; this._buf = ''; this._pending = new Map(); this._next = 1; this._retry = null; this._stopped = false;
  }
  start() { this._stopped = false; this._connect(); }
  dispose() { this._stopped = true; clearTimeout(this._retry); if (this._sock) this._sock.destroy(); }

  _connect() {
    if (this._sock || this._stopped) return;
    const s = net.connect(pipePath());
    this._sock = s;
    s.setEncoding('utf8');
    s.on('connect', () => {
      this.running = true;
      this._send('hello', { client: 'vscode', protocol: 1 });
      this._send('subscribe', { metrics: true });
      this.onChange('daemon');
    });
    s.on('data', (d) => {
      this._buf += d;
      let i;
      while ((i = this._buf.indexOf('\n')) >= 0) {
        const line = this._buf.slice(0, i); this._buf = this._buf.slice(i + 1);
        if (line.trim()) { try { this._line(JSON.parse(line)); } catch (e) { _dbg('daemon line: ' + e.message); } }
      }
    });
    const down = () => {
      if (this._sock !== s) return;
      this._sock = null; this._buf = '';
      for (const [, p] of this._pending) p.reject(new Error('daemon disconnected'));
      this._pending.clear();
      const was = this.running; this.running = false;
      if (was) this.onChange('daemon');
      clearTimeout(this._retry);
      // a failed open on a missing pipe costs microseconds — no process is spawned
      if (!this._stopped) this._retry = setTimeout(() => this._connect(), 2000);
    };
    s.on('error', down); s.on('close', down);
  }

  _send(method, params) {
    const id = this._next++;
    if (this._sock) this._sock.write(JSON.stringify({ id, method, params: params || {} }) + '\n');
    return id;
  }

  request(method, params) {
    return new Promise((resolve, reject) => {
      if (!this._sock || !this.running) return reject(new Error('daemon not running'));
      const id = this._send(method, params);
      this._pending.set(id, { resolve, reject });
    });
  }

  _line(m) {
    if (m.id !== undefined && ('result' in m || 'error' in m)) {
      const p = this._pending.get(m.id); if (!p) return;
      this._pending.delete(m.id);
      if (m.error) p.reject(new Error(m.error)); else p.resolve(m.result);
      return;
    }
    switch (m.event) {
      case 'snapshot':
        this.wts = new Map((m.snapshot.worktrees || []).map((w) => [w.id, w]));
        this.accounts = m.snapshot.accounts || [];
        this.metrics = m.snapshot.metrics || null;
        this.groups = m.snapshot.groups || [];
        this.messages = m.snapshot.messages || [];
        this.onChange('fleet'); this.onChange('accounts'); this.onChange('metrics'); this.onChange('messages');
        break;
      case 'upsert': {
        const prev = this.wts.get(m.worktree.id);
        this.wts.set(m.worktree.id, m.worktree);
        this.onChange('fleet', m.worktree, prev);
        break;
      }
      case 'remove': this.wts.delete(m.id); this.onChange('fleet'); break;
      case 'accounts': this.accounts = m.accounts || []; this.onChange('accounts'); break;
      case 'metrics': this.metrics = m.metrics; this.onChange('metrics'); break;
      case 'groups': this.groups = m.groups || []; this.onChange('fleet'); break;
      case 'messages': this.messages = m.messages || []; this.onChange('messages'); break;
      case 'shutdown': break;   // the pipe closes next → 'daemon' change
    }
  }
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
    this._postAll = null;         // set in resolveWebviewView; replayed on the webview's 'ready' handshake
    this._terms = new Map();      // worktree key -> Terminal we opened (to focus instead of duplicate)
    this._lastStatus = {};        // worktree key -> last status seen (to detect transitions)
    this._unread = {};            // worktree key -> true when it flipped to "your turn" and not yet opened
    this._current = null;         // the Terminal of the worktree session currently focused (marked in roster)
    this._asstTerm = null;        // the pinned assistant session's Terminal (focus instead of duplicate)
    this._term = null;            // the pinned plain terminal's Terminal (focus instead of duplicate)
    this._preview = {};           // roster key -> { label: staged-.html-path } (multiple previews/worktree)
    this._pvPanel = null;         // the design-preview WebviewPanel (follows the focused worktree)
    this._pvShownKey = null;      // roster key currently displayed in that panel
    this._pvLabel = null;         // which preview label (tab) is currently rendered
    this._pvLabelByKey = {};      // remembered tab selection per worktree key
    this._pvFollow = false;       // true once the user engages a preview → panel tracks focus
    this._pvAutoClosing = false;  // transient: distinguish a follow-driven close from a user close
    this._pvTimer = null;         // poll so an open panel live-refreshes when its files change
    this._pvSig = '';             // signature (labels+mtimes) of the last render, to detect changes
    this._pvTabs = '';            // '|'-joined tab labels of the last render (commits tab re-render trigger)
    this._cvSel = {};             // roster key -> selected commit sha in the `commits` tab (survives re-render)
    this._cvWidth = 0;            // commit-list width the user dragged to (0 = the tab's default)
  }
  _wtPath(slug, name) { return path.join(DEV, 'worktrees', slug, name); }
  _key(slug, name) { return slug + '' + name; }
  clearUnread(key) { if (this._unread[key]) { this._unread[key] = false; this._postRoster(); } }
  markUnread(key) { if (!this._unread[key]) { this._unread[key] = true; this._postRoster(); } }
  // The Claude account a worktree session currently runs under: its durable binding (set by
  // `account switch`) if any, else 'default'. Mirrors agent.sh's resolution for display/target-picking.
  // Which account a session is running under. The binding file is authoritative (agent.sh writes it on
  // every launch), but sessions started before that existed have none — infer those from where Claude
  // is actually writing their transcript, rather than assuming 'default' and offering to "switch" a
  // session to the account it's already on.
  _bindingAccount(slug, name) {
    const file = (slug + '-' + name).replace(/[.:]/g, '-').replace(/\//g, '__');
    try {
      const v = fs.readFileSync(path.join(WTD, 'state', 'session-accounts', file), 'utf8').trim();
      if (v) return v;
    } catch {}
    return this._inferAccount(slug, name, file) || 'default';
  }

  // Claude stores a session's transcript under <CLAUDE_CONFIG_DIR>/projects/<mangled cwd>/<id>.jsonl.
  // A worktree that has run under two accounts has a copy in each, so take the freshest — the stale
  // one is from before the switch.
  _inferAccount(slug, name, file) {
    let id = '';
    try { id = fs.readFileSync(path.join(WTD, 'state', 'session-ids', file), 'utf8').trim(); } catch {}
    if (!id) return null;
    const enc = this._wtPath(slug, name).replace(/\//g, '\\').replace(/[^A-Za-z0-9]/g, '-');
    let best = null, bestM = -1;
    for (const a of this._accounts()) {
      try {
        const m = fs.statSync(path.join(a.dir, 'projects', enc, id + '.jsonl')).mtimeMs;
        if (m > bestM) { bestM = m; best = a.name; }
      } catch {}
    }
    return best;
  }

  // max(5h, 7d) utilization % for an account, from the panel's cached usage poll (null = unknown)
  _utilOf(accName) {
    const u = this._lastUsage[accName];
    if (!u) return null;
    return Math.round(Math.max((u.five_hour && u.five_hour.used) || 0, (u.seven_day && u.seven_day.used) || 0));
  }
  // Pick the switch target: the logged-in account with the MOST remaining capacity (lowest max 5h/7d
  // utilization), excluding the current one and any fully-maxed. Uses the usage the panel already
  // polled (this._lastUsage) so we don't re-hit the rate-limited usage endpoint. null if none qualify.
  _pickSwitchTarget(currentName) {
    let best = null, bestUtil = 101;
    for (const a of this._accounts()) {
      if (a.name === currentName) continue;
      let email = '', tok = '';
      try { email = (JSON.parse(fs.readFileSync(a.json, 'utf8')).oauthAccount || {}).emailAddress || ''; } catch {}
      try { tok = (JSON.parse(fs.readFileSync(path.join(a.dir, '.credentials.json'), 'utf8')).claudeAiOauth || {}).accessToken || ''; } catch {}
      if (!email || !tok) continue;                       // not logged in → skip
      const util = this._utilOf(a.name) ?? 0;             // unknown usage → assume fresh
      if (util >= 100) continue;                          // fully maxed → skip
      if (util < bestUtil) { bestUtil = util; best = a.name; }
    }
    return best;
  }
  // Roster ⇄ action: move this session to the account with the most capacity, then reopen it there and
  // compact. Claude can't hot-swap accounts in place, so `account switch` copies the transcript into the
  // target account's store + records the binding; we then relaunch (agent resumes under the new account)
  // and auto-/compact to shrink context. Compaction runs on the TARGET account because the source is
  // typically exhausted (can't compact) — end state is: new account, compacted chat.
  switchAccount(slug, name) {
    const cur = this._bindingAccount(slug, name);
    const target = this._pickSwitchTarget(cur);
    if (!target) {
      vscode.window.showWarningMessage('No other logged-in account with remaining capacity to switch to. Add one with `account add <name>` (then log in), or check `account ls`.');
      return;
    }
    const pct = (n) => { const u = this._utilOf(n); return u === null ? 'usage unknown' : u + '% used'; };
    vscode.window.showWarningMessage(
      'Switch ' + slug + ' ' + name + " from '" + cur + "' to '" + target + "'?",
      { modal: true, detail: cur + ': ' + pct(cur) + '  →  ' + target + ': ' + pct(target)
        + '\n\nThe session reopens under ' + target + " (same conversation, via transcript copy) and auto-compacts to shrink context. Claude can't swap accounts in place, so the terminal restarts." },
      'Switch account'
    ).then((ch) => {
      if (ch !== 'Switch account') return;
      execScript(path.join(HOME, '.local', 'bin', 'account'), ['switch', slug, name, '--to', target], { timeout: 60000 }, (e, so, se) => {
        const out = ((se || '') + (so || '')).trim();
        if (e) { vscode.window.showErrorMessage('account switch failed: ' + (out || e.message)); return; }
        vscode.window.showInformationMessage('Switched ' + name + " → '" + target + "'. Reopening under it and compacting…");
        this._relaunchAndCompact(slug, name);
      });
    });
  }
  // Kill the session's terminal and reopen it (agent.sh now reads the new binding → resumes under the
  // target account), then send /compact once it's back up.
  _relaunchAndCompact(slug, name) {
    const key = this._key(slug, name);
    const t = this._terms.get(key) || vscode.window.terminals.find((x) => termName(x) === name);
    if (t) { try { t.dispose(); } catch {} this._terms.delete(key); }
    setTimeout(() => {
      this.openOrFocus(slug, name);
      setTimeout(() => { const nt = this._terms.get(key); if (nt && nt.exitStatus === undefined) nt.sendText('/compact', true); }, 7000);
    }, 1400);
  }
  // a roster row was opened: focus the existing terminal if we have one (incl. a reload-revived tab
  // matched by name), otherwise launch a new one.
  openOrFocus(slug, name, glyph) {
    const key = this._key(slug, name);
    let branch = '?';
    try {
    _dbg(`OPEN slug=${JSON.stringify(slug)} name=${JSON.stringify(name)} | terms=[${_termDump()}]`);
    let t = this._terms.get(key);
    const inMap = !!t;
    // Only reuse a LIVE terminal. After a window reload the tab may be revived but its agent/claude
    // process already exited (exitStatus set) — reusing it would focus a dead tab that never relaunches.
    if (!t || t.exitStatus !== undefined) {
      t = vscode.window.terminals.find((x) => termName(x) === name && x.exitStatus === undefined);
    }
    if (t && t.exitStatus === undefined) { branch = inMap ? 'reuse-map' : 'reuse-byname'; t.show(); }
    else {
      branch = 'create';
      disposeDeadTerminals(name);   // reap reload-orphaned dead tabs so they can't be focused instead
      const nm = name;   // tab = worktree name only (no slug, no status glyph — status shows in the roster)
      t = vscode.window.createTerminal({ name: nm, location: vscode.TerminalLocation.Editor,
        env: wtdTermEnv(), shellPath: bashShell(), shellArgs: ['-lc', 'agent ' + shq(slug) + ' ' + shq(name)] });
      t.show();
    }
    this._terms.set(key, t);
    this._current = t;            // the just-opened session is now the selected one
    this.clearUnread(key);
    _dbg(`  -> branch=${branch} tname=${JSON.stringify(t && t.name)} exit=${t && t.exitStatus} shell=${JSON.stringify(bashShell())}`);
    setTimeout(() => this._postRoster(), 1500);
    } catch (e) {
      _dbg(`  -> THREW branch=${branch}: ${e && (e.stack || e.message)}`);
      vscode.window.showErrorMessage('claude-status open failed (' + slug + ' ' + name + '): ' + (e && e.message));
    }
  }
  // Open (or focus) the pinned fleet assistant: one durable Claude session in the dev base that
  // manages worktrees via the wtd PATH commands. Mirrors openOrFocus but with a reserved name and
  // no slug/worktree of its own; `assistant` (the PATH command) handles resume.
  openOrFocusAssistant() {
    let t = this._asstTerm;
    if (!t || t.exitStatus !== undefined) t = vscode.window.terminals.find((x) => termName(x) === ASST_NAME && x.exitStatus === undefined);
    if (t && t.exitStatus === undefined) { t.show(); }
    else {
      disposeDeadTerminals(ASST_NAME);
      t = vscode.window.createTerminal({ name: ASST_NAME, location: vscode.TerminalLocation.Editor,
        env: wtdTermEnv(), shellPath: bashShell(), shellArgs: ['-lc', 'assistant'] });
      t.show();
    }
    this._asstTerm = t; this._current = t;
    this._unread[ASST_NAME] = false;   // opening/focusing the assistant clears its unread (like a worktree)
    setTimeout(() => this._postRoster(), 800);
  }

  // Open (or focus) the pinned plain terminal: an ordinary login shell in the dev base for quick
  // ad-hoc commands. Like the assistant row, but runs no Claude session — just an interactive shell.
  openOrFocusTerminal() {
    let t = this._term;
    if (!t || t.exitStatus !== undefined) t = vscode.window.terminals.find((x) => termName(x) === TERM_NAME && x.exitStatus === undefined);
    if (t && t.exitStatus === undefined) { t.show(); }
    else {
      disposeDeadTerminals(TERM_NAME);
      t = vscode.window.createTerminal({ name: TERM_NAME, location: vscode.TerminalLocation.Editor,
        env: wtdTermEnv(), shellPath: bashShell(), shellArgs: ['-l', '-i'] });
      t.show();
    }
    this._term = t; this._current = t;
    setTimeout(() => this._postRoster(), 800);
  }

  // Add an image to the focused session without the broken native-Windows clipboard paste: grab the
  // clipboard image (else let the user pick a file), stage it to a space-free path under .wtd/state,
  // and INSERT that absolute path into the terminal (no Enter). Claude Code auto-detects the image
  // path on submit and loads the picture — see the file-path image-input mechanism.
  pasteImage() {
    const target = vscode.window.activeTerminal || this._current;
    if (!target) { vscode.window.showInformationMessage('claude-status: open a session first, then add an image.'); return; }
    const dir = path.join(WTD, 'state', 'pasted-images');
    try { fs.mkdirSync(dir, { recursive: true }); } catch {}
    const insert = (file) => { target.show(); target.sendText((IS_WIN ? file.replace(/\//g, '\\') : file) + ' ', false); };
    const pick = () => {
      vscode.window.showOpenDialog({ canSelectMany: false, openLabel: 'Add image',
        filters: { Images: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'bmp'] } }).then((uris) => {
        if (!uris || !uris.length) return;
        const src = uris[0].fsPath;
        // copy into the space-free staging dir so the inserted path can't trip Claude's path auto-detect
        const staged = path.join(dir, 'img-' + Date.now() + (path.extname(src) || '.png'));
        try { fs.copyFileSync(src, staged); insert(staged); }
        catch (e) { vscode.window.showErrorMessage('claude-status: image copy failed: ' + e.message); }
      });
    };
    if (!IS_WIN) return pick();
    // Windows: try the clipboard image first (the case native paste can't handle); else pick a file.
    const dest = path.join(dir, 'img-' + Date.now() + '.png');
    const psPath = dest.replace(/\//g, '\\').replace(/'/g, "''");
    const ps = "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; $i=[System.Windows.Forms.Clipboard]::GetImage(); if($i){ $i.Save('" + psPath + "',[System.Drawing.Imaging.ImageFormat]::Png); $i.Dispose(); exit 0 } else { exit 3 }";
    cp.execFile('powershell.exe', ['-NoProfile', '-STA', '-Command', ps], { timeout: 8000 }, (e) => {
      if (!e) insert(dest);   // clipboard held an image → staged + inserted
      else pick();            // nothing on the clipboard → fall back to a file picker
    });
  }

  // Ctrl+V into a focused session: paste TEXT normally, but if the clipboard holds an IMAGE (and no
  // text), stage it to a space-free path and INSERT that path so Claude Code loads the picture — the
  // same trick as the 📷 button, minus the file-picker fallback. Bound to ctrl+v when a terminal is
  // focused (Windows only; tmux/native paste handles it elsewhere). We check clipboard text first so
  // the common text paste stays instant and never spawns powershell.
  async smartPaste() {
    const normalPaste = () => vscode.commands.executeCommand('workbench.action.terminal.paste');
    let text = '';
    try { text = await vscode.env.clipboard.readText(); } catch {}
    if (text) return normalPaste();          // any text on the clipboard → ordinary paste
    if (!IS_WIN) return normalPaste();
    const target = vscode.window.activeTerminal || this._current;
    if (!target) return normalPaste();
    const dir = path.join(WTD, 'state', 'pasted-images');
    try { fs.mkdirSync(dir, { recursive: true }); } catch {}
    const dest = path.join(dir, 'img-' + Date.now() + '.png');
    const psPath = dest.replace(/\//g, '\\').replace(/'/g, "''");
    const ps = "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; $i=[System.Windows.Forms.Clipboard]::GetImage(); if($i){ $i.Save('" + psPath + "',[System.Drawing.Imaging.ImageFormat]::Png); $i.Dispose(); exit 0 } else { exit 3 }";
    cp.execFile('powershell.exe', ['-NoProfile', '-STA', '-Command', ps], { timeout: 8000 }, (e) => {
      if (!e) { target.show(); target.sendText(dest.replace(/\//g, '\\') + ' ', false); }  // image → insert path
      else normalPaste();   // nothing pasteable as an image → ordinary paste
    });
  }

  // map a staged preview file (…/state/previews/<slug>/<name…>/<label>.html) back to slug/name/label.
  // name may contain '/', so: first segment = slug, last = label, the middle = name. Needs ≥3 segments
  // (old single-file layout has ≤2 and is ignored).
  _previewKey(fsPath) {
    let rel = path.relative(path.join(WTD, 'state', 'previews'), fsPath).replace(/\\/g, '/');
    if (rel.startsWith('..')) return null;
    rel = rel.replace(/\.html$/i, '');
    const segs = rel.split('/');
    if (segs.length < 3) return null;
    const slug = segs.shift(), label = segs.pop(), name = segs.join('/');
    if (!slug || !name || !label) return null;
    return { slug, name, label };
  }

  // read all preview tabs for a worktree straight from disk into this._preview[key]; returns {label:path}
  _scanPreviews(slug, name) {
    const key = this._key(slug, name);
    const dir = path.join(WTD, 'state', 'previews', slug, name);
    const out = {};
    try { for (const e of fs.readdirSync(dir, { withFileTypes: true }))
      if (e.isFile() && /\.html$/i.test(e.name)) out[e.name.replace(/\.html$/i, '')] = path.join(dir, e.name); } catch {}
    if (Object.keys(out).length) this._preview[key] = out; else delete this._preview[key];
    return out;
  }
  // tab order: the living plan first, then alphabetical
  _previewLabels(previews) {
    return Object.keys(previews).sort((a, b) => (a === 'plan' ? -1 : b === 'plan' ? 1 : a.localeCompare(b)));
  }
  // Every git worktree gets a built-in `commits` tab (the diff viewer) after its staged previews, so
  // the panel has something to show even when no agent has staged a design.
  _tabs(slug, name, previews) {
    const labels = this._previewLabels(previews);
    if (dv.isGitWorktree(this._wtPath(slug, name))) labels.push(dv.COMMITS_LABEL);
    return labels;
  }
  // which tab to show: explicit request → remembered selection → 'plan' → first
  _pickLabel(key, want, previews, labels) {
    if (want && labels.includes(want)) return want;
    const rem = this._pvLabelByKey[key];
    if (rem && labels.includes(rem)) return rem;
    if (previews.plan) return 'plan';
    return labels[0] || null;
  }
  // does this worktree have anything the panel can render? (a staged preview, or a git repo → commits)
  _hasPanelContent(key) {
    if (Object.keys(this._preview[key] || {}).length) return true;
    const i = key.indexOf('\x01'); if (i < 0) return false;
    return dv.isGitWorktree(this._wtPath(key.slice(0, i), key.slice(i + 1)));
  }
  _previewSig(previews) {
    return this._previewLabels(previews).map((l) => {
      let m = 0; try { m = fs.statSync(previews[l]).mtimeMs; } catch {} return l + ':' + m;
    }).join('|');
  }
  _pvHtmlEsc(s) { return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;'); }

  // open (or refresh) the design preview for a worktree in an editor-side webview panel. The HTML is
  // agent-authored, so it renders under a CSP: inline styles/scripts and data/https images only — no
  // network fetch. Self-contained mockups (inline CSS, data-URI images) are the intended input.
  showPreview(slug, name, label, reveal = true) {
    const key = this._key(slug, name);
    const previews = this._scanPreviews(slug, name);
    const labels = this._tabs(slug, name, previews);
    if (!labels.length) { vscode.window.showInformationMessage('claude-status: nothing to show for ' + slug + ' ' + name); return; }
    const chosen = this._pickLabel(key, label, previews, labels);
    let raw;
    if (chosen === dv.COMMITS_LABEL) {
      let hideTests = false; try { hideTests = fs.existsSync(TESTS_FLAG); } catch {}
      raw = dv.commitsHtml(slug, name, hideTests, this._cvWidth);   // skeleton; commits arrive over postMessage
    } else {
      try { raw = fs.readFileSync(previews[chosen], 'utf8'); } catch { return; }
    }
    this._pvFollow = true;   // engaging a preview → the panel now tracks the focused worktree
    if (!this._pvPanel) {
      this._pvPanel = vscode.window.createWebviewPanel('claudeStatus.preview', 'Design preview',
        { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
        { enableScripts: true, retainContextWhenHidden: true });
      this._pvPanel.onDidDispose(() => {
        this._pvPanel = null; this._pvShownKey = null;
        if (this._pvTimer) { clearInterval(this._pvTimer); this._pvTimer = null; }   // stop the refresh poll
        if (this._pvAutoClosing) this._pvAutoClosing = false;   // we closed it to follow focus → keep following
        else this._pvFollow = false;                            // the user closed it → stop following
      });
      this._pvPanel.webview.onDidReceiveMessage((msg) => {
        if (!msg) return;
        if (msg.cmd === 'close' && this._pvPanel) this._pvPanel.dispose();
        else if (msg.cmd === 'planAction' && msg.title) this.startPlanItem(msg.id || '', msg.title);
        else if (msg.cmd === 'pvSwitch' && msg.label && this._pvShownKey) {   // switcher tab clicked
          const j = this._pvShownKey.indexOf('\x01');
          if (j >= 0) this.showPreview(this._pvShownKey.slice(0, j), this._pvShownKey.slice(j + 1), msg.label);
        }
        else if (msg.cmd === 'cvReady' && msg.slug) this._cvList(msg.slug, msg.name);
        else if (msg.cmd === 'cvSelect' && msg.slug && msg.sha) this._cvDiff(msg.slug, msg.name, msg.sha);
        else if (msg.cmd === 'cvWidth' && msg.w) this._cvWidth = msg.w;   // remember the dragged divider
      });
    }
    const csp = '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; '
      + 'img-src data: https:; style-src \'unsafe-inline\' https:; font-src data: https:; script-src \'unsafe-inline\';">'
      + '<style>body{padding-top:44px !important}</style>';   // clear the fixed switcher bar
    // Fixed switcher bar at the very top: one button per preview this worktree has staged (living plan
    // first), the active one highlighted, plus Close on the right. acquireVsCodeApi is one-shot per
    // webview — grab it once and wire the tab switch, any ".go" (▶ Start) plan buttons, and Close.
    const tab = (l) => '<button class="__wttab" data-label="' + this._pvHtmlEsc(l) + '" '
      + 'style="cursor:pointer;white-space:nowrap;border-radius:6px;padding:3px 11px;font:600 12px system-ui;border:1px solid '
      + (l === chosen ? '#2f81f7;background:#1f6feb;color:#fff' : '#30363d;background:#161b22;color:#adbac7') + ';">'
      + this._pvHtmlEsc(l) + '</button>';
    const bar = '<div id="__wtbar" style="position:fixed;top:0;left:0;right:0;z-index:2147483647;display:flex;align-items:center;'
      + 'gap:6px;padding:6px 10px;background:#0d1117;border-bottom:1px solid #30363d;overflow-x:auto;">'
      + labels.map(tab).join('') + '<span style="flex:1"></span>'
      + '<div id="__wtclose" title="Close preview" style="cursor:pointer;white-space:nowrap;background:#21262d;color:#e6edf3;'
      + 'border:1px solid #444c56;border-radius:6px;padding:3px 11px;font:600 12px system-ui;">✕ Close</div></div>'
      // acquireVsCodeApi is one-shot per webview; the commits tab needs it too, so both go via __wtapi
      + '<script>(function(){var v=window.__wtapi||(window.__wtapi=acquireVsCodeApi());'
      + 'var b=document.getElementById("__wtclose");if(b)b.addEventListener("click",function(){v.postMessage({cmd:"close"});});'
      + 'document.addEventListener("click",function(e){'
      + 'var tb=e.target.closest&&e.target.closest(".__wttab");'
      + 'if(tb){e.preventDefault();v.postMessage({cmd:"pvSwitch",label:tb.getAttribute("data-label")});return;}'
      + 'var g=e.target.closest&&e.target.closest(".go");if(!g)return;e.preventDefault();e.stopPropagation();'
      + 'var it=g.closest(".item");if(!it)return;var idn=it.querySelector(".id"),tn=it.querySelector(".ttl");'
      + 'v.postMessage({cmd:"planAction",id:((idn&&idn.textContent)||"").trim(),title:((tn&&tn.textContent)||"").trim()});'
      + 'g.classList.add("go-fired");setTimeout(function(){g.classList.remove("go-fired");},1200);});'
      // preserve scroll position across live re-renders (setState survives an html swap on the same panel)
      + 'window.addEventListener("scroll",function(){try{v.setState({y:window.scrollY});}catch(e){}},{passive:true});'
      + 'try{var st=v.getState&&v.getState();if(st&&st.y)window.scrollTo(0,st.y);}catch(e){}'
      + '})();</script>';
    let html = /<head[^>]*>/i.test(raw) ? raw.replace(/<head[^>]*>/i, (m) => m + csp) : csp + raw;
    html = /<\/body>/i.test(html) ? html.replace(/<\/body>/i, bar + '</body>') : html + bar;
    this._pvPanel.title = slug + '/' + name + ' — ' + chosen;
    this._pvPanel.webview.html = html;
    this._pvShownKey = key; this._pvLabel = chosen; this._pvLabelByKey[key] = chosen;
    this._pvSig = this._previewSig(previews);
    this._pvTabs = labels.join('|');
    // poll disk so an OPEN panel live-refreshes when files change (new tab / re-stage) — VSCode's file
    // watcher misses the .wtd/state copy on Windows, so don't depend on it. Cleared on dispose.
    if (!this._pvTimer) this._pvTimer = setInterval(() => this._pollPreview(), 1000);
    if (reveal) this._pvPanel.reveal(vscode.ViewColumn.Beside, true);
  }

  // refresh poll: re-scan the shown worktree's preview dir; if a tab was added/removed or the shown
  // file changed, re-render in place (no reveal → no focus steal; injected script restores scroll).
  // The commits tab is git-backed, not file-backed: a preview re-stage must not blow away the diff
  // you're reading, so it only re-renders when the tab set itself changes.
  _pollPreview() {
    if (!this._pvPanel || !this._pvShownKey) return;
    const i = this._pvShownKey.indexOf('\x01'); if (i < 0) return;
    const slug = this._pvShownKey.slice(0, i), name = this._pvShownKey.slice(i + 1);
    const previews = this._scanPreviews(slug, name);
    if (this._pvLabel === dv.COMMITS_LABEL) {
      const tabs = this._tabs(slug, name, previews).join('|');
      if (tabs !== this._pvTabs) this.showPreview(slug, name, this._pvLabel, false);
      return;
    }
    if (!Object.keys(previews).length) return;   // keep last content if the dir momentarily empties
    if (this._previewSig(previews) !== this._pvSig) this.showPreview(slug, name, this._pvLabel, false);
  }

  // git is async, so a reply can arrive after the panel has switched tab or worktree — drop it rather
  // than paint one worktree's commits over another's.
  _cvPost(key, payload) {
    if (!this._pvPanel || this._pvLabel !== dv.COMMITS_LABEL || this._pvShownKey !== key) return;
    this._pvPanel.webview.postMessage(payload);
  }

  // the commits tab asked for its branch history (sent on load)
  _cvList(slug, name) {
    const wt = this._wtPath(slug, name);
    if (!dv.isGitWorktree(wt)) return;
    const key = this._key(slug, name);
    dv.listCommits(wt, (res) => this._cvPost(key, Object.assign({ type: 'cvList', selected: this._cvSel[key] || '' }, res)));
  }

  // the commits tab asked for one commit's patch (or the working tree, sha === dv.WORKING)
  _cvDiff(slug, name, sha) {
    const wt = this._wtPath(slug, name);
    if (!dv.isGitWorktree(wt)) return;
    const key = this._key(slug, name);
    this._cvSel[key] = sha;   // so switching to a design tab and back reopens the same commit
    dv.commitDiff(wt, sha, (res) => this._cvPost(key, Object.assign({ type: 'cvDiff' }, res)));
  }

  // A ▶ Start button in the living-plan preview was clicked: hand that step to the worktree the panel
  // is currently showing (_pvShownKey). Send a prompt into its Claude session — open the session first
  // if it isn't live yet (then wait for Claude to boot before typing).
  startPlanItem(id, title) {
    const key = this._pvShownKey; if (!key) return;
    const i = key.indexOf('\x01'); if (i < 0) return;
    const slug = key.slice(0, i), name = key.slice(i + 1);
    const label = (id ? id + ': ' : '') + title;
    const prompt = 'Start plan item ' + label
      + ' — when it lands, tick it in .claude/plans/active-plan.html and re-run `preview` to refresh the plan.';
    const send = (term) => { term.show(); term.sendText(prompt, true); this._current = term; };
    let t = this._terms.get(key);
    if (!t || t.exitStatus !== undefined) t = vscode.window.terminals.find((x) => termName(x) === name && x.exitStatus === undefined);
    if (t && t.exitStatus === undefined) { this._terms.set(key, t); send(t); }
    else {
      this.openOrFocus(slug, name);                     // launches `agent <slug> <name>`
      const created = this._terms.get(key);
      if (created) setTimeout(() => send(created), 2500);   // give Claude a moment to come up before typing
    }
    setTimeout(() => this._postRoster(), 1500);
  }

  _showPreviewByKey(key) { const i = key.indexOf('\x01'); if (i >= 0) this.showPreview(key.slice(0, i), key.slice(i + 1)); }

  // the worktree key of the currently-focused session (null for assistant/terminal/non-worktree)
  _focusedWorktreeKey() {
    const cur = this._current || vscode.window.activeTerminal;
    if (!cur || termName(cur) === ASST_NAME || termName(cur) === TERM_NAME) return null;
    for (const [k, v] of this._terms) if (v === cur) return k;
    for (const k of Object.keys(this._preview)) if (k.split('\x01')[1] === termName(cur)) return k;   // reload-revived
    return null;
  }

  // header 🖼 button: open the panel for the worktree you're focused on (then it follows focus). Every
  // git worktree has at least the `commits` tab, so this only fails on a non-worktree focus.
  previewFocused() {
    const key = this._focusedWorktreeKey();
    if (key && this._hasPanelContent(key)) this._showPreviewByKey(key);
    else vscode.window.showInformationMessage('claude-status: focus a worktree session first — the panel shows its commits, plus any design an agent staged with `preview <file> [label]`.');
  }

  // Keep the preview panel reflecting the focused worktree: show that worktree's staged preview, hide
  // when focus is on the assistant/terminal or a worktree without one, re-show on return. Only active
  // once the user has engaged a preview (clicked a 🖼); driven by terminal-focus changes, not the poll
  // (so clicking a 🖼 from another tab doesn't immediately close the preview it just opened).
  _syncPreviewPanel(t) {
    if (!this._pvFollow) return;
    let want = null;
    if (t && termName(t) !== ASST_NAME && termName(t) !== TERM_NAME) {
      for (const [k, v] of this._terms) if (v === t && this._hasPanelContent(k)) { want = k; break; }
      if (!want) for (const k of Object.keys(this._preview)) if (k.split('\x01')[1] === termName(t)) { want = k; break; }
    }
    if (want === this._pvShownKey) return;
    if (want) this._showPreviewByKey(want);
    else if (this._pvPanel) { this._pvAutoClosing = true; this._pvPanel.dispose(); }
  }

  onTermClosed(t) { if (this._current === t) { this._current = null; this._postRoster(); } if (this._asstTerm === t) this._asstTerm = null; if (this._term === t) this._term = null; for (const [k, v] of this._terms) if (v === t) { this._terms.delete(k); break; } }
  onTermActive(t) {
    if (!t) return;
    this._current = t; setTimeout(() => this._postRoster(), 0);   // mark the focused session as selected
    this._syncPreviewPanel(t);   // make the design-preview panel follow the worktree you switched to
    for (const [k, v] of this._terms) if (v === t) { this.clearUnread(k); return; }
    // also match a reload-revived terminal by name
    for (const k of Object.keys(this._unread)) { const name = k.split('')[1]; if (this._unread[k] && termName(t) === name) { this.clearUnread(k); return; } }
  }

  // "New session": the toolbar button and the `WorkTreeDev: New Session` palette command. With wtd.exe
  // installed it's the guided picker; without it, the original free-text prompt.
  newAgent() {
    if (daemonInstalled()) return newSessionWizard(this).catch((e) => vscode.window.showErrorMessage('New session: ' + e.message));
    return this._legacyNewAgent();
  }

  _legacyNewAgent() {
    vscode.window.showInputBox({
      prompt: 'New agent — enter: <slug> <name> [ref-tokens…]   (slug "plan" = a repo-less planning agent)',
      placeHolder: 'plan my-new-app    ·    <slug> feat/my-thing',
    }).then((v) => {
      if (!v || !v.trim()) return;
      const raw = v.trim();
      const toks = raw.split(/\s+/);
      const slug = toks[0], name = toks[1] || '';
      const launch = () => {
        const nm = name || slug || 'agent';   // tab = the <name> token (no slug)
        const t = vscode.window.createTerminal({ name: nm, location: vscode.TerminalLocation.Editor,
          env: wtdTermEnv(), shellPath: bashShell(), shellArgs: ['-lc', 'agent ' + raw] });
        t.show();
        setTimeout(() => this._postRoster(), 2500);
      };
      // 'plan' is the reserved repo-less planning slug; a registered slug launches straight away.
      // A brand-new slug (with a name to open) offers to create the repo for a new application.
      const known = new Set(registeredSlugs());
      if (slug === 'plan' || known.has(slug) || !name) { launch(); return; }
      vscode.window.showWarningMessage(
        "'" + slug + "' isn't a registered repo. Create it for a new application?",
        { modal: true, detail: 'New empty repo: a fresh local git repo (no remote yet) — start scaffolding immediately, add a GitHub remote later.\nClone from URL: bare-clone an existing remote.\nOr use the reserved "plan" slug for a repo-less planning agent.' },
        'New empty repo', 'Clone from URL…'
      ).then((ch) => {
        const addRepo = path.join(HOME, '.local', 'bin', 'add-repo');
        if (ch === 'New empty repo') {
          execScript(addRepo, ['--new', slug], { timeout: 30000 }, (e, so, se) => {
            if (e) { vscode.window.showErrorMessage('create repo failed: ' + ((se || '').trim() || e.message)); return; }
            launch();
          });
        } else if (ch === 'Clone from URL…') {
          vscode.window.showInputBox({ prompt: 'Git URL to clone for "' + slug + '"', placeHolder: 'https://github.com/you/repo.git' })
            .then((url) => {
              if (!url || !url.trim()) return;
              execScript(addRepo, [slug, url.trim()], { timeout: 120000 }, (e, so, se) => {
                if (e) { vscode.window.showErrorMessage('clone failed: ' + ((se || '').trim() || e.message)); return; }
                launch();
              });
            });
        }
      });
    });
  }

  // user-defined roster groups (stored by the daemon). Names come from native input boxes.
  async _groupOp(m) {
    const d = this.daemon;
    if (!d || !d.running) { vscode.window.showWarningMessage('Start the daemon to edit groups.'); return; }
    const groups = d.groups || [];
    const nameOf = (id) => (groups.find((g) => g.id === id) || {}).name || '';
    const ask = (value, title) => vscode.window.showInputBox({ title, value, prompt: 'Group name', validateInput: (v) => (v.trim() && v.trim().length <= 60) ? null : '1–60 characters' });
    switch (m.cmd) {
      case 'groupNew': {
        const name = await ask('', 'New group'); if (!name) return;
        const r = await d.request('group.create', { name: name.trim() });
        if (m.worktree) await d.request('group.assign', { worktree: m.worktree, group: r.id });
        return;
      }
      case 'groupRename': {
        const name = await ask(nameOf(m.id), 'Rename group'); if (!name) return;
        return d.request('group.update', { id: m.id, name: name.trim() });
      }
      case 'groupDelete': {
        const ch = await vscode.window.showWarningMessage('Delete the group "' + nameOf(m.id) + '"?', { modal: true, detail: 'Its worktrees move to Ungrouped. Nothing else changes.' }, 'Delete group');
        if (ch === 'Delete group') return d.request('group.delete', { id: m.id });
        return;
      }
      case 'groupCollapse': return d.request('group.update', { id: m.id, collapsed: !!m.collapsed });
      case 'groupAssign': return d.request('group.assign', { worktree: m.worktree, group: m.group || null });
      case 'groupReorder': return d.request('group.reorder', { ids: m.ids || [] });
      case 'groupMove': {
        const wt = d.wts.get(m.worktree);
        const items = groups.map((g) => ({ label: '$(folder) ' + g.name, id: g.id, description: wt && wt.group === g.id ? 'current' : '' }));
        items.push({ label: '$(circle-outline) Ungrouped', id: null, description: wt && !wt.group ? 'current' : '' });
        items.push({ label: '$(new-folder) New group…', id: '__new__' });
        const it = await vscode.window.showQuickPick(items, { placeHolder: 'Move ' + m.worktree + ' to…' });
        if (!it) return;
        if (it.id === '__new__') return this._groupOp({ cmd: 'groupNew', worktree: m.worktree });
        return d.request('group.assign', { worktree: m.worktree, group: it.id });
      }
    }
  }

  // the toolbar's ⋯ menu: occasional actions, kept out of the main row
  moreMenu() {
    let testsExcluded = false; try { testsExcluded = fs.existsSync(TESTS_FLAG); } catch {}
    let muted = false;
    try { const tb = vscode.workspace.getConfiguration('accessibility.signals').get('terminalBell') || {}; muted = (tb.sound || 'auto') === 'off'; } catch {}
    const items = [
      { label: '$(settings-gear) Settings', description: 'repos, accounts, GitHub, daemon', run: () => this._openSettings && this._openSettings() },
      { label: '$(terminal) Open terminal', description: 'a shell in the dev base', run: () => this.openOrFocusTerminal() },
      { label: '$(hubot) Open assistant', description: 'fleet-management session', run: () => this.openOrFocusAssistant() },
      { label: '$(device-camera) Add image to focused session', description: 'clipboard screenshot or a file', run: () => this.pasteImage() },
      { label: '$(file-media) Open design preview', description: 'of the focused worktree', run: () => this.previewFocused() },
      { label: '$(beaker) Test files in diffs: ' + (testsExcluded ? 'excluded' : 'included'), description: 'click to ' + (testsExcluded ? 'include' : 'exclude'),
        run: () => this._onMsg && this._onMsg({ cmd: 'toggleTests' }) },
      { label: (muted ? '$(bell-slash)' : '$(bell)') + ' Turn-end sound: ' + (muted ? 'off' : 'on'), description: 'click to ' + (muted ? 'enable' : 'mute'), run: () => this.toggleBell() },
    ];
    if (this._daemonMode() && this.daemon.running)
      items.push({ label: '$(refresh) Refresh fleet now', description: 'rescan worktrees + re-check git', run: () => this.daemon.request('refresh', {}).catch(() => {}) });
    vscode.window.showQuickPick(items, { placeHolder: 'WorkTreeDev' }).then((it) => { if (it) it.run(); });
  }

  resolveWebviewView(view) {
    this.view = view;
    const media = vscode.Uri.joinPath(this._extUri, 'media');
    view.webview.options = { enableScripts: true, localResourceRoots: [media] };
    view.webview.html = this._html().replace('__CODICON_HREF__', view.webview.asWebviewUri(vscode.Uri.joinPath(media, 'codicon.css')).toString());

    this._onMsg = (m) => {
      if (!m) return;
      if (m.cmd === 'ready') {
        // The webview's script just finished loading. Messages posted before that (the initial posts
        // fire the instant html is set, and a window reload re-parses the webview from scratch) are
        // silently dropped — so the script announces itself and we replay the full state.
        if (this._postAll) this._postAll();
      } else if (m.cmd === 'jsError') {
        _dbg('webview script error: ' + m.msg);
      } else if (m.cmd === 'open' && m.slug && m.name) {
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
      } else if (m.cmd === 'switchAccount' && m.slug && m.name) {
        this.switchAccount(m.slug, m.name);
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
        this.newAgent();
      } else if (m.cmd === 'daemonToggle') {
        this.toggleDaemon();
      } else if (m.cmd === 'more') {
        this.moreMenu();
      } else if (m.cmd && m.cmd.startsWith('group')) {
        this._groupOp(m).catch((e) => vscode.window.showErrorMessage('Group: ' + e.message));
      } else if (m.cmd === 'settings') {
        if (this._openSettings) this._openSettings();
      } else if (m.cmd === 'newAssistant') {
        this.openOrFocusAssistant();
      } else if (m.cmd === 'newTerminal') {
        this.openOrFocusTerminal();
      } else if (m.cmd === 'previewFocused') {
        this.previewFocused();
      } else if (m.cmd === 'openCommits' && m.slug) {
        this.showPreview(m.slug, m.name, dv.COMMITS_LABEL);
      } else if (m.cmd === 'pasteImage') {
        this.pasteImage();
      } else if (m.cmd === 'toggleTests') {
        // flip the global "exclude test files from diffs" flag, then re-seed live diffs so it shows now
        try {
          if (fs.existsSync(TESTS_FLAG)) fs.unlinkSync(TESTS_FLAG);
          else { fs.mkdirSync(path.dirname(TESTS_FLAG), { recursive: true }); fs.writeFileSync(TESTS_FLAG, ''); }
        } catch (e) { vscode.window.showErrorMessage('toggle tests failed: ' + e.message); }
        this._postTests();
        execScript(path.join(DEV, '.wtd', 'hooks', 'refresh-diffs.sh'), [], { timeout: 30000 }, () => {});
      } else if (m.cmd === 'toggleBell') {
        this.toggleBell();
      }
    };
    view.webview.onDidReceiveMessage((m) => this._onMsg(m));

    // Each post is independently fire-walled: a throw in one (a failing fetch, a bad file, a patched
    // https that throws synchronously) must not take down the others or the refresh timers — an
    // unguarded throw here aborts resolveWebviewView and leaves the panel permanently on its static
    // HTML ("waiting for a session…"). Failures land in .wtd/state/open-debug.log.
    const safe = (tag, fn) => {
      try {
        const r = fn();
        if (r && typeof r.catch === 'function') r.catch((e) => _dbg('panel ' + tag + ' FAILED: ' + ((e && e.stack) || e)));
      } catch (e) { _dbg('panel ' + tag + ' FAILED: ' + ((e && e.stack) || e)); }
    };
    const postAll = () => { safe('daemon', () => this._postDaemon()); safe('limits', () => this._postLimits());
      safe('roster', () => this._postRoster({ git: true }));
      safe('monitor', () => this._postMonitor()); safe('tests', () => this._postTests()); safe('bell', () => this._postBell()); };
    this._postAll = postAll;   // re-run on the webview's 'ready' handshake (initial posts can beat the script)
    postAll();
    // refresh accounts' usage every 60s. Active accounts come from their (free) statusline file; only
    // idle accounts hit the endpoint — 60s keeps API calls low enough to avoid 429. Git state (dirty /
    // ahead) every GIT_SECS; status flips arrive instantly via the status watchers, without git.
    // System monitor every MON_SECS (resident sampler on Windows; a timer elsewhere).
    this.limTimer = setInterval(() => safe('limits', () => this._postLimits()), 60000);
    // daemon mode is push-driven (see activate's daemon onChange); these timers are the legacy path.
    // The roster timer still runs (cheap without git) to catch terminal focus/liveness drift.
    this.rosTimer = setInterval(() => safe('roster', () => this._postRoster({ git: !this._daemonMode() })), GIT_SECS * 1000);
    if (!IS_WIN) this.monTimer = setInterval(() => safe('monitor', () => this._postMonitor()), MON_SECS * 1000);
    view.onDidChangeVisibility(() => { if (view.visible) postAll(); else this._stopMonitor(); });
    view.onDidDispose(() => {
      if (this.limTimer) clearInterval(this.limTimer);
      if (this.rosTimer) clearInterval(this.rosTimer);
      if (this.monTimer) clearInterval(this.monTimer);
      this._stopMonitor();
      this.limTimer = this.rosTimer = this.monTimer = this.view = this._postAll = null;
    });
  }

  // tell the webview whether test files are currently excluded from the diffs (flag-file presence)
  _postTests() {
    if (!this.view || !this.view.visible) return;
    let excluded = false; try { excluded = fs.existsSync(TESTS_FLAG); } catch {}
    this.view.webview.postMessage({ type: 'teststate', excluded });
  }

  // whether the turn-end bell SOUND is muted (accessibility.signals.terminalBell.sound === "off")
  _postBell() {
    if (!this.view || !this.view.visible) return;
    let muted = false;
    try { const tb = vscode.workspace.getConfiguration('accessibility.signals').get('terminalBell') || {}; muted = (tb.sound || 'auto') === 'off'; } catch {}
    this.view.webview.postMessage({ type: 'bell', muted });
  }

  // header 🔔 button: mute/unmute the turn-end sound alert (writes the workspace setting live)
  toggleBell() {
    const cfg = vscode.workspace.getConfiguration('accessibility.signals');
    const tb = cfg.get('terminalBell') || {};
    const muted = (tb.sound || 'auto') === 'off';
    cfg.update('terminalBell', { ...tb, sound: muted ? 'auto' : 'off' }, vscode.ConfigurationTarget.Workspace)
      .then(() => this._postBell(), (e) => vscode.window.showErrorMessage('claude-status: toggle bell failed: ' + (e && e.message)));
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
    if (this._daemonMode()) {
      // the daemon polls usage; while it's stopped keep showing the last pushed numbers
      const list = this.daemon.accounts || [];
      if (list.length) { this.view.webview.postMessage({ type: 'limits', accounts: list }); this._maybeNotifyLimit(list); }
      return;
    }
    const accts = this._accounts();
    if (!accts.length) { this.view.webview.postMessage({ type: 'limits', accounts: [] }); return; }
    const out = new Array(accts.length); let pending = accts.length;
    const done = () => {
      if (--pending) return;
      const list = out.filter(Boolean);
      if (this.view && this.view.visible) this.view.webview.postMessage({ type: 'limits', accounts: list });
      this._maybeNotifyLimit(list);
    };
    accts.forEach((a, i) => this._usageFor(a, (u) => { out[i] = u; done(); }));
  }

  // When a logged-in account crosses ~95% of a limit, nudge the user ONCE per reset window (deduped by
  // account + reset time) to switch a session off it — the assist half of the one-click switch feature.
  _maybeNotifyLimit(list) {
    this._notified = this._notified || {};
    try {
      for (const a of list) {
        if (!a || a.nologin) continue;
        const util = Math.max((a.five_hour && a.five_hour.used) || 0, (a.seven_day && a.seven_day.used) || 0);
        if (util < 95) continue;
        const rk = (a.five_hour && a.five_hour.resets_at) || (a.seven_day && a.seven_day.resets_at) || '';
        const key = a.name + '@' + rk;
        if (this._notified[key]) continue;
        this._notified[key] = true;
        const tgt = this._pickSwitchTarget(a.name);
        vscode.window.showWarningMessage(
          'Claude account "' + a.name + '" is at ' + Math.round(util) + '% of its limit. ' +
          (tgt ? 'Click ⇄ on a session to move it to "' + tgt + '" (reopens there + compacts).'
               : 'No other logged-in account has capacity to switch to.')
        );
      }
    } catch {}
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
    // In the extension host, https is monkey-patched by VSCode's proxy agent — a bad proxy config can
    // make get() throw SYNCHRONOUSLY, so the whole call is guarded; any failure just means fallback.
    let req;
    try {
      req = https.get({
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
    } catch (e) { _dbg('usage fetch threw: ' + ((e && e.stack) || e)); cb(null); }
  }

  async _postRoster(opts) {
    if (!this.view || !this.view.visible) return;
    // coalesce: a refresh requested while one is running runs once more after it (never in parallel)
    if (this._rosBusy) { this._rosAgain = { git: !!(opts && opts.git) || !!(this._rosAgain && this._rosAgain.git) }; return; }
    this._rosBusy = true;
    try { await this._postRosterNow(opts); }
    finally {
      this._rosBusy = false;
      const again = this._rosAgain; this._rosAgain = null;
      if (again) this._postRoster(again);
    }
  }

  // daemon mode: rows straight from the pushed fleet state (no disk scan, no git, no registry reads)
  _daemonRows() {
    const rows = []; const live = new Set();
    for (const w of this.daemon.wts.values()) {
      if (w.kind === 'dev') continue;   // the assistant has its own pinned row
      const slug = w.slug, name = w.name;
      rows.push({ id: w.id, group: w.group || null, slug, name, status: w.status === 'none' ? '' : w.status, dirty: !!(w.git && w.git.dirty), ahead: (w.git && w.git.ahead) || 0,
                  branch: (w.git && w.git.branch) || '', plan: w.plan_title || '', account: w.account || '', live: !!w.live });
      if (w.live) live.add(slug + '-' + name);
    }
    return { rows, live };
  }

  async _postRosterNow(opts) {
    let rows, live;
    if (this._daemonMode()) {
      ({ rows, live } = this._daemonRows());
    } else {
      rows = await this._roster(!!(opts && opts.git));
      live = await this._liveSessions();   // session names with a live tmux session
    }
    // resolve which row is the currently-focused session: by exact terminal if we opened it, else
    // (e.g. after a window reload, when _terms is empty) fall back to matching the terminal's name.
    // sync the focused terminal from VSCode (covers editor-area terminals — assistant/terminal/sessions —
    // whose focus the cached _current can miss); keep the cache when focus is on a non-terminal.
    if (vscode.window.activeTerminal) this._current = vscode.window.activeTerminal;
    const cur = this._current; let curKey = null;
    if (cur) for (const [k, v] of this._terms) if (v === cur) { curKey = k; break; }
    // mark a row "unread" when it flips to 'input' (agent finished → your turn); cleared on open/focus
    for (const w of rows) {
      const key = this._key(w.slug, w.name);
      const prev = this._lastStatus[key];
      // flipped to 'input' (your turn) — but only "unread" if you weren't focused on it when it did
      // (if it's the current session you saw the prompt live; don't nag when you switch away).
      if (w.status === 'input' && prev !== undefined && prev !== 'input' && key !== curKey) this._unread[key] = true;
      this._lastStatus[key] = w.status;
      w.unread = !!this._unread[key];
      w.active = live.has(w.slug + '-' + w.name);   // has a live tmux session (vs. closed/inactive)
      w.current = !!cur && (curKey ? key === curKey : termName(cur) === w.name);   // focused session
    }
    // pinned assistant row state: live if a terminal named "assistant" exists; selected if it's focused.
    // Unread works exactly like a worktree row: the assistant's status sentinel (in the dev base) flips
    // to 'input' (your turn) while you're not focused on it → highlight yellow; cleared when you focus it.
    const asstTerm = vscode.window.terminals.find((x) => termName(x) === ASST_NAME);
    const asstCurrent = !!cur && termName(cur) === ASST_NAME;
    let asstStatus = '';
    if (this._daemonMode()) { const d = this.daemon.wts.get('_dev'); asstStatus = d && d.status !== 'none' ? d.status : ''; }
    else { try { asstStatus = fs.readFileSync(path.join(DEV, STATUS_FILE), 'utf8').trim(); } catch {} }
    const aprev = this._lastStatus[ASST_NAME];
    if (asstStatus === 'input' && aprev !== undefined && aprev !== 'input' && !asstCurrent) this._unread[ASST_NAME] = true;
    this._lastStatus[ASST_NAME] = asstStatus;
    if (asstCurrent) this._unread[ASST_NAME] = false;   // focused → clear (mirrors clearUnread on worktrees)
    const assistant = { active: !!asstTerm, current: asstCurrent, unread: !!this._unread[ASST_NAME] };
    // pinned plain-terminal row state (same idea, for the "terminal" row above the assistant)
    const plainTerm = vscode.window.terminals.find((x) => termName(x) === TERM_NAME);
    const terminal = { active: !!plainTerm, current: !!cur && termName(cur) === TERM_NAME };
    let multiAccount = false; try { multiAccount = this._accounts().length > 1; } catch {}
    // groups: null = legacy grouping by repo (no daemon installed); edits need the daemon running
    const groups = this._daemonMode() ? (this.daemon.groups || []) : null;
    const canEditGroups = this._daemonMode() && this.daemon.running;
    if (this.view && this.view.visible) this.view.webview.postMessage({ type: 'roster', rows, assistant, terminal, multiAccount, groups, canEditGroups });
  }

  // daemon mode = wtd.exe is installed (whether or not the daemon is running right now)
  _daemonMode() { return !!this.daemon && daemonInstalled(); }

  _postDaemon() {
    if (!this.view || !this.view.visible) return;
    this.view.webview.postMessage({ type: 'daemon', installed: daemonInstalled(), running: !!(this.daemon && this.daemon.running), busy: !!this._daemonBusy });
  }

  // Start/Stop toggle (panel button, palette commands). The daemon never auto-starts.
  toggleDaemon(want) {
    const running = !!(this.daemon && this.daemon.running);
    const start = want === undefined ? !running : want;
    if (!daemonInstalled()) { vscode.window.showWarningMessage('wtd.exe is not installed — run install.sh (needs Rust: winget install Rustlang.Rustup).'); return; }
    if (start === running) return;
    this._daemonBusy = true; this._postDaemon();
    cp.execFile(wtdExe(), ['daemon', start ? 'start' : 'stop'], { timeout: 15000, windowsHide: true }, (e, so, se) => {
      this._daemonBusy = false;
      if (e) vscode.window.showErrorMessage('wtd daemon ' + (start ? 'start' : 'stop') + ' failed: ' + ((se || '').trim() || e.message));
      this._postDaemon();
    });
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
  _postMonitorLine(out) {
    if (!this.view || !this.view.visible) return;
    const p = (out || '').trim().split('|');   // sess|nag|rev|acpu|amem|mt|msys|ncpu|load
    if (p.length < 9) return;
    this.view.webview.postMessage({ type: 'monitor', m: {
      sess: +p[0], nag: +p[1], rev: +p[2], acpu: +p[3], amem: +p[4], mt: +p[5], msys: +p[6], ncpu: +p[7], load: p[8] } });
  }

  _postMonitor() {
    if (!this.view || !this.view.visible) return;
    if (this._daemonMode()) {
      const m = this.daemon.running ? this.daemon.metrics : null;
      this.view.webview.postMessage({ type: 'monitor', m: m && {
        sess: m.sessions, nag: m.agents, rev: m.reviews, acpu: m.cpu_pct, amem: m.mem_mb,
        mt: m.sys_total_mb, msys: m.sys_used_mb, ncpu: m.ncpu, load: 'n/a' } });
      return;
    }
    if (IS_WIN) return this._startMonitor();   // resident sampler; it pushes lines on its own
    if (this._monBusy) return;                 // never stack runs (a slow sample used to overlap the next)
    this._monBusy = true;
    execScript(path.join(DEV, '.wtd', 'hooks', 'monitor-stats.sh'), [], { timeout: 8000 }, (e, out) => {
      this._monBusy = false;
      if (!e) this._postMonitorLine(out);
    });
  }

  // Windows: one long-lived PowerShell sampling every MON_SECS, instead of a fresh bash → powershell →
  // WMI cold start per sample (~7s each, on a 5s timer). Runs only while the panel is visible.
  _startMonitor() {
    if (this._monProc) return;
    const ps1 = path.join(DEV, '.wtd', 'hooks', 'monitor-stats.ps1');
    let buf = '';
    try {
      const proc = cp.spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
        '-File', ps1, '-Loop', String(MON_SECS)], { windowsHide: true, stdio: ['ignore', 'pipe', 'ignore'] });
      this._monProc = proc;
      proc.stdout.on('data', (d) => {
        buf += d; let i;
        while ((i = buf.indexOf('\n')) >= 0) { this._postMonitorLine(buf.slice(0, i)); buf = buf.slice(i + 1); }
      });
      const gone = () => { if (this._monProc === proc) this._monProc = null; };
      proc.on('exit', gone); proc.on('error', gone);
    } catch (e) { _dbg('monitor spawn failed: ' + ((e && e.stack) || e)); this._monProc = null; }
  }

  _stopMonitor() {
    const p = this._monProc; this._monProc = null;
    if (p) { try { p.kill(); } catch {} }
  }

  _roster(git) {
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

    this._gitCache = this._gitCache || new Map();   // dir → { dirty, ahead }
    const cache = this._gitCache;
    for (const k of cache.keys()) if (!wts.some((w) => w.dir === k)) cache.delete(k);
    // git status only when asked (timer / explicit refresh) or for a worktree never seen; otherwise
    // reuse the cache — a status flip only needs the sentinel file, not a working-tree scan.
    const stale = wts.filter((w) => git || !cache.has(w.dir));
    return this._gitStatusAll(stale).then(() => wts.map((w) => {
      let status = '';
      try { status = fs.readFileSync(path.join(w.dir, STATUS_FILE), 'utf8').trim(); } catch {}
      const g = cache.get(w.dir) || { dirty: false, ahead: 0 };
      return { slug: w.slug, name: w.name, status, dirty: g.dirty, ahead: g.ahead };
    }));
  }

  // `git status` the given worktrees into _gitCache, at most GIT_CONCURRENCY at a time.
  _gitStatusAll(wts) {
    const queue = wts.slice();
    const one = (w) => new Promise((res) => {
      cp.execFile('git', ['-C', w.dir, 'status', '--porcelain', '--branch'], { timeout: 10000, windowsHide: true }, (e, out) => {
        let dirty = false, ahead = 0;
        if (!e && out) {
          dirty = out.split('\n').slice(1).some((l) => l.trim().length > 0);
          const m = out.match(/ahead (\d+)/);
          if (m) ahead = parseInt(m[1], 10) || 0;
        }
        if (!e) this._gitCache.set(w.dir, { dirty, ahead });
        else if (!this._gitCache.has(w.dir)) this._gitCache.set(w.dir, { dirty: false, ahead: 0 });
        res();
      });
    });
    const worker = () => queue.length ? one(queue.shift()).then(worker) : Promise.resolve();
    return Promise.all(Array.from({ length: Math.min(GIT_CONCURRENCY, queue.length) }, worker));
  }

  _html() {
    return `<!DOCTYPE html><html><head><meta charset="utf-8">
<link rel="stylesheet" href="__CODICON_HREF__">
<style>
  html{height:100%;}
  body{padding:4px 8px 6px;margin:0;font:12px var(--vscode-font-family);color:var(--vscode-foreground);display:flex;flex-direction:column;min-height:100vh;box-sizing:border-box;}
  .codicon{font-size:14px;line-height:1;}
  .none{color:var(--vscode-descriptionForeground);font-size:12px;padding:3px 2px;}
  hr{border:none;border-top:1px solid var(--vscode-panel-border,rgba(127,127,127,.2));margin:6px 0;}
  .sect{font-size:11px;font-weight:600;letter-spacing:.4px;text-transform:uppercase;color:var(--vscode-descriptionForeground);margin:8px 2px 2px;display:flex;align-items:center;gap:6px;}
  .sect .n{font-weight:400;opacity:.8;}

  /* --- account usage --- */
  .acctblk{margin-bottom:6px;}
  .acct{color:var(--vscode-descriptionForeground);margin:0 0 2px 2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
  .bar{display:flex;align-items:center;gap:6px;height:15px;padding:0 2px;}
  .lbl{width:18px;color:var(--vscode-descriptionForeground);}
  .track{flex:1;height:4px;border-radius:2px;background:var(--vscode-input-background,rgba(127,127,127,.18));overflow:hidden;}
  .fill{display:block;height:100%;width:0;border-radius:2px;transition:width .3s ease;}
  .pct{width:30px;text-align:right;font-variant-numeric:tabular-nums;}
  .meta{min-width:36px;color:var(--vscode-descriptionForeground);font-variant-numeric:tabular-nums;}
  .stale{opacity:.45;} .staleNote{font-size:11px;color:var(--vscode-editorWarning-foreground,#d2a000);margin:1px 2px;}

  /* --- toolbar: native view-toolbar look (no tiles; hover/pressed backgrounds only) --- */
  .toolbar{display:flex;align-items:center;gap:2px;height:26px;margin:2px 0 4px;position:relative;}
  .tb{display:inline-flex;align-items:center;gap:4px;height:22px;min-width:22px;padding:0 4px;box-sizing:border-box;border:1px solid transparent;border-radius:4px;
      background:none;color:var(--vscode-icon-foreground,var(--vscode-foreground));font:inherit;cursor:pointer;white-space:nowrap;}
  .tb:hover{background:var(--vscode-toolbar-hoverBackground,rgba(90,93,94,.31));}
  .tb:focus-visible{outline:1px solid var(--vscode-focusBorder);outline-offset:-1px;}
  .tb.on{background:var(--vscode-inputOption-activeBackground,rgba(0,127,212,.4));border-color:var(--vscode-inputOption-activeBorder,transparent);color:var(--vscode-inputOption-activeForeground,inherit);}
  .tb .t{font-size:12px;}
  .tb.primary{background:var(--vscode-button-background);color:var(--vscode-button-foreground);padding:0 7px 0 5px;}
  .tb.primary:hover{background:var(--vscode-button-hoverBackground);}
  .spacer{flex:1;}
  /* daemon pill: state at rest, the action on hover */
  .pill{gap:5px;padding:0 7px 0 5px;}
  .pill .dot{font-size:10px;}
  .pill.run .dot{color:var(--vscode-testing-iconPassed,#3fb950);}
  .pill.off .dot{color:var(--vscode-descriptionForeground);}
  .pill .act{display:none;} .pill:hover .rest{display:none;} .pill:hover .act{display:inline-flex;align-items:center;gap:5px;}
  .pill.busy{opacity:.6;pointer-events:none;}

  /* search + filter popover */
  #searchRow{display:none;margin:0 0 4px;}
  #searchRow.open{display:block;}
  #q{width:100%;box-sizing:border-box;height:24px;padding:2px 6px;font:inherit;color:var(--vscode-input-foreground);background:var(--vscode-input-background);
     border:1px solid var(--vscode-input-border,transparent);border-radius:2px;outline:none;}
  #q:focus{border-color:var(--vscode-focusBorder);}
  #q::placeholder{color:var(--vscode-input-placeholderForeground);}
  .menu{position:absolute;top:26px;z-index:10;min-width:170px;padding:4px 0;background:var(--vscode-menu-background,var(--vscode-editorWidget-background));
        color:var(--vscode-menu-foreground,inherit);border:1px solid var(--vscode-menu-border,var(--vscode-widget-border,transparent));border-radius:5px;
        box-shadow:0 2px 8px var(--vscode-widget-shadow,rgba(0,0,0,.36));display:none;}
  .menu.open{display:block;}
  .mi{display:flex;align-items:center;gap:8px;height:24px;padding:0 10px;cursor:pointer;}
  .mi:hover{background:var(--vscode-menu-selectionBackground,var(--vscode-list-hoverBackground));color:var(--vscode-menu-selectionForeground,inherit);}
  .mi .chk{width:14px;visibility:hidden;} .mi.sel .chk{visibility:visible;}
  .msep{height:1px;margin:4px 0;background:var(--vscode-menu-separatorBackground,var(--vscode-panel-border));}

  /* --- rows --- */
  .daemonOff{display:flex;align-items:center;gap:8px;margin:2px 0 6px;padding:6px 8px;border-radius:4px;
             background:var(--vscode-inputValidation-warningBackground,rgba(210,160,0,.12));border:1px solid var(--vscode-inputValidation-warningBorder,transparent);}
  .daemonOff .msg{flex:1;color:var(--vscode-foreground);}
  #roster.dim .row.wt{opacity:.55;}
  .row{position:relative;display:flex;align-items:center;gap:6px;height:22px;padding:0 4px 0 6px;border-radius:3px;cursor:pointer;font-size:13px;}
  .row:hover{background:var(--vscode-list-hoverBackground);}
  .row .st{flex:none;width:16px;text-align:center;}
  .row .nm{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
  .row .sub{color:var(--vscode-descriptionForeground);font-size:12px;margin-left:4px;}
  .row .git{flex:none;color:var(--vscode-descriptionForeground);font-size:12px;font-variant-numeric:tabular-nums;display:flex;gap:4px;}
  .ahead{color:var(--vscode-charts-blue,#4aa3ff);} .dirty{color:var(--vscode-charts-yellow,#d2a000);}
  .badge{flex:none;font-size:10px;padding:0 5px;border-radius:8px;line-height:15px;background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);}
  .rb{flex:none;font-size:11px;color:var(--vscode-descriptionForeground);opacity:.85;}
  /* user-defined groups */
  .ghdr{position:relative;display:flex;align-items:center;gap:4px;height:22px;margin-top:6px;padding:0 4px 0 2px;border-radius:3px;cursor:pointer;
        font-size:11px;font-weight:600;letter-spacing:.3px;text-transform:uppercase;color:var(--vscode-descriptionForeground);user-select:none;}
  .ghdr:hover{background:var(--vscode-list-hoverBackground);}
  .ghdr .gn{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
  .ghdr .n{font-weight:400;padding:0 6px;border-radius:8px;background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);font-size:10px;line-height:15px;letter-spacing:0;}
  .ghdr .gacts{display:none;align-items:center;} .ghdr:hover .gacts{display:flex;}
  .ghdr .gacts .codicon{padding:3px;border-radius:3px;color:var(--vscode-icon-foreground);} .ghdr .gacts .codicon:hover{background:var(--vscode-toolbar-hoverBackground);}
  .ghdr .gacts .danger:hover{color:var(--vscode-errorForeground);}
  .ghdr.drop,.gbody.drop{outline:1px dashed var(--vscode-focusBorder);outline-offset:-1px;background:var(--vscode-list-dropBackground,rgba(0,127,212,.12));}
  .gbody{min-height:4px;border-radius:3px;}
  .gempty{font-size:12px;color:var(--vscode-descriptionForeground);padding:2px 0 4px 24px;font-style:italic;}
  .row.dragging{opacity:.4;}
  .row.wt:not(.live) .st{opacity:.55;} .row.wt:not(.live) .nm{color:var(--vscode-descriptionForeground);}
  .row.unread{box-shadow:inset 2px 0 0 var(--vscode-charts-yellow,#d2a000);background:rgba(255,216,61,.08);}
  .row.unread .nm{font-weight:600;color:var(--vscode-foreground);}
  .row.current{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground,inherit);}
  .row.current .nm{color:inherit;}
  /* hover actions overlay the right edge so names keep the full width at rest */
  .row .acts{position:absolute;right:2px;top:0;height:22px;display:flex;align-items:center;gap:0;opacity:0;pointer-events:none;padding-left:16px;
             background:linear-gradient(to right,transparent,var(--vscode-sideBar-background,#181818) 35%);}
  .row:hover .acts{opacity:1;pointer-events:auto;}
  .row .acts .codicon{padding:3px;border-radius:3px;color:var(--vscode-icon-foreground);}
  .row .acts .codicon:hover{background:var(--vscode-toolbar-hoverBackground);}
  .row .acts .danger:hover{color:var(--vscode-errorForeground);}
  .pins{margin-bottom:2px;}
  .row.pin .st{opacity:.9;} .row.pin:not(.live) .st,.row.pin:not(.live) .nm{opacity:.6;}

  /* --- monitor --- */
  #monwrap{margin-top:auto;}
  .statline{display:flex;flex-wrap:wrap;gap:3px 14px;margin:2px 2px 5px;}
  .stat{display:flex;gap:5px;align-items:baseline;} .sk{color:var(--vscode-descriptionForeground);} .sv{font-variant-numeric:tabular-nums;}
  .mlbl{width:26px;flex:none;color:var(--vscode-descriptionForeground);}
  #mon .bar{height:16px;margin:1px 0;} #mon .meta{min-width:0;text-align:right;white-space:nowrap;}
</style></head><body>
<div id="lim"><div class="none">waiting for usage…</div></div>
<hr>
<div class="toolbar" id="toolbar">
  <button class="tb pill off" id="daemon" title="WorkTreeDev daemon"></button>
  <button class="tb" id="active" title="Show only worktrees with a live session"><i class="codicon codicon-zap"></i><span class="t" id="activeTxt">Active</span></button>
  <button class="tb" id="filter" title="Filter by status"><i class="codicon codicon-filter" id="filterIco"></i></button>
  <button class="tb" id="search" title="Search worktrees (name, branch, plan, account)"><i class="codicon codicon-search"></i></button>
  <button class="tb" id="newGroup" title="New group"><i class="codicon codicon-new-folder"></i></button>
  <span class="spacer"></span>
  <button class="tb" id="settings" title="Settings — repos, accounts, GitHub, daemon"><i class="codicon codicon-settings-gear"></i></button>
  <button class="tb" id="more" title="More actions"><i class="codicon codicon-ellipsis"></i></button>
  <button class="tb primary" id="add" title="New session (also: Command Palette → WorkTreeDev: New Session)"><i class="codicon codicon-add"></i><span class="t">New</span></button>
  <div class="menu" id="filterMenu"></div>
</div>
<div id="searchRow"><input id="q" type="text" placeholder="Search worktrees" spellcheck="false"></div>
<div id="daemonOff"></div>
<div class="pins" id="pins"></div>
<div id="roster"></div>
<div id="monwrap">
<hr>
<div class="sect">Monitor</div>
<div id="mon"><div class="none">…</div></div>
</div>
<script>
  const vsc = acquireVsCodeApi();
  window.addEventListener('error', e => { try{ vsc.postMessage({cmd:'jsError', msg: String(e.message||e)+' @'+(e.lineno||'?')}); }catch(_){} });
  let accts=[], ros=[], mon=null, asstState={}, termState={}, multiAcct=false, daemon={installed:false,running:false,busy:false};
  let groupsData=null, canEditGroups=false;   // null = legacy repo grouping
  const saved = vsc.getState() || {};
  let activeOnly = !!saved.activeOnly, query = saved.query || '', searchOpen = !!saved.searchOpen, statuses = new Set(saved.statuses || []);
  let ungroupedCollapsed = !!saved.ungroupedCollapsed;
  function save(){ vsc.setState({activeOnly, query, searchOpen, statuses:[...statuses], ungroupedCollapsed}); }

  const GLYPH={working:'🔵',input:'🟡',reviewing:'🟣',pr:'🔹',done:'🟢',stopped:'🔴'};   // editor tab name prefix (host side)
  const STATUS=[
    ['input','Your turn','bell-dot','var(--vscode-charts-yellow,#d2a000)'],
    ['working','Working','loading codicon-modifier-spin','var(--vscode-charts-blue,#4aa3ff)'],
    ['reviewing','Reviewing','eye','var(--vscode-charts-purple,#c586f0)'],
    ['pr','PR ready','git-pull-request','var(--vscode-charts-blue,#5cc8ff)'],
    ['done','Done','pass-filled','var(--vscode-charts-green,#3fb950)'],
    ['stopped','Stopped','circle-large-outline','var(--vscode-descriptionForeground)'],
  ];
  const SMAP=Object.fromEntries(STATUS.map(s=>[s[0],s]));
  const PRIO={input:0,reviewing:1,working:2,pr:3,done:4,stopped:5,'':6};
  function esc(s){ return String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }
  function col(p){ return p>=90?'var(--vscode-charts-red,#e5534b)':p>=70?'var(--vscode-charts-yellow,#d2a000)':'var(--vscode-charts-green,#3fb950)'; }
  function rel(ts){ if(!ts) return ''; const s=ts-Math.floor(Date.now()/1000); if(s<=0) return 'resetting'; const h=Math.floor(s/3600),m=Math.floor((s%3600)/60); return h>0?(h+'h'+m+'m'):(m+'m'); }
  function ago(ts){ if(!ts) return 0; return Math.max(0, Math.floor(Date.now()/1000)-ts); }
  function agoTxt(s){ const m=Math.floor(s/60); return m>=60?(Math.floor(m/60)+'h'+(m%60)+'m'):(m>0?(m+'m'):(s+'s')); }
  const ico=(name,extra)=>'<i class="codicon codicon-'+name+(extra?' '+extra:'')+'"></i>';

  function bar(lbl,p,resets){ const has=typeof p==='number'; const w=has?Math.min(100,Math.max(0,p)):0;
    return '<div class="bar" title="'+lbl+' limit '+(has?w+'%':'unknown')+(resets?(' · resets in '+rel(resets)):'')+'">'
      +'<span class="lbl">'+lbl+'</span><div class="track"><div class="fill" style="width:'+w+'%;background:'+col(w)+'"></div></div>'
      +'<span class="pct">'+(has?w+'%':'--')+'</span><span class="meta">'+(resets?rel(resets):'')+'</span></div>';
  }
  function renderLim(){
    const el=document.getElementById('lim');
    if(!accts || !accts.length){ el.innerHTML='<div class="none">'+(daemon.installed&&!daemon.running?'usage paused — daemon stopped':'waiting for usage…')+'</div>'; return; }
    el.innerHTML = accts.map(a=>{
      const label = a.email || a.name;
      const head = '<div class="acct" title="'+esc(label)+'">'+esc(label)+'</div>';
      if(a.nologin) return '<div class="acctblk">'+head+'<div class="none">not logged in</div></div>';
      const f=a.five_hour||{}, s=a.seven_day||{};
      const age = ago(a.ts); const stale = age >= 150;
      const bars = '<div class="'+(stale?'stale':'')+'">'+bar('5h', f.used, f.resets_at)+bar('7d', s.used, s.resets_at)+'</div>';
      const note = stale ? '<div class="staleNote" title="Live fetch failing (token expired / offline / rate-limited, or the daemon is stopped); showing last known.">'+ico('history')+' '+agoTxt(age)+' old</div>' : '';
      return '<div class="acctblk">'+head+bars+note+'</div>';
    }).join('');
  }

  function renderToolbar(){
    const d=document.getElementById('daemon');
    if(!daemon.installed){ d.style.display='none'; }
    else {
      d.style.display='';
      d.className='tb pill '+(daemon.running?'run':'off')+(daemon.busy?' busy':'');
      d.innerHTML = daemon.running
        ? '<span class="rest">'+ico('circle-filled','dot')+'<span class="t">Running</span></span><span class="act">'+ico('debug-stop')+'<span class="t">Stop</span></span>'
        : '<span class="rest">'+ico('circle-outline','dot')+'<span class="t">Stopped</span></span><span class="act">'+ico('play')+'<span class="t">Start</span></span>';
      d.title = daemon.running ? 'Daemon running — click to stop (sessions keep running)' : 'Daemon stopped — click to start (live status, git state, usage)';
    }
    const n = ros.filter(w=>w.active).length;
    document.getElementById('activeTxt').textContent = 'Active · '+n;
    document.getElementById('active').classList.toggle('on', activeOnly);
    document.getElementById('filter').classList.toggle('on', statuses.size>0);
    document.getElementById('filterIco').className = 'codicon codicon-'+(statuses.size?'filter-filled':'filter');
    document.getElementById('search').classList.toggle('on', searchOpen);
    const ng=document.getElementById('newGroup'); ng.style.display = groupsData ? '' : 'none'; ng.disabled = !canEditGroups;
    ng.title = canEditGroups ? 'New group' : 'Start the daemon to edit groups';
    document.getElementById('searchRow').classList.toggle('open', searchOpen);
    const off=document.getElementById('daemonOff');
    off.innerHTML = (daemon.installed && !daemon.running)
      ? '<div class="daemonOff">'+ico('warning')+'<span class="msg">Daemon stopped — live status, git state and usage are paused.</span><button class="tb primary" id="startNow">'+ico('play')+'<span class="t">Start</span></button></div>' : '';
    const sn=document.getElementById('startNow'); if(sn) sn.onclick=()=>vsc.postMessage({cmd:'daemonToggle'});
    document.getElementById('roster').classList.toggle('dim', daemon.installed && !daemon.running);
  }

  function renderFilterMenu(){
    const m=document.getElementById('filterMenu');
    m.innerHTML = STATUS.map(s=>'<div class="mi'+(statuses.has(s[0])?' sel':'')+'" data-s="'+s[0]+'">'+ico('check','chk')+'<span style="color:'+s[3]+'">'+ico(s[1]==='Working'?'loading':s[2])+'</span><span>'+s[1]+'</span></div>').join('')
      + '<div class="msep"></div><div class="mi" data-s="">'+ico('clear-all')+'<span>Clear filter</span></div>';
    m.querySelectorAll('.mi').forEach(el=>el.onclick=(ev)=>{ ev.stopPropagation(); const s=el.dataset.s;
      if(!s) statuses.clear(); else if(statuses.has(s)) statuses.delete(s); else statuses.add(s);
      save(); renderFilterMenu(); renderRoster(); });
    const b=document.getElementById('filter'); m.style.left = b.offsetLeft+'px';
  }

  function matches(w){
    if(activeOnly && !w.active) return false;
    if(statuses.size && !statuses.has(w.status||'stopped')) return false;
    if(query){
      const hay=[w.slug,w.name,w.status,w.branch,w.plan,w.account].join(' ').toLowerCase();
      return query.toLowerCase().split(/\\s+/).filter(Boolean).every(t=>hay.includes(t));
    }
    return true;
  }

  function wtRow(w){
    const s=SMAP[w.status]||['','No status','circle-outline','var(--vscode-descriptionForeground)'];
    const g=GLYPH[w.status]||GLYPH.stopped;
    const git=(w.ahead?'<span class="ahead" title="'+w.ahead+' unpushed commit(s)">↑'+w.ahead+'</span>':'')+(w.dirty?'<span class="dirty" title="uncommitted changes">●</span>':'');
    const acct = (multiAcct && w.active && w.account && w.account!=='default') ? '<span class="badge" title="Claude account">'+esc(w.account)+'</span>' : '';
    const tip = w.slug+'/'+w.name+' — '+s[1]+(w.active?' · live session':'')+(w.branch?' · '+w.branch:'')+(w.plan?'\\n'+w.plan:'')+'\\nclick to open';
    const drag = groupsData && canEditGroups;
    return '<div class="row wt'+(w.active?' live':'')+(w.unread?' unread':'')+(w.current?' current':'')+'" data-id="'+esc(w.id||'')+'" data-slug="'+esc(w.slug)+'" data-name="'+esc(w.name)+'" data-glyph="'+g+'" title="'+esc(tip)+'"'+(drag?' draggable="true"':'')+'>'
      +'<span class="st" style="color:'+s[3]+'">'+ico(s[2])+'</span>'
      +'<span class="nm">'+esc(w.name)+'</span>'+(groupsData?'<span class="rb" title="repository">'+esc(w.slug)+'</span>':'')+acct
      +'<span class="git">'+git+'</span>'
      +'<span class="acts">'
      +'<i class="codicon codicon-diff dif" title="Commits & diffs"></i>'
      +(groupsData&&canEditGroups?'<i class="codicon codicon-folder mv" title="Move to group…"></i>':'')
      +(w.active&&!w.unread?'<i class="codicon codicon-mail unr" title="Mark unread"></i>':'')
      +(w.active&&multiAcct?'<i class="codicon codicon-arrow-swap acc" title="Switch account (reopens under the one with most capacity)"></i>':'')
      +(w.active?'<i class="codicon codicon-debug-stop term danger" title="End session (worktree stays)"></i>':'')
      +'<i class="codicon codicon-archive arch" title="Archive"></i>'
      +'<i class="codicon codicon-trash del danger" title="Delete worktree"></i>'
      +'</span></div>';
  }

  function renderPins(){
    const pin=(id,icon,label,sub,state)=>'<div class="row pin'+(state.active?' live':'')+(state.current?' current':'')+(state.unread?' unread':'')+'" id="'+id+'" title="'+esc(label+' — '+sub+' · click to open')+'">'
      +'<span class="st">'+ico(icon)+'</span><span class="nm">'+label+'<span class="sub">'+sub+'</span></span></div>';
    document.getElementById('pins').innerHTML = pin('pinAsst','hubot','Assistant','fleet manager',asstState) + pin('pinTerm','terminal','Terminal','dev base shell',termState);
    document.getElementById('pinAsst').onclick=()=>vsc.postMessage({cmd:'newAssistant'});
    document.getElementById('pinTerm').onclick=()=>vsc.postMessage({cmd:'newTerminal'});
  }

  function renderRoster(){
    renderToolbar(); renderPins();
    const shown = ros.filter(matches);
    const el=document.getElementById('roster');
    if(!ros.length){ el.innerHTML='<div class="none">No worktrees yet — click New to start a session.</div>'; return; }
    if(!shown.length){ el.innerHTML='<div class="none">No worktrees match the current filter.</div>'; return; }
    const byPrio=(rows)=>rows.sort((a,b)=>(PRIO[a.status]??9)-(PRIO[b.status]??9) || a.name.localeCompare(b.name));
    if(groupsData){
      // user-defined groups in their order, then Ungrouped; empty groups stay visible as drop targets
      // unless a filter is narrowing the list
      const filtering = activeOnly || statuses.size>0 || !!query;
      const known = new Set(groupsData.map(g=>g.id));
      const byG = {}; shown.forEach(w=>{ const g = w.group && known.has(w.group) ? w.group : ''; (byG[g]=byG[g]||[]).push(w); });
      const sec=(id,name,collapsed,rows,editable)=>{
        if(filtering && !rows.length) return '';
        const hdr='<div class="ghdr" data-g="'+esc(id)+'"'+(editable&&canEditGroups?' draggable="true"':'')+' title="'+esc(name)+(collapsed?' — click to expand':' — click to collapse')+'">'
          +ico(collapsed?'chevron-right':'chevron-down')+'<span class="gn">'+esc(name)+'</span><span class="n">'+rows.length+'</span>'
          +(editable&&canEditGroups?'<span class="gacts"><i class="codicon codicon-edit gren" title="Rename"></i><i class="codicon codicon-trash gdel danger" title="Delete group"></i></span>':'')+'</div>';
        if(collapsed) return hdr;
        return hdr+'<div class="gbody" data-g="'+esc(id)+'">'+(rows.length?byPrio(rows).map(wtRow).join(''):'<div class="gempty">'+(canEditGroups?'Drag worktrees here':'Empty')+'</div>')+'</div>';
      };
      el.innerHTML = groupsData.map(g=>sec(g.id,g.name,g.collapsed,byG[g.id]||[],true)).join('')
        + sec('', 'Ungrouped', ungroupedCollapsed, byG['']||[], false);
      wireGroups(el);
    } else {
      // legacy (no daemon installed): grouped by repo
      const groups={}; shown.forEach(w=>{ (groups[w.slug]=groups[w.slug]||[]).push(w); });
      el.innerHTML = Object.keys(groups).sort((a,b)=>a.localeCompare(b)).map(slug=>{
        const rows=byPrio(groups[slug]);
        return '<div class="sect">'+esc(slug)+'<span class="n">'+rows.length+'</span></div>'+rows.map(wtRow).join('');
      }).join('');
    }
    const on=(sel,cmd)=>el.querySelectorAll(sel).forEach(x=>x.onclick=(ev)=>{ ev.stopPropagation(); const p=x.closest('.wt'); vsc.postMessage({cmd,slug:p.dataset.slug,name:p.dataset.name}); });
    el.querySelectorAll('.wt').forEach(x=>x.onclick=()=>vsc.postMessage({cmd:'open',slug:x.dataset.slug,name:x.dataset.name,glyph:x.dataset.glyph}));
    on('.arch','archive'); on('.del','delete'); on('.term','terminate'); on('.unr','markunread'); on('.acc','switchAccount'); on('.dif','openCommits');
    el.querySelectorAll('.mv').forEach(x=>x.onclick=(ev)=>{ ev.stopPropagation(); vsc.postMessage({cmd:'groupMove', worktree:x.closest('.wt').dataset.id}); });
  }

  // group headers: collapse, rename, delete; drag rows onto a group, drag headers to reorder
  const DT_WT='application/x-wtd-worktree', DT_GRP='application/x-wtd-group';
  function wireGroups(el){
    el.querySelectorAll('.ghdr').forEach(h=>{
      const id=h.dataset.g;
      h.onclick=(ev)=>{
        if(ev.target.closest('.gren')){ ev.stopPropagation(); vsc.postMessage({cmd:'groupRename', id}); return; }
        if(ev.target.closest('.gdel')){ ev.stopPropagation(); vsc.postMessage({cmd:'groupDelete', id}); return; }
        if(!id){ ungroupedCollapsed=!ungroupedCollapsed; save(); renderRoster(); return; }
        const g=groupsData.find(x=>x.id===id); if(!g) return;
        g.collapsed=!g.collapsed; renderRoster();   // optimistic; the daemon's push confirms it
        if(canEditGroups) vsc.postMessage({cmd:'groupCollapse', id, collapsed:g.collapsed});
      };
      h.addEventListener('dragstart', e=>{ e.dataTransfer.setData(DT_GRP, id); e.dataTransfer.effectAllowed='move'; });
    });
    el.querySelectorAll('.row.wt[draggable]').forEach(r=>{
      r.addEventListener('dragstart', e=>{ e.dataTransfer.setData(DT_WT, r.dataset.id); e.dataTransfer.effectAllowed='move'; r.classList.add('dragging'); });
      r.addEventListener('dragend', ()=>r.classList.remove('dragging'));
    });
    if(!canEditGroups) return;
    el.querySelectorAll('.ghdr,.gbody').forEach(t=>{
      const g=t.dataset.g;
      t.addEventListener('dragover', e=>{
        const types=[...e.dataTransfer.types];
        const ok = types.includes(DT_WT) || (types.includes(DT_GRP) && t.classList.contains('ghdr') && g);
        if(ok){ e.preventDefault(); e.dataTransfer.dropEffect='move'; t.classList.add('drop'); }
      });
      t.addEventListener('dragleave', ()=>t.classList.remove('drop'));
      t.addEventListener('drop', e=>{
        t.classList.remove('drop'); e.preventDefault();
        const wt=e.dataTransfer.getData(DT_WT);
        if(wt){ vsc.postMessage({cmd:'groupAssign', worktree:wt, group:g||null}); return; }
        const moving=e.dataTransfer.getData(DT_GRP);
        if(moving && g && moving!==g){
          const ids=groupsData.map(x=>x.id).filter(x=>x!==moving);
          ids.splice(ids.indexOf(g), 0, moving);   // drop onto a header → insert before it
          vsc.postMessage({cmd:'groupReorder', ids});
        }
      });
    });
  }

  function renderMonitor(){
    const el=document.getElementById('mon'); if(!el) return;
    if(!mon){ el.innerHTML='<div class="none">'+(daemon.installed&&!daemon.running?'paused — daemon stopped':'…')+'</div>'; return; }
    const gb=x=>(x/1024).toFixed(1);
    const cpuPct = mon.ncpu>0?Math.min(100,Math.round(mon.acpu/mon.ncpu)):0;
    const memPct = mon.mt>0?Math.min(100,Math.round(mon.amem*100/mon.mt)):0;
    const sysPct = mon.mt>0?Math.round(mon.msys*100/mon.mt):0;
    const cores  = (mon.acpu/100).toFixed(1);
    const stat=(k,v,t)=>'<span class="stat" title="'+t+'"><span class="sk">'+k+'</span><span class="sv">'+v+'</span></span>';
    const mbar=(lbl,pct,meta,title)=>'<div class="bar" title="'+title+'"><span class="mlbl">'+lbl+'</span><div class="track"><div class="fill" style="width:'+pct+'%;background:'+col(pct)+'"></div></div><span class="pct">'+pct+'%</span><span class="meta">'+meta+'</span></div>';
    el.innerHTML = '<div class="statline">'+stat('sessions', mon.sess, 'fleet sessions running')+stat('agents', mon.nag, 'interactive agents')+stat('reviews', mon.rev, 'review workers')+'</div>'
      + mbar('cpu', cpuPct, cores+'/'+mon.ncpu+'c', 'Agents: '+cores+' of '+mon.ncpu+' cores ('+cpuPct+'% of CPU)')
      + mbar('mem', memPct, gb(mon.amem)+'G', 'Agents using '+gb(mon.amem)+' GB ('+memPct+'% of '+gb(mon.mt)+'G) · system '+gb(mon.msys)+'/'+gb(mon.mt)+'G ('+sysPct+'%)');
  }

  // --- toolbar wiring ---
  document.getElementById('daemon').onclick=()=>vsc.postMessage({cmd:'daemonToggle'});
  document.getElementById('active').onclick=()=>{ activeOnly=!activeOnly; save(); renderRoster(); };
  document.getElementById('search').onclick=()=>{ searchOpen=!searchOpen; if(!searchOpen){ query=''; document.getElementById('q').value=''; } save(); renderRoster(); if(searchOpen) document.getElementById('q').focus(); };
  const q=document.getElementById('q'); q.value=query;
  q.oninput=()=>{ query=q.value; save(); renderRoster(); };
  q.onkeydown=(e)=>{ if(e.key==='Escape'){ searchOpen=false; query=''; q.value=''; save(); renderRoster(); } };
  document.getElementById('filter').onclick=(ev)=>{ ev.stopPropagation(); const m=document.getElementById('filterMenu'); const open=!m.classList.contains('open'); if(open) renderFilterMenu(); m.classList.toggle('open', open); };
  document.addEventListener('click', ()=>document.getElementById('filterMenu').classList.remove('open'));
  document.getElementById('more').onclick=()=>vsc.postMessage({cmd:'more'});
  document.getElementById('newGroup').onclick=()=>vsc.postMessage({cmd:'groupNew'});
  document.getElementById('settings').onclick=()=>vsc.postMessage({cmd:'settings'});
  document.getElementById('add').onclick=()=>vsc.postMessage({cmd:'newAgent'});

  window.addEventListener('message', e => {
    const m=e.data; if(!m) return;
    if(m.type==='limits'){ accts=m.accounts||[]; renderLim(); }
    else if(m.type==='roster'){ ros=m.rows||[]; asstState=m.assistant||{}; termState=m.terminal||{}; multiAcct=!!m.multiAccount;
      groupsData=m.groups||null; canEditGroups=!!m.canEditGroups; renderRoster(); }
    else if(m.type==='monitor'){ mon=m.m; renderMonitor(); }
    else if(m.type==='daemon'){ daemon={installed:!!m.installed, running:!!m.running, busy:!!m.busy}; renderRoster(); renderLim(); renderMonitor(); }
  });
  renderRoster();
  setInterval(renderLim, 15000);   // keep the reset countdown ticking
  vsc.postMessage({cmd:'ready'});  // handshake: the host replays full state once this script is live
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

// ---------------------------------------------------------------------------------------------------
// New Session: account → repo → work item → branch name (→ group). A native multi-step Quick Pick.

// `wtd <args>` → parsed JSON from its last stdout line (error text from stderr)
function wtdJson(args) {
  return new Promise((resolve, reject) => {
    cp.execFile(wtdExe(), args, { windowsHide: true, maxBuffer: 16 * 1024 * 1024 }, (e, so, se) => {
      if (e) return reject(new Error(((se || '').trim().split('\n').pop() || e.message).replace(/^wtd \S+: /, '')));
      try { resolve(JSON.parse((so || '').trim().split('\n').pop() || 'null')); } catch (x) { reject(x); }
    });
  });
}

const BACK = Symbol('back');

// One wizard step. `items` may be a promise (shown busy until it resolves). Resolves with the chosen
// item, BACK, or undefined (dismissed).
function wizardPick({ title, step, total, placeholder, items, active }) {
  return new Promise((resolve) => {
    const qp = vscode.window.createQuickPick();
    let done = false; const finish = (v) => { if (!done) { done = true; resolve(v); qp.hide(); } };
    Object.assign(qp, { title, step, totalSteps: total, placeholder, matchOnDescription: true, matchOnDetail: true, ignoreFocusOut: true });
    if (step > 1) qp.buttons = [vscode.QuickInputButtons.Back];
    const setItems = (list) => {
      qp.items = list;
      const a = active && list.find(active);
      if (a) qp.activeItems = [a];
    };
    if (items && typeof items.then === 'function') {
      qp.busy = true; qp.items = [{ label: '$(loading~spin) Loading…', alwaysShow: true, _loading: true }];
      items.then((l) => { if (!done) { qp.busy = false; setItems(l); } },
                 (e) => { if (!done) { qp.busy = false; qp.items = [{ label: '$(warning) ' + e.message, alwaysShow: true, _error: true }]; } });
    } else setItems(items);
    qp.onDidAccept(() => { const it = qp.selectedItems[0]; if (it && !it._loading && !it._error && it.kind !== vscode.QuickPickItemKind.Separator) finish(it); });
    qp.onDidTriggerButton((b) => { if (b === vscode.QuickInputButtons.Back) finish(BACK); });
    qp.onDidHide(() => { finish(undefined); qp.dispose(); });
    qp.show();
  });
}

function wizardInput({ title, step, total, value, prompt, validate, selection }) {
  return new Promise((resolve) => {
    const ib = vscode.window.createInputBox();
    let done = false; const finish = (v) => { if (!done) { done = true; resolve(v); ib.hide(); } };
    Object.assign(ib, { title, step, totalSteps: total, value, prompt, ignoreFocusOut: true, buttons: [vscode.QuickInputButtons.Back] });
    if (selection) ib.valueSelection = selection;
    ib.onDidChangeValue((v) => { ib.validationMessage = validate ? validate(v) : undefined; });
    ib.validationMessage = validate ? validate(value) : undefined;
    ib.onDidAccept(() => { if (!(validate && validate(ib.value) && validate(ib.value).severity !== vscode.InputBoxValidationSeverity.Warning)) finish(ib.value.trim()); });
    ib.onDidTriggerButton(() => finish(BACK));
    ib.onDidHide(() => { finish(undefined); ib.dispose(); });
    ib.show();
  });
}

function slugify(t, max) {
  return String(t || '').toLowerCase().replace(/[`'"]/g, '').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, max).replace(/-+$/, '');
}

// fix/… for bugs, docs/… for docs, feat/… otherwise; issue number first so branches sort by issue
function branchFor(item) {
  const labels = (item.labels || []).join(' ').toLowerCase();
  const type = /bug|fix|regression|crash/.test(labels) ? 'fix' : /doc/.test(labels) ? 'docs' : 'feat';
  const s = slugify(item.title, 40);
  return type + '/' + (item.number ? item.number + (s ? '-' + s : '') : s);
}

function validBranch(v) {
  v = (v || '').trim();
  if (!v) return 'Enter a branch name';
  if (!/^[A-Za-z0-9._\/-]+$/.test(v)) return 'Use letters, digits, . _ - and /';
  if (/^[\/.]|\/$|\.\.|\/\/|\.lock$|@\{/.test(v)) return 'Not a valid git branch name';
  return null;
}

function usageText(u) {
  if (!u) return '';
  const p = (l) => (l && typeof l.used === 'number' ? Math.round(l.used) + '%' : '--');
  return '5h ' + p(u.five_hour) + ' · 7d ' + p(u.seven_day);
}

function issueMarkdown(item, detail) {
  const ref = item.number ? '#' + item.number : '';
  const title = (detail && detail.title) || item.title;
  const head = item.url ? '[' + (ref ? ref + ' ' : '') + title + '](' + item.url + ')' : (ref ? ref + ' ' : '') + title;
  const lines = ['**Work item:** ' + head + (item.repo ? ' — ' + item.repo : ''), ''];
  if ((item.labels || []).length) lines.push('Labels: ' + item.labels.join(', '), '');
  if (item.number) lines.push('When this work is ready for a PR, include `Closes ' + (item.repo ? item.repo : '') + '#' + item.number + '` in pr-notes.md.', '');
  const body = ((detail && detail.body) || '').trim();
  if (body) lines.push('### Issue description', '', body.length > 8000 ? body.slice(0, 8000) + '\n\n… (truncated — see the issue)' : body, '');
  return lines.join('\n');
}

async function newSessionWizard(dev) {
  const T = 'New Session';
  const usage = new Map(((dev.daemon && dev.daemon.accounts) || []).map((a) => [a.name, a]));
  const accountsP = wtdJson(['account', 'ls']);
  const reposP = wtdJson(['repo', 'ls']);
  const st = { step: 1 };
  for (;;) {
    if (st.step === 1) {
      const accts = await accountsP.catch(() => []);
      const items = [];
      for (const prov of ['claude', 'codex']) {
        const list = accts.filter((a) => a.provider === prov);
        if (!list.length) continue;
        items.push({ label: prov === 'claude' ? 'Claude' : 'Codex', kind: vscode.QuickPickItemKind.Separator });
        for (const a of list) items.push({
          label: (prov === 'claude' ? '$(sparkle) ' : '$(symbol-misc) ') + a.name + (a.logged_in ? '' : ' $(circle-slash)'),
          description: [a.email, a.plan, prov === 'claude' && a.logged_in ? usageText(usage.get(a.name)) : ''].filter(Boolean).join(' · '),
          detail: !a.logged_in ? 'Not logged in — log in from Settings → Accounts' : (a.roles || []).includes('dev') ? 'Default for new sessions' : undefined,
          acct: a,
        });
      }
      const it = await wizardPick({ title: T, step: 1, total: 4, placeholder: 'Run the session under which account?', items,
        active: (i) => i.acct && (i.acct.roles || []).includes('dev') });
      if (!it || it === BACK) return;
      if (!it.acct.logged_in) { vscode.window.showWarningMessage(it.acct.provider + ' account "' + it.acct.name + '" is not logged in.', 'Open Settings').then((c) => { if (c) dev._openSettings && dev._openSettings(); }); continue; }
      st.account = it.acct; st.step = 2; continue;
    }
    if (st.step === 2) {
      const repos = await reposP.catch(() => []);
      const items = repos.map((r) => ({ label: '$(repo) ' + r.slug, description: r.github_detected || (r.local_only ? 'local repo' : r.url),
        detail: r.worktrees + ' worktree' + (r.worktrees === 1 ? '' : 's') + (r.default_branch ? ' · ' + r.default_branch : ''), repo: r }));
      items.push({ label: '', kind: vscode.QuickPickItemKind.Separator },
        { label: '$(lightbulb) Planning agent', description: 'no repo yet — scope a new app', plan: true },
        { label: '$(add) Add a repository…', description: 'opens Settings', add: true });
      const it = await wizardPick({ title: T, step: 2, total: 4, placeholder: 'Which repository?', items,
        active: (i) => st.repo && i.repo && i.repo.slug === st.repo.slug });
      if (!it) return; if (it === BACK) { st.step = 1; continue; }
      if (it.add) { dev._openSettings && dev._openSettings(); return; }
      st.plan = !!it.plan; st.repo = it.repo || null; st.item = null; st.step = st.plan ? 4 : 3; continue;
    }
    if (st.step === 3) {
      const slug = st.repo.slug;
      const base = [
        { label: '$(add) Blank session', description: 'start from ' + (st.repo.default_branch || 'the default branch'), blank: true },
        { label: '$(git-branch) Existing branch…', description: 'check out a branch that already exists', existing: true },
      ];
      const loaded = wtdJson(['issues', 'ls', slug]).then((r) => {
        const src = r.source || {};
        const where = src.kind === 'project' ? 'Project #' + src.number + (src.offer && src.offer.length ? ' · ' + src.offer.join(', ') : '') : 'Issues · ' + (src.repo || '');
        const list = (r.items || []).map((i) => ({
          label: (i.kind === 'draft' ? '$(note) ' : '$(issues) ') + (i.number ? '#' + i.number + ' ' : '') + i.title,
          description: [i.status, (i.labels || []).join(', ')].filter(Boolean).join(' · '),
          detail: (i.mine ? '$(account) assigned to you' : (i.assignees || []).length ? 'assigned to ' + i.assignees.join(', ') : undefined),
          item: i,
        }));
        return [...base, { label: where + ' (' + list.length + ')', kind: vscode.QuickPickItemKind.Separator }, ...list];
      }, (e) => [...base, { label: '', kind: vscode.QuickPickItemKind.Separator }, { label: '$(warning) Couldn’t load issues', detail: e.message, settings: true }]);
      const it = await wizardPick({ title: T, step: 3, total: 4, placeholder: 'What will this session work on? (type to search issues)', items: loaded });
      if (!it) return; if (it === BACK) { st.step = 2; continue; }
      if (it.settings) { dev._openSettings && dev._openSettings(); return; }
      if (it.existing) {
        const br = await pickBranch(dev, slug); if (br === BACK) continue; if (!br) return;
        st.item = null; st.branch = br; st.existing = true; st.step = 5; continue;
      }
      st.item = it.item || null; st.existing = false; st.branch = null; st.step = 4; continue;
    }
    if (st.step === 4) {
      const proposed = st.plan ? 'new-app' : st.item ? branchFor(st.item) : 'feat/';
      const v = await wizardInput({ title: T, step: 4, total: 4, value: st.branch || proposed,
        prompt: st.plan ? 'Name for the planning agent' : 'Branch name (also the worktree name)',
        selection: st.item || st.plan ? undefined : [proposed.length, proposed.length],
        validate: (x) => {
          const bad = validBranch(x); if (bad) return bad;
          const id = (st.plan ? 'plan' : st.repo.slug) + '/' + x.trim();
          if (dev.daemon && dev.daemon.wts.has(id)) return { message: 'A worktree with this name exists — Enter opens it', severity: vscode.InputBoxValidationSeverity.Warning };
          return null;
        } });
      if (!v) return; if (v === BACK) { st.step = st.plan ? 2 : 3; continue; }
      st.branch = v; st.step = 5; continue;
    }
    if (st.step === 5) {
      const groups = (dev.daemon && dev.daemon.running && dev.daemon.groups) || [];
      if (groups.length) {
        const items = [{ label: '$(circle-outline) Ungrouped', g: null }, ...groups.map((g) => ({ label: '$(folder) ' + g.name, g: g.id }))];
        const it = await wizardPick({ title: T, step: 5, total: 5, placeholder: 'Add it to a group?', items });
        if (!it) return; if (it === BACK) { st.step = st.existing ? 3 : 4; continue; }
        st.group = it.g;
      }
      return launchSession(dev, st);
    }
  }
}

// branches on origin, newest first (for "Existing branch…")
async function pickBranch(dev, slug) {
  const bare = path.join(DEV, 'repos', slug, '.bare');
  const branches = new Promise((resolve, reject) => cp.execFile('git', ['-c', 'safe.bareRepository=all', '-C', bare, 'for-each-ref', '--sort=-committerdate',
    '--format=%(refname:short)\t%(committerdate:relative)\t%(subject)', 'refs/remotes/origin', 'refs/heads'], { windowsHide: true, maxBuffer: 8 * 1024 * 1024 }, (e, so) => {
    if (e) return reject(e);
    const seen = new Set();
    resolve((so || '').split('\n').filter(Boolean).map((l) => l.split('\t')).map(([ref, when, subj]) => ({ name: ref.replace(/^origin\//, ''), when, subj }))
      .filter((b) => b.name !== 'HEAD' && b.name !== 'origin' && !seen.has(b.name) && seen.add(b.name))
      .map((b) => ({ label: '$(git-branch) ' + b.name, description: b.when, detail: b.subj, branch: b.name })));
  }));
  const it = await wizardPick({ title: 'New Session', step: 3, total: 4, placeholder: 'Which branch?', items: branches });
  if (!it) return undefined; if (it === BACK) return BACK;
  return it.branch;
}

async function launchSession(dev, st) {
  const slug = st.plan ? 'plan' : st.repo.slug, name = st.branch;
  const id = slug + '/' + name;
  if (dev.daemon && dev.daemon.wts.has(id)) { dev.openOrFocus(slug, name); return; }
  const args = [];
  if (st.account) args.push('--account', st.account.provider === 'codex' ? 'codex:' + st.account.name : st.account.name);
  if (st.item) {
    let detail = null;
    if (st.item.number && st.item.repo) detail = await wtdJson(['issues', 'show', st.item.repo, String(st.item.number)]).catch(() => null);
    const file = path.join(WTD, 'state', 'issues', id.replace(/\//g, '__') + '.md');
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, issueMarkdown(st.item, detail));
    args.push('--issue-file', file.replace(/\\/g, '/'));
  }
  const t = vscode.window.createTerminal({ name, location: vscode.TerminalLocation.Editor, env: wtdTermEnv(), shellPath: bashShell(),
    shellArgs: ['-lc', ['agent', slug, name].concat(args).map(shq).join(' ')] });
  dev._terms.set(dev._key(slug, name), t); dev._current = t;
  t.show();
  // after the worktree exists: join the chosen group, move the project card
  const afterCreate = async () => {
    for (let i = 0; i < 60 && !(dev.daemon && dev.daemon.wts.has(id)); i++) await new Promise((r) => setTimeout(r, 1000));
    if (st.group && dev.daemon && dev.daemon.running) dev.daemon.request('group.assign', { worktree: id, group: st.group }).catch(() => {});
    if (st.item && st.item.item_id) {
      wtdJson(['issues', 'start', slug, st.item.item_id]).then((r) => { if (r && r.moved) vscode.window.setStatusBarMessage('$(project) Moved the card to ' + r.to, 5000); },
        (e) => vscode.window.showWarningMessage('Couldn’t move the project card: ' + e.message));
    }
  };
  afterCreate();
  setTimeout(() => dev._postRoster(), 2500);
}

function activate(context) {
  refreshDevRoot();   // resolve the real dev base (the tree is relocatable) before anything scans it

  const provider = new ClaudeStatusProvider();
  context.subscriptions.push(vscode.window.registerFileDecorationProvider(provider));

  const dev = new DevSummaryProvider();
  dev._extUri = context.extensionUri;

  // v2 daemon: one pipe subscription replaces the panel's scanning, polling and process spawning.
  // Pushes are coalesced per kind so a burst of agent activity costs one repaint.
  const pending = new Set(); let flush = null;
  const kick = (kind) => {
    pending.add(kind);
    if (flush) return;
    flush = setTimeout(() => {
      flush = null;
      const kinds = new Set(pending); pending.clear();
      if (kinds.has('daemon')) { dev._postDaemon(); dev._postMonitor(); }
      if (kinds.has('fleet') || kinds.has('daemon')) dev._postRoster();
      if (kinds.has('accounts')) dev._postLimits();
      if (kinds.has('metrics')) dev._postMonitor();
    }, 150);
  };
  let settings = null;
  dev.daemon = new DaemonClient((kind, wt, prev) => {
    // folder colours: refresh just the worktree whose status changed
    if (kind === 'fleet' && wt && (!prev || prev.status !== wt.status)) provider.refresh(vscode.Uri.file(wt.path));
    if (settings && (kind === 'daemon' || kind === 'accounts')) settings.onDaemonChange();
    kick(kind);
  });
  settings = new SettingsPanel({
    vscode, wtdExe, daemonInstalled, daemon: dev.daemon, log: _dbg,
    media: vscode.Uri.joinPath(context.extensionUri, 'media'),
    toggleDaemon: (want) => dev.toggleDaemon(want),
  });
  dev._openSettings = () => settings.open();
  context.subscriptions.push({ dispose: () => settings.dispose() });
  if (daemonInstalled()) dev.daemon.start();
  context.subscriptions.push({ dispose: () => dev.daemon.dispose() });
  const reg = (id, fn) => context.subscriptions.push(vscode.commands.registerCommand(id, fn));
  reg('claudeStatus.startDaemon', () => dev.toggleDaemon(true));
  reg('claudeStatus.stopDaemon', () => dev.toggleDaemon(false));
  reg('claudeStatus.toggleDaemon', () => dev.toggleDaemon());
  reg('claudeStatus.newSession', () => dev.newAgent());
  reg('claudeStatus.openSettings', () => dev._openSettings());
  reg('claudeStatus.openAssistant', () => dev.openOrFocusAssistant());
  reg('claudeStatus.openTerminal', () => dev.openOrFocusTerminal());
  context.subscriptions.push(vscode.window.registerWebviewViewProvider('claudeStatus.limit', dev));
  // ctrl+v in a focused session → image-aware paste (see DevSummaryProvider.smartPaste)
  context.subscriptions.push(vscode.commands.registerCommand('claudeStatus.smartPaste', () => dev.smartPaste()));

  // agents publish design mockups via the `preview` command (→ .wtd/state/previews/<slug>/<name>.html);
  // watch that tree so a 🖼 appears on the worktree's roster row, and clears when the file is removed.
  const pvDir = path.join(WTD, 'state', 'previews');
  const pvWatcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(pvDir, '**/*.html'));
  const pvChange = (uri) => {
    const k = dev._previewKey(uri.fsPath); if (!k) return;
    const key = dev._key(k.slug, k.name);
    dev._scanPreviews(k.slug, k.name); dev._postRoster();   // refresh the worktree's tab set
    // live-refresh the open panel in place when a preview it's showing changes (e.g. an agent re-stages
    // its living plan, or adds a new tab) — no reveal, so focus isn't stolen. (Poll is the backstop.)
    if (dev._pvPanel && dev._pvShownKey === key) dev.showPreview(k.slug, k.name, dev._pvLabel, false);
  };
  pvWatcher.onDidCreate(pvChange); pvWatcher.onDidChange(pvChange); pvWatcher.onDidDelete(pvChange);
  context.subscriptions.push(pvWatcher);
  // seed from any previews already on disk (so they survive a window reload)
  try { for (const slug of fs.readdirSync(pvDir, { withFileTypes: true }).filter((d) => d.isDirectory())) {
    const walk = (d) => { for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const fp = path.join(d, e.name);
      if (e.isDirectory()) walk(fp);
      else if (e.name.endsWith('.html')) { const k = dev._previewKey(fp); if (k) (dev._preview[dev._key(k.slug, k.name)] = dev._preview[dev._key(k.slug, k.name)] || {})[k.label] = fp; }
    } };
    walk(path.join(pvDir, slug.name));
  } } catch {}
  // the dev base is derived from the open folders — re-resolve it (and repaint) if they change
  context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(() => { refreshDevRoot(); dev._postRoster(); }));
  // keep the roster's terminal map + unread flags in sync with the actual terminals
  context.subscriptions.push(vscode.window.onDidCloseTerminal((t) => dev.onTermClosed(t)));
  context.subscriptions.push(vscode.window.onDidChangeActiveTerminal((t) => dev.onTermActive(t)));
  // Status changes arrive through ONE central folder the status hook mirrors into: $WTD/state/status/<key>
  // (key = path under worktrees/ with '/' → '__'; '_dev' = the dev base / assistant). A single
  // non-recursive fs.watch replaces a workspace-wide '**/.claude-status' glob — no recursive watching of
  // every worktree, works with worktrees/** in files.watcherExclude, and holds no handle on any worktree
  // dir (which on Windows blocks `git worktree move/remove`).
  let rosBump = null, statusWatch = null, statusWatchDir = '';
  const onStatus = (key) => {
    const dir = key === '_dev' ? DEV : path.join(DEV, 'worktrees', ...key.split('__'));
    provider.refresh(vscode.Uri.file(dir));
    if (dev._gitCache) dev._gitCache.delete(dir);   // a turn started/ended: re-check just this worktree's git state
    // throttle, not debounce: with many agents firing, a resetting debounce can starve the roster
    if (!rosBump) rosBump = setTimeout(() => { rosBump = null; dev._postRoster(); }, 250);
  };
  const watchStatus = () => {
    const dir = path.join(WTD, 'state', 'status');
    if (statusWatch && statusWatchDir === dir) return;
    if (statusWatch) { try { statusWatch.close(); } catch {} statusWatch = null; }
    try { fs.mkdirSync(dir, { recursive: true }); } catch {}
    try {
      const w = fs.watch(dir, (ev, file) => { if (file) onStatus(String(file)); });
      w.on('error', () => { try { w.close(); } catch {} if (statusWatch === w) statusWatch = null; });
      statusWatch = w; statusWatchDir = dir;
    } catch (e) { _dbg('status watch failed: ' + ((e && e.stack) || e)); }
  };
  watchStatus();
  const statusRewatch = setInterval(watchStatus, 30000);   // re-arm after an error or a dev-root change
  context.subscriptions.push({ dispose: () => { clearInterval(statusRewatch); if (statusWatch) try { statusWatch.close(); } catch {} } });

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
