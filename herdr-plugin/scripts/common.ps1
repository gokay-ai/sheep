# Shared plumbing for Sheep's herdr hooks on Windows. Dot-sourced, never run.
# The PowerShell 5.1 twin of common.sh — keep the two in step.

function Get-SheepPluginRoot {
    if ($env:HERDR_PLUGIN_ROOT) { return $env:HERDR_PLUGIN_ROOT }
    return (Split-Path -Parent $PSScriptRoot)
}

function Get-SheepBinary {
    $root = Get-SheepPluginRoot
    $candidates = @(
        (Join-Path $root 'bin\sheep.exe'),
        (Join-Path $root '..\target\release\sheep.exe'),
        (Join-Path $root '..\target\debug\sheep.exe')
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    $onPath = Get-Command sheep.exe -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    [Console]::Error.WriteLine("sheep: no sheep.exe found under $root\bin — run herdr-plugin\install.ps1")
    exit 1
}

# One field out of HERDR_PLUGIN_CONTEXT_JSON. Set-StrictMode makes a missing
# property throw, so the property bag is consulted rather than the dotted name.
function Get-SheepContextField([string] $Name) {
    if (-not $env:HERDR_PLUGIN_CONTEXT_JSON) { return '' }
    try {
        $context = $env:HERDR_PLUGIN_CONTEXT_JSON | ConvertFrom-Json
    } catch {
        return ''
    }
    $property = $context.PSObject.Properties[$Name]
    if (-not $property) { return '' }
    if ($null -eq $property.Value) { return '' }
    return [string] $property.Value
}

function Get-SheepTargetCwd {
    $cwd = Get-SheepContextField 'focused_pane_cwd'
    if (-not $cwd) { $cwd = Get-SheepContextField 'workspace_cwd' }
    if (-not $cwd) { $cwd = (Get-Location).Path }
    return $cwd
}

function Get-SheepTargetLine {
    if ($env:HERDR_PANE_ID) { return $env:HERDR_PANE_ID }
    $pane = Get-SheepContextField 'focused_pane_id'
    if ($pane) { return $pane }
    return 'default'
}

function Get-SheepStateDir {
    if ($env:HERDR_PLUGIN_STATE_DIR) { return $env:HERDR_PLUGIN_STATE_DIR }
    if ($env:SHEEP_STATE_DIR) { return $env:SHEEP_STATE_DIR }
    if ($env:XDG_STATE_HOME) { return (Join-Path $env:XDG_STATE_HOME 'sheep') }
    return (Join-Path $env:LOCALAPPDATA 'sheep')
}
