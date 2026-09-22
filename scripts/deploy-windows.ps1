<#
.SYNOPSIS
Install postdoc as a Windows service - `worker` or `serve`.

.DESCRIPTION
Same contract as the systemd and rc.d paths: the binary owns its defaults and
this passes only what it cannot know - where hopper is, and where the token
lives. What differs is the supervision, which is NSSM.

.EXAMPLE
scripts\deploy-windows.ps1 -Mode worker -Url https://hopper.example

.EXAMPLE
scripts\deploy-windows.ps1 -Mode serve -Url https://hopper.example
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('worker', 'serve')][string]$Mode,
    # Required for a worker; optional for a server, where it turns on result
    # renewal, corpus deferral and the companion idle worker.
    [string]$Url,
    # SCAN_LLM: `local`, `openrouter`, or a base URL. Empty turns the pass off.
    [string]$Llm = '',
    # SCAN_LLM_MODEL. Unset lets postdoc pick what the endpoint reports.
    [string]$LlmModel = '',
    # Hopper bearer token to install into the service's state directory.
    [string]$TokenFile = (Join-Path $HOME '.tok\hopper')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Log($m) { Write-Host "==> $m" }
function Die($m) { Write-Error $m; exit 1 }

if ($Mode -eq 'worker' -and -not $Url) {
    Die 'worker needs a hopper URL'
}

$Binary      = 'postdoc.exe'
$ServiceName = if ($Mode -eq 'worker') { 'postdoc-worker' } else { 'postdoc' }
$InstallDir  = Join-Path $env:ProgramFiles 'Atomdrift'
$BinDst      = Join-Path $InstallDir $Binary
$StateHome   = Join-Path $env:ProgramData 'Atomdrift\postdoc'
$LogDir      = Join-Path $StateHome 'logs'
$TokDir      = Join-Path $StateHome '.tok'

$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Die 'run this from an elevated prompt'
}

$nssm = (Get-Command nssm -ErrorAction SilentlyContinue)
if (-not $nssm) { Die 'nssm not found on PATH (winget install NSSM.NSSM)' }
$nssm = $nssm.Source

# --- Retire atomscan ----------------------------------------------------------
#
# postdoc replaces `atomscan worker`, and the two must not run at the same time.
# Each sizes its memory ceiling against the whole host, so a box running both
# carries two analysis daemons that each believe they own it: they claim from
# the same queue and reach the same memory ceiling together, which is how a host
# that survived either one alone gets killed running the pair.
#
# `scan-worker` is the only service scan installs here - it has no Windows
# server - so it is the only collision, and it is stood down whichever mode
# postdoc is being installed in.
#
# Stopped before postdoc starts, because the overlap is the dangerous window and
# the gap is not: hopper re-leases anything left unfinished. Disabled as well,
# so a reboot before scan is uninstalled cannot restore the collision. The
# service stays registered - removing it is uninstalling scan, which is a
# separate decision from standing it down here.
$scanWorker = Get-Service 'scan-worker' -ErrorAction SilentlyContinue
if ($scanWorker) {
    Log "Standing scan-worker down (currently $($scanWorker.Status)); postdoc replaces it"

    # Through the SCM rather than `nssm set scan-worker Start SERVICE_DISABLED`,
    # so this still holds if nssm is ever removed while its service is not.
    Set-Service -Name 'scan-worker' -StartupType Disabled

    # nssm's own stop honours the AppStopMethodConsole that scan's service sets,
    # giving an in-flight analysis its 30s to drain. Stop-Service is the blunt
    # fallback for a service nssm no longer recognises.
    & $nssm stop 'scan-worker' 2>$null | Out-Null
    if ((Get-Service 'scan-worker').Status -ne 'Stopped') {
        Stop-Service 'scan-worker' -Force -ErrorAction SilentlyContinue
    }

    # nssm kills the tree it supervises, but a worker started by hand or
    # orphaned by an earlier crash sits outside that tree and still holds its
    # leases. Matched on the command line the way scan's own uninstaller does
    # it, so an interactive `atomscan scan` is left alone. postdoc ships as
    # postdoc.exe, so this can never reach postdoc itself.
    Get-CimInstance Win32_Process -Filter "Name = 'atomscan.exe'" |
        Where-Object { $_.CommandLine -match 'worker' } |
        ForEach-Object {
            Log "Reaping orphaned atomscan worker (pid $($_.ProcessId))"
            Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
        }

    # Starting postdoc anyway would produce exactly the simultaneous run this
    # section exists to prevent, so this is a hard gate.
    if ((Get-Service 'scan-worker').Status -ne 'Stopped') {
        Die 'scan-worker did not stop; refusing to start postdoc beside it'
    }
}

# --- Install the binary -------------------------------------------------------

$binSrc = Join-Path (Join-Path $PSScriptRoot '..') "target\release\$Binary"
if (-not (Test-Path $binSrc)) { Die "$binSrc not found; run ``cargo build --release`` first" }

New-Item -ItemType Directory -Force -Path $InstallDir, $StateHome, $LogDir, $TokDir | Out-Null

$svc = Get-Service $ServiceName -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -eq 'Running') {
    Log "Stopping $ServiceName"
    & $nssm stop $ServiceName | Out-Null
}
Log "Installing $BinDst"
Copy-Item $binSrc "$BinDst.new" -Force
Move-Item "$BinDst.new" $BinDst -Force

# Hopper rejects an unauthenticated claim with 401, so a worker without this
# starts, looks healthy, and never gets a job.
$tokDst = Join-Path $TokDir 'hopper'
if (Test-Path $TokenFile) {
    Log "Installing hopper token from $TokenFile"
    Copy-Item $TokenFile $tokDst -Force
} elseif ($Mode -eq 'worker') {
    Die "no hopper token at $TokenFile; a worker cannot claim without one"
}

# --- Arguments ----------------------------------------------------------------
#
# The whole list. Everything absent is postdoc's own default, which is the
# point: one place decides, and `postdoc <mode> --help` states it.
$svcArgs = if ($Mode -eq 'worker') {
    "worker --url $Url"
} else {
    $a = "serve --token-file `"$tokDst`""
    if ($Url) { $a += " --hopper $Url" }
    $a
}

# --- Service ------------------------------------------------------------------

if (-not $svc) {
    Log "Creating service $ServiceName"
    & $nssm install $ServiceName $BinDst | Out-Null
}
# `nssm set` is idempotent; the full configuration runs every deploy so a
# changed setting applies without special-casing.
& $nssm set $ServiceName Application $BinDst | Out-Null
& $nssm set $ServiceName AppParameters $svcArgs | Out-Null
& $nssm set $ServiceName AppDirectory $StateHome | Out-Null
& $nssm set $ServiceName DisplayName "Atomdrift postdoc ($Mode)" | Out-Null
& $nssm set $ServiceName Start SERVICE_AUTO_START | Out-Null

# Restart always, 10s back-off. The throttle stops a tight crash loop from
# pegging the box - the same shape as systemd's Restart/RestartSec.
& $nssm set $ServiceName AppExit Default Restart | Out-Null
& $nssm set $ServiceName AppRestartDelay 10000 | Out-Null
& $nssm set $ServiceName AppThrottle 10000 | Out-Null
# 30s to drain in-flight analyses, matching TimeoutStopSec elsewhere.
& $nssm set $ServiceName AppStopMethodConsole 30000 | Out-Null

& $nssm set $ServiceName AppStdout (Join-Path $LogDir "$Mode.out.log") | Out-Null
& $nssm set $ServiceName AppStderr (Join-Path $LogDir "$Mode.err.log") | Out-Null
& $nssm set $ServiceName AppRotateFiles 1 | Out-Null
& $nssm set $ServiceName AppRotateOnline 1 | Out-Null
& $nssm set $ServiceName AppRotateBytes 52428800 | Out-Null

# HOME steers every ~/.tok lookup into the state directory. The tokens
# themselves never enter the environment block, which any local user can read
# out of the registry. PATH is the deploying user's, so rizin/7z/upx installed
# per-user stay reachable.
$svcEnv = @(
    "HOME=$StateHome",
    "USERPROFILE=$StateHome",
    'RUST_BACKTRACE=1',
    "SCAN_LLM=$Llm",
    "PATH=$env:PATH"
)
if ($LlmModel) { $svcEnv += "SCAN_LLM_MODEL=$LlmModel" }
& $nssm set $ServiceName AppEnvironmentExtra @svcEnv | Out-Null

Log "Starting $ServiceName"
& $nssm start $ServiceName | Out-Null

# A service that starts and then dies still reports "started", so wait and look.
Start-Sleep -Seconds 3
$svc = Get-Service $ServiceName
if ($svc.Status -ne 'Running') {
    Get-Content (Join-Path $LogDir "$Mode.err.log") -Tail 40 -ErrorAction SilentlyContinue
    Die "$ServiceName failed to start"
}
Log "$ServiceName is running"
