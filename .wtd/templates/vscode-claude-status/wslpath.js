// WSL worktrees: their folder is seen from Windows as \\wsl.localhost\<distro>\…, but git and shells
// for them run inside the distro through wsl.exe (git over UNC is slow and breaks). Pure helpers so the
// command lines can be checked with plain node (check-wslpath.js).

// '\\wsl.localhost\Ubuntu\home\u\x' (also \\wsl$\, forward slashes) → { distro: 'Ubuntu', linux: '/home/u/x' }
function fromUnc(p) {
  const m = /^[\\/]{2}(?:\?[\\/]UNC[\\/])?(wsl\.localhost|wsl\$)[\\/]([^\\/]+)([\\/].*)?$/i.exec(String(p || ''));
  if (!m) return null;
  const linux = ('/' + (m[3] || '').replace(/\\/g, '/').replace(/^\/+/, '')).replace(/\/+$/, '') || '/';
  return { distro: m[2], linux };
}

// git in a worktree: `git -C <wt> …`, or `wsl.exe -d <distro> --cd <path> --exec git …` for a WSL one
function gitCommand(wt, args) {
  const w = fromUnc(wt);
  if (!w) return { cmd: 'git', args: ['-C', wt].concat(args) };
  return { cmd: 'wsl.exe', args: ['-d', w.distro, '--cd', w.linux, '--exec', 'git'].concat(args) };
}

// the shell for a roster row's terminals: a login shell inside the distro for a WSL worktree, else null
function wslShell(row) {
  const m = /^wsl:(.+)$/.exec((row && row.platform) || '');
  if (!m || !row.wsl_path) return null;
  return { shellPath: 'wsl.exe', shellArgs: ['-d', m[1], '--cd', row.wsl_path] };
}

// a file of a WSL worktree as a Remote-WSL URI string (the caller falls back to the UNC file URI)
function remoteUri(fp) {
  const w = fromUnc(fp);
  return w ? 'vscode-remote://wsl+' + encodeURIComponent(w.distro) + w.linux.split('/').map(encodeURIComponent).join('/') : null;
}

module.exports = { fromUnc, gitCommand, wslShell, remoteUri };
