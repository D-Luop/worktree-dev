// WorkTreeDev Resources: an editor-tab webview with the daemon's per-session CPU / memory / disk I/O
// (expandable to each session's top processes), the biggest programs outside the fleet, and disk
// usage per worktree. Data comes from the daemon's `metrics` pushes; the daemon samples only while a
// view that shows them is visible. No top-level `require('vscode')`: the build gate loads this module
// in plain node to parse-check the page script (resourcesHtml).

class ResourcesPanel {
  // deps: { vscode, media (Uri), daemon () => DaemonClient|null, onVisibility () => void }
  constructor(deps) { this.d = deps; this.panel = null; }

  visible() { return !!(this.panel && this.panel.visible); }

  open() {
    const vscode = this.d.vscode;
    if (this.panel) { this.panel.reveal(); return; }
    this.panel = vscode.window.createWebviewPanel('wtdResources', 'Resources', vscode.ViewColumn.Active,
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [this.d.media] });
    this.panel.iconPath = new vscode.ThemeIcon('pulse');
    const css = this.panel.webview.asWebviewUri(vscode.Uri.joinPath(this.d.media, 'codicon.css')).toString();
    this.panel.webview.html = resourcesHtml().replace('__CODICON_HREF__', css);
    this.panel.webview.onDidReceiveMessage((m) => this._onMsg(m));
    this.panel.onDidChangeViewState(() => { this.d.onVisibility(); if (this.panel && this.panel.visible) this.update(); });
    this.panel.onDidDispose(() => { this.panel = null; this.d.onVisibility(); });
    this.d.onVisibility();
  }

  dispose() { if (this.panel) this.panel.dispose(); }

  // a metrics / daemon push
  update() {
    if (!this.visible()) return;
    const dc = this.d.daemon();
    this.panel.webview.postMessage({ type: 'metrics', running: !!(dc && dc.running), m: dc && dc.running ? dc.metrics : null });
  }

  _onMsg(m) {
    const dc = this.d.daemon();
    if (m.cmd === 'ready') this.update();
    else if (m.cmd === 'refreshDisk' && dc) dc.request('resources.disk_refresh').catch((e) => this.d.vscode.window.showErrorMessage('Refresh disk: ' + e.message));
  }
}

function resourcesHtml() {
  return `<!DOCTYPE html><html><head><meta charset="utf-8">
<link rel="stylesheet" href="__CODICON_HREF__">
<style>
  body{margin:0;padding:16px 20px 40px;font:13px var(--vscode-font-family);color:var(--vscode-foreground);background:var(--vscode-editor-background);}
  .codicon{font-size:14px;line-height:1;vertical-align:-2px;}
  h1{font-size:16px;font-weight:600;margin:0 0 4px;}
  h2{font-size:11px;font-weight:600;text-transform:uppercase;letter-spacing:.04em;color:var(--vscode-descriptionForeground);margin:22px 0 6px;display:flex;align-items:center;gap:10px;}
  .muted{color:var(--vscode-descriptionForeground);}
  .summary{display:flex;flex-wrap:wrap;gap:6px 22px;margin:6px 0 2px;}
  .kv .k{color:var(--vscode-descriptionForeground);margin-right:6px;} .kv .v{font-variant-numeric:tabular-nums;}
  table{border-collapse:collapse;width:100%;max-width:1100px;}
  th,td{padding:3px 8px;text-align:right;white-space:nowrap;font-variant-numeric:tabular-nums;}
  th:first-child,td:first-child{text-align:left;}
  th{font-weight:600;color:var(--vscode-descriptionForeground);border-bottom:1px solid var(--vscode-panel-border,rgba(127,127,127,.3));user-select:none;}
  th.sort{cursor:pointer;} th.sort:hover{color:var(--vscode-foreground);} th .codicon{font-size:12px;}
  tr.s{cursor:pointer;} tr.s:hover td{background:var(--vscode-list-hoverBackground);}
  tr.p td{color:var(--vscode-descriptionForeground);font-size:12px;} tr.p td:first-child{padding-left:30px;}
  td .tw{display:inline-block;width:14px;color:var(--vscode-descriptionForeground);}
  .kind{font-size:11px;color:var(--vscode-descriptionForeground);margin-left:6px;}
  .hot{color:var(--vscode-editorWarning-foreground,#d2a000);} .vhot{color:var(--vscode-errorForeground);}
  .shared{font-size:10px;padding:0 5px;border-radius:8px;background:var(--vscode-badge-background);color:var(--vscode-badge-foreground);margin-left:6px;}
  button{font:inherit;font-size:12px;padding:2px 9px;border:none;border-radius:2px;background:var(--vscode-button-secondaryBackground);color:var(--vscode-button-secondaryForeground);cursor:pointer;text-transform:none;letter-spacing:0;}
  button:hover{background:var(--vscode-button-secondaryHoverBackground);} button:disabled{opacity:.5;cursor:default;}
  .none{color:var(--vscode-descriptionForeground);padding:6px 8px;}
  .bar{display:inline-block;width:60px;height:4px;border-radius:2px;background:var(--vscode-input-background,rgba(127,127,127,.18));vertical-align:middle;margin-left:6px;overflow:hidden;}
  .bar i{display:block;height:100%;background:var(--vscode-progressBar-background,#0e70c0);}
</style></head><body>
<h1>Resources</h1>
<div class="muted" id="sub">waiting for the daemon…</div>
<div class="summary" id="summary"></div>
<h2>Sessions</h2>
<div id="sessions"></div>
<h2>Other programs <span class="muted" style="text-transform:none;letter-spacing:0;font-weight:400">not part of the fleet</span></h2>
<div id="others"></div>
<h2>Disk <button id="refreshDisk"><i class="codicon codicon-refresh"></i> Refresh disk</button><span class="muted" id="diskWhen" style="text-transform:none;letter-spacing:0;font-weight:400"></span></h2>
<div id="disk"></div>
<script>
  const vsc = acquireVsCodeApi();
  const st = Object.assign({ sort: 'cpu_pct', desc: true, open: [] }, vsc.getState() || {});
  let m = null, running = false;
  const esc = s => String(s == null ? '' : s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
  const size = b => !b ? '0' : b < 1048576 ? Math.round(b / 1024) + ' KB' : b < 1073741824 ? Math.round(b / 1048576) + ' MB' : (b / 1073741824).toFixed(1) + ' GB';
  const mb = x => size((x || 0) * 1048576);
  const rate = b => b ? size(b) + '/s' : '—';
  const pct = x => (x || 0).toFixed(1) + '%';
  const heat = x => x >= 50 ? 'vhot' : x >= 20 ? 'hot' : '';
  const ago = ts => { const s = Math.max(0, Math.round(Date.now() / 1000 - ts)); return s < 90 ? s + 's ago' : s < 5400 ? Math.round(s / 60) + 'm ago' : Math.round(s / 3600) + 'h ago'; };
  const save = () => vsc.setState(st);
  const COLS = [['worktree', 'Session'], ['cpu_pct', 'CPU'], ['mem_mb', 'Memory'], ['private_mb', 'Private'], ['procs', 'Procs'], ['read_bps', 'Read'], ['write_bps', 'Write']];
  const key = s => s.worktree + '|' + s.kind;

  function renderSummary() {
    const sub = document.getElementById('sub'), sum = document.getElementById('summary');
    if (!m) { sub.textContent = running ? 'sampling…' : 'The daemon is stopped.'; sum.innerHTML = ''; return; }
    const fleet = (m.per_session || []).reduce((a, s) => a + s.cpu_pct, 0);
    sub.textContent = 'CPU is a percent of all ' + m.ncpu + ' cores. Updated every ' + Math.round((m.interval_ms || 2000) / 1000) + ' s while this view is open.';
    const kv = (k, v, t) => '<span class="kv" title="' + esc(t || '') + '"><span class="k">' + k + '</span><span class="v">' + v + '</span></span>';
    sum.innerHTML = kv('fleet CPU', pct(fleet), 'all sessions, incl. their child processes')
      + kv('fleet RAM', mb(m.mem_mb), 'working set of every session process tree')
      + kv('fleet disk', '↓' + rate(m.read_bps) + ' ↑' + rate(m.write_bps))
      + kv('system CPU', pct(m.sys_cpu_pct))
      + kv('system RAM', mb(m.sys_used_mb) + ' / ' + mb(m.sys_total_mb));
  }

  function renderSessions() {
    const el = document.getElementById('sessions');
    const rows = ((m && m.per_session) || []).slice();
    if (!rows.length) { el.innerHTML = '<div class="none">' + (m ? 'No live sessions.' : '…') + '</div>'; return; }
    const k = st.sort, dir = st.desc ? -1 : 1;
    rows.sort((a, b) => (typeof a[k] === 'string' ? a[k].localeCompare(b[k]) : (a[k] || 0) - (b[k] || 0)) * dir || a.worktree.localeCompare(b.worktree));
    const head = '<tr>' + COLS.map(c => '<th class="sort" data-k="' + c[0] + '">' + c[1] + (st.sort === c[0] ? ' <i class="codicon codicon-chevron-' + (st.desc ? 'down' : 'up') + '"></i>' : '') + '</th>').join('') + '</tr>';
    const body = rows.map(s => {
      const open = st.open.includes(key(s));
      const vm = s.vm ? '<span class="shared" title="Runs in WSL: its real work is inside the distro VM (' + esc(s.vm.name) + '), shared by every WSL session">WSL</span>' : '';
      let h = '<tr class="s" data-key="' + esc(key(s)) + '"><td><span class="tw"><i class="codicon codicon-chevron-' + (open ? 'down' : 'right') + '"></i></span>' + esc(s.worktree) + '<span class="kind">' + esc(s.kind) + '</span>' + vm + '</td>'
        + '<td class="' + heat(s.cpu_pct) + '">' + pct(s.cpu_pct) + '<span class="bar"><i style="width:' + Math.min(100, s.cpu_pct) + '%"></i></span></td>'
        + '<td>' + mb(s.mem_mb) + '</td><td>' + mb(s.private_mb) + '</td><td>' + s.procs + '</td><td>' + rate(s.read_bps) + '</td><td>' + rate(s.write_bps) + '</td></tr>';
      if (open) {
        h += (s.top || []).map(p => '<tr class="p"><td>' + esc(p.name) + ' <span class="muted">' + p.pid + '</span></td><td class="' + heat(p.cpu_pct) + '">' + pct(p.cpu_pct) + '</td><td>' + mb(p.mem_mb) + '</td><td></td><td></td><td></td><td></td></tr>').join('');
        if (s.procs > (s.top || []).length) h += '<tr class="p"><td>+ ' + (s.procs - s.top.length) + ' more, idle</td><td colspan="6"></td></tr>';
        if (s.vm) h += '<tr class="p"><td>' + esc(s.vm.name) + ' <span class="shared">shared</span> <span class="muted">the WSL VM, all WSL sessions</span></td><td>' + pct(s.vm.cpu_pct) + '</td><td>' + mb(s.vm.mem_mb) + '</td><td colspan="4"></td></tr>';
      }
      return h;
    }).join('');
    el.innerHTML = '<table>' + head + body + '</table>';
    el.querySelectorAll('th.sort').forEach(th => th.onclick = () => {
      const c = th.dataset.k;
      if (st.sort === c) st.desc = !st.desc; else { st.sort = c; st.desc = c !== 'worktree'; }
      save(); renderSessions();
    });
    el.querySelectorAll('tr.s').forEach(tr => tr.onclick = () => {
      const k = tr.dataset.key, i = st.open.indexOf(k);
      if (i >= 0) st.open.splice(i, 1); else st.open.push(k);
      save(); renderSessions();
    });
  }

  function renderOthers() {
    const el = document.getElementById('others');
    const rows = (m && m.others) || [];
    if (!rows.length) { el.innerHTML = '<div class="none">…</div>'; return; }
    el.innerHTML = '<table><tr><th>Program</th><th>CPU</th><th>Memory</th></tr>' + rows.map(p =>
      '<tr><td>' + esc(p.name) + (p.count > 1 ? ' <span class="muted">×' + p.count + '</span>' : '') + (/^vmmem/i.test(p.name) ? ' <span class="shared" title="The WSL VM: the work of every WSL session">WSL VM</span>' : '') + '</td>'
      + '<td class="' + heat(p.cpu_pct) + '">' + pct(p.cpu_pct) + '</td><td>' + mb(p.mem_mb) + '</td></tr>').join('') + '</table>';
  }

  function renderDisk() {
    const el = document.getElementById('disk'), when = document.getElementById('diskWhen'), btn = document.getElementById('refreshDisk');
    const d = m && m.disk;
    btn.disabled = !running || !!(d && d.busy);
    if (!d) { when.textContent = running ? 'scanning…' : ''; el.innerHTML = '<div class="none">' + (running ? 'Not measured yet.' : '') + '</div>'; return; }
    when.textContent = d.busy ? 'scanning…' : d.ts ? 'scanned ' + ago(d.ts) : '';
    const head = '<div class="summary"><span class="kv"><span class="k">dev root</span><span class="v">' + size(d.dev_root_bytes) + '</span></span>'
      + '<span class="kv"><span class="k">volume free</span><span class="v">' + size(d.volume_free_bytes) + ' of ' + size(d.volume_total_bytes) + '</span></span></div>';
    const rows = (d.worktrees || []).map(w => w.bytes == null
      ? '<tr><td>' + esc(w.id) + '</td><td class="muted" colspan="4">inside WSL, not measured</td></tr>'
      : '<tr><td>' + esc(w.id) + '</td><td>' + size(w.bytes) + '</td><td>' + size(w.target_bytes) + '</td><td>' + size(w.node_modules_bytes) + '</td><td>' + size(w.bytes - w.target_bytes - w.node_modules_bytes) + '</td></tr>').join('');
    el.innerHTML = head + '<table><tr><th>Worktree</th><th>Total</th><th>target/</th><th>node_modules/</th><th>Other</th></tr>' + rows + '</table>';
  }

  document.getElementById('refreshDisk').onclick = () => { vsc.postMessage({ cmd: 'refreshDisk' }); document.getElementById('refreshDisk').disabled = true; document.getElementById('diskWhen').textContent = 'scanning…'; };
  window.addEventListener('message', e => {
    const msg = e.data; if (!msg || msg.type !== 'metrics') return;
    m = msg.m; running = msg.running;
    renderSummary(); renderSessions(); renderOthers(); renderDisk();
  });
  setInterval(() => { if (m && m.disk && !m.disk.busy) renderDisk(); }, 30000);   // keep "scanned Xm ago" moving
  vsc.postMessage({ cmd: 'ready' });
</script></body></html>`;
}

module.exports = { ResourcesPanel, resourcesHtml };
