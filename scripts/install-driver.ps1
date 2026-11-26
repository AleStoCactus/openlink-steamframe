# Build the SteamVR driver, assemble the driver folder in dist\openlink and register it with
# SteamVR (Windows counterpart of install-driver.sh). Run again after every driver change and
# after every PROTOCOL_VERSION bump (SteamVR must be closed).
#
#   scripts\install-driver.ps1           # build release + register
#   scripts\install-driver.ps1 -Remove   # unregister
param([switch]$Remove)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist\openlink'

# SteamVR: $env:STEAMVR_PATH, else every Steam library listed in libraryfolders.vdf.
function Find-SteamVR {
    if ($env:STEAMVR_PATH) { return $env:STEAMVR_PATH }
    $steam = (Get-ItemProperty 'HKCU:\Software\Valve\Steam' -ErrorAction SilentlyContinue).SteamPath
    if (-not $steam) { $steam = "${env:ProgramFiles(x86)}\Steam" }
    $libraries = @($steam)
    $vdf = Join-Path $steam 'steamapps\libraryfolders.vdf'
    if (Test-Path $vdf) {
        $libraries += Select-String -Path $vdf -Pattern '"path"\s+"([^"]+)"' |
            ForEach-Object { $_.Matches[0].Groups[1].Value -replace '\\\\', '\' }
    }
    foreach ($lib in $libraries) {
        $candidate = Join-Path $lib 'steamapps\common\SteamVR'
        if (Test-Path (Join-Path $candidate 'bin\win64\vrpathreg.exe')) { return $candidate }
    }
    return $null
}

$steamvr = Find-SteamVR
if (-not $steamvr) {
    Write-Error 'SteamVR not found (set STEAMVR_PATH)'
}
$vrpathreg = Join-Path $steamvr 'bin\win64\vrpathreg.exe'

if ($Remove) {
    & $vrpathreg removedriver $dist
    Write-Host "unregistered $dist"
    exit 0
}

if (Get-Process vrserver -ErrorAction SilentlyContinue) {
    Write-Error 'SteamVR is running; close it first (vrserver keeps the old driver loaded).'
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}
cargo build --release -p openlink-driver --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force (Join-Path $dist 'bin\win64') | Out-Null
Copy-Item -Recurse -Force (Join-Path $root 'driver\openlink\*') $dist
Copy-Item (Join-Path $root 'target\release\driver_openlink.dll') (Join-Path $dist 'bin\win64\driver_openlink.dll')

& $vrpathreg adddriver $dist
Write-Host "registered $dist"
Write-Host "Start 'openlink' (or 'cargo run --release -p openlink-server'), then SteamVR."
