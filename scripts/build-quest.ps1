# Builds the Quest app into a signed APK, and with -Install also installs and starts it
# (Windows counterpart of build-quest.sh).
#
#   scripts\build-quest.ps1 [-DebugBuild] [-Install]
#
# Needs: Rust target aarch64-linux-android, cargo-ndk, the Android NDK, SDK build-tools
# (aapt2, zipalign, apksigner), platform android-34 and a JDK. The SDK is looked for in
# $env:ANDROID_HOME, then %LOCALAPPDATA%\Android\Sdk; the NDK in $env:ANDROID_NDK_HOME, then the
# newest under the SDK's ndk folder. The OpenXR loader is downloaded from Maven Central on first
# use. Signs with %USERPROFILE%\.android\debug.keystore (created if missing).
param([switch]$DebugBuild, [switch]$Install)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem

$root = Split-Path -Parent $PSScriptRoot
$profileName = if ($DebugBuild) { 'debug' } else { 'release' }

function Invoke-Checked {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Exe failed with exit code $LASTEXITCODE" }
}

$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { Join-Path $env:LOCALAPPDATA 'Android\Sdk' }
if (-not (Test-Path $sdk)) { throw "Android SDK not found at $sdk (set ANDROID_HOME)" }
if (-not $env:ANDROID_NDK_HOME) {
    $ndk = Get-ChildItem (Join-Path $sdk 'ndk') -Directory -ErrorAction SilentlyContinue |
        Sort-Object { [version]$_.Name } | Select-Object -Last 1
    if (-not $ndk) { throw "no NDK under $sdk\ndk (set ANDROID_NDK_HOME)" }
    $env:ANDROID_NDK_HOME = $ndk.FullName
}
$platformJar = Join-Path $sdk 'platforms\android-34\android.jar'
$buildTools = Get-ChildItem (Join-Path $sdk 'build-tools') -Directory |
    Sort-Object { [version]$_.Name } | Select-Object -Last 1 -ExpandProperty FullName
$aapt2 = Join-Path $buildTools 'aapt2.exe'
$zipalign = Join-Path $buildTools 'zipalign.exe'
$apksigner = Join-Path $buildTools 'apksigner.bat'

# A JDK for apksigner and keytool: JAVA_HOME, else java on PATH, else Microsoft's OpenJDK.
if (-not $env:JAVA_HOME -and -not (Get-Command java -ErrorAction SilentlyContinue)) {
    $jdk = Get-ChildItem 'C:\Program Files\Microsoft' -Directory -Filter 'jdk-*' -ErrorAction SilentlyContinue |
        Sort-Object Name | Select-Object -Last 1
    if (-not $jdk) { throw 'no JDK found (install one, e.g. winget install Microsoft.OpenJDK.17)' }
    $env:JAVA_HOME = $jdk.FullName
}
if ($env:JAVA_HOME) { $env:PATH = "$env:JAVA_HOME\bin;$env:PATH" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH" }

$loaderVersion = '1.1.65'
$cache = Join-Path $root ".cache\openxr-loader-$loaderVersion"
$loader = Join-Path $cache 'jni\arm64-v8a\libopenxr_loader.so'
if (-not (Test-Path $loader)) {
    Write-Host "Downloading the Khronos OpenXR loader $loaderVersion"
    New-Item -ItemType Directory -Force (Split-Path $loader) | Out-Null
    $aar = Join-Path $cache 'loader.aar'
    Invoke-WebRequest -UseBasicParsing -OutFile $aar `
        "https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$loaderVersion/openxr_loader_for_android-$loaderVersion.aar"
    $zip = [System.IO.Compression.ZipFile]::OpenRead($aar)
    try {
        $entry = $zip.GetEntry('jni/arm64-v8a/libopenxr_loader.so')
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $loader, $true)
    } finally { $zip.Dispose() }
}

Push-Location $root
try {
    $cargoArgs = @('ndk', '-t', 'arm64-v8a', '-P', '32', 'build', '-p', 'openlink-quest')
    if (-not $DebugBuild) { $cargoArgs += '--release' }
    Invoke-Checked cargo $cargoArgs

    $out = Join-Path $root 'target\quest'
    New-Item -ItemType Directory -Force $out | Out-Null
    $unaligned = Join-Path $out 'unaligned.apk'
    $aligned = Join-Path $out 'aligned.apk'
    $apk = Join-Path $out 'openlink.apk'

    Invoke-Checked $aapt2 @('link', '-o', $unaligned, '--manifest', 'crates\openlink-quest\android\AndroidManifest.xml', '-I', $platformJar)
    # The native libraries go into the APK under lib/arm64-v8a/.
    $libs = @(
        (Join-Path $root "target\aarch64-linux-android\$profileName\libopenlink_quest.so"),
        $loader
    )
    $zip = [System.IO.Compression.ZipFile]::Open($unaligned, 'Update')
    try {
        foreach ($lib in $libs) {
            $name = 'lib/arm64-v8a/' + (Split-Path $lib -Leaf)
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $lib, $name, 'Optimal') | Out-Null
        }
    } finally { $zip.Dispose() }
    Invoke-Checked $zipalign @('-f', '-p', '4', $unaligned, $aligned)

    $keystore = Join-Path $env:USERPROFILE '.android\debug.keystore'
    if (-not (Test-Path $keystore)) {
        New-Item -ItemType Directory -Force (Split-Path $keystore) | Out-Null
        Invoke-Checked keytool @('-genkeypair', '-keystore', $keystore, '-storepass', 'android', '-keypass', 'android',
            '-alias', 'androiddebugkey', '-keyalg', 'RSA', '-keysize', '2048', '-validity', '10000',
            '-dname', 'CN=Android Debug,O=Android,C=US')
    }
    Invoke-Checked $apksigner @('sign', '--ks', $keystore, '--ks-pass', 'pass:android', '--key-pass', 'pass:android',
        '--out', $apk, $aligned)
    Remove-Item -Force $unaligned, $aligned, "$apk.idsig" -ErrorAction SilentlyContinue
    Write-Host "Built $apk"

    if ($Install) {
        # An app signed with another machine's debug key cannot be updated in place
        # (INSTALL_FAILED_UPDATE_INCOMPATIBLE): uninstall it first with
        # `adb uninstall org.openlink.quest`.
        Invoke-Checked adb @('install', '-r', $apk)
        Invoke-Checked adb @('shell', 'am', 'start', '-n', 'org.openlink.quest/android.app.NativeActivity')
    }
} finally {
    Pop-Location
}
