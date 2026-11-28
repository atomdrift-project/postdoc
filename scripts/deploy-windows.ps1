<#
.SYNOPSIS
Install postdoc as a Windows service - `worker` or `serve`.

.DESCRIPTION
Same contract as the systemd and rc.d paths: the binary owns its defaults and
this passes the hopper URL and rule feed and installs the separate API,
hopper and rules credentials. What differs is the supervision, which is NSSM.

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
    [string]$OpenRouterTokenFile = $(if ($env:OPENROUTER_TOKEN_FILE) { $env:OPENROUTER_TOKEN_FILE } else { Join-Path $HOME '.tok\openrouter' }),
    # Zero-day rule feed token.
    [string]$RulesTokenFile = $(if ($env:RULES_TOKEN_FILE) { $env:RULES_TOKEN_FILE } else { Join-Path $HOME '.tok\rules' }),
    # `oss` runs the open-source feed, needing no rules token.
    [string]$Feed = $env:FEED,
    # CLEAVE_TRAITS_DIR: a local rules tree, in place of a feed.
    [string]$TraitsDir = $env:TRAITS_DIR
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
# postdoc resolves `~/.tok` through the Windows profile API, which ignores the
# HOME and USERPROFILE variables, so the tokens have to live in the profile of
# the account the service runs as (LocalSystem). POSTDOC_SERVICE_HOME overrides
# the lookup for tests.
$ServiceHome = if ($env:POSTDOC_SERVICE_HOME) {
    $env:POSTDOC_SERVICE_HOME
} else {
    $profileKey = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\S-1-5-18'
    $profilePath = (Get-ItemProperty -Path $profileKey -ErrorAction SilentlyContinue).ProfileImagePath
    if ($profilePath) { [Environment]::ExpandEnvironmentVariables($profilePath) }
    else { Join-Path $env:SystemRoot 'System32\config\systemprofile' }
}
$TokDir      = Join-Path $ServiceHome '.tok'
# Unlike ~/.tok, the config directory follows USERPROFILE, which the service
# sets to $StateHome.
$RulesDir    = Join-Path $StateHome 'AppData\Roaming\atomdrift'
$RulesTokDst = Join-Path $RulesDir 'isotope13-api.json'

$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Die 'run this from an elevated prompt'
}

$nssm = (Get-Command nssm -ErrorAction SilentlyContinue)
if (-not $nssm) { Die 'nssm not found on PATH (winget install NSSM.NSSM)' }
$nssm = $nssm.Source

# Validate the PATH inherited by the service before stopping any running work.
# Windows tools are commonly installed with Scoop or winget by the operator.
foreach ($tool in 'upx', 'innoextract') {
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
# The rules: a TraitsDir tree, the open-source feed, or by default the
# zero-day feed, whose token is required. An installed token serves when the
# source is absent.
$installRulesToken = $false
if ($TraitsDir) {
    if ($Feed) { Die 'set FEED or TRAITS_DIR, not both' }
    if (-not [IO.Path]::IsPathRooted($TraitsDir)) { Die 'TRAITS_DIR must be an absolute path' }
    if (-not (Test-Path -LiteralPath $TraitsDir -PathType Container)) { Die "TRAITS_DIR $TraitsDir is not a directory" }
} elseif (-not $Feed -or $Feed -eq 'zero-day') {
    if (Test-Path $RulesTokenFile) {
        if ([string]::IsNullOrWhiteSpace([IO.File]::ReadAllText($RulesTokenFile))) { Die "blank credential: $RulesTokenFile" }
        $installRulesToken = $true
    } elseif (-not (Test-Path $RulesTokDst) -or [string]::IsNullOrWhiteSpace([IO.File]::ReadAllText($RulesTokDst))) {
        Die "the zero-day feed needs a token in $RulesTokenFile; FEED=oss or TRAITS_DIR runs without one"
    }
} elseif ($Feed -ne 'oss') {
    Die 'FEED must be zero-day or oss'
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
# The service runs with USERPROFILE=$StateHome, and Windows resolves the data
# and cache directories as %USERPROFILE%\AppData\{Roaming,Local}. It reports
# "no data directory" rather than creating them, so they have to exist.
New-Item -ItemType Directory -Force -Path (Join-Path $StateHome 'AppData\Roaming'), (Join-Path $StateHome 'AppData\Local') | Out-Null
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
if ($installRulesToken) {
    New-Item -ItemType Directory -Force -Path $RulesDir | Out-Null
    & icacls.exe $RulesDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
    if ($LASTEXITCODE -ne 0) { Die "could not restrict rules token directory ACL: $RulesDir" }
    $rulesText = (@{ token = (Get-Content $RulesTokenFile | Where-Object { $_.Trim() } | Select-Object -First 1).Trim() } | ConvertTo-Json -Compress) + "`n"
    if (-not (Test-Path $RulesTokDst) -or [IO.File]::ReadAllText($RulesTokDst) -ne $rulesText) {
        [IO.File]::WriteAllText($RulesTokDst, $rulesText)
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
# Stop in any state but Stopped (Running, Paused by NSSM's throttle,
# StartPending...) and wait for it: `nssm start` on a live service fails.
function Stop-ServiceFully {
    if ((Get-Service $ServiceName).Status -eq 'Stopped') { return }
    & $nssm stop $ServiceName | Out-Null
    for ($i = 0; $i -lt 60; $i++) {
        if ((Get-Service $ServiceName).Status -eq 'Stopped') { return }
        Start-Sleep -Seconds 1
    }
    Die "could not stop service $ServiceName"
}
# `nssm start` reports failure ("Unexpected status SERVICE_PAUSED in response to
# START control") when the app exits inside the throttle window and NSSM parks
# the service in Paused before the SCM's start call returns. That is a
# crash-on-startup, not an NSSM failure, so don't die blind here: fall through
# to the grace-period check, which dumps the logs and reports the real status.
# Only a failure that leaves the service Stopped is an NSSM error in itself.
function Start-ServiceChecked {
    & $nssm start $ServiceName 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 0) { return }
    Start-Sleep -Milliseconds 500
    $status = (Get-Service $ServiceName).Status
    if ($status -eq 'Stopped') { Die "NSSM failed to start $ServiceName" }
    Log "nssm start returned $LASTEXITCODE with $ServiceName $status; checking startup"
}
$arguments = @($Mode)
# Named, so a refused token stops startup rather than falling back to another
# feed. cleave refuses a feed named beside a TraitsDir tree.
if (-not $TraitsDir) { $arguments += @('--feed', $(if ($Feed) { $Feed } else { 'zero-day' })) }
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
if ($Feed) { $configuration['FEED'] = $Feed }
if ($TraitsDir) { $configuration['TRAITS_DIR'] = $TraitsDir }
$configuration[$(if ($Mode -eq 'serve') { 'HOPPER' } else { 'URL' })] = $(if ($Url) { $Url } else { 'none' })
$configPath = Join-Path $StateHome "deploy-$Mode.json"
$configText = $configuration | ConvertTo-Json
if (-not (Test-Path $configPath) -or [IO.File]::ReadAllText($configPath) -ne $configText) { $changed = $true }
if ($binaryChanged -or $changed) {
    if ($svc) { Stop-ServiceFully }
    if ($binaryChanged) {
        Copy-Item $binSrc "$BinDst.new" -Force
        # Move-Item -Force can still fail with "Cannot create a file when that
        # file already exists" on an existing destination, so remove it first.
        # The service is stopped above, so the old binary is not locked.
        if (Test-Path -LiteralPath $BinDst) { Remove-Item -LiteralPath $BinDst -Force -ErrorAction Stop }
        Move-Item -LiteralPath "$BinDst.new" -Destination $BinDst
    }
} elseif ($svc) {
    Log 'No changes; leaving service running'
    # Like `systemctl start` on a unit that is not active: a service the SCM
    # shows as anything but Running (stopped, or Paused by the throttle after
    # a crash loop) is restarted rather than reported healthy.
    if ($svc.Status -ne 'Running') {
        Log "$ServiceName is $($svc.Status); restarting"
        Stop-ServiceFully
        Start-ServiceChecked
    }
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

# HOME points child tools at the state directory. It does not move postdoc's
# own ~/.tok lookup, which follows the service account's profile (see $TokDir).
# The tokens themselves never enter the environment block, which any local user
# can read out of the registry. PATH is the deploying user's, so 7z/upx
# installed per-user stay reachable.
$svcEnv = @(
    "HOME=$StateHome",
    "USERPROFILE=$StateHome",
    'RUST_BACKTRACE=1',
    "SCAN_LLM=$Llm",
    "PATH=$env:PATH"
)
if ($LlmModel) { $svcEnv += "SCAN_LLM_MODEL=$LlmModel" }
if ($TraitsDir) { $svcEnv += "CLEAVE_TRAITS_DIR=$TraitsDir" }
& $nssm set $ServiceName AppEnvironmentExtra @svcEnv | Out-Null
if ($LASTEXITCODE -ne 0) { Die "NSSM operation failed for $ServiceName" }

[IO.File]::WriteAllText($configPath, $configText)

Log "Starting $ServiceName"
Start-ServiceChecked
}

# A service that starts and then dies still reports "started", and NSSM turns a
# crash loop into Paused, so watch it through a grace period (this also covers
# the no-change path) rather than trusting the instant after `start`.
for ($i = 0; $i -lt 15; $i++) {
    $svc = Get-Service $ServiceName
    if ($svc.Status -ne 'Running') {
        foreach ($stream in 'err', 'out') {
            Log "--- $Mode.$stream.log (tail) ---"
            Get-Content (Join-Path $LogDir "$Mode.$stream.log") -Tail 40 -ErrorAction SilentlyContinue
        }
        Die "$ServiceName exited during startup (status $($svc.Status))"
    }
    Start-Sleep -Seconds 1
}
Log "$ServiceName is running"

if ($Mode -eq 'serve') {
    $bind = if ($env:BIND) { $env:BIND } else { '127.0.0.1:49999' }
    $bind = $bind -replace '^0\.0\.0\.0:', '127.0.0.1:' -replace '^\[::\]:', '[::1]:'
    $deadline = [DateTime]::UtcNow.AddSeconds(300)
    $ready = $false
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            # Readiness is public and one word; /_/stats then proves the
            # installed token is the one the server loaded.
            $word = (Invoke-RestMethod "http://$bind/_/health" -TimeoutSec 5).Trim()
            if ($word -eq 'ok') {
                $null = Invoke-RestMethod "http://$bind/_/stats" -Headers @{Authorization=('Bearer ' + [IO.File]::ReadAllText($scanTokDst).Trim())} -TimeoutSec 5
                $ready = $true; break
            }
        } catch { }
        Start-Sleep -Seconds 1
    }
    if (-not $ready) { Die 'API failed readiness check' }
    Log 'API ready'
}
