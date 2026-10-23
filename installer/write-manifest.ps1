# Zips the payload and writes the updater manifest next to it (distribution contract):
#   worktree-dev-<ver>-windows-x64.zip, and latest.json (stable) or latest-beta.json (prerelease).
param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Payload,
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$OutDir,
    [string]$Repo = 'D-Luop/worktree-dev'
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression.FileSystem

$OutDir = (New-Item -ItemType Directory -Force $OutDir).FullName
$zip = Join-Path $OutDir "worktree-dev-$Version-windows-x64.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
[IO.Compression.ZipFile]::CreateFromDirectory((Resolve-Path $Payload).Path, $zip,
    [IO.Compression.CompressionLevel]::Optimal, $false)

$prerelease = $Version.Contains('-')
$base = "https://github.com/$Repo/releases/download/v$Version"
$setupItem = Get-Item $Setup
$manifest = [ordered]@{
    version             = $Version
    channel             = $(if ($prerelease) { 'beta' } else { 'stable' })
    published           = [DateTime]::UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss'Z'")
    payload             = [ordered]@{
        url    = "$base/$(Split-Path $zip -Leaf)"
        sha256 = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        size   = (Get-Item $zip).Length
    }
    setup               = [ordered]@{
        url    = "$base/$($setupItem.Name)"
        sha256 = (Get-FileHash $setupItem.FullName -Algorithm SHA256).Hash.ToLower()
    }
    min_daemon_protocol = 1
    notes_url           = "https://github.com/$Repo/releases/tag/v$Version"
}
$name = if ($prerelease) { 'latest-beta.json' } else { 'latest.json' }
$json = $manifest | ConvertTo-Json -Depth 4
[IO.File]::WriteAllText((Join-Path $OutDir $name), "$json`n", (New-Object System.Text.UTF8Encoding $false))
Write-Host "wrote $zip"
Write-Host "wrote $(Join-Path $OutDir $name)"
