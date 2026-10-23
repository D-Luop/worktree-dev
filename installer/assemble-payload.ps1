# Lays out the release payload in the program dir's `current\` shape (distribution contract):
#   bin\wtd.exe, bin\wtd-tray.exe, templates\, hooks\, scripts\, claude-status.vsix, version.txt
# The exes must already be built (and signed, in CI) in -BinDir.
param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Out,
    [string]$BinDir
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
if (-not $BinDir) { $BinDir = Join-Path $root 'wtd\target\release' }

if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force (Join-Path $Out 'bin') | Out-Null

foreach ($exe in 'wtd.exe', 'wtd-tray.exe') {
    Copy-Item (Join-Path $BinDir $exe) (Join-Path $Out "bin\$exe")
}
foreach ($dir in 'templates', 'hooks', 'scripts') {
    Copy-Item (Join-Path $root ".wtd\$dir") (Join-Path $Out $dir) -Recurse
}
# build artifacts that may sit in a dev checkout's templates
Get-ChildItem (Join-Path $Out 'templates') -Recurse -Force -Include '*.vsix', '__pycache__' |
    Remove-Item -Recurse -Force

# the vsix carries the release's numeric X.Y.Z (VS Code rejects prerelease suffixes)
$vsixVersion = $Version -replace '-.*$', ''
& python (Join-Path $root '.wtd\templates\vscode-claude-status\build-vsix.py') `
    --version $vsixVersion --out (Join-Path $Out 'claude-status.vsix')
if ($LASTEXITCODE -ne 0) { throw "build-vsix.py failed ($LASTEXITCODE)" }

[IO.File]::WriteAllText((Join-Path $Out 'version.txt'), "$Version`n", (New-Object System.Text.UTF8Encoding $false))
Write-Host "payload $Version -> $Out"
