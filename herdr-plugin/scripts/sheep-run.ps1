<#
.SYNOPSIS
Run a Sheep subcommand against the worktree the action was invoked on.

.DESCRIPTION
The PowerShell twin of sheep-run.sh. Output goes to herdr's plugin log rather
than a terminal:

    herdr plugin log list --plugin sheep

Nothing here writes to the working tree: `snap` only reads it, and `doctor` reads
nothing but git metadata. The restoring half of Sheep is deliberately not an
action — it belongs behind the rewind overlay's plan-then-confirm.

.PARAMETER Verb
snap or doctor (default).
#>
[CmdletBinding()]
param(
    [ValidateSet('snap', 'doctor')]
    [string] $Verb = 'doctor'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'common.ps1')

$Binary = Get-SheepBinary
$Cwd = Get-SheepTargetCwd
$Line = Get-SheepTargetLine

if ($Verb -eq 'snap') {
    $arguments = @('--repo', $Cwd, '--line', $Line, 'snap', '--note', 'manual snapshot')
    if ($env:HERDR_PANE_ID) { $arguments += @('--pane', $env:HERDR_PANE_ID) }
} else {
    $arguments = @('--repo', $Cwd, '--line', $Line, 'doctor')
}

& $Binary @arguments
exit $LASTEXITCODE
