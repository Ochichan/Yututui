<#
.SYNOPSIS
Runs paired ytt performance scenarios from fresh source-bound builds on Windows.

.DESCRIPTION
Render scenarios execute directly. Process scenarios execute the sampler and ytt inside a
run-unique off-screen ConPTY with exact geometry, controlled empty input, an explicit fake-home
environment, null audio, and a kill-on-close Job Object. The wrapper alternates AB/BA order,
records exact PID/ConPTY evidence, and never resizes or inherits the parent console.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $Scenario,
    [Parameter(Mandatory = $true)] [string] $Output,
    [Parameter(Mandatory = $true)] [string] $BaselineSourceRoot,
    [string] $CandidateSourceRoot,
    [string] $SeedHome,
    [string] $BaselineSeedHome,
    [string] $CandidateSeedHome,
    [string] $MpvSelectionManifest,
    [string] $Scenarios
)

$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $PSScriptRoot
$PythonTool = Join-Path $PSScriptRoot "tui-perf.py"
if (-not $Scenarios) { $Scenarios = Join-Path $PSScriptRoot "tui-perf-scenarios.json" }
if (-not $CandidateSourceRoot) { $CandidateSourceRoot = $Repo }

$pythonCommand = Get-Command python -ErrorAction SilentlyContinue
if (-not $pythonCommand) { $pythonCommand = Get-Command python3 -ErrorAction SilentlyContinue }
if (-not $pythonCommand) { throw "python or python3 is required" }
$script:PythonExe = $pythonCommand.Source
$script:ActiveConpty = $null
$script:ActiveController = $null
$script:ActiveServer = $null
$script:ActiveServerIdentity = $null
$script:ActiveServerRunId = $null
$script:ActiveServerToken = $null

function Invoke-PythonChecked([object[]] $Arguments, [string] $Failure) {
    $value = & $script:PythonExe $PythonTool @Arguments
    if ($LASTEXITCODE -ne 0) { throw $Failure }
    return $value
}

function Get-ReceiptArtifact([string] $Receipt, [string] $Label) {
    return ((Invoke-PythonChecked @(
        "receipt", "--receipt", $Receipt, "--artifact", $Label, "--field", "path"
    ) "cannot read artifact $Label from build receipt") | Out-String).Trim()
}

function ConvertTo-WindowsArgument([AllowEmptyString()] [string] $Value) {
    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') { return $Value }
    $builder = New-Object System.Text.StringBuilder
    [void] $builder.Append('"')
    $backslashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') {
            $backslashes++
            continue
        }
        if ($character -eq '"') {
            for ($index = 0; $index -lt ($backslashes * 2 + 1); $index++) {
                [void] $builder.Append('\')
            }
            [void] $builder.Append('"')
        } else {
            for ($index = 0; $index -lt $backslashes; $index++) {
                [void] $builder.Append('\')
            }
            [void] $builder.Append($character)
        }
        $backslashes = 0
    }
    for ($index = 0; $index -lt ($backslashes * 2); $index++) {
        [void] $builder.Append('\')
    }
    [void] $builder.Append('"')
    return $builder.ToString()
}

function Start-ExactProcess(
    [string] $FilePath,
    [object[]] $Arguments,
    [System.Collections.IDictionary] $Environment
) {
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = (($Arguments | ForEach-Object {
        ConvertTo-WindowsArgument ([string] $_)
    }) -join " ")
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    if ($null -ne $Environment) {
        $startInfo.EnvironmentVariables.Clear()
        foreach ($key in $Environment.Keys) {
            $startInfo.EnvironmentVariables[[string] $key] = [string] $Environment[$key]
        }
    }
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    if (-not $process.Start()) { throw "failed to start exact process: $FilePath" }
    return $process
}

function Write-Utf8Json([string] $Path, [object] $Value) {
    $json = ConvertTo-Json -InputObject $Value -Compress -Depth 20
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $json + "`n", $utf8)
}

function Wait-ForFile(
    [string] $Path,
    [System.Diagnostics.Process] $Process,
    [int] $TimeoutSeconds,
    [string] $Label
) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        if ($Process.HasExited) { throw "$Label exited before publishing $Path" }
        if ([DateTime]::UtcNow -ge $deadline) { throw "timed out waiting for $Label" }
        Start-Sleep -Milliseconds 50
    }
}

function New-ShutdownToken {
    $bytes = New-Object byte[] 32
    $generator = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try { $generator.GetBytes($bytes) } finally { $generator.Dispose() }
    return [Convert]::ToBase64String($bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

function Stop-FixtureServer([bool] $Strict) {
    if ($null -eq $script:ActiveServer) { return }
    $failure = $null
    try {
        if (-not $script:ActiveServer.HasExited) {
            if (-not $script:ActiveServerIdentity -or -not $script:ActiveServerRunId -or
                -not $script:ActiveServerToken) {
                throw "fixture server has no exact authenticated shutdown identity"
            }
            & $script:PythonExe $PythonTool stop-server `
                --identity $script:ActiveServerIdentity `
                --expected-run-id $script:ActiveServerRunId `
                ("--shutdown-token=" + $script:ActiveServerToken) `
                --timeout-secs 10 | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "authenticated fixture shutdown failed" }
        }
        if (-not $script:ActiveServer.WaitForExit(10000)) {
            throw "fixture server survived authenticated shutdown"
        }
        if ($script:ActiveServer.ExitCode -ne 0) {
            throw "fixture server exited with code $($script:ActiveServer.ExitCode)"
        }
    } catch {
        $failure = $_
        # This Process object is an exact live kernel handle captured at creation, never a
        # process-name or numeric-PID lookup. It is safe for emergency teardown after failure.
        if (-not $script:ActiveServer.HasExited) {
            $script:ActiveServer.Kill()
            [void] $script:ActiveServer.WaitForExit(5000)
        }
    } finally {
        $script:ActiveServer.Dispose()
        $script:ActiveServer = $null
        $script:ActiveServerIdentity = $null
        $script:ActiveServerRunId = $null
        $script:ActiveServerToken = $null
    }
    if ($Strict -and $null -ne $failure) { throw $failure }
}

function Stop-ActiveExactProcesses {
    if ($null -ne $script:ActiveController) {
        if (-not $script:ActiveController.HasExited) { $script:ActiveController.Kill() }
        [void] $script:ActiveController.WaitForExit(5000)
        $script:ActiveController.Dispose()
        $script:ActiveController = $null
    }
    if ($null -ne $script:ActiveConpty) {
        # Killing this exact launcher handle closes its kill-on-close Job Object and therefore
        # contains sampler, ytt, mpv, and every descendant without a process-name-wide signal.
        if (-not $script:ActiveConpty.HasExited) { $script:ActiveConpty.Kill() }
        [void] $script:ActiveConpty.WaitForExit(5000)
        $script:ActiveConpty.Dispose()
        $script:ActiveConpty = $null
    }
    Stop-FixtureServer $false
}

function Copy-Seed([string] $Source, [string] $Destination) {
    if (-not $Source) { return }
    Get-ChildItem -LiteralPath $Source -Force | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $Destination -Recurse -Force
    }
}

function Run-Render(
    [string] $Role,
    [string] $Binary,
    [string] $RunDir,
    [string] $RunId
) {
    New-Item -ItemType Directory -Force -Path $RunDir | Out-Null
    $oldHash = [Environment]::GetEnvironmentVariable("TUI_PERF_SCENARIO_SHA256", "Process")
    $oldYtmPerf = [Environment]::GetEnvironmentVariable("YTM_PERF", "Process")
    $oldSourceRateBound = [Environment]::GetEnvironmentVariable(
        "YTM_PERF_SOURCE_RATE_BOUND_BPS", "Process"
    )
    $oldRunId = [Environment]::GetEnvironmentVariable("TUI_PERF_RUN_ID", "Process")
    try {
        [Environment]::SetEnvironmentVariable("TUI_PERF_SCENARIO_SHA256", $script:ScenarioHash, "Process")
        [Environment]::SetEnvironmentVariable("TUI_PERF_RUN_ID", $RunId, "Process")
        [Environment]::SetEnvironmentVariable("YTM_PERF", $null, "Process")
        [Environment]::SetEnvironmentVariable(
            "YTM_PERF_SOURCE_RATE_BOUND_BPS", $null, "Process"
        )
        & $Binary --output (Join-Path $RunDir "render.json") `
            --warmup $script:ScenarioData.warmup_draws `
            --batches $script:ScenarioData.batches `
            --draws $script:ScenarioData.draws_per_batch
        if ($LASTEXITCODE -ne 0) { throw "$Role render harness failed" }
    } finally {
        [Environment]::SetEnvironmentVariable("TUI_PERF_SCENARIO_SHA256", $oldHash, "Process")
        [Environment]::SetEnvironmentVariable("YTM_PERF", $oldYtmPerf, "Process")
        [Environment]::SetEnvironmentVariable(
            "YTM_PERF_SOURCE_RATE_BOUND_BPS", $oldSourceRateBound, "Process"
        )
        [Environment]::SetEnvironmentVariable("TUI_PERF_RUN_ID", $oldRunId, "Process")
    }
}

function New-ChildEnvironment(
    [string] $RunDir,
    [string] $Home,
    [string] $Runtime,
    [string] $Temp,
    [string] $RunId
) {
    $environment = [ordered]@{
        "PATH" = $env:PATH
        "SystemRoot" = $env:SystemRoot
        "WINDIR" = $env:WINDIR
        "COMSPEC" = $env:COMSPEC
        "PATHEXT" = $env:PATHEXT
        "HOME" = $Home
        "USERPROFILE" = $Home
        "APPDATA" = (Join-Path $Home "AppData\Roaming")
        "LOCALAPPDATA" = (Join-Path $Home "AppData\Local")
        "XDG_CONFIG_HOME" = (Join-Path $Home ".config")
        "XDG_DATA_HOME" = (Join-Path $Home ".local\share")
        "XDG_CACHE_HOME" = (Join-Path $Home ".cache")
        "XDG_STATE_HOME" = (Join-Path $Home ".local\state")
        "XDG_RUNTIME_DIR" = $Runtime
        "YTM_CONFIG_DIR" = (Join-Path $Home "stores\config")
        "YTM_DATA_DIR" = (Join-Path $Home "stores\data")
        "YTM_CACHE_DIR" = (Join-Path $Home "stores\cache")
        "TEMP" = $Temp
        "TMP" = $Temp
        "TERM" = "xterm-256color"
        "YTM_MPV_EXTRA" = "--ao=null --volume=0 --audio-display=no"
        "TUI_PERF_SCENARIO_SHA256" = $script:ScenarioHash
        "TUI_PERF_RUN_ID" = $RunId
    }
    if ($script:RequiresMpv) {
        $environment["YTM_MPV"] = $script:SelectedMpv
        if ([int64] $script:EnforcedSourceRateBoundBps -gt 0) {
            $environment["YTM_PERF_SOURCE_RATE_BOUND_BPS"] = `
                [string] $script:EnforcedSourceRateBoundBps
        }
    }
    return $environment
}

function Run-ProcessScenario(
    [string] $Role,
    [string] $Binary,
    [string] $RunRoot,
    [int] $Width,
    [int] $Height,
    [string] $Label,
    [string] $RunId
) {
    $runDir = $RunRoot
    if ($script:GeometryCount -gt 1) {
        $runDir = Join-Path $RunRoot ("geometry-{0}x{1}" -f $Width, $Height)
    }
    $home = Join-Path $runDir "home"
    $runtime = Join-Path $runDir "runtime"
    $temp = Join-Path $runDir "tmp"
    $configStore = Join-Path $home "stores\config"
    $dataStore = Join-Path $home "stores\data"
    $cacheStore = Join-Path $home "stores\cache"
    $samples = Join-Path $runDir "samples.ndjson"
    $pidFile = Join-Path $runDir "ytt.pid"
    $identityFile = Join-Path $runDir "process-identity.json"
    $controllerReady = Join-Path $runDir "controller-ready.json"
    $conptyProof = Join-Path $runDir "conpty.json"
    $environmentFile = Join-Path $runDir "child-environment.json"
    $script:EnforcedSourceRateBoundBps = [int64] 0
    foreach ($directory in @(
        $runDir, $home, $runtime, $temp, $configStore, $dataStore, $cacheStore,
        (Join-Path $home "AppData\Roaming"), (Join-Path $home "AppData\Local"),
        (Join-Path $home ".config"), (Join-Path $home ".local\share"),
        (Join-Path $home ".cache"), (Join-Path $home ".local\state")
    )) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }
    $roleSeed = $script:CandidateSeedHome
    if ($Role -eq "baseline") { $roleSeed = $script:BaselineSeedHome }
    Copy-Seed $roleSeed $home

    try {
        if ($script:RequiresMpv) {
            $readyFile = Join-Path $runDir "http-ready.json"
            $requestsFile = Join-Path $runDir "http-requests.ndjson"
            $shutdownToken = New-ShutdownToken
            $profile = $script:TrafficProfile
            $serverArgs = @(
                $PythonTool, "serve",
                "--file", $script:FixtureFile,
                "--ready-file", $readyFile,
                "--request-log", $requestsFile,
                "--run-id", $RunId,
                ("--shutdown-token=" + $shutdownToken),
                "--throttle-bps", [string] $profile.throttle_bps,
                "--maximum-source-rate-bps", [string] $profile.maximum_source_rate_bps,
                "--outage-every-bytes", [string] $profile.outage_every_bytes,
                "--outage-ms", [string] $profile.outage_ms,
                "--disconnect-every-bytes", [string] $profile.disconnect_every_bytes,
                "--header-delay-ms", [string] $profile.header_delay_ms,
                "--range-response-delay-ms", [string] $profile.range_response_delay_ms,
                "--range-behavior", [string] $profile.range_behavior,
                "--fault-profile", [string] $profile.fault_profile
            )
            $script:ActiveServer = Start-ExactProcess $script:PythonExe $serverArgs $null
            $script:ActiveServerIdentity = $readyFile
            $script:ActiveServerRunId = $RunId
            $script:ActiveServerToken = $shutdownToken
            Wait-ForFile $readyFile $script:ActiveServer 10 "fixture server for $Label"
            $ready = Get-Content -LiteralPath $readyFile -Raw | ConvertFrom-Json
            $script:EnforcedSourceRateBoundBps = [int64] $ready.maximum_source_rate_bps
            if ($script:EnforcedSourceRateBoundBps -ne [int64] $profile.maximum_source_rate_bps) {
                throw "fixture server source-rate ceiling disagrees with the selected profile"
            }
            Invoke-PythonChecked @(
                "materialize", "--root", $home, "--home", $home,
                "--fixture-url", ("http://127.0.0.1:{0}/fixture.wav" -f $ready.port),
                "--seed-label", $Role,
                "--input-snapshot", (Join-Path $runDir "materialized-inputs"),
                "--manifest", (Join-Path $runDir "materialize.json")
            ) "failed to materialize fixture playlist for $Label" | Out-Null
        }

        if ($null -ne $script:ScenarioData.setting_leaf_overrides) {
            Invoke-PythonChecked @(
                "apply-setting-overrides", "--scenarios", $Scenarios,
                "--scenario", $Scenario, "--role", $Role,
                "--root", $home,
                "--output", (Join-Path $runDir "setting-overrides.json")
            ) "failed to apply measured setting overrides for $Label" | Out-Null
        }

        Invoke-PythonChecked @(
            "launch-policy", "--root", $home,
            "--output", (Join-Path $runDir "launch-policy.json")
        ) "failed to freeze launch policy for $Label" | Out-Null

        $childEnvironment = New-ChildEnvironment $runDir $home $runtime $temp $RunId
        Write-Utf8Json $environmentFile $childEnvironment
        $samplerArgs = @(
            "--output", $samples,
            "--pid-file", $pidFile,
            "--identity-file", $identityFile,
            "--cache-root", $cacheStore,
            "--binary", $Binary,
            "--warmup-secs", [string] $script:ScenarioData.warmup_s,
            "--duration-secs", [string] $script:ScenarioData.sample_s,
            "--interval-ms", [string] $script:SamplingInterval
        )
        if ($script:RequiresMpv) { $samplerArgs += "--require-silent-mpv" }
        if ($script:ScenarioData.controller) {
            $samplerArgs += @("--controller-ready-file", $controllerReady)
        } else {
            $samplerArgs += @("--", "--new-instance")
        }
        $timeoutSeconds = [int] [Math]::Ceiling(
            [double] $script:ScenarioData.warmup_s +
            [double] $script:ScenarioData.sample_s + 120.0
        )
        $conptyArgs = @(
            "--width", [string] $Width,
            "--height", [string] $Height,
            "--timeout-secs", [string] $timeoutSeconds,
            "--proof", $conptyProof,
            "--environment-json", $environmentFile,
            "--working-directory", $runDir,
            "--", $script:Sampler
        ) + $samplerArgs
        Remove-Item -LiteralPath $pidFile, $identityFile, $controllerReady, $conptyProof `
            -Force -ErrorAction SilentlyContinue
        $script:ActiveConpty = Start-ExactProcess $script:Conpty $conptyArgs $null
        Wait-ForFile $pidFile $script:ActiveConpty 30 "$Label sampler"

        if ($script:ScenarioData.controller) {
            $observeSeconds = [double] $script:ScenarioData.warmup_s + [double] $script:ScenarioData.sample_s
            $controllerArgs = @(
                "--output", (Join-Path $runDir "control.ndjson"),
                "--ready-file", $controllerReady,
                "--wait-secs", "45",
                "--observe-secs", [string] $observeSeconds,
                "--close-grace-secs", "15",
                "--load", [string] $script:ScenarioData.controller_load
            )
            if ($null -ne $script:ScenarioData.actions -and $script:ScenarioData.actions.Count -gt 0) {
                $actionsJson = ConvertTo-Json -InputObject $script:ScenarioData.actions -Compress -Depth 20
                $controllerArgs += @("--actions-json", $actionsJson)
            } elseif ($script:ScenarioData.seeks_s.Count -gt 0) {
                $controllerArgs += @("--seeks", ($script:ScenarioData.seeks_s -join ","))
            }
            if ($script:ScenarioData.pause_policy -eq "pause-resume") {
                $controllerArgs += @("--pause-hold-ms", [string] $script:ScenarioData.pause_hold_ms)
            } else {
                $controllerArgs += "--no-pause"
            }
            $script:ActiveController = Start-ExactProcess `
                $script:Controller $controllerArgs $childEnvironment
            if (-not $script:ActiveController.WaitForExit(($timeoutSeconds + 30) * 1000)) {
                throw "timed out waiting for $Label controller"
            }
            $controllerExit = $script:ActiveController.ExitCode
            $script:ActiveController.Dispose()
            $script:ActiveController = $null
            if ($controllerExit -ne 0) { throw "$Label controller exited with code $controllerExit" }
        }

        if (-not $script:ActiveConpty.WaitForExit(($timeoutSeconds + 30) * 1000)) {
            throw "timed out waiting for $Label ConPTY process run"
        }
        $conptyExit = $script:ActiveConpty.ExitCode
        $script:ActiveConpty.Dispose()
        $script:ActiveConpty = $null
        if ($conptyExit -ne 0) { throw "$Label ConPTY process run exited with code $conptyExit" }
        if (-not (Test-Path -LiteralPath $conptyProof -PathType Leaf)) {
            throw "$Label did not publish ConPTY proof"
        }

        $checkArgs = @(
            "check", "--samples", $samples, "--scenario-sha256", $script:ScenarioHash
        )
        if ($script:RequiresMpv) { $checkArgs += "--require-silent-mpv" }
        if ($script:ScenarioData.controller) {
            $checkArgs += @(
                "--control", (Join-Path $runDir "control.ndjson"),
                "--require-observer-close"
            )
        }
        Invoke-PythonChecked $checkArgs "raw process artifact check failed for $Label" | Out-Null
        Stop-FixtureServer $true
    } finally {
        if ($null -ne $script:ActiveController) {
            if (-not $script:ActiveController.HasExited) { $script:ActiveController.Kill() }
            [void] $script:ActiveController.WaitForExit(5000)
            $script:ActiveController.Dispose()
            $script:ActiveController = $null
        }
        if ($null -ne $script:ActiveConpty) {
            if (-not $script:ActiveConpty.HasExited) { $script:ActiveConpty.Kill() }
            [void] $script:ActiveConpty.WaitForExit(5000)
            $script:ActiveConpty.Dispose()
            $script:ActiveConpty = $null
        }
        Stop-FixtureServer $false
        if (Test-Path -LiteralPath $home -PathType Container) {
            Invoke-PythonChecked @(
                "sanitize-runtime-evidence", "--root", $home
            ) "failed to sanitize runtime evidence for $Label" | Out-Null
        }
    }
}

function Run-ProcessGeometries(
    [string] $Role,
    [string] $Binary,
    [string] $RunRoot,
    [string] $Label,
    [string] $Kind,
    [int] $RunIndex,
    [string] $RootRunId
) {
    for ($geometryIndex = 0; $geometryIndex -lt $script:GeometryCount; $geometryIndex++) {
        $width = [int] $script:ScenarioData.geometry[$geometryIndex][0]
        $height = [int] $script:ScenarioData.geometry[$geometryIndex][1]
        $geometryDir = $RunRoot
        $geometryRunId = $RootRunId
        if ($script:GeometryCount -gt 1) {
            $geometryDir = Join-Path $RunRoot ("geometry-{0}x{1}" -f $width, $height)
            $startArgs = @(
                "run-start", "--scenarios", $Scenarios, "--scenario", $Scenario,
                "--output", (Join-Path $geometryDir "run-contract.json"),
                "--kind", $Kind, "--role", $Role,
                "--geometry-index", [string] $geometryIndex,
                "--width", [string] $width, "--height", [string] $height
            )
            if ($Kind -eq "paired") {
                $startArgs += @("--pair-index", [string] $RunIndex)
            } else {
                $startArgs += @("--repeat-index", [string] $RunIndex)
            }
            $geometryRunId = ((Invoke-PythonChecked $startArgs "failed to start geometry contract") | Out-String).Trim()
        }
        Run-ProcessScenario $Role $Binary $RunRoot $width $height `
            "$Label geometry ${width}x${height}" $geometryRunId
        if ($script:GeometryCount -gt 1) {
            Invoke-PythonChecked @(
                "run-finish", "--contract", (Join-Path $geometryDir "run-contract.json")
            ) "failed to finish geometry contract" | Out-Null
        }
    }
}

& $script:PythonExe $PythonTool validate --scenarios $Scenarios | Out-Null
if ($LASTEXITCODE -ne 0) { throw "invalid scenarios file" }
$scenarioJson = & $script:PythonExe $PythonTool scenario `
    --scenarios $Scenarios --id $Scenario
if ($LASTEXITCODE -ne 0) { throw "cannot load scenario $Scenario" }
$script:ScenarioData = ($scenarioJson | Out-String) | ConvertFrom-Json
$script:ScenarioHash = [string] $script:ScenarioData.scenario_sha256
$script:GeometryCount = [int] $script:ScenarioData.geometry.Count
$script:RequiresMpv = [bool] $script:ScenarioData.requires_mpv
$isRender = $Scenario -eq "render_and_interaction"

foreach ($sourceRoot in @($BaselineSourceRoot, $CandidateSourceRoot)) {
    if (-not (Test-Path -LiteralPath $sourceRoot -PathType Container)) {
        throw "source root is not a directory: $sourceRoot"
    }
}
$BaselineSeedHome = if ($BaselineSeedHome) { $BaselineSeedHome } else { $SeedHome }
$CandidateSeedHome = if ($CandidateSeedHome) { $CandidateSeedHome } else { $SeedHome }
if ($script:RequiresMpv) {
    foreach ($seedRoot in @($BaselineSeedHome, $CandidateSeedHome)) {
        if (-not $seedRoot -or -not (Test-Path -LiteralPath $seedRoot -PathType Container)) {
            throw "baseline and candidate seed homes are required for playback scenarios"
        }
    }
    if (-not $MpvSelectionManifest -or -not (Test-Path -LiteralPath $MpvSelectionManifest -PathType Leaf)) {
        throw "MpvSelectionManifest is required for playback scenarios"
    }
    $script:SelectedMpv = ((Invoke-PythonChecked @(
        "mpv-selection", "--manifest", $MpvSelectionManifest,
        "--field", "binary.path"
    ) "invalid target-local mpv selection") | Out-String).Trim()
    $script:MpvTargetRoot = ((Invoke-PythonChecked @(
        "mpv-selection", "--manifest", $MpvSelectionManifest,
        "--field", "target_root"
    ) "invalid target-local mpv selection root") | Out-String).Trim()
} elseif ($MpvSelectionManifest) {
    throw "non-playback scenarios reject MpvSelectionManifest"
}

$preflightArgs = @(
    "path-preflight", "--output-root", $Output,
    "--protected-root", $BaselineSourceRoot,
    "--protected-root", $CandidateSourceRoot
)
if ($script:RequiresMpv) {
    $preflightArgs += @(
        "--protected-root", $BaselineSeedHome,
        "--protected-root", $CandidateSeedHome
    )
    $preflightArgs += @("--protected-root", $script:MpvTargetRoot)
}
$resolvedOutput = Invoke-PythonChecked $preflightArgs "output/source path preflight failed"
$script:OutputRoot = ($resolvedOutput | Out-String).Trim()
if (-not $script:OutputRoot) { throw "path preflight returned an empty output root" }
New-Item -ItemType Directory -Path $script:OutputRoot | Out-Null

$controlledReceipt = Join-Path $script:OutputRoot "build-receipt.json"
$buildTarget = Join-Path ([IO.Path]::GetTempPath()) ("ytt-perf-build-" + [Guid]::NewGuid().ToString("N"))
try {
    Invoke-PythonChecked @(
        "build", "--scenarios", $Scenarios, "--scenario", $Scenario,
        "--baseline-root", $BaselineSourceRoot,
        "--candidate-root", $CandidateSourceRoot,
        "--output", $controlledReceipt,
        "--target-root", $buildTarget
    ) "controlled source-bound build failed" | Out-Null
} finally {
    if (Test-Path -LiteralPath $buildTarget) {
        Remove-Item -LiteralPath $buildTarget -Recurse -Force
    }
}

if ($isRender) {
    $script:BaselineRender = Get-ReceiptArtifact $controlledReceipt "baseline_render"
    $script:CandidateRender = Get-ReceiptArtifact $controlledReceipt "candidate_render"
} else {
    $script:BaselineBinary = Get-ReceiptArtifact $controlledReceipt "baseline_ytt"
    $script:CandidateBinary = Get-ReceiptArtifact $controlledReceipt "candidate_ytt"
    $script:Sampler = Get-ReceiptArtifact $controlledReceipt "sampler"
    $script:Controller = Get-ReceiptArtifact $controlledReceipt "controller"
    $script:Conpty = Get-ReceiptArtifact $controlledReceipt "conpty"
}

if ($script:RequiresMpv) {
    Invoke-PythonChecked @(
        "seed-contract", "--scenarios", $Scenarios, "--scenario", $Scenario,
        "--baseline-root", $BaselineSeedHome,
        "--candidate-root", $CandidateSeedHome,
        "--snapshot", (Join-Path $script:OutputRoot "seed-template"),
        "--output", (Join-Path $script:OutputRoot "seed-contract.json")
    ) "seed contract failed" | Out-Null
    $script:BaselineSeedHome = Join-Path $script:OutputRoot "seed-template"
    $script:CandidateSeedHome = $script:BaselineSeedHome
    $fixtureProfile = [string] $script:ScenarioData.fixture_profile
    $fixtureContainer = ((Invoke-PythonChecked @(
        "setting", "--scenarios", $Scenarios,
        "--field", "fixture_profiles.$fixtureProfile.container"
    ) "cannot load fixture container $fixtureProfile") | Out-String).Trim()
    $script:FixtureFile = Join-Path $script:OutputRoot (
        "fixture\{0}.{1}" -f $fixtureProfile, $fixtureContainer
    )
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $script:FixtureFile) | Out-Null
    Invoke-PythonChecked @(
        "fixture", "--output", $script:FixtureFile,
        "--manifest", (Join-Path $script:OutputRoot "fixture\manifest.json"),
        "--profile", $fixtureProfile, "--scenarios", $Scenarios
    ) "fixture generation failed" | Out-Null
    $trafficName = [string] $script:ScenarioData.traffic_profile
    $trafficJson = & $script:PythonExe $PythonTool traffic `
        --scenarios $Scenarios --name $trafficName
    if ($LASTEXITCODE -ne 0) { throw "cannot load traffic profile $trafficName" }
    $script:TrafficProfile = ($trafficJson | Out-String) | ConvertFrom-Json
} else {
    $script:BaselineSeedHome = $null
    $script:CandidateSeedHome = $null
    $script:FixtureFile = $null
    $script:TrafficProfile = $null
}
$samplingJson = & $script:PythonExe $PythonTool setting `
    --scenarios $Scenarios --field sampling.interval_ms
if ($LASTEXITCODE -ne 0) { throw "cannot load sampling interval" }
$script:SamplingInterval = [int] (($samplingJson | Out-String).Trim())

$manifestArgs = @(
    "manifest", "--scenarios", $Scenarios, "--scenario", $Scenario,
    "--output", (Join-Path $script:OutputRoot "host-manifest.json"),
    "--build-receipt", $controlledReceipt
)
if ($script:RequiresMpv) {
    $manifestArgs += @("--mpv-selection-manifest", $MpvSelectionManifest)
}
Invoke-PythonChecked $manifestArgs "failed to write host manifest" | Out-Null

$baselineRuns = @()
$candidateRuns = @()
$candidateRepeatRuns = @()
try {
    for ($pair = 1; $pair -le [int] $script:ScenarioData.pairs; $pair++) {
        $order = if ($pair % 2) { @("baseline", "candidate") } else { @("candidate", "baseline") }
        foreach ($role in $order) {
            $runRoot = Join-Path $script:OutputRoot ("pair-{0:D2}\{1}" -f $pair, $role)
            $runId = ""
            $rootContract = $isRender -or $script:GeometryCount -eq 1
            if ($rootContract) {
                $runId = ((Invoke-PythonChecked @(
                    "run-start", "--scenarios", $Scenarios, "--scenario", $Scenario,
                    "--output", (Join-Path $runRoot "run-contract.json"),
                    "--kind", "paired", "--role", $role,
                    "--pair-index", [string] $pair
                ) "failed to start paired run contract") | Out-String).Trim()
            }
            if ($isRender) {
                $binary = if ($role -eq "baseline") { $script:BaselineRender } else { $script:CandidateRender }
                Run-Render $role $binary $runRoot $runId
            } else {
                $binary = if ($role -eq "baseline") { $script:BaselineBinary } else { $script:CandidateBinary }
                Run-ProcessGeometries $role $binary $runRoot "$role pair $pair" `
                    "paired" $pair $runId
            }
            if ($rootContract) {
                Invoke-PythonChecked @(
                    "run-finish", "--contract", (Join-Path $runRoot "run-contract.json")
                ) "failed to finish paired run contract" | Out-Null
            }
        }
        $baselineRuns += @(
            "--baseline-run", (Join-Path $script:OutputRoot ("pair-{0:D2}\baseline" -f $pair))
        )
        $candidateRuns += @(
            "--candidate-run", (Join-Path $script:OutputRoot ("pair-{0:D2}\candidate" -f $pair))
        )
    }

    for ($repeat = 1; $repeat -le [int] $script:ScenarioData.candidate_repeats; $repeat++) {
        $repeatDir = Join-Path $script:OutputRoot ("candidate-repeat-{0:D2}" -f $repeat)
        $runId = ""
        $rootContract = $isRender -or $script:GeometryCount -eq 1
        if ($rootContract) {
            $runId = ((Invoke-PythonChecked @(
                "run-start", "--scenarios", $Scenarios, "--scenario", $Scenario,
                "--output", (Join-Path $repeatDir "run-contract.json"),
                "--kind", "candidate_repeat", "--role", "candidate",
                "--repeat-index", [string] $repeat
            ) "failed to start candidate repeat contract") | Out-String).Trim()
        }
        if ($isRender) {
            Run-Render "candidate" $script:CandidateRender $repeatDir $runId
        } else {
            Run-ProcessGeometries "candidate" $script:CandidateBinary $repeatDir `
                "candidate diagnostic repeat $repeat" "candidate_repeat" $repeat $runId
        }
        if ($rootContract) {
            Invoke-PythonChecked @(
                "run-finish", "--contract", (Join-Path $repeatDir "run-contract.json")
            ) "failed to finish candidate repeat contract" | Out-Null
        }
        $candidateRepeatRuns += @("--candidate-repeat-run", $repeatDir)
    }

    $compareArgs = @(
        "compare", "--scenarios", $Scenarios, "--scenario", $Scenario,
        "--host-manifest", (Join-Path $script:OutputRoot "host-manifest.json")
    ) + $baselineRuns + $candidateRuns + $candidateRepeatRuns + @(
        "--output-json", (Join-Path $script:OutputRoot "report.json"),
        "--output-markdown", (Join-Path $script:OutputRoot "report.md")
    )
    Invoke-PythonChecked @(
        "privacy-check", "--root", $script:OutputRoot
    ) "pre-report privacy gate failed" | Out-Null
    & $script:PythonExe $PythonTool @compareArgs
    $comparisonExitCode = $LASTEXITCODE
    Invoke-PythonChecked @(
        "privacy-check", "--root", $script:OutputRoot
    ) "report privacy gate failed" | Out-Null
    Invoke-PythonChecked @(
        "create-checksums", "--root", $script:OutputRoot,
        "--output", (Join-Path $script:OutputRoot "SHA256SUMS")
    ) "failed to write or verify SHA256SUMS" | Out-Null
    if ($comparisonExitCode -ne 0) {
        throw "paired performance gate failed with exit code $comparisonExitCode"
    }
} finally {
    Stop-ActiveExactProcesses
}

Write-Host ("Transport verification: {0} `"{1}`" verify-checksums --root `"{2}`" --output `"{3}`"" -f `
    $script:PythonExe, $PythonTool, $script:OutputRoot, (Join-Path $script:OutputRoot "SHA256SUMS"))
