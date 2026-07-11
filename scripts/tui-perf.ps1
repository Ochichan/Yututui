<#
.SYNOPSIS
Runs paired ytt TUI performance scenarios in isolated per-run profiles.

.PARAMETER SeedHome
Playback seed template. Persisted ytt state must be stored under
stores\config, stores\data, and stores\cache; the whole template is copied per run.
The resumed Song.local_path must be the literal {{TUI_PERF_PLAYLIST}} placeholder.
BaselineSeedHome and CandidateSeedHome override this template per role, enabling explicit
32MiB/8MiB reference vs candidate cache-pair exploration without mutating either seed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $Scenario,
    [Parameter(Mandatory = $true)] [string] $Output,
    [string] $BaselineBinary,
    [string] $CandidateBinary,
    [string] $BaselineRender,
    [string] $CandidateRender,
    [string] $BaselineSourceRoot,
    [string] $CandidateSourceRoot,
    [Parameter(Mandatory = $true)] [string] $BaselineBuildCommand,
    [Parameter(Mandatory = $true)] [string] $CandidateBuildCommand,
    [string] $SeedHome,
    [string] $BaselineSeedHome,
    [string] $CandidateSeedHome,
    [string] $Sampler,
    [string] $Controller,
    [string] $Scenarios,
    [switch] $NoBuild
)

$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $PSScriptRoot
$PythonTool = Join-Path $PSScriptRoot "tui-perf.py"
if (-not $Scenarios) { $Scenarios = Join-Path $PSScriptRoot "tui-perf-scenarios.json" }
if (-not $Sampler) { $Sampler = Join-Path $Repo "target\release\examples\tui_perf_sampler.exe" }
if (-not $Controller) { $Controller = Join-Path $Repo "target\release\examples\tui_perf_control.exe" }

function Invoke-Python {
    param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $Arguments)
    & python $PythonTool @Arguments
    if ($LASTEXITCODE -ne 0) { throw "tui-perf.py failed with exit code $LASTEXITCODE" }
}

function Get-ScenarioField([string] $Name) {
    $value = & python $PythonTool scenario --scenarios $Scenarios --id $Scenario --field $Name
    if ($LASTEXITCODE -ne 0) { throw "cannot read scenario field $Name" }
    return ($value | Out-String).Trim()
}

function Get-TrafficField([string] $Name) {
    $value = & python $PythonTool traffic --scenarios $Scenarios --name $script:TrafficProfile --field $Name
    if ($LASTEXITCODE -ne 0) { throw "cannot read traffic field $Name" }
    return ($value | Out-String).Trim()
}

function Get-SettingField([string] $Name) {
    $value = & python $PythonTool setting --scenarios $Scenarios --field $Name
    if ($LASTEXITCODE -ne 0) { throw "cannot read top-level scenario setting $Name" }
    return ($value | Out-String).Trim()
}

function ConvertTo-WindowsArgument([string] $Value) {
    if ($Value -notmatch '[\s"]') { return $Value }
    $builder = New-Object System.Text.StringBuilder
    [void] $builder.Append('"')
    $slashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') {
            $slashes++
        } elseif ($character -eq '"') {
            [void] $builder.Append((('\' * ($slashes * 2 + 1)) -join ''))
            [void] $builder.Append('"')
            $slashes = 0
        } else {
            if ($slashes) { [void] $builder.Append((('\' * $slashes) -join '')); $slashes = 0 }
            [void] $builder.Append($character)
        }
    }
    if ($slashes) { [void] $builder.Append((('\' * ($slashes * 2)) -join '')) }
    [void] $builder.Append('"')
    return $builder.ToString()
}

function Start-ExactProcess([string] $FilePath, [string[]] $Arguments) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $FilePath
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $false
    if ($info.PSObject.Properties.Name -contains "ArgumentList") {
        foreach ($argument in $Arguments) { [void] $info.ArgumentList.Add($argument) }
    } else {
        $info.Arguments = (($Arguments | ForEach-Object { ConvertTo-WindowsArgument $_ }) -join ' ')
    }
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $info
    if (-not $process.Start()) { throw "failed to launch $FilePath" }
    return $process
}

function Set-IsolatedEnvironment([string] $RunDir, [string] $Home) {
    $runtime = Join-Path $RunDir "runtime"
    $temp = Join-Path $RunDir "tmp"
    $roaming = Join-Path $Home "AppData\Roaming"
    $local = Join-Path $Home "AppData\Local"
    $configStore = Join-Path $Home "stores\config"
    $dataStore = Join-Path $Home "stores\data"
    $cacheStore = Join-Path $Home "stores\cache"
    foreach ($directory in @($runtime, $temp, $roaming, $local, $configStore, $dataStore, $cacheStore)) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }
    $values = @{
        HOME = $Home
        USERPROFILE = $Home
        APPDATA = $roaming
        LOCALAPPDATA = $local
        XDG_CONFIG_HOME = (Join-Path $Home ".config")
        XDG_DATA_HOME = (Join-Path $Home ".local\share")
        XDG_CACHE_HOME = (Join-Path $Home ".cache")
        XDG_STATE_HOME = (Join-Path $Home ".local\state")
        XDG_RUNTIME_DIR = $runtime
        YTM_CONFIG_DIR = $configStore
        YTM_DATA_DIR = $dataStore
        YTM_CACHE_DIR = $cacheStore
        TEMP = $temp
        TMP = $temp
        TERM = "xterm-256color"
        YTM_MPV_EXTRA = "--ao=null --volume=0 --audio-display=no"
        YTM_PERF = "1"
        TUI_PERF_SCENARIO_SHA256 = $script:ScenarioHash
    }
    $prior = @{}
    foreach ($name in $values.Keys) {
        $prior[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
        [Environment]::SetEnvironmentVariable($name, $values[$name], "Process")
    }
    return $prior
}

function Restore-Environment([hashtable] $Prior) {
    foreach ($name in $Prior.Keys) {
        [Environment]::SetEnvironmentVariable($name, $Prior[$name], "Process")
    }
}

function Stop-RecordedYtt([string] $PidFile) {
    if (-not (Test-Path -LiteralPath $PidFile)) { return }
    $recorded = 0
    if (-not [int]::TryParse((Get-Content -LiteralPath $PidFile -Raw).Trim(), [ref] $recorded)) { return }
    $process = Get-Process -Id $recorded -ErrorAction SilentlyContinue
    if ($process) {
        # Exact sampler-recorded PID only. ytt's Windows Job Object owns its mpv descendants.
        Stop-Process -Id $recorded -Force -ErrorAction SilentlyContinue
    }
}

function Run-Render([string] $Role, [string] $Binary, [string] $RunDir) {
    $runDir = $RunDir
    New-Item -ItemType Directory -Force -Path $runDir | Out-Null
    $oldHash = $env:TUI_PERF_SCENARIO_SHA256
    try {
        $env:TUI_PERF_SCENARIO_SHA256 = $script:ScenarioHash
        & $Binary --output (Join-Path $runDir "render.json") `
            --warmup (Get-ScenarioField "warmup_draws") `
            --batches (Get-ScenarioField "batches") `
            --draws (Get-ScenarioField "draws_per_batch")
        if ($LASTEXITCODE -ne 0) { throw "$Role render harness failed" }
    } finally {
        $env:TUI_PERF_SCENARIO_SHA256 = $oldHash
    }
}

function Run-Process(
    [string] $Role,
    [string] $Binary,
    [string] $RunRoot,
    [int] $Width,
    [int] $Height,
    [string] $Label
) {
    $runDir = $RunRoot
    if ($script:GeometryCount -gt 1) {
        $runDir = Join-Path $RunRoot ("geometry-{0}x{1}" -f $Width, $Height)
    }
    $roleSeedHome = if ($Role -eq "baseline") { $BaselineSeedHome } else { $CandidateSeedHome }
    $home = Join-Path $runDir "home"
    $samples = Join-Path $runDir "samples.ndjson"
    $control = Join-Path $runDir "control.ndjson"
    $pidFile = Join-Path $runDir "ytt.pid"
    $readyFile = Join-Path $runDir "http-ready.json"
    New-Item -ItemType Directory -Force -Path $home | Out-Null
    if ($roleSeedHome) {
        Get-ChildItem -LiteralPath $roleSeedHome -Force |
            Copy-Item -Destination $home -Recurse -Force
    }
    $serverProcess = $null
    $samplerProcess = $null
    $prior = $null
    try {
        if ($script:RequiresMpv) {
            Remove-Item -LiteralPath $readyFile -Force -ErrorAction SilentlyContinue
            $serverArgs = @(
                $PythonTool, "serve",
                "--file", $script:FixtureFile,
                "--ready-file", $readyFile,
                "--throttle-bps", (Get-TrafficField "throttle_bps"),
                "--outage-every-bytes", (Get-TrafficField "outage_every_bytes"),
                "--outage-ms", (Get-TrafficField "outage_ms"),
                "--disconnect-every-bytes", (Get-TrafficField "disconnect_every_bytes")
            )
            $serverProcess = Start-ExactProcess "python" $serverArgs
            $serverDeadline = [DateTime]::UtcNow.AddSeconds(10)
            while (-not (Test-Path -LiteralPath $readyFile)) {
                if ($serverProcess.HasExited) { throw "fixture server exited before publishing its URL" }
                if ([DateTime]::UtcNow -ge $serverDeadline) { throw "fixture server startup timed out" }
                Start-Sleep -Milliseconds 50
            }
            $fixtureUrl = (Get-Content -LiteralPath $readyFile -Raw | ConvertFrom-Json).url
            & python $PythonTool materialize `
                --root $home `
                --home $home `
                --fixture-url $fixtureUrl `
                --seed-label $Role `
                --manifest (Join-Path $runDir "materialize.json") | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "failed to materialize isolated seed home" }
        }
        $prior = Set-IsolatedEnvironment $runDir $home
        try {
            $buffer = $Host.UI.RawUI.BufferSize
            if ($buffer.Width -lt $Width -or $buffer.Height -lt $Height) {
                $Host.UI.RawUI.BufferSize = [System.Management.Automation.Host.Size]::new(
                    [Math]::Max($buffer.Width, $Width),
                    [Math]::Max($buffer.Height, $Height)
                )
            }
            $Host.UI.RawUI.WindowSize = [System.Management.Automation.Host.Size]::new($Width, $Height)
        } catch {
            throw "cannot establish the required ${Width}x${Height} interactive console for ${Label}: $_"
        }
        $samplerArgs = @(
            "--output", $samples,
            "--pid-file", $pidFile,
            "--binary", $Binary,
            "--warmup-secs", (Get-ScenarioField "warmup_s"),
            "--duration-secs", (Get-ScenarioField "sample_s"),
            "--interval-ms", "1000"
        )
        if ($script:RequiresMpv) { $samplerArgs += "--require-silent-mpv" }
        $controllerEnabled = (Get-ScenarioField "controller") -eq "true"
        # Controller runs need ytt to publish its primary descriptor inside this run's isolated
        # runtime directory. Non-controller runs retain the private new-instance endpoint.
        if (-not $controllerEnabled) { $samplerArgs += @("--", "--new-instance") }
        Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
        $samplerProcess = Start-ExactProcess $Sampler $samplerArgs

        $readyAt = [DateTime]::UtcNow.AddSeconds(30)
        while (-not (Test-Path -LiteralPath $pidFile)) {
            if ($samplerProcess.HasExited) { throw "$Role sampler exited before publishing ytt PID" }
            if ([DateTime]::UtcNow -ge $readyAt) { throw "$Role sampler timed out before publishing ytt PID" }
            Start-Sleep -Milliseconds 100
        }

        if ($controllerEnabled) {
            $controllerArgs = @(
                "--output", $control,
                "--wait-secs", "45",
                "--observe-secs", ([double](Get-ScenarioField "warmup_s") + [double](Get-ScenarioField "sample_s")),
                "--close-grace-secs", "15",
                "--load", (Get-ScenarioField "controller_load")
            )
            $seekValues = (Get-ScenarioField "seeks_s") | ConvertFrom-Json
            if ($seekValues.Count -gt 0) { $controllerArgs += @("--seeks", ($seekValues -join ',')) }
            $pausePolicy = Get-ScenarioField "pause_policy"
            $pauseHoldMs = Get-ScenarioField "pause_hold_ms"
            if ($pausePolicy -eq "pause-resume") {
                $controllerArgs += @("--pause-hold-ms", $pauseHoldMs)
            } else {
                $controllerArgs += "--no-pause"
            }
            & $Controller @controllerArgs
            if ($LASTEXITCODE -ne 0) { throw "$Role controller failed" }
        }

        $timeoutSeconds = [Math]::Ceiling(
            [double](Get-ScenarioField "warmup_s") + [double](Get-ScenarioField "sample_s") + 90
        )
        $deadline = [DateTime]::UtcNow.AddSeconds($timeoutSeconds)
        while (-not $samplerProcess.HasExited) {
            if ([DateTime]::UtcNow -ge $deadline) { throw "$Role sampler timed out" }
            Start-Sleep -Seconds 1
        }
        if ($samplerProcess.ExitCode -ne 0) { throw "$Role sampler exited $($samplerProcess.ExitCode)" }

        $checkArgs = @("check", "--samples", $samples, "--scenario-sha256", $script:ScenarioHash)
        if ($script:RequiresMpv) { $checkArgs += "--require-silent-mpv" }
        if ($controllerEnabled) {
            $checkArgs += @("--control", $control, "--require-observer-close")
        }
        & python $PythonTool @checkArgs | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "$Role artifacts failed validation" }
    } finally {
        if ($samplerProcess -and -not $samplerProcess.HasExited) {
            Stop-RecordedYtt $pidFile
            try { $samplerProcess.Kill() } catch { }
        }
        if ($serverProcess -and -not $serverProcess.HasExited) {
            try { $serverProcess.Kill() } catch { }
            try { $serverProcess.WaitForExit() } catch { }
        }
        if ($prior) { Restore-Environment $prior }
    }
}

function Run-ProcessGeometries(
    [string] $Role,
    [string] $Binary,
    [string] $RunRoot,
    [string] $Label
) {
    for ($geometryIndex = 0; $geometryIndex -lt $script:GeometryCount; $geometryIndex++) {
        $width = [int](Get-ScenarioField ("geometry.{0}.0" -f $geometryIndex))
        $height = [int](Get-ScenarioField ("geometry.{0}.1" -f $geometryIndex))
        Run-Process $Role $Binary $RunRoot $width $height `
            ("{0} geometry {1}x{2}" -f $Label, $width, $height)
    }
}

& python $PythonTool validate --scenarios $Scenarios | Out-Null
if ($LASTEXITCODE -ne 0) { throw "invalid scenarios file" }
$script:ScenarioHash = Get-ScenarioField "sha256"
$pairs = [int](Get-ScenarioField "pairs")
$candidateRepeats = [int](Get-ScenarioField "candidate_repeats")
$script:GeometryCount = [int](Get-ScenarioField "geometry.length")
$script:RequiresMpv = (Get-ScenarioField "requires_mpv") -eq "true"
$script:TrafficProfile = Get-ScenarioField "traffic_profile"
$fixtureSeconds = Get-SettingField "fixture.duration_s"
$fixtureSampleRate = Get-SettingField "fixture.sample_rate_hz"
$isRender = $Scenario -eq "render_and_interaction"
if (-not $BaselineSeedHome) { $BaselineSeedHome = $SeedHome }
if (-not $CandidateSeedHome) { $CandidateSeedHome = $SeedHome }
$script:OutputRoot = [IO.Path]::GetFullPath($Output)
if (Test-Path -LiteralPath $script:OutputRoot) {
    throw "-Output must name a new path; existing evidence is never reused"
}
New-Item -ItemType Directory -Path $script:OutputRoot | Out-Null
$script:FixtureFile = Join-Path $script:OutputRoot ("fixture\silence-{0}s.wav" -f $fixtureSeconds)
if ($script:RequiresMpv -and -not (Test-Path -LiteralPath $script:FixtureFile)) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $script:FixtureFile) | Out-Null
    & python $PythonTool fixture `
        --output $script:FixtureFile `
        --manifest (Join-Path (Split-Path -Parent $script:FixtureFile) "manifest.json") `
        --seconds $fixtureSeconds `
        --sample-rate $fixtureSampleRate | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "failed to generate deterministic silence fixture" }
}

if (-not $NoBuild) {
    Push-Location $Repo
    try {
        & cargo build --release --locked --example tui_perf_sampler --example tui_perf_control --example tui_render_perf
        if ($LASTEXITCODE -ne 0) { throw "harness build failed" }
    } finally {
        Pop-Location
    }
}

if ($isRender) {
    if (-not $CandidateRender) { $CandidateRender = Join-Path $Repo "target\release\examples\tui_render_perf.exe" }
    if (-not (Test-Path -LiteralPath $BaselineRender -PathType Leaf)) { throw "--BaselineRender is required" }
    if (-not (Test-Path -LiteralPath $CandidateRender -PathType Leaf)) { throw "candidate render harness missing" }
} else {
    if ([Console]::IsOutputRedirected -or -not [Environment]::UserInteractive) {
        throw "Windows process scenarios require a local interactive console/ConPTY; stage and retrieve artifacts over SSH, but run this script locally"
    }
    foreach ($binary in @($BaselineBinary, $CandidateBinary, $Sampler, $Controller)) {
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "required executable missing: $binary" }
    }
    if ($script:RequiresMpv) {
        foreach ($seedSpec in @(
            @{ Role = "baseline"; Path = $BaselineSeedHome },
            @{ Role = "candidate"; Path = $CandidateSeedHome }
        )) {
            if (-not $seedSpec.Path -or -not (Test-Path -LiteralPath $seedSpec.Path -PathType Container)) {
                throw "playback scenarios require a $($seedSpec.Role) seed home"
            }
            foreach ($store in @("config", "data", "cache")) {
                $storePath = Join-Path $seedSpec.Path ("stores\{0}" -f $store)
                if (-not (Test-Path -LiteralPath $storePath -PathType Container)) {
                    throw "$($seedSpec.Role) seed home must contain stores\$store"
                }
            }
        }
    }
}

if (-not $CandidateSourceRoot) { $CandidateSourceRoot = $Repo }
if (-not $BaselineSourceRoot) {
    $identityBinary = if ($isRender) { $BaselineRender } else { $BaselineBinary }
    $binaryParent = Split-Path -Parent ([IO.Path]::GetFullPath($identityBinary))
    $detectedRoot = & git -C $binaryParent rev-parse --show-toplevel
    if ($LASTEXITCODE -ne 0) {
        throw "cannot infer -BaselineSourceRoot from $identityBinary"
    }
    $BaselineSourceRoot = ($detectedRoot | Out-String).Trim()
}
foreach ($sourceRoot in @($BaselineSourceRoot, $CandidateSourceRoot)) {
    if (-not (Test-Path -LiteralPath $sourceRoot -PathType Container)) {
        throw "source root is not a directory: $sourceRoot"
    }
}

$manifestArgs = @(
    "manifest",
    "--scenarios", $Scenarios,
    "--scenario", $Scenario,
    "--output", (Join-Path $script:OutputRoot "host-manifest.json"),
    "--source-root", "baseline=$BaselineSourceRoot",
    "--source-root", "candidate=$CandidateSourceRoot",
    "--build-command", "baseline=$BaselineBuildCommand",
    "--build-command", "candidate=$CandidateBuildCommand"
)
if ($isRender) {
    $manifestArgs += @("--binary", "baseline_render=$BaselineRender")
    $manifestArgs += @("--binary", "candidate_render=$CandidateRender")
} else {
    $manifestArgs += @("--binary", "baseline_ytt=$BaselineBinary")
    $manifestArgs += @("--binary", "candidate_ytt=$CandidateBinary")
    $manifestArgs += @("--binary", "sampler=$Sampler")
    $manifestArgs += @("--binary", "controller=$Controller")
}
& python $PythonTool @manifestArgs | Out-Null
if ($LASTEXITCODE -ne 0) { throw "failed to write host manifest" }

$baselineRuns = @()
$candidateRuns = @()
for ($pair = 1; $pair -le $pairs; $pair++) {
    $order = if ($pair % 2) { @("baseline", "candidate") } else { @("candidate", "baseline") }
    foreach ($role in $order) {
        if ($isRender) {
            $binary = if ($role -eq "baseline") { $BaselineRender } else { $CandidateRender }
            $runDir = Join-Path $script:OutputRoot ("pair-{0:D2}\{1}" -f $pair, $role)
            Run-Render $role $binary $runDir
        } else {
            $binary = if ($role -eq "baseline") { $BaselineBinary } else { $CandidateBinary }
            $runRoot = Join-Path $script:OutputRoot ("pair-{0:D2}\{1}" -f $pair, $role)
            Run-ProcessGeometries $role $binary $runRoot ("$role pair $pair")
        }
    }
    $baselineRuns += @("--baseline-run", (Join-Path $script:OutputRoot ("pair-{0:D2}\baseline" -f $pair)))
    $candidateRuns += @("--candidate-run", (Join-Path $script:OutputRoot ("pair-{0:D2}\candidate" -f $pair)))
}

$candidateRepeatRuns = @()
for ($repeat = 1; $repeat -le $candidateRepeats; $repeat++) {
    $repeatDir = Join-Path $script:OutputRoot ("candidate-repeat-{0:D2}" -f $repeat)
    if ($isRender) {
        Run-Render "candidate" $CandidateRender $repeatDir
    } else {
        Run-ProcessGeometries "candidate" $CandidateBinary $repeatDir `
            ("candidate diagnostic repeat $repeat")
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
& python $PythonTool @compareArgs
$comparisonExitCode = $LASTEXITCODE
& python $PythonTool checksums `
    --root $script:OutputRoot `
    --output (Join-Path $script:OutputRoot "SHA256SUMS") | Out-Null
if ($LASTEXITCODE -ne 0) { throw "failed to write or verify SHA256SUMS" }
if ($comparisonExitCode -ne 0) { throw "paired performance gate failed with exit code $comparisonExitCode" }
