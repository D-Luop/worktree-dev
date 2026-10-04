// WorkTreeDev Settings: an editor-tab webview for repos, Claude/Codex accounts, role defaults, GitHub
// and the daemon/tray. All reads and writes go through wtd.exe (`wtd repo|account|env …`, JSON out),
// so this file is UI + plumbing only. No top-level `require('vscode')`: the build gate loads this
// module in plain node to parse-check the page script (settingsHtml).

const cp = require('child_process');

class SettingsPanel {
  // deps: { vscode, wtdExe(), media (Uri), daemon (DaemonClient|null), daemonInstalled(), toggleDaemon(want), log(msg) }
  constructor(deps) { this.d = deps; this.panel = null; this._logins = new Map(); }

  open() {
    const vscode = this.d.vscode;
    if (this.panel) { this.panel.reveal(); this.reload(); return; }
    this.panel = vscode.window.createWebviewPanel('wtdSettings', 'WorkTreeDev Settings', vscode.ViewColumn.Active,
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [this.d.media] });
    const css = this.panel.webview.asWebviewUri(vscode.Uri.joinPath(this.d.media, 'codicon.css')).toString();
    this.panel.webview.html = settingsHtml().replace('__CODICON_HREF__', css);
    this.panel.webview.onDidReceiveMessage((m) => this._onMsg(m).catch((e) => this.d.log('settings: ' + (e && e.stack || e))));
    this.panel.onDidDispose(() => { this.panel = null; });
    this._closeSub = vscode.window.onDidCloseTerminal((t) => {   // a login terminal finished → re-read accounts / GitHub
      if (this._logins.has(t)) { this._logins.delete(t); setTimeout(() => this.reload(), 500); }
    });
  }

  dispose() { if (this._closeSub) this._closeSub.dispose(); if (this.panel) this.panel.dispose(); }

  post(m) { if (this.panel) this.panel.webview.postMessage(m); }

  // a daemon push (accounts/daemon state) → keep the page current while it's open
  onDaemonChange() { if (this.panel) this.post({ type: 'live', daemon: this._daemonState(), usage: this._usage() }); }

  _daemonState() { return { installed: this.d.daemonInstalled(), running: !!(this.d.daemon && this.d.daemon.running) }; }
  _usage() { return (this.d.daemon && this.d.daemon.accounts) || []; }

  // run `wtd <args>` → parsed JSON. onLine streams stdout lines (clone progress).
  wtd(args, onLine) {
    return new Promise((resolve, reject) => {
      const p = cp.spawn(this.d.wtdExe(), args, { windowsHide: true });
      let out = '', err = '', buf = '';
      p.stdout.on('data', (b) => {
        const s = b.toString(); out += s;
        if (onLine) { buf += s; let i; while ((i = buf.search(/[\r\n]/)) >= 0) { const l = buf.slice(0, i).trim(); buf = buf.slice(i + 1); if (l) onLine(l); } }
      });
      p.stderr.on('data', (b) => { const s = b.toString(); err += s; if (onLine) s.split(/[\r\n]+/).map((l) => l.trim()).filter(Boolean).forEach(onLine); });
      p.on('error', reject);
      p.on('close', (code) => {
        if (code !== 0) return reject(new Error((err.trim().split('\n').pop() || 'wtd ' + args.join(' ') + ' failed').replace(/^wtd \S+: /, '')));
        const last = out.trim().split('\n').pop() || 'null';
        try { resolve(JSON.parse(last)); } catch { resolve(null); }
      });
    });
  }

  async reload() {
    if (!this.panel) return;
    const [repos, accounts, env] = await Promise.all([
      this.wtd(['repo', 'ls']).catch((e) => ({ error: e.message })),
      this.wtd(['account', 'ls']).catch((e) => ({ error: e.message })),
      this.wtd(['env']).catch((e) => ({ error: e.message })),
    ]);
    this.post({ type: 'data', repos, accounts, env, daemon: this._daemonState(), usage: this._usage() });
  }

  terminal(name, env, command, why) {
    const vscode = this.d.vscode;
    const t = vscode.window.createTerminal({ name, env: env || {}, location: vscode.TerminalLocation.Editor });
    this._logins.set(t, true);
    t.show();
    t.sendText(command);
    if (why) vscode.window.showInformationMessage(why);
  }

  async _onMsg(m) {
    if (!m || !m.op) return;
    const vscode = this.d.vscode;
    const done = (ok, extra) => this.post(Object.assign({ type: 'done', op: m.op, key: m.key, ok }, extra || {}));
    const run = async (args, onLine, after) => {
      try { const r = await this.wtd(args, onLine); done(true, { result: r }); if (after !== false) this.reload(); return r; }
      catch (e) { done(false, { error: e.message }); return null; }
    };
    switch (m.op) {
      case 'ready': case 'reload': return this.reload();

      // --- repos ---
      case 'repoAdd': {
        const args = m.local ? ['repo', 'add', '--new', m.slug] : ['repo', 'add', m.slug, m.url];
        return run(args, (line) => this.post({ type: 'log', op: m.op, key: m.key, line }));
      }
      case 'repoFetch':
        return run(['repo', 'fetch', m.slug], (line) => this.post({ type: 'log', op: m.op, key: m.key, line }));
      case 'repoRm': {
        const ch = await vscode.window.showWarningMessage('Remove repository "' + m.slug + '"?', {
          modal: true, detail: 'Unregisters it from WorkTreeDev. "Remove + delete clone" also deletes repos/' + m.slug + '/ (the bare clone). Nothing on GitHub is touched.',
        }, 'Remove', 'Remove + delete clone');
        if (!ch) return done(false, { cancelled: true });
        return run(['repo', 'rm', m.slug].concat(ch === 'Remove + delete clone' ? ['--delete-clone'] : []));
      }
      case 'repoSetGithub': return run(['repo', 'set-github', m.slug, JSON.stringify(m.github)]);
      case 'repoTest': return run(['repo', 'test-issues', m.slug], null, false);
      case 'projectFields': return run(['repo', 'project-fields', m.owner, String(m.number)], null, false);

      // --- accounts ---
      case 'accountAdd': {
        const r = await run(['account', 'add', m.provider, m.name]);
        if (r && r.dir) this._login(m.provider, m.name, r.dir);
        return;
      }
      case 'accountLogin': return this._login(m.provider, m.name, m.dir);
      case 'accountRm': {
        const ch = await vscode.window.showWarningMessage('Remove the ' + m.provider + ' account "' + m.name + '"?',
          { modal: true, detail: 'Deletes ' + m.dir + ' — its login and local history. Roles that used it fall back to the default login.' }, 'Remove account');
        if (ch !== 'Remove account') return done(false, { cancelled: true });
        return run(['account', 'rm', m.provider, m.name]);
      }
      case 'roleSet': return run(['account', 'use', m.role, m.target]);
      case 'installCodex':
        return this.terminal('Install Codex CLI', {}, 'npm install -g @openai/codex', 'Installing the Codex CLI — Settings refreshes when you close the terminal.');

      // --- GitHub ---
      case 'ghLogin':
        return this.terminal('GitHub login', {}, 'gh auth login --hostname github.com --git-protocol https --web --scopes "read:project,project"',
          'Follow the browser prompt; close the terminal when it says "Logged in".');
      case 'ghScopes':
        return this.terminal('GitHub: project access', {}, 'gh auth refresh --hostname github.com -s read:project,project',
          'Grant the project scopes in the browser, then close the terminal.');

      // --- daemon / tray ---
      case 'daemonToggle': this.d.toggleDaemon(m.want); return;
      case 'trayLogon': return run(['tray', '--logon', m.on ? 'on' : 'off']);
      case 'traySpawn': return run(['tray', '--spawn'], null, false);
    }
  }

  _login(provider, name, dir) {
    const isDefault = name === 'default';
    if (provider === 'codex') {
      return this.terminal('Codex login · ' + name, isDefault ? {} : { CODEX_HOME: dir }, 'codex login',
        'Sign in to Codex in the browser; close the terminal when it reports success.');
    }
    // a fresh config dir starts Claude's own sign-in flow; an existing one needs /login
    return this.terminal('Claude login · ' + name, isDefault ? {} : { CLAUDE_CONFIG_DIR: dir }, 'claude',
      'Sign in to "' + name + '" (type /login if Claude does not ask), then /exit and close the terminal.');
  }
}

function settingsHtml() {
  return `<!DOCTYPE html><html><head><meta charset="utf-8">
<link rel="stylesheet" href="__CODICON_HREF__">
<style>
  :root{--gap:14px;}
  body{margin:0;padding:0;font:13px var(--vscode-font-family);color:var(--vscode-foreground);background:var(--vscode-editor-background);}
  .codicon{font-size:14px;line-height:1;vertical-align:-2px;}
  a{color:var(--vscode-textLink-foreground);text-decoration:none;cursor:pointer;} a:hover{text-decoration:underline;}
  .mono{font-family:var(--vscode-editor-font-family);font-size:12px;}
  .muted{color:var(--vscode-descriptionForeground);}
  .layout{display:flex;max-width:1080px;margin:0 auto;padding:22px 24px 60px;gap:28px;}
  nav{position:sticky;top:22px;align-self:flex-start;width:170px;flex:none;}
  nav .ttl{font-size:18px;font-weight:600;margin:0 0 14px 8px;}
  nav a{display:flex;align-items:center;gap:8px;padding:5px 8px;border-radius:4px;color:var(--vscode-foreground);text-decoration:none;}
  nav a:hover{background:var(--vscode-list-hoverBackground);text-decoration:none;}
  nav a.on{background:var(--vscode-list-activeSelectionBackground);color:var(--vscode-list-activeSelectionForeground);}
  main{flex:1;min-width:0;}
  section{margin-bottom:36px;scroll-margin-top:16px;}
  h2{font-size:15px;font-weight:600;margin:0 0 4px;display:flex;align-items:center;gap:8px;}
  .lead{margin:0 0 var(--gap);color:var(--vscode-descriptionForeground);max-width:680px;line-height:1.5;}
  h3{font-size:11px;font-weight:600;letter-spacing:.5px;text-transform:uppercase;color:var(--vscode-descriptionForeground);margin:18px 0 8px;}

  .card{border:1px solid var(--vscode-panel-border,rgba(127,127,127,.25));border-radius:6px;background:var(--vscode-sideBar-background,transparent);margin-bottom:10px;}
  .card .hd{display:flex;align-items:center;gap:10px;padding:10px 12px;}
  .card .hd .main{flex:1;min-width:0;}
  .card .name{font-weight:600;display:flex;align-items:center;gap:8px;flex-wrap:wrap;}
  .card .sub{color:var(--vscode-descriptionForeground);font-size:12px;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
  .card .body{border-top:1px solid var(--vscode-panel-border,rgba(127,127,127,.2));padding:12px;}
  .ico{width:30px;height:30px;border-radius:6px;display:flex;align-items:center;justify-content:center;flex:none;
       background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);font-weight:600;}
  .ico .codicon{font-size:16px;}
  .chip{display:inline-flex;align-items:center;gap:4px;font-size:11px;font-weight:400;padding:1px 7px;border-radius:9px;line-height:16px;
        border:1px solid var(--vscode-panel-border,rgba(127,127,127,.35));color:var(--vscode-descriptionForeground);}
  .chip.ok{color:var(--vscode-testing-iconPassed,#3fb950);border-color:currentColor;}
  .chip.warn{color:var(--vscode-editorWarning-foreground,#d2a000);border-color:currentColor;}
  .chip.role{background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);border-color:transparent;}
  .acts{display:flex;align-items:center;gap:4px;flex:none;}

  button{font:inherit;cursor:pointer;border-radius:3px;border:1px solid var(--vscode-button-border,transparent);padding:4px 11px;display:inline-flex;align-items:center;gap:6px;}
  button.primary{background:var(--vscode-button-background);color:var(--vscode-button-foreground);}
  button.primary:hover{background:var(--vscode-button-hoverBackground);}
  button.secondary{background:var(--vscode-button-secondaryBackground);color:var(--vscode-button-secondaryForeground);}
  button.secondary:hover{background:var(--vscode-button-secondaryHoverBackground);}
  button.icon{background:none;border:none;padding:4px;color:var(--vscode-icon-foreground);}
  button.icon:hover{background:var(--vscode-toolbar-hoverBackground);}
  button.icon.danger:hover{color:var(--vscode-errorForeground);}
  button:disabled{opacity:.5;cursor:default;}
  button:focus-visible{outline:1px solid var(--vscode-focusBorder);outline-offset:1px;}

  .row{display:flex;gap:12px;align-items:flex-start;margin-bottom:12px;}
  .row label.k{width:150px;flex:none;padding-top:5px;}
  .row .v{flex:1;min-width:0;}
  .help{font-size:12px;color:var(--vscode-descriptionForeground);margin-top:4px;line-height:1.45;}
  input[type=text],select{font:inherit;color:var(--vscode-input-foreground);background:var(--vscode-input-background);
       border:1px solid var(--vscode-input-border,var(--vscode-panel-border,transparent));border-radius:2px;padding:4px 6px;width:100%;max-width:420px;box-sizing:border-box;}
  select{background:var(--vscode-dropdown-background);color:var(--vscode-dropdown-foreground);border-color:var(--vscode-dropdown-border,transparent);}
  input[type=text]:focus,select:focus{outline:none;border-color:var(--vscode-focusBorder);}
  input::placeholder{color:var(--vscode-input-placeholderForeground);}
  .seg{display:inline-flex;border:1px solid var(--vscode-panel-border,rgba(127,127,127,.35));border-radius:4px;overflow:hidden;}
  .seg button{border:none;border-radius:0;background:none;color:var(--vscode-foreground);padding:4px 12px;}
  .seg button+button{border-left:1px solid var(--vscode-panel-border,rgba(127,127,127,.35));}
  .seg button.on{background:var(--vscode-inputOption-activeBackground,rgba(0,127,212,.4));color:var(--vscode-inputOption-activeForeground,inherit);}
  .chk{display:inline-flex;align-items:center;gap:6px;margin:2px 12px 2px 0;cursor:pointer;}
  .toggle{display:flex;align-items:center;gap:8px;cursor:pointer;}
  .note{display:flex;gap:8px;align-items:flex-start;padding:8px 10px;border-radius:4px;margin:8px 0;line-height:1.45;
        background:var(--vscode-textBlockQuote-background,rgba(127,127,127,.1));border-left:3px solid var(--vscode-textLink-foreground);}
  .note.warn{border-left-color:var(--vscode-editorWarning-foreground,#d2a000);}
  .note.err{border-left-color:var(--vscode-errorForeground);}
  .note.ok{border-left-color:var(--vscode-testing-iconPassed,#3fb950);}
  .log{font-family:var(--vscode-editor-font-family);font-size:12px;max-height:160px;overflow:auto;white-space:pre-wrap;margin-top:8px;padding:6px 8px;
       background:var(--vscode-terminal-background,var(--vscode-editor-background));border:1px solid var(--vscode-panel-border,transparent);border-radius:3px;}
  .bars{display:flex;gap:16px;margin-top:6px;}
  .bar{display:flex;align-items:center;gap:6px;font-size:11px;color:var(--vscode-descriptionForeground);}
  .track{width:90px;height:4px;border-radius:2px;background:var(--vscode-input-background,rgba(127,127,127,.2));overflow:hidden;}
  .fill{height:100%;border-radius:2px;}
  .empty{padding:14px;color:var(--vscode-descriptionForeground);}
  .grid2{display:grid;grid-template-columns:1fr;gap:0;}
  .spin{animation:sp 1s linear infinite;display:inline-block;} @keyframes sp{to{transform:rotate(360deg);}}
</style></head><body>
<div class="layout">
  <nav id="nav">
    <div class="ttl">Settings</div>
    <a data-to="repos"><i class="codicon codicon-repo"></i>Repositories</a>
    <a data-to="accounts"><i class="codicon codicon-account"></i>Accounts</a>
    <a data-to="defaults"><i class="codicon codicon-settings"></i>Defaults</a>
    <a data-to="github"><i class="codicon codicon-github"></i>GitHub</a>
    <a data-to="daemon"><i class="codicon codicon-server-process"></i>Daemon &amp; tray</a>
  </nav>
  <main id="main"><div class="empty"><i class="codicon codicon-loading spin"></i> Loading…</div></main>
</div>
<script>
  const vsc = acquireVsCodeApi();
  window.addEventListener('error', e => { try{ vsc.postMessage({op:'jsError', msg:String(e.message||e)}); }catch(_){} });
  let D = null;                       // last data payload
  const ui = { open:{}, addMode:'clone', logs:{}, busy:{}, results:{}, forms:{}, fields:{} };
  const esc = s => String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
  const ico = (n,x) => '<i class="codicon codicon-'+n+(x?' '+x:'')+'"></i>';
  const send = m => vsc.postMessage(m);
  const busy = k => !!ui.busy[k];
  function col(p){ return p>=90?'var(--vscode-charts-red,#e5534b)':p>=70?'var(--vscode-charts-yellow,#d2a000)':'var(--vscode-charts-green,#3fb950)'; }

  // ---------- Repositories ----------
  function ghSummary(r){
    const g=r.github||{}; const s=g.issueSource;
    if(!s) return (r.github_detected?'GitHub '+r.github_detected+' · ':'')+'no issue source';
    if(s.kind==='repo') return 'Issues from '+s.repo;
    return 'Project #'+s.number+' ('+s.owner+')'+(s.offer&&s.offer.length?' · '+s.offer.join(', '):'');
  }
  function repoCard(r){
    const k='repo:'+r.slug, open=!!ui.open[k];
    const badges=[
      r.default_branch?'<span class="chip">'+ico('git-branch')+esc(r.default_branch)+'</span>':'',
      '<span class="chip">'+r.worktrees+' worktree'+(r.worktrees===1?'':'s')+(r.archived?' · '+r.archived+' archived':'')+'</span>',
      r.local_only?'<span class="chip warn">local only</span>':'',
      !r.cloned?'<span class="chip warn">not cloned</span>':'',
    ].join('');
    const log=ui.logs[k]?'<div class="log">'+esc(ui.logs[k].join('\\n'))+'</div>':'';
    return '<div class="card"><div class="hd">'
      +'<div class="ico">'+ico('repo')+'</div>'
      +'<div class="main"><div class="name"><span>'+esc(r.slug)+'</span>'+badges+'</div>'
      +'<div class="sub mono" title="'+esc(r.url)+'">'+esc(r.local_only?'local repository (no remote yet)':r.url)+'</div>'
      +'<div class="sub">'+ico('issues')+' '+esc(ghSummary(r))+'</div></div>'
      +'<div class="acts">'
      +(r.local_only?'':'<button class="icon" title="Fetch from origin" data-act="repoFetch" data-slug="'+esc(r.slug)+'"'+(busy(k)?' disabled':'')+'>'+ico(busy(k)?'loading':'sync', busy(k)?'spin':'')+'</button>')
      +'<button class="icon" title="GitHub link & issue source" data-act="toggle" data-k="'+k+'">'+ico(open?'chevron-up':'chevron-down')+'</button>'
      +'<button class="icon danger" title="Remove repository" data-act="repoRm" data-slug="'+esc(r.slug)+'"'+(r.worktrees||r.archived?' disabled':'')+'>'+ico('trash')+'</button>'
      +'</div></div>'
      +(open?'<div class="body">'+ghForm(r)+'</div>':'')+(log&&!open?'<div class="body">'+log+'</div>':'')
      +'</div>';
  }
  function ghForm(r){
    const k='repo:'+r.slug;
    const f = ui.forms[k] || (ui.forms[k] = JSON.parse(JSON.stringify(r.github||{})));
    const src = f.issueSource || null; const kind = src?src.kind:'none';
    const ghRepo = f.repo || r.github_detected || '';
    let h = '<div class="row"><label class="k">GitHub repository</label><div class="v"><input type="text" data-f="repo" data-k="'+k+'" value="'+esc(f.repo||'')+'" placeholder="'+esc(r.github_detected||'owner/name')+'">'
      +'<div class="help">Where the code lives. Detected from the clone URL when left blank.</div></div></div>'
      +'<div class="row"><label class="k">Issue source</label><div class="v"><div class="seg">'
      +['none:None','repo:Repo issues','project:Project board'].map(x=>{const [v,l]=x.split(':'); return '<button data-act="kind" data-k="'+k+'" data-v="'+v+'" class="'+(kind===v?'on':'')+'">'+l+'</button>';}).join('')
      +'</div><div class="help">What New Session offers to work on for this repo.</div></div></div>';
    if(kind==='repo'){
      h += '<div class="row"><label class="k">Issues repository</label><div class="v"><input type="text" data-f="src.repo" data-k="'+k+'" value="'+esc(src.repo||ghRepo)+'" placeholder="owner/name"><div class="help">Usually the same repo; point it at a shared tracker repo if you keep issues elsewhere.</div></div></div>'
        + '<div class="row"><label class="k">Labels</label><div class="v"><input type="text" data-f="src.labels" data-k="'+k+'" value="'+esc((src.labels||[]).join(', '))+'" placeholder="any label"><div class="help">Comma-separated; only issues with all of these labels.</div></div></div>'
        + '<div class="row"><label class="k">Assignee</label><div class="v"><input type="text" data-f="src.assignee" data-k="'+k+'" value="'+esc(src.assignee||'')+'" placeholder="anyone (or @me)"></div></div>';
    } else if(kind==='project'){
      const fields = ui.fields[k];
      const url = src.url || (src.owner&&src.number?('https://github.com/'+(src.ownerType==='org'?'orgs':'users')+'/'+src.owner+'/projects/'+src.number):'');
      h += '<div class="row"><label class="k">Project URL</label><div class="v"><div style="display:flex;gap:6px;max-width:520px"><input type="text" data-f="src.url" data-k="'+k+'" value="'+esc(url)+'" placeholder="https://github.com/users/you/projects/1">'
        + '<button class="secondary" data-act="loadFields" data-k="'+k+'"'+(busy(k+':fields')?' disabled':'')+'>'+ico(busy(k+':fields')?'loading':'refresh',busy(k+':fields')?'spin':'')+'Load columns</button></div>'
        + '<div class="help">Paste the board’s URL, then load its columns.</div></div></div>';
      if(fields && fields.length){
        const sf = src.statusField || (fields.find(x=>/status/i.test(x.name))||fields[0]).name;
        const opts = (fields.find(x=>x.name===sf)||{options:[]}).options||[];
        h += '<div class="row"><label class="k">Status field</label><div class="v"><select data-f="src.statusField" data-k="'+k+'">'+fields.map(x=>'<option'+(x.name===sf?' selected':'')+'>'+esc(x.name)+'</option>').join('')+'</select></div></div>'
          + '<div class="row"><label class="k">Offer columns</label><div class="v">'+opts.map(o=>'<label class="chk"><input type="checkbox" data-f="src.offer" data-k="'+k+'" value="'+esc(o)+'"'+((src.offer||[]).includes(o)?' checked':'')+'>'+esc(o)+'</label>').join('')
          + '<div class="help">Only items in these columns appear in New Session (none checked = all).</div></div></div>'
          + '<div class="row"><label class="k">On session start</label><div class="v"><select data-f="src.onStart" data-k="'+k+'"><option value="">Leave the card where it is</option>'
          + opts.map(o=>'<option'+(src.onStart===o?' selected':'')+'>'+esc(o)+'</option>').join('')+'</select><div class="help">Move the card to this column when a session starts on it (needs the project scope).</div></div></div>';
      } else if(fields){ h += '<div class="note warn">'+ico('warning')+'<div>This project has no single-select fields to use as columns.</div></div>'; }
    }
    const res = ui.results[k];
    if(res) h += '<div class="note '+(res.ok?'ok':'err')+'">'+ico(res.ok?'pass':'error')+'<div>'+res.html+'</div></div>';
    h += '<div style="display:flex;gap:6px;margin-top:6px">'
      + '<button class="primary" data-act="saveGh" data-k="'+k+'" data-slug="'+esc(r.slug)+'"'+(busy(k+':save')?' disabled':'')+'>'+ico('save')+'Save</button>'
      + (kind!=='none'?'<button class="secondary" data-act="testGh" data-k="'+k+'" data-slug="'+esc(r.slug)+'"'+(busy(k+':test')?' disabled':'')+'>'+ico(busy(k+':test')?'loading':'beaker',busy(k+':test')?'spin':'')+'Save &amp; test</button>':'')
      + '</div>';
    return h;
  }
  function addRepoCard(){
    const k='repo:+', m=ui.addMode, f=ui.forms[k]||(ui.forms[k]={});
    const log=ui.logs[k]?'<div class="log">'+esc(ui.logs[k].join('\\n'))+'</div>':'';
    const res=ui.results[k];
    return '<div class="card"><div class="hd"><div class="ico">'+ico('add')+'</div><div class="main"><div class="name">Add a repository</div>'
      + '<div class="sub">Clone it once as a bare repo; every worktree shares that clone.</div></div></div><div class="body">'
      + '<div class="row"><label class="k">Source</label><div class="v"><div class="seg">'
      + '<button data-act="addMode" data-v="clone" class="'+(m==='clone'?'on':'')+'">Clone from URL</button><button data-act="addMode" data-v="local" class="'+(m==='local'?'on':'')+'">New local repo</button></div>'
      + '<div class="help">'+(m==='clone'?'Any git URL your git can authenticate to.':'An empty repo on <span class="mono">main</span> with no remote — add GitHub later.')+'</div></div></div>'
      + (m==='clone'?'<div class="row"><label class="k">Git URL</label><div class="v"><input type="text" id="addUrl" value="'+esc(f.url||'')+'" placeholder="https://github.com/owner/repo"></div></div>':'')
      + '<div class="row"><label class="k">Short name</label><div class="v"><input type="text" id="addSlug" value="'+esc(f.slug||'')+'" placeholder="e.g. app"><div class="help">Used in worktree paths and on the roster. Letters, digits, - and _.</div></div></div>'
      + (res?'<div class="note '+(res.ok?'ok':'err')+'">'+ico(res.ok?'pass':'error')+'<div>'+res.html+'</div></div>':'')
      + '<button class="primary" data-act="repoAdd"'+(busy(k)?' disabled':'')+'>'+ico(busy(k)?'loading':'cloud-download',busy(k)?'spin':'')+(m==='clone'?'Clone':'Create')+'</button>'
      + log + '</div></div>';
  }

  // ---------- Accounts ----------
  function usageFor(a){ if(a.provider!=='claude') return null; return (D.usage||[]).find(u=>u.name===a.name)||null; }
  function bar(l,v){ const p=typeof v==='number'?Math.round(v):null; return '<span class="bar">'+l+'<span class="track"><span class="fill" style="display:block;width:'+(p||0)+'%;background:'+col(p||0)+'"></span></span>'+(p==null?'--':p+'%')+'</span>'; }
  const ROLE_LABEL={dev:'Sessions',review:'Reviews',assistant:'Assistant'};
  function accountCard(a){
    const u=usageFor(a);
    const roles=(a.roles||[]).map(r=>'<span class="chip role" title="Default for '+ROLE_LABEL[r].toLowerCase()+'">'+ROLE_LABEL[r]+'</span>').join('');
    const status=a.logged_in?'<span class="chip ok">'+ico('pass')+'logged in</span>':'<span class="chip warn">'+ico('circle-slash')+'not logged in</span>';
    return '<div class="card"><div class="hd">'
      + '<div class="ico">'+esc((a.name==='default'?(a.email||'d'):a.name).slice(0,1).toUpperCase())+'</div>'
      + '<div class="main"><div class="name"><span>'+esc(a.name)+'</span>'+status+(a.plan?'<span class="chip">'+esc(a.plan)+'</span>':'')+roles+'</div>'
      + '<div class="sub">'+esc(a.email||(a.logged_in?'':'Log in to use this account'))+' <span class="mono muted">· '+esc(a.dir)+'</span></div>'
      + (u&&a.logged_in?'<div class="bars">'+bar('5h',u.five_hour&&u.five_hour.used)+bar('7d',u.seven_day&&u.seven_day.used)+'</div>':'')
      + '</div><div class="acts">'
      + '<button class="secondary" data-act="accountLogin" data-p="'+a.provider+'" data-n="'+esc(a.name)+'" data-dir="'+esc(a.dir)+'"'+(a.provider==='codex'&&!D.env.codex?' disabled title="Install the Codex CLI first"':'')+'>'+ico('sign-in')+(a.logged_in?'Re-login':'Log in')+'</button>'
      + (a.name!=='default'?'<button class="icon danger" title="Remove account" data-act="accountRm" data-p="'+a.provider+'" data-n="'+esc(a.name)+'" data-dir="'+esc(a.dir)+'">'+ico('trash')+'</button>':'')
      + '</div></div></div>';
  }
  function accountsSection(){
    const accts=Array.isArray(D.accounts)?D.accounts:[];
    const claude=accts.filter(a=>a.provider==='claude'), codex=accts.filter(a=>a.provider==='codex');
    const f=ui.forms['acct:+']||(ui.forms['acct:+']={provider:'claude'});
    let h='<h3>'+ico('sparkle')+' Claude</h3>'+claude.map(accountCard).join('');
    h+='<h3>'+ico('symbol-misc')+' Codex</h3>';
    if(!D.env.codex) h+='<div class="note warn">'+ico('info')+'<div>The Codex CLI isn’t installed. <a data-act="installCodex">Install it</a> (npm i -g @openai/codex), then log in below. Then pick a Codex account in New Session.</div></div>';
    h+=codex.map(accountCard).join('');
    h+='<div class="card"><div class="hd"><div class="ico">'+ico('person-add')+'</div><div class="main"><div class="name">Add an account</div>'
      +'<div class="sub">Each account is its own login with separate usage and billing.</div></div></div><div class="body">'
      +'<div class="row"><label class="k">Provider</label><div class="v"><div class="seg">'
      +'<button data-act="acctProv" data-v="claude" class="'+(f.provider==='claude'?'on':'')+'">Claude</button><button data-act="acctProv" data-v="codex" class="'+(f.provider==='codex'?'on':'')+'">Codex</button></div></div></div>'
      +'<div class="row"><label class="k">Name</label><div class="v"><input type="text" id="acctName" value="'+esc(f.name||'')+'" placeholder="e.g. work"><div class="help">A label for this login. You’ll sign in right after it’s created.</div></div></div>'
      +(ui.results['acct:+']?'<div class="note '+(ui.results['acct:+'].ok?'ok':'err')+'">'+ico(ui.results['acct:+'].ok?'pass':'error')+'<div>'+ui.results['acct:+'].html+'</div></div>':'')
      +'<button class="primary" data-act="accountAdd"'+(f.provider==='codex'&&!D.env.codex?' disabled':'')+'>'+ico('add')+'Add &amp; log in</button></div></div>';
    return h;
  }

  // ---------- Defaults ----------
  function defaultsSection(){
    const accts=(Array.isArray(D.accounts)?D.accounts:[]);
    const cur=r=>{ const a=accts.find(x=>(x.roles||[]).includes(r)); return a?a.provider+':'+a.name:'claude:default'; };
    const sel=(role,label,help,allowCodex)=>'<div class="row"><label class="k">'+label+'</label><div class="v"><select data-act="role" data-role="'+role+'">'
      + accts.map(a=>{ const v=a.provider+':'+a.name; const dis=(a.provider==='codex'&&!allowCodex)||(!a.logged_in&&v!==cur(role));
          return '<option value="'+esc(v)+'"'+(v===cur(role)?' selected':'')+(dis?' disabled':'')+'>'+(a.provider==='codex'?'Codex':'Claude')+' · '+esc(a.name)+(a.email?' ('+esc(a.email)+')':'')+(a.logged_in?'':' — not logged in')+'</option>'; }).join('')
      + '</select><div class="help">'+help+'</div></div></div>';
    return sel('dev','New sessions','The login new agent sessions run under — Claude or Codex (you can still pick another per session).',true)
      + sel('review','Reviews','The reviewer and skeptic bill here.',false)
      + sel('assistant','Assistant','The fleet assistant session.',false);
  }

  // ---------- GitHub ----------
  function githubSection(){
    const g=D.env.github||{};
    if(!g.installed) return '<div class="note warn">'+ico('warning')+'<div>The GitHub CLI isn’t installed: <span class="mono">winget install GitHub.cli</span></div></div>';
    if(!g.logged_in) return '<div class="note warn">'+ico('github')+'<div>Not logged in to GitHub. Issue pickers need it.</div></div><button class="primary" data-act="ghLogin">'+ico('sign-in')+'Log in to GitHub</button>';
    return '<div class="card"><div class="hd"><div class="ico">'+ico('github')+'</div><div class="main"><div class="name">'+esc(g.login)+'<span class="chip ok">'+ico('pass')+'logged in</span>'
      + (g.has_project_scope?'<span class="chip ok">project access</span>':'<span class="chip warn">no project access</span>')+'</div>'
      + '<div class="sub">scopes: '+esc((g.scopes||[]).join(', ')||'none')+'</div></div><div class="acts">'
      + (g.has_project_scope?'':'<button class="secondary" data-act="ghScopes">'+ico('key')+'Grant project access</button>')
      + '<button class="icon" title="Re-check" data-act="reload">'+ico('refresh')+'</button></div></div></div>'
      + (g.has_project_scope?'':'<div class="help">Project boards as an issue source need the <span class="mono">read:project</span> scope (and <span class="mono">project</span> to move cards).</div>');
  }

  // ---------- Daemon & tray ----------
  function daemonSection(){
    const d=D.daemon||{};
    if(!d.installed) return '<div class="note warn">'+ico('warning')+'<div>wtd.exe isn’t installed — run <span class="mono">install.sh</span> (needs Rust).</div></div>';
    return '<div class="card"><div class="hd"><div class="ico">'+ico('server-process')+'</div><div class="main"><div class="name">Daemon'
      + (d.running?'<span class="chip ok">'+ico('circle-filled')+'running</span>':'<span class="chip">'+ico('circle-outline')+'stopped</span>')+'</div>'
      + '<div class="sub">Live status, git state, usage and metrics. It never starts by itself; sessions keep running when it stops.</div></div><div class="acts">'
      + (d.running?'<button class="secondary" data-act="daemon" data-v="0">'+ico('debug-stop')+'Stop</button>':'<button class="primary" data-act="daemon" data-v="1">'+ico('play')+'Start</button>')
      + '</div></div></div>'
      + '<div class="card"><div class="hd"><div class="ico">'+ico('pulse')+'</div><div class="main"><div class="name">Tray icon</div>'
      + '<div class="sub">Start/stop the daemon and jump to this window from the notification area.</div></div><div class="acts">'
      + '<button class="secondary" data-act="traySpawn">'+ico('eye')+'Show icon</button></div></div>'
      + '<div class="body"><label class="toggle"><input type="checkbox" data-act="trayLogon"'+(D.env.tray_logon?' checked':'')+'> Start the tray icon when I sign in to Windows</label>'
      + '<div class="help">Only the icon starts — not the daemon. New icons land in the taskbar’s ^ overflow; drag it onto the taskbar to keep it visible.</div></div></div>';
  }

  function section(id,icon,title,lead,body){ return '<section id="'+id+'"><h2>'+ico(icon)+title+'</h2>'+(lead?'<p class="lead">'+lead+'</p>':'')+body+'</section>'; }
  function render(){
    if(!D) return;
    const repos=Array.isArray(D.repos)?D.repos:[];
    const scroll=document.scrollingElement.scrollTop;
    const focusId=document.activeElement&&document.activeElement.id;
    document.getElementById('main').innerHTML =
        section('repos','repo','Repositories','Each repo is cloned once; worktrees branch off it. Link a repo to GitHub to pick issues when starting a session.',
          (D.repos&&D.repos.error?'<div class="note err">'+esc(D.repos.error)+'</div>':'')+(repos.length?repos.map(repoCard).join(''):'<div class="empty">No repositories yet.</div>')+addRepoCard())
      + section('accounts','account','Accounts','Claude and Codex logins. Usage and billing follow the account a session runs under.', accountsSection())
      + section('defaults','settings','Defaults','Which login each kind of work uses unless you choose otherwise.', defaultsSection())
      + section('github','github','GitHub','Used to list issues and project items in New Session.', githubSection())
      + section('daemon','server-process','Daemon & tray','', daemonSection());
    document.scrollingElement.scrollTop=scroll;
    if(focusId){ const el=document.getElementById(focusId); if(el){ el.focus(); if(el.setSelectionRange) el.setSelectionRange(el.value.length, el.value.length); } }
    spy();
  }

  // ---------- form state ----------
  function setPath(o,path,v){ const ks=path.split('.'); let x=o; for(let i=0;i<ks.length-1;i++){ x[ks[i]]=x[ks[i]]||{}; x=x[ks[i]]; } x[ks[ks.length-1]]=v; }
  document.addEventListener('input', e=>{
    const t=e.target;
    if(t.id==='addUrl'){ const f=ui.forms['repo:+']; f.url=t.value; const m=t.value.match(/([^\\/:]+?)(\\.git)?\\/?$/); if(m&&!f.slugTouched){ f.slug=m[1].toLowerCase().replace(/[^a-z0-9_-]/g,'-'); const s=document.getElementById('addSlug'); if(s) s.value=f.slug; } return; }
    if(t.id==='addSlug'){ ui.forms['repo:+'].slug=t.value; ui.forms['repo:+'].slugTouched=true; return; }
    if(t.id==='acctName'){ ui.forms['acct:+'].name=t.value; return; }
    const k=t.dataset.k, fp=t.dataset.f; if(!k||!fp) return;
    const f=ui.forms[k];
    if(fp==='src.labels') setPath(f,'issueSource.labels',t.value.split(',').map(s=>s.trim()).filter(Boolean));
    else if(fp==='src.offer'){ const all=[...document.querySelectorAll('input[data-f="src.offer"][data-k="'+k+'"]')].filter(x=>x.checked).map(x=>x.value); setPath(f,'issueSource.offer',all); }
    else if(fp==='src.url'){ setPath(f,'issueSource.url',t.value); parseProjectUrl(f); }
    else if(fp==='src.statusField'){ setPath(f,'issueSource.statusField',t.value); render(); }
    else if(fp.startsWith('src.')) setPath(f,'issueSource.'+fp.slice(4),t.value);
    else f[fp]=t.value;
  });
  document.addEventListener('change', e=>{
    const t=e.target;
    if(t.dataset.act==='role') send({op:'roleSet', role:t.dataset.role, target:t.value});
    else if(t.dataset.act==='trayLogon') send({op:'trayLogon', on:t.checked});
    else if(t.dataset.f==='src.onStart'||t.dataset.f==='src.statusField'){ const f=ui.forms[t.dataset.k]; setPath(f,'issueSource.'+t.dataset.f.slice(4),t.value); }
  });
  function parseProjectUrl(f){
    const s=f.issueSource; const m=(s.url||'').match(/github\\.com\\/(orgs|users)\\/([^\\/]+)\\/projects\\/(\\d+)/);
    if(m){ s.ownerType=m[1]==='orgs'?'org':'user'; s.owner=m[2]; s.number=parseInt(m[3],10); }
  }
  function cleanGithub(f){
    const out={}; if(f.repo) out.repo=f.repo;
    const s=f.issueSource;
    if(s&&s.kind==='repo') out.issueSource={kind:'repo', repo:s.repo, labels:s.labels||[], assignee:s.assignee||''};
    if(s&&s.kind==='project') out.issueSource={kind:'project', owner:s.owner, ownerType:s.ownerType||'user', number:s.number,
        statusField:s.statusField||'Status', offer:s.offer||[], onStart:s.onStart||''};
    return Object.keys(out).length?out:null;
  }

  // ---------- actions ----------
  document.addEventListener('click', e=>{
    const n=e.target.closest('[data-to]'); if(n){ document.getElementById(n.dataset.to).scrollIntoView({behavior:'smooth'}); return; }
    const b=e.target.closest('[data-act]'); if(!b||b.disabled) return;
    const a=b.dataset.act, k=b.dataset.k;
    if(a==='role'||a==='trayLogon') return;   // handled on change
    if(a==='toggle'){ ui.open[k]=!ui.open[k]; render(); }
    else if(a==='kind'){ const f=ui.forms[k]; const v=b.dataset.v; const slug=k.slice(5); const r=D.repos.find(x=>x.slug===slug)||{};
      f.issueSource = v==='none'?null:(v==='repo'?{kind:'repo',repo:(f.repo||r.github_detected||''),labels:[],assignee:''}:{kind:'project'});
      delete ui.results[k]; render(); }
    else if(a==='loadFields'){ const f=ui.forms[k]; parseProjectUrl(f); const s=f.issueSource;
      if(!s.owner||!s.number){ ui.results[k]={ok:false,html:'Paste a project URL like https://github.com/users/you/projects/1'}; return render(); }
      ui.busy[k+':fields']=true; render(); send({op:'projectFields', key:k, owner:s.owner, number:s.number}); }
    else if(a==='saveGh'||a==='testGh'){ const f=ui.forms[k]; if(f.issueSource&&f.issueSource.kind==='project') parseProjectUrl(f);
      ui.busy[k+':'+(a==='saveGh'?'save':'test')]=true; delete ui.results[k]; render();
      send({op:'repoSetGithub', key:k, slug:b.dataset.slug, github:cleanGithub(f), then:a==='testGh'?'test':null}); }
    else if(a==='repoFetch'){ const kk='repo:'+b.dataset.slug; ui.busy[kk]=true; ui.logs[kk]=[]; render(); send({op:'repoFetch', key:kk, slug:b.dataset.slug}); }
    else if(a==='repoRm') send({op:'repoRm', slug:b.dataset.slug});
    else if(a==='addMode'){ ui.addMode=b.dataset.v; delete ui.results['repo:+']; render(); }
    else if(a==='repoAdd'){ const f=ui.forms['repo:+']; const local=ui.addMode==='local';
      if(!f.slug||(!local&&!f.url)){ ui.results['repo:+']={ok:false,html:local?'Give it a short name.':'Enter a Git URL and a short name.'}; return render(); }
      ui.busy['repo:+']=true; ui.logs['repo:+']=[]; delete ui.results['repo:+']; render(); send({op:'repoAdd', key:'repo:+', slug:f.slug, url:f.url, local}); }
    else if(a==='acctProv'){ ui.forms['acct:+'].provider=b.dataset.v; render(); }
    else if(a==='accountAdd'){ const f=ui.forms['acct:+']; if(!f.name){ ui.results['acct:+']={ok:false,html:'Give the account a name.'}; return render(); }
      delete ui.results['acct:+']; send({op:'accountAdd', key:'acct:+', provider:f.provider, name:f.name}); }
    else if(a==='accountLogin') send({op:'accountLogin', provider:b.dataset.p, name:b.dataset.n, dir:b.dataset.dir});
    else if(a==='accountRm') send({op:'accountRm', provider:b.dataset.p, name:b.dataset.n, dir:b.dataset.dir});
    else if(a==='installCodex') send({op:'installCodex'});
    else if(a==='ghLogin') send({op:'ghLogin'});
    else if(a==='ghScopes') send({op:'ghScopes'});
    else if(a==='reload') send({op:'reload'});
    else if(a==='daemon') send({op:'daemonToggle', want:b.dataset.v==='1'});
    else if(a==='traySpawn') send({op:'traySpawn'});
  });

  // nav highlight follows scroll
  function spy(){ const ids=['repos','accounts','defaults','github','daemon']; let cur=ids[0];
    for(const id of ids){ const s=document.getElementById(id); if(s && s.getBoundingClientRect().top<120) cur=id; }
    document.querySelectorAll('nav a').forEach(a=>a.classList.toggle('on', a.dataset.to===cur)); }
  window.addEventListener('scroll', spy, {passive:true});

  window.addEventListener('message', e=>{
    const m=e.data; if(!m) return;
    if(m.type==='data'){ D=m;
      // fresh server state for collapsed cards; an open form keeps its unsaved edits across reloads
      Object.keys(ui.forms).forEach(k=>{ if(k.startsWith('repo:') && k!=='repo:+' && !ui.open[k]) delete ui.forms[k]; });
      render(); }
    else if(m.type==='live'){ if(D){ D.daemon=m.daemon; D.usage=m.usage; render(); } }
    else if(m.type==='log'){ (ui.logs[m.key]=ui.logs[m.key]||[]).push(m.line); if(ui.logs[m.key].length>200) ui.logs[m.key].shift();
      render(); }
    else if(m.type==='done'){
      const k=m.key;
      if(m.op==='repoAdd'){ ui.busy[k]=false; ui.results[k]=m.ok?{ok:true,html:'Added. Start a session on it from New Session.'}:(m.cancelled?null:{ok:false,html:esc(m.error)}); if(m.ok){ ui.forms['repo:+']={}; } }
      else if(m.op==='repoFetch'){ ui.busy[k]=false; if(m.ok) setTimeout(()=>{ delete ui.logs[k]; render(); }, 4000); else (ui.logs[k]=ui.logs[k]||[]).push('✗ '+m.error); }
      else if(m.op==='repoSetGithub'){
        ui.busy[k+':save']=false;
        if(!m.ok){ ui.busy[k+':test']=false; ui.results[k]={ok:false,html:esc(m.error)}; }
        else if(ui.busy[k+':test']){ send({op:'repoTest', key:k, slug:k.slice(5)}); return; }
        else ui.results[k]={ok:true,html:'Saved.'};
      }
      else if(m.op==='repoTest'){ ui.busy[k+':test']=false;
        if(!m.ok) ui.results[k]={ok:false,html:esc(m.error).replace(/\\n/g,'<br>')};
        else { const r=m.result||{}; const items=(r.sample||[]).map(s=>esc(s.number?'#'+s.number+' '+s.title:s.title+(s.status?' ('+s.status+')':''))).join(' · ');
          ui.results[k]={ok:true,html:'Saved — <b>'+r.count+'</b> item'+(r.count===1?'':'s')+' would be offered'+(r.total!=null?' (of '+r.total+' on the board)':'')+(items?': '+items:'.')}; } }
      else if(m.op==='projectFields'){ ui.busy[k+':fields']=false;
        if(!m.ok){ ui.results[k]={ok:false,html:esc(m.error).replace(/\\n/g,'<br>')}; }
        else { ui.fields[k]=(m.result&&m.result.fields)||[]; const s=ui.forms[k].issueSource; if(!s.statusField){ const f=ui.fields[k].find(x=>/status/i.test(x.name))||ui.fields[k][0]; if(f) s.statusField=f.name; } delete ui.results[k]; } }
      else if(m.op==='accountAdd'){ ui.results[k]=m.ok?{ok:true,html:'Created — finish signing in in the terminal that just opened.'}:{ok:false,html:esc(m.error)}; if(m.ok) ui.forms['acct:+']={provider:ui.forms['acct:+'].provider}; }
      render();
    }
  });
  send({op:'ready'});
</script></body></html>`;
}

module.exports = { SettingsPanel, settingsHtml };
