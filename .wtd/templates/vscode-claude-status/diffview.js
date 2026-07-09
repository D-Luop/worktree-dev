// Commit diff viewer — the `commits` tab of the preview panel.
//
// The tmux backend gives each session a live diff pane and a commit pane (see .wtd/hooks/diff-pane.sh,
// commit-pane.sh). Native Windows has no tmux, so it had no commit/diff surface at all. This renders
// one into the preview webview: the branch's commits on the left, the selected commit's patch on the
// right, filterable by path and file type.
//
// Everything here is pure — git queries + HTML/JSON generation. extension.js owns the panel, routes
// the webview messages, and decides when to call in.

const cp = require('child_process');
const fs = require('fs');
const path = require('path');

const COMMITS_LABEL = 'commits';
const WORKING = '~working';        // pseudo-sha for the uncommitted working-tree entry

const MAX_COMMITS = 200;           // branch histories longer than this are rare; a full render is slow
const FALLBACK_COMMITS = 50;       // when the branch has no commits over its base (see listCommits)
const MAX_FILES = 300;             // per commit
const MAX_PATCH_LINES = 1200;      // per file — a 40k-line generated blob would freeze the webview
const US = '\x1f', RS = '\x1e';    // git --format field / record separators (never appear in messages)

// Test-file globs mirror test_excludes() in .wtd/hooks/generated-filter.sh, so the `commits` tab
// hides exactly what the tmux diff panes hide. Kept in sync by hand — it's six patterns.
const TEST_RE = /(^|\/)(testdata\/|.*_test\.(go|sql|ts)$|.*\.(test|spec)\.(ts|tsx|js|jsx)$)/i;

function isGitWorktree(wt) { try { return fs.existsSync(path.join(wt, '.git')); } catch { return false; } }

function git(wt, args, cb) {
  cp.execFile('git', ['-C', wt].concat(args),
    { maxBuffer: 64 * 1024 * 1024, timeout: 20000, windowsHide: true, encoding: 'utf8' },
    (e, so) => cb(e, so || ''));
}
// Small, fast queries only (branch name, ref existence). Never used for patches.
function gitSync(wt, args) {
  try {
    return cp.execFileSync('git', ['-C', wt].concat(args),
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 5000, windowsHide: true }).trim();
  } catch { return ''; }
}

// The ref this branch forked from, so "commits related to the branch" excludes trunk history.
// origin/HEAD is authoritative (it's what `add-repo` sets); the rest are fallbacks for repos with no
// remote — e.g. the `plan` slug's plain `git init` worktrees.
function baseRef(wt) {
  const sym = gitSync(wt, ['symbolic-ref', '-q', 'refs/remotes/origin/HEAD']);
  if (sym) return sym.replace(/^refs\/remotes\//, '');
  for (const c of ['origin/main', 'origin/master', 'main', 'master'])
    if (gitSync(wt, ['rev-parse', '--verify', '--quiet', c + '^{commit}'])) return c;
  return '';
}

// commits on this branch but not on its base. A worktree sitting ON the base (or a repo-less `plan`
// worktree with no trunk to compare against) yields nothing — fall back to recent history and say so.
function listCommits(wt, cb) {
  const branch = gitSync(wt, ['rev-parse', '--abbrev-ref', 'HEAD']) || 'HEAD';
  // A worktree sitting on trunk itself has no base to diff against ('main'..HEAD is always empty).
  // origin/main..HEAD is still meaningful there — it's the unpushed commits — so only drop a LOCAL
  // base that names the current branch.
  let base = baseRef(wt);
  if (base === branch) base = '';
  const fmt = ['%H', '%h', '%an', '%ar', '%s'].join(US) + RS;

  const run = (extra, done) => git(wt, ['log', '--no-color', '--format=' + fmt].concat(extra), (e, so) => {
    if (e) return done([]);
    done(so.split(RS).map((s) => s.replace(/^\r?\n/, '')).filter((s) => s.trim()).map((s) => {
      const p = s.split(US);
      return { sha: p[0], short: p[1], author: p[2] || '', when: p[3] || '', subject: p[4] || '' };
    }));
  });
  const finish = (commits, fallback) => {
    const porcelain = gitSync(wt, ['status', '--porcelain']);
    cb({ branch, base, fallback, commits, dirty: !!porcelain });
  };
  if (base) {
    run(['--max-count=' + MAX_COMMITS, base + '..HEAD'],
      (c) => (c.length ? finish(c, false) : run(['--max-count=' + FALLBACK_COMMITS, 'HEAD'], (c2) => finish(c2, true))));
  } else {
    run(['--max-count=' + FALLBACK_COMMITS, 'HEAD'], (c) => finish(c, true));
  }
}

// git C-quotes a path with non-ASCII/control bytes ("src/caf\303\251.ts"); octal escapes are UTF-8
// bytes, so decode them as bytes and then as UTF-8 — char-by-char would mangle multi-byte names.
function unquoteC(s) {
  const bytes = [];
  const simple = { n: 10, t: 9, r: 13, b: 8, f: 12, a: 7, v: 11, '"': 34, '\\': 92 };
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '\\' && i + 1 < s.length) {
      const n = s[i + 1];
      if (n >= '0' && n <= '7') { bytes.push(parseInt(s.substr(i + 1, 3), 8) & 0xff); i += 3; continue; }
      if (n in simple) { bytes.push(simple[n]); i += 1; continue; }
    }
    for (const b of Buffer.from(s[i], 'utf8')) bytes.push(b);
  }
  return Buffer.from(bytes).toString('utf8');
}

// One path off a ---/+++ line. git pads a name containing a space with a trailing TAB, and C-quotes
// names needing escapes — strip both before the a//b/ prefix.
function strip(p) {
  p = p.replace(/\t+$/, '');
  if (p.length > 1 && p[0] === '"' && p[p.length - 1] === '"') p = unquoteC(p.slice(1, -1));
  if (p === '/dev/null') return '';
  return p.replace(/^[ab]\//, '');
}

// `diff --git <a> <b>` — only consulted when the chunk has no ---/+++ lines (binary, mode-only). Both
// sides naming the same path is the common case and can be matched exactly, which keeps spaces intact.
function pathsFromHeader(h) {
  let m = /^"(.+)" "(.+)"$/.exec(h);
  if (m) return [strip('"' + m[1] + '"'), strip('"' + m[2] + '"')];
  m = /^a\/(.+) b\/\1$/.exec(h);
  if (m) return [m[1], m[1]];
  m = /^a\/(.+?) b\/(.+)$/.exec(h);
  if (m) return [m[1], m[2]];
  return ['', ''];
}

// One `diff --git` section → {path, status, adds, dels, binary, patch}. Counts come from the hunk
// lines rather than a second `--numstat` call, so a rename or a mode-only change can't desync them.
function parseFileChunk(part) {
  const lines = ('diff --git ' + part).replace(/\r\n/g, '\n').split('\n');
  let oldPath = '', newPath = '', status = 'modified', binary = false, hunkAt = -1;

  for (let i = 1; i < lines.length; i++) {
    const l = lines[i];
    if (l.startsWith('@@')) { hunkAt = i; break; }
    if (l.startsWith('--- ')) oldPath = strip(l.slice(4));
    else if (l.startsWith('+++ ')) newPath = strip(l.slice(4));
    else if (l.startsWith('new file mode')) status = 'added';
    else if (l.startsWith('deleted file mode')) status = 'deleted';
    else if (l.startsWith('rename from')) status = 'renamed';
    else if (l.startsWith('Binary files') || l.startsWith('GIT binary patch')) binary = true;
  }
  // no ---/+++ (binary, or a pure mode change): fall back to the `a/x b/y` header
  if (!oldPath && !newPath) [oldPath, newPath] = pathsFromHeader(lines[0].replace(/^diff --git /, ''));
  const file = newPath || oldPath;
  if (!file) return null;

  let adds = 0, dels = 0;
  const body = hunkAt >= 0 ? lines.slice(hunkAt) : [];
  for (const l of body) {
    if (l[0] === '+') adds++;
    else if (l[0] === '-') dels++;
  }
  let patch = body, truncated = 0;
  if (patch.length > MAX_PATCH_LINES) { truncated = patch.length - MAX_PATCH_LINES; patch = patch.slice(0, MAX_PATCH_LINES); }

  return {
    path: file,
    from: status === 'renamed' && oldPath !== newPath ? oldPath : '',
    status, adds, dels, binary,
    ext: (/\.([A-Za-z0-9_+-]+)$/.exec(file) || [, ''])[1].toLowerCase(),
    test: TEST_RE.test(file),
    patch: patch.join('\n'), truncated,
  };
}

// A `git show`/`git diff` patch → file list. Split on the diff header at line-start; a context line
// always begins with a space, so it can never be mistaken for one.
function parsePatch(text) {
  if (!text || !text.trim()) return { files: [], omitted: 0 };
  const parts = text.replace(/\r\n/g, '\n').split(/^diff --git /m).filter((s) => s.trim());
  const files = [];
  for (const p of parts) { const f = parseFileChunk(p); if (f) files.push(f); }
  const omitted = Math.max(0, files.length - MAX_FILES);
  return { files: files.slice(0, MAX_FILES), omitted };
}

// Untracked files aren't in `git diff HEAD`; surface them as name-only rows rather than pretending
// the working tree is clean. Their contents aren't diffed (they have no blob to diff against).
function untrackedFiles(wt) {
  const out = gitSync(wt, ['status', '--porcelain', '--untracked-files=all']);
  if (!out) return [];
  return out.split('\n').filter((l) => l.startsWith('??')).map((l) => {
    const file = l.slice(3).replace(/^"|"$/g, '');
    return {
      path: file, from: '', status: 'untracked', adds: 0, dels: 0, binary: false,
      ext: (/\.([A-Za-z0-9_+-]+)$/.exec(file) || [, ''])[1].toLowerCase(),
      test: TEST_RE.test(file), patch: '', truncated: 0, untracked: true,
    };
  });
}

// The patch for one entry: a real commit, or the uncommitted working tree (tracked edits vs HEAD,
// plus untracked files listed by name).
function commitDiff(wt, sha, cb) {
  if (sha === WORKING) {
    return git(wt, ['diff', '--no-color', '--find-renames', 'HEAD'], (e, so) => {
      const { files, omitted } = parsePatch(e ? '' : so);
      cb({ sha: WORKING, subject: 'Uncommitted changes', files: files.concat(untrackedFiles(wt)), omitted });
    });
  }
  const subject = gitSync(wt, ['show', '--no-patch', '--format=%s', sha]);
  const meta = gitSync(wt, ['show', '--no-patch', '--format=' + ['%an', '%ad', '%h'].join(US), '--date=format:%Y-%m-%d %H:%M', sha]).split(US);
  git(wt, ['show', '--no-color', '--format=', '--find-renames', '--patch', sha], (e, so) => {
    const { files, omitted } = parsePatch(e ? '' : so);
    cb({ sha, short: meta[2] || sha.slice(0, 8), subject, author: meta[0] || '', date: meta[1] || '', files, omitted });
  });
}

// The tab's static shell. Commit list and patches arrive over postMessage (git is async and a big
// `git show` shouldn't block the extension host), so this renders a skeleton and fills in.
//
// It must NOT call acquireVsCodeApi() outright: extension.js appends a switcher-bar script that also
// needs the handle, and the API is one-shot per webview. Both sides go through window.__wtapi.
function commitsHtml(slug, name, hideTests, sideWidth) {
  const sw = Math.max(150, Math.min(Number(sideWidth) || 230, 700));
  return `<!doctype html><html><head><meta charset="utf-8"><title>commits</title><style>
:root{--bg:#0d1117;--fg:#e6edf3;--dim:#8b949e;--line:#30363d;--panel:#161b22;--accent:#2f81f7;--add:#2ea043;--del:#f85149}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:13px/1.5 system-ui,-apple-system,Segoe UI,sans-serif}
#cv{display:flex;height:calc(100vh - 44px);overflow:hidden}
#side{width:${sw}px;flex:0 0 auto;min-width:150px;border-right:1px solid var(--line);display:flex;flex-direction:column;background:var(--panel)}
#grip{flex:0 0 5px;cursor:col-resize;background:transparent;transition:background .12s}
#grip:hover,#grip.drag{background:var(--accent)}
#sidehead{padding:8px 10px;border-bottom:1px solid var(--line);font-size:11px;color:var(--dim)}
#sidehead b{color:var(--fg);font-size:12px}
#clist{overflow-y:auto;flex:1}
.c{padding:7px 10px;border-bottom:1px solid #21262d;cursor:pointer}
.c:hover{background:#1c2128}
.c.sel{background:#1f6feb26;box-shadow:inset 2px 0 0 var(--accent)}
.c .s{font:600 12px/1.4 system-ui;color:var(--fg);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.c .m{font:11px ui-monospace,Consolas,monospace;color:var(--dim);margin-top:2px}
.c.work .s{color:#d29922}
#main{flex:1;display:flex;flex-direction:column;overflow:hidden}
#bar{padding:8px 10px;border-bottom:1px solid var(--line);background:var(--panel);display:flex;gap:8px;align-items:center;flex-wrap:wrap}
#bar input[type=search]{background:#0d1117;border:1px solid var(--line);color:var(--fg);border-radius:6px;padding:4px 8px;font:12px system-ui;min-width:180px}
#bar label{font-size:12px;color:var(--dim);display:flex;align-items:center;gap:4px;cursor:pointer;user-select:none}
#chips{display:flex;gap:4px;flex-wrap:wrap}
.chip{border:1px solid var(--line);background:#0d1117;color:var(--dim);border-radius:999px;padding:2px 9px;font:11px ui-monospace,Consolas,monospace;cursor:pointer}
.chip.on{border-color:var(--accent);background:#1f6feb;color:#fff}
#stats{margin-left:auto;font:11px ui-monospace,Consolas,monospace;color:var(--dim);white-space:nowrap}
#files{overflow-y:auto;flex:1;padding:10px}
.f{border:1px solid var(--line);border-radius:6px;margin-bottom:10px;overflow:hidden}
.fh{display:flex;gap:8px;align-items:center;padding:6px 10px;background:var(--panel);cursor:pointer;font:12px ui-monospace,Consolas,monospace}
.fh:hover{background:#1c2128}
.fh .p{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;flex:1;color:var(--fg)}
.fh .st{font-size:10px;text-transform:uppercase;letter-spacing:.4px;color:var(--dim);border:1px solid var(--line);border-radius:4px;padding:0 5px}
.fh .n{white-space:nowrap}
.fh .n .a{color:var(--add)}.fh .n .d{color:var(--del)}
.fh .tw{color:var(--dim);width:10px;text-align:center}
pre.d{margin:0;overflow-x:auto;font:12px/1.45 ui-monospace,Consolas,monospace;background:#0d1117;border-top:1px solid var(--line)}
pre.d span{display:block;padding:0 10px;white-space:pre}
pre.d .pl{background:#1f4d2a4d;color:#aff5b4}
pre.d .mi{background:#5d1c1c4d;color:#ffdcd7}
pre.d .hh{background:#1e2a3a;color:#79c0ff}
pre.d .ct{color:#adbac7}
.note{color:var(--dim);padding:8px 10px;font-size:12px;font-style:italic}
.empty{color:var(--dim);padding:24px;text-align:center}
@media(max-width:720px){#cv{flex-direction:column;height:auto}#side{width:auto;max-height:34vh;border-right:0;border-bottom:1px solid var(--line)}#grip{display:none}}
</style></head><body>
<div id="cv">
  <div id="side">
    <div id="sidehead"><b id="branch">…</b><div id="baseline">loading commits…</div></div>
    <div id="clist"></div>
  </div>
  <div id="grip" title="Drag to resize"></div>
  <div id="main">
    <div id="bar">
      <input type="search" id="q" placeholder="filter files by path…" autocomplete="off">
      <div id="chips"></div>
      <label><input type="checkbox" id="notest"${hideTests ? ' checked' : ''}> hide tests</label>
      <span id="stats"></span>
    </div>
    <div id="files"><div class="empty">Select a commit.</div></div>
  </div>
</div>
<script>
(function(){
  var vsc = window.__wtapi || (window.__wtapi = acquireVsCodeApi());
  var SLUG=${JSON.stringify(slug)}, NAME=${JSON.stringify(name)};
  var commits=[], cur=null, files=[], exts=[], offExts={}, collapsed={};

  function esc(s){return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');}

  function renderCommits(){
    var el=document.getElementById('clist');
    if(!commits.length){ el.innerHTML='<div class="empty">No commits.</div>'; return; }
    el.innerHTML=commits.map(function(c){
      var work=c.sha===${JSON.stringify(WORKING)};
      return '<div class="c'+(work?' work':'')+(c.sha===cur?' sel':'')+'" data-sha="'+esc(c.sha)+'">'
        +'<div class="s">'+esc(c.subject)+'</div>'
        +'<div class="m">'+(work?'uncommitted':esc(c.short)+' · '+esc(c.author)+' · '+esc(c.when))+'</div></div>';
    }).join('');
    Array.prototype.forEach.call(el.querySelectorAll('.c'),function(n){
      n.onclick=function(){ select(n.dataset.sha); };
    });
  }
  function select(sha){
    cur=sha; renderCommits();
    document.getElementById('files').innerHTML='<div class="empty">Loading diff…</div>';
    vsc.postMessage({cmd:'cvSelect',slug:SLUG,name:NAME,sha:sha});
  }

  // visible = passes the path filter, the type chips, and the hide-tests toggle
  function visible(){
    var q=document.getElementById('q').value.trim().toLowerCase();
    var notest=document.getElementById('notest').checked;
    return files.filter(function(f){
      if(q && f.path.toLowerCase().indexOf(q)<0) return false;
      if(notest && f.test) return false;
      if(offExts[f.ext||'(none)']) return false;
      return true;
    });
  }
  function renderChips(){
    var el=document.getElementById('chips');
    el.innerHTML=exts.map(function(e){
      return '<span class="chip'+(offExts[e]?'':' on')+'" data-ext="'+esc(e)+'">'+esc(e)+'</span>';
    }).join('');
    Array.prototype.forEach.call(el.querySelectorAll('.chip'),function(n){
      n.onclick=function(){ var e=n.dataset.ext; offExts[e]=!offExts[e]; renderChips(); renderFiles(); };
    });
  }
  function body(f){
    if(f.untracked) return '<div class="note">Untracked — no previous version to diff against.</div>';
    if(f.binary)    return '<div class="note">Binary file.</div>';
    if(!f.patch)    return '<div class="note">No textual changes (mode or rename only).</div>';
    var html=f.patch.split('\\n').map(function(l){
      var c = l[0]==='+' ? 'pl' : l[0]==='-' ? 'mi' : l[0]==='@' ? 'hh' : 'ct';
      return '<span class="'+c+'">'+esc(l||' ')+'</span>';
    }).join('');
    if(f.truncated) html+='<span class="ct">… '+f.truncated+' more lines (truncated)</span>';
    return '<pre class="d">'+html+'</pre>';
  }
  function renderFiles(){
    var vis=visible(), el=document.getElementById('files');
    var a=0,d=0; vis.forEach(function(f){a+=f.adds;d+=f.dels;});
    document.getElementById('stats').textContent =
      vis.length+' / '+files.length+' files · +'+a+' −'+d;
    if(!cur){ el.innerHTML='<div class="empty">Select a commit.</div>'; return; }
    if(!files.length){ el.innerHTML='<div class="empty">No changes in this commit.</div>'; return; }
    if(!vis.length){ el.innerHTML='<div class="empty">No files match the filter.</div>'; return; }
    el.innerHTML=vis.map(function(f,i){
      var key=f.path, open=!collapsed[key];
      return '<div class="f"><div class="fh" data-k="'+esc(key)+'">'
        +'<span class="tw">'+(open?'▾':'▸')+'</span>'
        +'<span class="st">'+esc(f.status)+'</span>'
        +'<span class="p">'+esc(f.from?f.from+' → '+f.path:f.path)+'</span>'
        +'<span class="n"><span class="a">+'+f.adds+'</span> <span class="d">−'+f.dels+'</span></span>'
        +'</div>'+(open?body(f):'')+'</div>';
    }).join('');
    Array.prototype.forEach.call(el.querySelectorAll('.fh'),function(n){
      n.onclick=function(){ var k=n.dataset.k; collapsed[k]=!collapsed[k]; renderFiles(); };
    });
  }

  document.getElementById('q').addEventListener('input',renderFiles);
  document.getElementById('notest').addEventListener('change',renderFiles);

  // drag the divider to resize the commit list. The width is sent back to the extension rather than
  // kept in webview state, because setState is shared with the switcher bar's scroll restore.
  (function(){
    var grip=document.getElementById('grip'), side=document.getElementById('side'), on=false;
    grip.addEventListener('mousedown',function(e){ on=true; grip.classList.add('drag'); e.preventDefault(); });
    window.addEventListener('mousemove',function(e){
      if(!on) return;
      var w=e.clientX-side.getBoundingClientRect().left;
      w=Math.max(150,Math.min(w,Math.round(window.innerWidth*0.6)));
      side.style.width=w+'px';
    });
    window.addEventListener('mouseup',function(){
      if(!on) return;
      on=false; grip.classList.remove('drag');
      vsc.postMessage({cmd:'cvWidth',w:side.offsetWidth});
    });
  })();

  window.addEventListener('message',function(ev){
    var m=ev.data||{};
    if(m.type==='cvList'){
      commits=m.commits||[];
      document.getElementById('branch').textContent=m.branch||'';
      // three distinct states: ahead of a base / no commits ahead of a base / no base at all
      document.getElementById('baseline').textContent = !m.fallback
        ? (commits.length+' commit'+(commits.length===1?'':'s')+' ahead of '+m.base)
        : m.base
          ? ('no commits ahead of '+m.base+' — last '+commits.length)
          : ('last '+commits.length+' commits (no base branch)');
      if(m.dirty) commits=[{sha:${JSON.stringify(WORKING)},subject:'Uncommitted changes'}].concat(commits);
      var known=function(s){return s && commits.some(function(c){return c.sha===s;});};
      if(!known(cur)) cur = known(m.selected) ? m.selected : (commits.length?commits[0].sha:null);
      renderCommits();
      if(cur) select(cur);
    } else if(m.type==='cvDiff'){
      if(m.sha!==cur) return;                       // a stale reply for a commit we've navigated away from
      files=m.files||[];
      var seen={}; exts=[];
      files.forEach(function(f){ var e=f.ext||'(none)'; if(!seen[e]){seen[e]=1;exts.push(e);} });
      exts.sort();
      collapsed={};
      if(files.length>12) files.forEach(function(f){ collapsed[f.path]=true; });   // big commit → start folded
      renderChips(); renderFiles();
      if(m.omitted) document.getElementById('stats').textContent += ' · '+m.omitted+' files omitted';
    }
  });
  vsc.postMessage({cmd:'cvReady',slug:SLUG,name:NAME});
})();
</script></body></html>`;
}

module.exports = { COMMITS_LABEL, WORKING, isGitWorktree, listCommits, commitDiff, commitsHtml };
