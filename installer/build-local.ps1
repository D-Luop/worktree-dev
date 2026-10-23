# Builds the release artifacts locally, the same way release.yml does, minus signing and upload:
#   dist\WorkTreeDev-Setup-<ver>.exe, dist\worktree-dev-<ver>-windows-x64.zip, dist\latest[-beta].json
# Needs cargo, python, node and Inno Setup 6 (winget install JRSoftware.InnoSetup).
#   powershell -File installer\build-local.ps1 [-Version 0.2.0[-beta.1]] [-OutDir dist] [-SkipBuild]
param(
    [string]$Version,
    [string]$OutDir,
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
$wtd = Join-Path $root 'wtd'
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }
$OutDir = (New-Item -ItemType Directory -Force $OutDir).FullName
if (-not $Version) {
    $Version = [regex]::Match([IO.File]::ReadAllText((Join-Path $wtd 'Cargo.toml')),
        '(?ms)^\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"').Groups[1].Value
}

function Find-Iscc {
    $candidates = @($env:ISCC,
        (Get-Command iscc.exe -ErrorAction SilentlyContinue).Source,
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe")
    foreach ($c in $candidates) { if ($c -and (Test-Path $c)) { return $c } }
    throw 'ISCC.exe not found: winget install JRSoftware.InnoSetup (or set $env:ISCC)'
}
$iscc = Find-Iscc

if (-not $SkipBuild) {
    # the version is stamped for the build only; Cargo.toml/Cargo.lock are restored afterwards
    $saved = @{}
    foreach ($f in 'Cargo.toml', 'Cargo.lock') { $saved[$f] = [IO.File]::ReadAllBytes((Join-Path $wtd $f)) }
    try {
        & (Join-Path $PSScriptRoot 'set-crate-version.ps1') -Version $Version
        & cargo build --release --manifest-path (Join-Path $wtd 'Cargo.toml')
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        foreach ($f in $saved.Keys) { [IO.File]::WriteAllBytes((Join-Path $wtd $f), $saved[$f]) }
    }
}

$payload = Join-Path $OutDir 'payload'
& (Join-Path $PSScriptRoot 'assemble-payload.ps1') -Version $Version -Out $payload

& $iscc /Q "/DAppVersion=$Version" "/DPayloadDir=$payload" "/O$OutDir" (Join-Path $PSScriptRoot 'WorkTreeDev.iss')
if ($LASTEXITCODE -ne 0) { throw "iscc failed ($LASTEXITCODE)" }

& (Join-Path $PSScriptRoot 'write-manifest.ps1') -Version $Version -Payload $payload `
    -Setup (Join-Path $OutDir "WorkTreeDev-Setup-$Version.exe") -OutDir $OutDir

Write-Host "`nartifacts in $OutDir"
