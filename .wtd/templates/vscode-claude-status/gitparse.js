// Pure git-output parsing shared by the Changes panel: unified patches → per-file entries, and
// untracked files → synthesised all-adds entries (git emits no patch for them).

const fs = require('fs');
const path = require('path');

const MAX_FILES = 300;             // per commit
const MAX_PATCH_LINES = 1500;      // per file — a 40k-line generated blob would freeze the webview
const UNTRACKED_MAX_BYTES = 512 * 1024;   // above this an untracked file is listed, not inlined

// Test-file globs mirror test_excludes() in .wtd/hooks/generated-filter.sh.
const TEST_RE = /(^|\/)(testdata\/|.*_test\.(go|sql|ts)$|.*\.(test|spec)\.(ts|tsx|js|jsx)$)/i;

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

// An untracked file has no blob to diff against, so git won't emit a patch for it. Reading it and
// synthesising an all-adds hunk is what `git diff --no-index /dev/null <f>` would print, without one
// git process per file — and it keeps these rows from claiming "+0 −0" when they're entirely new.
function untrackedEntry(wt, file) {
  const base = {
    path: file, from: '', status: 'untracked', adds: 0, dels: 0, binary: false, untracked: true,
    ext: (/\.([A-Za-z0-9_+-]+)$/.exec(file) || [, ''])[1].toLowerCase(),
    test: TEST_RE.test(file), patch: '', truncated: 0,
  };
  let buf;
  try {
    const st = fs.statSync(path.join(wt, file));
    if (!st.isFile()) return null;                                   // a symlink to a dir, say
    if (st.size > UNTRACKED_MAX_BYTES) return Object.assign(base, { tooBig: true });
    buf = fs.readFileSync(path.join(wt, file));
  } catch { return Object.assign(base, { unreadable: true }); }
  if (buf.includes(0)) return Object.assign(base, { binary: true });  // NUL ⇒ git would call it binary

  const lines = buf.toString('utf8').replace(/\r\n/g, '\n').split('\n');
  if (lines.length && lines[lines.length - 1] === '') lines.pop();    // trailing newline isn't a line
  const adds = lines.length;
  let body = lines, truncated = 0;
  if (body.length > MAX_PATCH_LINES) { truncated = body.length - MAX_PATCH_LINES; body = body.slice(0, MAX_PATCH_LINES); }
  return Object.assign(base, {
    adds, truncated,
    patch: ['@@ -0,0 +1,' + adds + ' @@'].concat(body.map((l) => '+' + l)).join('\n'),
  });
}

// `git status --porcelain=v1 -z --untracked-files=all` output → untracked entries
function untrackedFromPorcelain(wt, text) {
  return (text || '').split('\0').filter((e) => e.startsWith('?? ')).map((e) => untrackedEntry(wt, e.slice(3))).filter(Boolean);
}

module.exports = { TEST_RE, parsePatch, untrackedEntry, untrackedFromPorcelain, unquoteC };
