<#
.SYNOPSIS
Put a working sheep.exe at <plugin root>\bin\sheep.exe without needing Rust.

.DESCRIPTION
The Windows half of install.sh. herdr runs it as the plugin's [[build]] step on
`herdr plugin install`, with cwd = the plugin root and every HERDR_* runtime
variable scrubbed, so it resolves everything from $PSScriptRoot. Re-running is
cheap: an already-installed binary of the right version is left alone.

Written for Windows PowerShell 5.1 as well as pwsh 7 — no ?? operator, no
ternaries, and TLS 1.2 forced on because 5.1 still defaults lower.

.PARAMETER DryRun
Print what would be fetched and exit without touching anything.

.PARAMETER FromSource
Build with cargo instead of downloading. Needs Rust.

.PARAMETER Force
Reinstall even when the right version is already present.
#>
[CmdletBinding()]
param(
    [switch] $DryRun,
    [switch] $FromSource,
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Where the release assets come from. Deliberately not overridable: an
# environment variable must not be able to redirect where a binary is fetched.
$GithubRepo = 'gokay-ai/sheep'

$PluginRoot = $PSScriptRoot
$RepoRoot = Split-Path -Parent $PluginRoot
$OutDir = Join-Path $PluginRoot 'bin'
$Out = Join-Path $OutDir 'sheep.exe'

function Die([string] $Message) {
    [Console]::Error.WriteLine("sheep: $Message")
    exit 1
}

if ($env:SHEEP_FROM_SOURCE -eq '1') { $FromSource = $true }

# The manifest version, not Cargo.toml's: it is what herdr shows and what the
# release tag is cut from. CI asserts the two agree.
$Manifest = Join-Path $PluginRoot 'herdr-plugin.toml'
if (-not (Test-Path -LiteralPath $Manifest)) { Die "cannot find $Manifest" }
$Version = $null
foreach ($line in Get-Content -LiteralPath $Manifest) {
    if ($line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
}
if (-not $Version) { Die "could not read the plugin version from $Manifest" }

# Only x86_64-pc-windows-msvc is published. Windows on ARM runs x64 binaries
# under emulation, so arm64 machines get the same asset rather than an error.
$Triple = 'x86_64-pc-windows-msvc'
$Asset = "sheep-$Triple.exe"
$BaseUrl = "https://github.com/$GithubRepo/releases/download/v$Version"

if ($DryRun) {
    Write-Output "os          Windows"
    Write-Output ("arch        " + $env:PROCESSOR_ARCHITECTURE)
    Write-Output "version     $Version"
    Write-Output "target      $Triple"
    Write-Output "asset       $Asset"
    Write-Output "url         $BaseUrl/$Asset"
    Write-Output "install to  $Out"
    exit 0
}

function Build-FromSource {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Die 'building from source needs Rust — install it from https://rustup.rs and retry'
    }
    $cargoToml = Join-Path $RepoRoot 'Cargo.toml'
    if (-not (Test-Path -LiteralPath $cargoToml)) {
        Die "building from source needs the full checkout; $cargoToml is missing"
    }
    Write-Output "sheep: building v$Version from source (this takes a few minutes)."
    Push-Location $RepoRoot
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { Die "cargo build --release failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
    $built = Join-Path $RepoRoot 'target\release\sheep.exe'
    if (-not (Test-Path -LiteralPath $built)) { Die "cargo finished but $built does not exist" }
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    Copy-Item -LiteralPath $built -Destination $Out -Force
    Write-Output "sheep: installed a source build at $Out."
}

if ($FromSource) {
    Build-FromSource
    exit 0
}

if ((-not $Force) -and (Test-Path -LiteralPath $Out)) {
    $installed = $null
    try { $installed = (& $Out --version 2>$null | Select-Object -First 1) } catch { $installed = $null }
    if ($installed -and ($installed.Trim() -split '\s+')[-1] -eq $Version) {
        Write-Output "sheep: v$Version is already installed at $Out."
        exit 0
    }
}

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$TmpDir = Join-Path ([IO.Path]::GetTempPath()) ("sheep-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null
try {
    $sumsPath = Join-Path $TmpDir 'SHA256SUMS'
    $assetPath = Join-Path $TmpDir $Asset

    try {
        Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
    } catch {
        Die ("release v$Version has no SHA256SUMS. Is the release published? " +
            "See https://github.com/$GithubRepo/releases/tag/v$Version")
    }
    try {
        Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $assetPath -UseBasicParsing
    } catch {
        Die ("release v$Version has no asset named '$Asset'. " +
            "See https://github.com/$GithubRepo/releases/tag/v$Version")
    }

    # sha256sum writes "<hash>  <name>", shasum -b writes "<hash> *<name>".
    $expected = $null
    foreach ($line in Get-Content -LiteralPath $sumsPath) {
        $fields = $line.Trim() -split '\s+', 2
        if ($fields.Count -ne 2) { continue }
        if ($fields[0] -notmatch '^[0-9a-fA-F]{64}$') { continue }
        if ($fields[1].TrimStart('*') -eq $Asset) { $expected = $fields[0].ToLower(); break }
    }
    if (-not $expected) { Die "SHA256SUMS for v$Version does not list '$Asset'" }

    $actual = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Die "checksum mismatch for $Asset (expected $expected, got $actual). Nothing was installed."
    }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    # Windows refuses to overwrite a running exe, so move the old one aside
    # first; a dock still on the old binary keeps working until it exits.
    if (Test-Path -LiteralPath $Out) {
        $stale = "$Out.old"
        Remove-Item -LiteralPath $stale -Force -ErrorAction SilentlyContinue
        try { Move-Item -LiteralPath $Out -Destination $stale -Force } catch { }
    }
    Move-Item -LiteralPath $assetPath -Destination $Out -Force
    Write-Output "sheep: installed verified v$Version ($Triple) at $Out."
} finally {
    Remove-Item -LiteralPath $TmpDir -Recurse -Force -ErrorAction SilentlyContinue
}
