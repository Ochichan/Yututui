<#
.SYNOPSIS
Runs paired ytt render-performance scenarios from fresh source-bound builds.

.DESCRIPTION
Render scenarios are supported on Windows. Process scenarios fail closed before creating output
until this wrapper can provide a run-unique off-screen ConPTY with controlled empty input and
exact geometry, without inheriting or resizing the parent console.

Seed parameters remain accepted so an existing cross-platform invocation reaches the explicit
isolation error instead of failing during parameter binding. They are never read on Windows.
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
    [string] $Scenarios
)

$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $PSScriptRoot
$PythonTool = Join-Path $PSScriptRoot "tui-perf.py"
if (-not $Scenarios) { $Scenarios = Join-Path $PSScriptRoot "tui-perf-scenarios.json" }

function Get-ScenarioField([string] $Name) {
    $value = & python $PythonTool scenario --scenarios $Scenarios --id $Scenario --field $Name
    if ($LASTEXITCODE -ne 0) { throw "cannot read scenario field $Name" }
    return ($value | Out-String).Trim()
}

function Assert-WindowsProcessIsolation {
    throw "Windows process scenarios are unsupported: this PowerShell 5.1 wrapper cannot provide a run-unique off-screen ConPTY with controlled empty input and exact geometry without mutating or inheriting the parent console. No process measurement was started and no output was created."
}

function Run-Render([string] $Role, [string] $Binary, [string] $RunDir, [string] $RunId) {
    New-Item -ItemType Directory -Force -Path $RunDir | Out-Null
    $oldHash = [Environment]::GetEnvironmentVariable("TUI_PERF_SCENARIO_SHA256", "Process")
    $oldYtmPerf = [Environment]::GetEnvironmentVariable("YTM_PERF", "Process")
    $oldRunId = [Environment]::GetEnvironmentVariable("TUI_PERF_RUN_ID", "Process")
    try {
        [Environment]::SetEnvironmentVariable("TUI_PERF_SCENARIO_SHA256", $script:ScenarioHash, "Process")
        [Environment]::SetEnvironmentVariable("TUI_PERF_RUN_ID", $RunId, "Process")
        # Render evidence also stays on the normal product path. No Windows wrapper path may
        # silently opt into the optional hot-path instrumentation switch.
        [Environment]::SetEnvironmentVariable("YTM_PERF", $null, "Process")
        & $Binary --output (Join-Path $RunDir "render.json") `
            --warmup (Get-ScenarioField "warmup_draws") `
            --batches (Get-ScenarioField "batches") `
            --draws (Get-ScenarioField "draws_per_batch")
        if ($LASTEXITCODE -ne 0) { throw "$Role render harness failed" }
    } finally {
        [Environment]::SetEnvironmentVariable("TUI_PERF_SCENARIO_SHA256", $oldHash, "Process")
        [Environment]::SetEnvironmentVariable("YTM_PERF", $oldYtmPerf, "Process")
        [Environment]::SetEnvironmentVariable("TUI_PERF_RUN_ID", $oldRunId, "Process")
    }
}

& python $PythonTool validate --scenarios $Scenarios | Out-Null
if ($LASTEXITCODE -ne 0) { throw "invalid scenarios file" }
$script:ScenarioHash = Get-ScenarioField "sha256"
$pairs = [int](Get-ScenarioField "pairs")
$candidateRepeats = [int](Get-ScenarioField "candidate_repeats")
$isRender = $Scenario -eq "render_and_interaction"
if (-not $isRender) { Assert-WindowsProcessIsolation }

if (-not $CandidateSourceRoot) { $CandidateSourceRoot = $Repo }
foreach ($sourceRoot in @($BaselineSourceRoot, $CandidateSourceRoot)) {
    if (-not (Test-Path -LiteralPath $sourceRoot -PathType Container)) {
        throw "source root is not a directory: $sourceRoot"
    }
}

$resolvedOutput = & python $PythonTool path-preflight `
    --output-root $Output `
    --protected-root $BaselineSourceRoot `
    --protected-root $CandidateSourceRoot
if ($LASTEXITCODE -ne 0) { throw "output/source path preflight failed" }
$script:OutputRoot = ($resolvedOutput | Out-String).Trim()
if (-not $script:OutputRoot) { throw "path preflight returned an empty output root" }
New-Item -ItemType Directory -Path $script:OutputRoot | Out-Null

$controlledReceipt = Join-Path $script:OutputRoot "build-receipt.json"
$buildTarget = Join-Path ([IO.Path]::GetTempPath()) ("ytt-perf-build-" + [Guid]::NewGuid().ToString("N"))
$buildArgs = @(
    "build", "--scenarios", $Scenarios, "--scenario", $Scenario,
    "--baseline-root", $BaselineSourceRoot,
    "--candidate-root", $CandidateSourceRoot,
    "--output", $controlledReceipt,
    "--target-root", $buildTarget
)
try {
    & python $PythonTool @buildArgs | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "controlled source-bound build failed" }
} finally {
    if (Test-Path -LiteralPath $buildTarget) {
        Remove-Item -LiteralPath $buildTarget -Recurse -Force
    }
}

function Get-ReceiptArtifact([string] $Label) {
    $value = & python $PythonTool receipt `
        --receipt $controlledReceipt --artifact $Label --field path
    if ($LASTEXITCODE -ne 0) { throw "cannot read artifact $Label from build receipt" }
    return ($value | Out-String).Trim()
}
$BaselineRender = Get-ReceiptArtifact "baseline_render"
$CandidateRender = Get-ReceiptArtifact "candidate_render"

$manifestArgs = @(
    "manifest",
    "--scenarios", $Scenarios,
    "--scenario", $Scenario,
    "--output", (Join-Path $script:OutputRoot "host-manifest.json"),
    "--build-receipt", $controlledReceipt
)
& python $PythonTool @manifestArgs | Out-Null
if ($LASTEXITCODE -ne 0) { throw "failed to write host manifest" }

$baselineRuns = @()
$candidateRuns = @()
for ($pair = 1; $pair -le $pairs; $pair++) {
    $order = if ($pair % 2) { @("baseline", "candidate") } else { @("candidate", "baseline") }
    foreach ($role in $order) {
        $binary = if ($role -eq "baseline") { $BaselineRender } else { $CandidateRender }
        $runDir = Join-Path $script:OutputRoot ("pair-{0:D2}\{1}" -f $pair, $role)
        $runId = (& python $PythonTool run-start `
            --scenarios $Scenarios --scenario $Scenario `
            --output (Join-Path $runDir "run-contract.json") `
            --kind paired --role $role --pair-index $pair | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or -not $runId) { throw "failed to start paired run contract" }
        Run-Render $role $binary $runDir $runId
        & python $PythonTool run-finish --contract (Join-Path $runDir "run-contract.json") | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "failed to finish paired run contract" }
    }
    $baselineRuns += @(
        "--baseline-run", (Join-Path $script:OutputRoot ("pair-{0:D2}\baseline" -f $pair))
    )
    $candidateRuns += @(
        "--candidate-run", (Join-Path $script:OutputRoot ("pair-{0:D2}\candidate" -f $pair))
    )
}

$candidateRepeatRuns = @()
for ($repeat = 1; $repeat -le $candidateRepeats; $repeat++) {
    $repeatDir = Join-Path $script:OutputRoot ("candidate-repeat-{0:D2}" -f $repeat)
    $runId = (& python $PythonTool run-start `
        --scenarios $Scenarios --scenario $Scenario `
        --output (Join-Path $repeatDir "run-contract.json") `
        --kind candidate_repeat --role candidate --repeat-index $repeat | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $runId) { throw "failed to start repeat run contract" }
    Run-Render "candidate" $CandidateRender $repeatDir $runId
    & python $PythonTool run-finish --contract (Join-Path $repeatDir "run-contract.json") | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "failed to finish repeat run contract" }
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
& python $PythonTool create-checksums `
    --root $script:OutputRoot `
    --output (Join-Path $script:OutputRoot "SHA256SUMS") | Out-Null
if ($LASTEXITCODE -ne 0) { throw "failed to write or verify SHA256SUMS" }
if ($comparisonExitCode -ne 0) {
    throw "paired performance gate failed with exit code $comparisonExitCode"
}
Write-Host ("Transport verification: python `"{0}`" verify-checksums --root `"{1}`" --output `"{2}`"" -f `
    $PythonTool, $script:OutputRoot, (Join-Path $script:OutputRoot "SHA256SUMS"))
