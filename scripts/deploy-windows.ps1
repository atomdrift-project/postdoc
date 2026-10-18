<#
.SYNOPSIS
Install postdoc as a Windows service - `worker` or `serve`.

.DESCRIPTION
Same contract as the systemd and rc.d paths: the binary owns its defaults and
this passes the hopper URL and installs the separate API and hopper
credentials. What differs is the supervision, which is NSSM.

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
    [string]$Llm = $env:LLM,
    # SCAN_LLM_MODEL. Unset lets postdoc pick what the endpoint reports.
    [string]$LlmModel = $env:LLM_MODEL,
    # Hopper bearer token. Environment overrides keep parity with Unix deploys.
    [string]$TokenFile = $(if ($env:HOPPER_TOKEN_FILE) { $env:HOPPER_TOKEN_FILE } elseif ($env:TOKEN_FILE) { $env:TOKEN_FILE } else { Join-Path $HOME '.tok\hopper' }),
    # API bearer token used by Beamline as SCAN_TOKEN.
    [string]$ScanTokenFile = $(if ($env:SCAN_TOKEN_FILE) { $env:SCAN_TOKEN_FILE } else { Join-Path $HOME '.tok\scan' }),
    [string]$LlmTokenFile = $(if ($env:LLM_TOKEN_FILE) { $env:LLM_TOKEN_FILE } else { Join-Path $HOME '.tok\llm' }),
    [string]$OpenRouterTokenFile = $(if ($env:OPENROUTER_TOKEN_FILE) { $env:OPENROUTER_TOKEN_FILE } else { Join-Path $HOME '.tok\openrouter' })
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

# Validate the PATH inherited by the service before stopping any running work.
# Windows tools are commonly installed with Scoop or winget by the operator.
foreach ($tool in 'rizin', 'upx', 'innoextract') {
    if (-not (Get-Command $tool -CommandType Application -ErrorAction SilentlyContinue)) {
        Die "$tool is required; install it and add it to PATH before deploying"
    }
}
$sevenZip = @('7z', '7za', '7zz', '7zr') | Where-Object {
    Get-Command $_ -CommandType Application -ErrorAction SilentlyContinue
} | Select-Object -First 1
if (-not $sevenZip) {
    Die '7-Zip is required; install it and add 7z, 7za, 7zz, or 7zr to PATH before deploying'
}

# Validate every supplied token before interrupting services. Reuse installed
# credentials when the source file is absent; never accept a blank rotation.
$orEndpoints = @($Llm -split ',' | Where-Object { $_ -match '^\s*(openrouter|https?://openrouter\.ai/.*)\s*$' })
$endpoints = @($Llm -split ',' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$hasExplicitKey = -not [string]::IsNullOrWhiteSpace($env:SCAN_LLM_KEY)
$tokenSpecs = @(
    @{ Name='hopper'; Source=$TokenFile; Required=($Mode -eq 'worker' -or [bool]$Url) },
    @{ Name='scan'; Source=$ScanTokenFile; Required=($Mode -eq 'serve') },
    @{ Name='llm'; Source=$LlmTokenFile; Required=$false },
    @{ Name='openrouter'; Source=$OpenRouterTokenFile; Required=($orEndpoints.Count -gt 0 -and $orEndpoints.Count -eq $endpoints.Count -and -not $hasExplicitKey) }
)
foreach ($spec in $tokenSpecs) {
    if ($hasExplicitKey -and (($orEndpoints.Count -gt 0 -and $spec.Name -eq 'openrouter') -or ($orEndpoints.Count -eq 0 -and $spec.Name -eq 'llm'))) { continue }
    if (Test-Path $spec.Source) {
        if ([string]::IsNullOrWhiteSpace([IO.File]::ReadAllText($spec.Source))) { Die "blank credential: $($spec.Source)" }
    } elseif ($spec.Required) {
        $installed = Join-Path $TokDir $spec.Name
        if (-not (Test-Path $installed) -or [string]::IsNullOrWhiteSpace([IO.File]::ReadAllText($installed))) {
            Die "no usable $($spec.Name) credential at $($spec.Source) or $installed"
        }
    }
}

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
# Tokens are inherited by the service as LocalSystem. Remove ProgramData's
# broader inherited read access so ordinary local users cannot read either
# credential.
& icacls.exe $TokDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
if ($LASTEXITCODE -ne 0) { Die "could not restrict token directory ACL: $TokDir" }

$changed = $false
$svc = Get-Service $ServiceName -ErrorAction SilentlyContinue
function Same-File($a, $b) {
    return (Test-Path $a) -and (Test-Path $b) -and ((Get-FileHash $a).Hash -eq (Get-FileHash $b).Hash)
}
foreach ($spec in $tokenSpecs) {
    if ($hasExplicitKey -and (($orEndpoints.Count -gt 0 -and $spec.Name -eq 'openrouter') -or ($orEndpoints.Count -eq 0 -and $spec.Name -eq 'llm'))) { continue }
    $destination = Join-Path $TokDir $spec.Name
    if ((Test-Path $spec.Source) -and -not (Same-File $spec.Source $destination)) {
        Copy-Item $spec.Source $destination -Force
        $changed = $true
    }
}
if ($hasExplicitKey) {
    $keyName = if ($orEndpoints.Count -gt 0) { 'openrouter' } else { 'llm' }
    $keyDestination = Join-Path $TokDir $keyName
    $keyText = $env:SCAN_LLM_KEY + "`n"
    if (-not (Test-Path $keyDestination) -or [IO.File]::ReadAllText($keyDestination) -ne $keyText) {
        [IO.File]::WriteAllText($keyDestination, $keyText)
        $changed = $true
    }
}
$scanTokDst = Join-Path $TokDir 'scan'
$binaryChanged = -not (Same-File $binSrc $BinDst)
$skipUpdate = $false

# --- Arguments ----------------------------------------------------------------
#
# The whole list. Everything absent is postdoc's own default, which is the
# point: one place decides, and `postdoc <mode> --help` states it.
function Quote-Argument([string]$value) {
    # CommandLineToArgvW escaping, including trailing backslashes.
    return '"' + ([regex]::Replace($value, '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"'
}
$arguments = @($Mode)
if ($Mode -eq 'worker') {
    $arguments += @('--url', $Url)
    if ($env:DATA_DIR) { $arguments += @('--data-dir', $env:DATA_DIR) }
} else {
    $arguments += @('--token-file', $scanTokDst)
    if ($Url) { $arguments += @('--hopper', $Url) }
    if ($env:BIND) { $arguments += @('--bind', $env:BIND) }
    if ($env:ALLOW_CIDR) { $arguments += @('--allow-cidr', $env:ALLOW_CIDR) }
    if ($env:IDLE) { $arguments += @('--idle-worker-slots', $env:IDLE) }
}
if ($env:WORKERS) { $arguments += @('--workers', $env:WORKERS) }
if ($env:MAX_RSS_GB) { $arguments += @('--max-rss-gb', $env:MAX_RSS_GB) }
$svcArgs = ($arguments | ForEach-Object { Quote-Argument $_ }) -join ' '
$configuration = [ordered]@{}
foreach ($key in @('BIND','ALLOW_CIDR','WORKERS','DATA_DIR','MAX_RSS_GB','IDLE','MEMORY_MAX','MEMORY_LOW','IDLE_MEMORY_MAX','NICE')) {
    $value = [Environment]::GetEnvironmentVariable($key)
    if ($null -ne $value) { $configuration[$key] = $value }
}
$configuration['LLM'] = [string]$Llm
$configuration['LLM_MODEL'] = [string]$LlmModel
$configuration[$(if ($Mode -eq 'serve') { 'HOPPER' } else { 'URL' })] = $(if ($Url) { $Url } else { 'none' })
$configPath = Join-Path $StateHome "deploy-$Mode.json"
$configText = $configuration | ConvertTo-Json
if (-not (Test-Path $configPath) -or [IO.File]::ReadAllText($configPath) -ne $configText) { $changed = $true }
if ($binaryChanged -or $changed) {
    if ($svc -and $svc.Status -eq 'Running') { & $nssm stop $ServiceName | Out-Null; if ($LASTEXITCODE -ne 0) { Die 'could not stop service' } }
    if ($binaryChanged) {
        Copy-Item $binSrc "$BinDst.new" -Force
        Move-Item "$BinDst.new" $BinDst -Force
    }
} elseif ($svc) {
    Log 'No changes; leaving service running'
    if ($svc.Status -ne 'Running') { & $nssm start $ServiceName | Out-Null }
    $skipUpdate = $true
}

# --- Service ------------------------------------------------------------------

if (-not $skipUpdate) {
if (-not $svc) {
    Log "Creating service $ServiceName"
    & $nssm install $ServiceName $BinDst | Out-Null
}
# `nssm set` is idempotent; the full configuration runs every deploy so a
# changed setting applies without special-casing.
& $nssm set $ServiceName Application $BinDst | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppParameters $svcArgs | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppDirectory $StateHome | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName DisplayName "Atomdrift postdoc ($Mode)" | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName Start SERVICE_AUTO_START | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }

# Restart always, 10s back-off. The throttle stops a tight crash loop from
# pegging the box - the same shape as systemd's Restart/RestartSec.
& $nssm set $ServiceName AppExit Default Restart | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppRestartDelay 10000 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppThrottle 10000 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
# 30s to drain in-flight analyses, matching TimeoutStopSec elsewhere.
& $nssm set $ServiceName AppStopMethodConsole 30000 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }

& $nssm set $ServiceName AppStdout (Join-Path $LogDir "$Mode.out.log") | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppStderr (Join-Path $LogDir "$Mode.err.log") | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppRotateFiles 1 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppRotateOnline 1 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }
& $nssm set $ServiceName AppRotateBytes 52428800 | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }

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
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }

[IO.File]::WriteAllText($configPath, $configText)

Log "Starting $ServiceName"
& $nssm start $ServiceName | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }

# A service that starts and then dies still reports "started", so wait and look.
Start-Sleep -Seconds 3
$svc = Get-Service $ServiceName
if ($svc.Status -ne 'Running') {
    Get-Content (Join-Path $LogDir "$Mode.err.log") -Tail 40 -ErrorAction SilentlyContinue
    Die "$ServiceName failed to start"
}
Log "$ServiceName is running"
}

if ($Mode -eq 'serve') {
    $bind = if ($env:BIND) { $env:BIND } else { '127.0.0.1:49999' }
    $bind = $bind -replace '^0\.0\.0\.0:', '127.0.0.1:' -replace '^\[::\]:', '[::1]:'
    $deadline = [DateTime]::UtcNow.AddSeconds(300)
    $ready = $false
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $reply = Invoke-RestMethod "http://$bind/_/health" -Headers @{Authorization=('Bearer ' + [IO.File]::ReadAllText($scanTokDst).Trim())} -TimeoutSec 5
            if ($reply.status -in @('ok','saturated')) { $ready = $true; break }
        } catch { }
        Start-Sleep -Seconds 1
    }
    if (-not $ready) { Die 'API failed readiness check' }
    Log 'API ready'
}
