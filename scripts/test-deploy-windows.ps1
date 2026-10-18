$ErrorActionPreference='Stop'
$fixture=Join-Path ([IO.Path]::GetTempPath()) ('postdoc-windows-test-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path "$fixture/scripts","$fixture/target/release","$fixture/tokens","$fixture/mockbin" | Out-Null
$env:ProgramFiles="$fixture/programfiles"
$env:ProgramData="$fixture/programdata"
$env:WINDOWS_DEPLOY_TEST_STATE="$fixture/state.json"
$env:LLM='https://llm.example/v1'
$env:LLM_MODEL='test/model'
$env:WORKERS='4'
$env:SCAN_LLM_KEY='private-test-key'
@{exists=$false;running=$false;starts=0;calls=@()} | ConvertTo-Json -Depth 10 | Set-Content $env:WINDOWS_DEPLOY_TEST_STATE
foreach($name in @('hopper','scan','llm','openrouter')) { [IO.File]::WriteAllText("$fixture/tokens/$name", "$name fake token") }
[IO.File]::WriteAllText("$fixture/target/release/postdoc.exe", 'fake release binary')
$mock=@'
$state=Get-Content $env:WINDOWS_DEPLOY_TEST_STATE -Raw | ConvertFrom-Json -AsHashtable
$state.calls += ,@($args)
if($args[0] -eq 'install'){$state.exists=$true}
if($args[0] -eq 'stop'){$state.running=$false}
if($args[0] -eq 'start'){$state.running=$true;$state.starts++}
$state | ConvertTo-Json -Depth 10 | Set-Content $env:WINDOWS_DEPLOY_TEST_STATE
$global:LASTEXITCODE=0
'@
[IO.File]::WriteAllText("$fixture/mockbin/nssm.ps1", $mock)
function global:icacls.exe { $global:LASTEXITCODE=0 }
function global:Get-Command { param($Name,$CommandType,$ErrorAction); return [pscustomobject]@{Source="$fixture/mockbin/nssm.ps1"} }
function global:Get-Service { param($Name,$ErrorAction); $s=Get-Content $env:WINDOWS_DEPLOY_TEST_STATE -Raw | ConvertFrom-Json; if ($Name -eq 'postdoc-worker' -and $s.exists) { return [pscustomobject]@{Status=$(if($s.running){'Running'}else{'Stopped'})} } }
function global:Start-Sleep { param($Seconds) }
$source=Get-Content (Join-Path $PSScriptRoot 'deploy-windows.ps1') -Raw
$source=$source -replace '(?ms)^\$identity = .*?^}\r?\n',''
[IO.File]::WriteAllText("$fixture/scripts/deploy-windows.ps1",$source)
$deployArgs=@{Mode='worker';Url='https://hopper.example';TokenFile="$fixture/tokens/hopper";ScanTokenFile="$fixture/tokens/scan";LlmTokenFile="$fixture/tokens/llm";OpenRouterTokenFile="$fixture/tokens/openrouter"}
& "$fixture/scripts/deploy-windows.ps1" @deployArgs
& "$fixture/scripts/deploy-windows.ps1" @deployArgs
$s=Get-Content $env:WINDOWS_DEPLOY_TEST_STATE -Raw | ConvertFrom-Json
if($s.starts -ne 1){throw "Unchanged redeploy restarted service: $($s.starts)"}
$environmentCall=@($s.calls | Where-Object { $_[0] -eq 'set' -and $_[2] -eq 'AppEnvironmentExtra' })[0]
if($environmentCall -notcontains 'SCAN_LLM=https://llm.example/v1'){throw 'LLM environment missing'}
if($environmentCall -notcontains 'SCAN_LLM_MODEL=test/model'){throw 'LLM model missing'}
if(($s.calls | ConvertTo-Json -Depth 10) -match 'private-test-key'){throw 'credential leaked through NSSM arguments'}
if([IO.File]::ReadAllText("$fixture/programdata/Atomdrift/postdoc/.tok/llm").Trim() -ne 'private-test-key'){throw 'explicit LLM credential was not installed'}
[IO.File]::WriteAllText("$fixture/tokens/hopper",'rotated fake hopper token')
& "$fixture/scripts/deploy-windows.ps1" @deployArgs
$s=Get-Content $env:WINDOWS_DEPLOY_TEST_STATE -Raw | ConvertFrom-Json
if($s.starts -ne 2){throw 'credential rotation did not restart the worker'}
Write-Output 'PASS: Windows LLM settings, key provisioning, no-op deployment and credential rotation'

Remove-Item -LiteralPath $fixture -Recurse -Force
