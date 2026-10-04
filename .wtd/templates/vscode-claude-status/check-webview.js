// Build gate: parse-check the inline <script> blocks the extension generates for its webviews.
// A single syntax error (e.g. an unescaped apostrophe in a concatenated HTML string) kills the
// whole webview script at parse time — the panel renders its static HTML and dies silently, with
// nothing in any log. Run by build-vsix.py; exits non-zero on any parse error.
const fs = require('fs'), vm = require('vm'), path = require('path');
const HERE = __dirname;
let failures = 0;

function checkScripts(tag, html) {
  const re = /<script>([\s\S]*?)<\/script>/g;
  let m, n = 0;
  while ((m = re.exec(html)) !== null) {
    n++;
    try { new vm.Script(m[1], { filename: tag + '-script-' + n + '.js' }); }
    catch (e) {
      failures++;
      console.error(tag + ' script ' + n + ' PARSE ERROR:\n' + e.stack.split('\n').slice(0, 4).join('\n'));
    }
  }
  if (!n) { failures++; console.error(tag + ': no <script> blocks found (extraction broken?)'); }
  else if (!failures) console.log(tag + ': ' + n + ' script(s) parse OK');
}

// 1. The Dev-summary panel: _html() is a template literal in extension.js. Extract it textually and
//    render its escape sequences the way JS would (\\x -> x), then check its <script>.
const src = fs.readFileSync(path.join(HERE, 'extension.js'), 'utf8');
const start = src.indexOf('_html() {');
const tickOpen = src.indexOf('`', start);
let i = tickOpen + 1, end = -1;
while (i < src.length) {
  const c = src[i];
  if (c === '\\') { i += 2; continue; }
  if (c === '`') { end = i; break; }
  if (c === '$' && src[i + 1] === '{') {
    failures++;
    console.error('_html() contains a ${} interpolation — this checker only handles literal templates: '
      + JSON.stringify(src.slice(i, i + 60)));
  }
  i++;
}
if (start < 0 || tickOpen < 0 || end < 0) { failures++; console.error('could not locate the _html() template in extension.js'); }
else checkScripts('panel', src.slice(tickOpen + 1, end).replace(/\\(.)/g, (m, ch) => ch === 'n' ? '\n' : ch === 't' ? '\t' : ch));

// 2. The commits tab: diffview.commitsHtml() is directly callable (git calls fail gracefully).
try {
  const dv = require(path.join(HERE, 'diffview.js'));
  checkScripts('diffview', dv.commitsHtml('x', 'y', false));
} catch (e) { failures++; console.error('diffview check failed: ' + e.message); }

// 3. The Settings page: settings.settingsHtml() is a plain function (no vscode import at load).
try {
  const st = require(path.join(HERE, 'settings.js'));
  checkScripts('settings', st.settingsHtml());
} catch (e) { failures++; console.error('settings check failed: ' + e.message); }

process.exit(failures ? 1 : 0);
