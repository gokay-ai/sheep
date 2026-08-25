<#
.SYNOPSIS
Open one of Sheep's declared panes for the agent the action was invoked on.

.DESCRIPTION
The PowerShell twin of open-pane.sh. The manifest cannot express "start in the
focused agent's worktree" — a pane command is a fixed argv and herdr defaults the
pane's cwd to the plugin root — so this goes through `herdr plugin pane open`,
which takes --cwd, and threads the timeline name through --env.

Note the entrypoint ids: manifest ids must be unique across the whole manifest
even when platform-gated, so the Windows panes are `dock-windows` and
`rewind-windows`.

.PARAMETER Pane
dock (default) or rewind.
#>
[CmdletBinding()]
param(
    [ValidateSet('dock', 'rewind')]
    [string] $Pane = 'dock'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'common.ps1')

$HerdrBin = if ($env:HERDR_BIN_PATH) { $env:HERDR_BIN_PATH } else { 'herdr' }
$PluginId = if ($env:HERDR_PLUGIN_ID) { $env:HERDR_PLUGIN_ID } else { 'sheep' }
$Entrypoint = "$Pane-windows"
$Cwd = Get-SheepTargetCwd
$Line = Get-SheepTargetLine

# The dock is a split and has to say which pane to split off, or herdr falls
# back to whatever happens to be focused — which is not necessarily the pane the
# action was invoked on. The overlay needs no target.
$arguments = @('plugin', 'pane', 'open', '--plugin', $PluginId, '--entrypoint', $Entrypoint)
if ($Pane -eq 'dock' -and $env:HERDR_PANE_ID) {
    $arguments += @('--target-pane', $env:HERDR_PANE_ID, '--direction', 'right')
}
$arguments += @('--cwd', $Cwd, '--env', "SHEEP_LINE=$Line", '--focus')

& $HerdrBin @arguments
exit $LASTEXITCODE
