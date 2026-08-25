<#
.SYNOPSIS
Keep exactly one detached `sheep watch` alive for this herdr session (Windows).

.DESCRIPTION
The PowerShell twin of watchd.sh. herdr's [[startup]] hook is a one-shot, not a
supervisor, so the recorder is launched as a detached process and this script
returns immediately. `start` is idempotent — the worktree.created and
workspace.created hooks call it to heal a recorder that died with a previous
session.

.PARAMETER Command
start (default), stop, restart or status.
#>
[CmdletBinding()]
param(
    [ValidateSet('start', 'stop', 'restart', 'status')]
    [string] $Command = 'start'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'common.ps1')

$RunDir = Join-Path (Get-SheepStateDir) 'recorder'
$PidFile = Join-Path $RunDir 'watch.pid'
$LogFile = Join-Path $RunDir 'watch.log'

# A pid can be recycled by anything, so the process is only claimed when it
# still looks like the recorder — killing a stranger's process would be far
# worse than starting a second watcher.
function Get-RunningRecorder {
    if (-not (Test-Path -LiteralPath $PidFile)) { return $null }
    $raw = (Get-Content -LiteralPath $PidFile -TotalCount 1 -ErrorAction SilentlyContinue)
    if (-not $raw) { return $null }
    $recorderPid = 0
    if (-not [int]::TryParse($raw.Trim(), [ref] $recorderPid)) { return $null }
    $process = Get-Process -Id $recorderPid -ErrorAction SilentlyContinue
    if (-not $process) { return $null }
    if ($process.ProcessName -ne 'sheep') { return $null }
    return $process
}

function Start-Recorder {
    $running = Get-RunningRecorder
    if ($running) {
        Write-Output ("sheep: recorder already running (pid " + $running.Id + ")")
        return
    }
    $binary = Get-SheepBinary
    New-Item -ItemType Directory -Force -Path $RunDir | Out-Null
    # -WindowStyle Hidden keeps a console window from flashing on every hook.
    $process = Start-Process -FilePath $binary -ArgumentList 'watch' `
        -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $LogFile -RedirectStandardError "$LogFile.err"
    Set-Content -LiteralPath $PidFile -Value $process.Id -Encoding ASCII
    Write-Output ("sheep: recorder started (pid " + $process.Id + "), logging to $LogFile")
}

function Stop-Recorder {
    $running = Get-RunningRecorder
    if ($running) {
        Stop-Process -Id $running.Id -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
        Write-Output ("sheep: recorder stopped (pid " + $running.Id + ")")
    } else {
        Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
        Write-Output 'sheep: recorder is not running'
    }
}

switch ($Command) {
    'start' { Start-Recorder }
    'stop' { Stop-Recorder }
    'restart' { Stop-Recorder; Start-Recorder }
    'status' {
        $running = Get-RunningRecorder
        if ($running) {
            Write-Output ("sheep: recorder running (pid " + $running.Id + ")")
            Write-Output "sheep: log $LogFile"
        } else {
            Write-Output 'sheep: recorder is not running'
            exit 1
        }
    }
}
