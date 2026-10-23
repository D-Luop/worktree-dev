// Build gate: the WSL command lines and path translation (run by build-vsix.py; exits non-zero on failure).
const assert = require('assert');
const { fromUnc, gitCommand, wslShell, remoteUri } = require('./wslpath.js');

assert.deepStrictEqual(fromUnc('\\\\wsl.localhost\\Ubuntu\\home\\u\\w'), { distro: 'Ubuntu', linux: '/home/u/w' });
assert.deepStrictEqual(fromUnc('//wsl$/Debian/home/u/'), { distro: 'Debian', linux: '/home/u' });
assert.deepStrictEqual(fromUnc('\\\\wsl.localhost\\Ubuntu'), { distro: 'Ubuntu', linux: '/' });
assert.strictEqual(fromUnc('D:\\dev\\x'), null);
assert.strictEqual(fromUnc('\\\\server\\share\\x'), null);

assert.deepStrictEqual(gitCommand('D:\\dev\\w', ['status']), { cmd: 'git', args: ['-C', 'D:\\dev\\w', 'status'] });
assert.deepStrictEqual(gitCommand('\\\\wsl.localhost\\Ubuntu\\home\\u\\w', ['log', '-1']),
  { cmd: 'wsl.exe', args: ['-d', 'Ubuntu', '--cd', '/home/u/w', '--exec', 'git', 'log', '-1'] });

assert.deepStrictEqual(wslShell({ platform: 'wsl:Ubuntu', wsl_path: '/home/u/w' }), { shellPath: 'wsl.exe', shellArgs: ['-d', 'Ubuntu', '--cd', '/home/u/w'] });
assert.strictEqual(wslShell({ platform: 'windows', path: 'D:\\x' }), null);
assert.strictEqual(wslShell(null), null);

assert.strictEqual(remoteUri('\\\\wsl.localhost\\Ubuntu\\home\\u\\a b.rs'), 'vscode-remote://wsl+Ubuntu/home/u/a%20b.rs');
assert.strictEqual(remoteUri('D:\\x'), null);
console.log('wslpath: ok');
