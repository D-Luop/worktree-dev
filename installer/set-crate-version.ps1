# Sets the wtd workspace version (Cargo.toml + Cargo.lock) to a release version. Build-time only:
# the release pipeline and build-local.ps1 run it; the result is never committed (#58).
param([Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') { throw "not a semver X.Y.Z[-pre]: $Version" }

$wtd = Join-Path (Split-Path $PSScriptRoot) 'wtd'
$utf8 = New-Object System.Text.UTF8Encoding $false

$toml = Join-Path $wtd 'Cargo.toml'
$s = [IO.File]::ReadAllText($toml)
$re = '(?ms)(^\[workspace\.package\][^\[]*?^version\s*=\s*")[^"]*(")'
if (-not [regex]::IsMatch($s, $re)) { throw "no [workspace.package] version in $toml" }
[IO.File]::WriteAllText($toml, [regex]::Replace($s, $re, "`${1}$Version`${2}"), $utf8)

$lock = Join-Path $wtd 'Cargo.lock'
$s = [IO.File]::ReadAllText($lock)
$s = [regex]::Replace($s, '(?m)(^name = "wtd(-core)?"\r?\nversion = ")[^"]*(")', "`${1}$Version`${3}")
[IO.File]::WriteAllText($lock, $s, $utf8)

Write-Host "wtd crate version -> $Version"
