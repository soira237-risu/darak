# Native CI correctness comparison; timings are diagnostics, with only 3 samples.
# This script intentionally requires PowerShell 7 (ProcessStartInfo.ArgumentList).
param([Parameter(Mandatory = $true)][string]$OutDir)
$ErrorActionPreference = 'Stop'
$baselineSha = '6d5fd1ce0384a228401f74567d156e12f757fd85'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = (Resolve-Path $OutDir).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) ('darak-search-ci-' + [Guid]::NewGuid().ToString('N'))
$baseline = Join-Path $scratch 'baseline'
$priorThreads = $env:RAYON_NUM_THREADS
$summary = @()
$captureCleanupErrors = @()
$scriptFailure = $null
$metadata = [ordered]@{
    baseline_sha = $baselineSha; candidate_sha = (& git -C $repo rev-parse HEAD)
    rust = (& rustc --version); os = [Environment]::OSVersion.ToString()
    cpu = $env:PROCESSOR_IDENTIFIER; machine = $env:COMPUTERNAME
    logical_processors = [Environment]::ProcessorCount
    powershell = $PSVersionTable.PSVersion.ToString(); utc = [DateTime]::UtcNow.ToString('o')
    nodes = @(100000, 4000000); shape = 'balanced'; unicode_percent = 25
    threads = 4; iterations = 3; warmups = 3; harness = @()
    timing_use = 'CI diagnostic only: three samples, no performance threshold or product-target claim'
    status = 'running'; error = $null
}
# Cleanup only: keep output destinations alive until both asynchronous copies
# settle if an exceptional I/O stall outlives the bounded cleanup window.
Add-Type -TypeDefinition @'
using System;
using System.Threading;
using System.Threading.Tasks;
public static class DarakCiSearchCleanup {
    public static void DisposeAfter(Task[] copies, IDisposable[] resources) {
        Task.WhenAll(copies).ContinueWith(done => {
            GC.KeepAlive(done.Exception);
            foreach (var resource in resources) {
                if (resource != null) { try { resource.Dispose(); } catch (Exception e) { Console.Error.WriteLine(e.Message); } }
            }
        }, CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
    }
}
'@

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Invoke-Captured([string]$Binary, [string[]]$Arguments, [string]$Prefix, [int]$TimeoutSeconds = 600) {
    $process = [Diagnostics.Process]::new()
    $process.StartInfo.FileName = $Binary
    $process.StartInfo.WorkingDirectory = $repo
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $process.StartInfo.ArgumentList.Add($argument) }
    $stdout = $null
    $stderr = $null
    $cancellation = [Threading.CancellationTokenSource]::new()
    $copies = @()
    $started = $false
    $failure = $null
    try {
        $stdout = [IO.File]::Create("$Prefix.csv")
        $stderr = [IO.File]::Create("$Prefix.stderr.txt")
        Assert-True ($process.Start()) 'Could not launch native command'
        $started = $true
        $copies += $process.StandardOutput.BaseStream.CopyToAsync($stdout, 81920, $cancellation.Token)
        $copies += $process.StandardError.BaseStream.CopyToAsync($stderr, 81920, $cancellation.Token)
        $timer = [Diagnostics.Stopwatch]::StartNew()
        while (-not $process.WaitForExit(100)) {
            foreach ($copy in $copies) { if ($copy.IsFaulted -or $copy.IsCanceled) { $copy.GetAwaiter().GetResult() } }
            if ($timer.Elapsed.TotalSeconds -gt $TimeoutSeconds) { throw "Native command exceeded $TimeoutSeconds seconds: $Binary" }
        }
        foreach ($copy in $copies) {
            Assert-True ($copy.Wait(10000)) 'Native output did not drain'
            $copy.GetAwaiter().GetResult()
        }
        Assert-True ($process.ExitCode -eq 0) "Native command exited $($process.ExitCode): $Binary; see $Prefix.stderr.txt"
    } catch {
        $failure = $_
    } finally {
        try {
            if ($started -and -not $process.HasExited) {
                $process.Kill($true)
                if (-not $process.WaitForExit(5000)) { throw 'Owned command did not exit after termination' }
            }
        } catch {
            $script:captureCleanupErrors += $_.Exception.Message
            if ($null -eq $failure) { $failure = $_ }
        }
        # Drain retained bytes, then cancel exceptional stalled I/O. Wait faults
        # never prevent independent observation of the other stream's task.
        foreach ($copy in $copies) { try { $copy.Wait(5000) | Out-Null } catch { } }
        if (@($copies | Where-Object { -not $_.IsCompleted }).Count -ne 0) {
            try { $cancellation.Cancel() } catch { $script:captureCleanupErrors += $_.Exception.Message; if ($null -eq $failure) { $failure = $_ } }
            foreach ($copy in $copies) { try { $copy.Wait(5000) | Out-Null } catch { } }
        }
        foreach ($copy in $copies) {
            if ($copy.IsCompleted) {
                try { $copy.GetAwaiter().GetResult() } catch {
                    $script:captureCleanupErrors += $_.Exception.Message
                    if ($null -eq $failure) { $failure = $_ }
                }
            }
        }
        $resources = [IDisposable[]]@($stdout, $stderr, $process, $cancellation)
        if (@($copies | Where-Object { -not $_.IsCompleted }).Count -ne 0) {
            $script:captureCleanupErrors += 'Output cleanup timed out; resource disposal deferred until both copies settle'
            [DarakCiSearchCleanup]::DisposeAfter([Threading.Tasks.Task[]]$copies, $resources)
        } else {
            foreach ($resource in $resources) {
                if ($null -ne $resource) {
                    try { $resource.Dispose() } catch {
                        $script:captureCleanupErrors += $_.Exception.Message
                        if ($null -eq $failure) { $failure = $_ }
                    }
                }
            }
        }
    }
    if ($null -ne $failure) { throw $failure }
}
function Read-Search([string]$Prefix, [int]$Nodes) {
    $rows = @(Import-Csv -Encoding utf8 "$Prefix.csv")
    $map = @(Import-Csv -Encoding utf8 "$Prefix.stderr.txt")
    Assert-True ($rows.Count -eq 90 -and $map.Count -eq 30) "Incomplete 30-query/scope matrix: $Prefix"
    $names = @('all', 'contains-broad', 'contains-few', 'no-hit', 'exclude', 'unicode', 'unicode-case', 'wildcard', 'ext', 'size', 'date', 'files', 'dirs', 'path', 'multi')
    $expected = @($names | ForEach-Object { "$($_)-root"; "$($_)-focus" } | Sort-Object)
    $actual = @($rows.query | Sort-Object -Unique)
    Assert-True (($expected -join ',') -ceq ($actual -join ',')) "Wrong query matrix: $Prefix"
    Assert-True (@($rows.fixture | Sort-Object -Unique).Count -eq 1) "Multiple fixtures in $Prefix"
    Assert-True ($rows[0].fixture -match "^seed20261007-balanced-n$Nodes-u25-[0-9a-f]{16}$") "Wrong fixture identity in $Prefix"
    foreach ($row in $rows) {
        Assert-True ($row.schema -eq '1' -and $row.nodes -eq [string]$Nodes) "Wrong schema/node count in $Prefix"
        foreach ($column in @('hits', 'total_size', 'elapsed_us', 'result_fingerprint')) {
            Assert-True ($row.$column -match '^\d+$') "Invalid $column in $Prefix"
        }
    }
    foreach ($group in @($rows | Group-Object query)) {
        Assert-True ($group.Count -eq 3 -and (@($group.Group.iteration | Sort-Object) -join ',') -eq '1,2,3') "Wrong measured iterations in $Prefix"
        foreach ($column in @('hits', 'total_size', 'result_fingerprint')) {
            Assert-True (@($group.Group.$column | Sort-Object -Unique).Count -eq 1) "Unstable $column for $($group.Name)"
        }
    }
    Assert-True ((@($map.query | Sort-Object) -join ',') -ceq ($expected -join ',')) "Incomplete query mapping in $Prefix"
    foreach ($row in $map) {
        Assert-True ($row.schema -eq '1' -and $row.fixture -eq $rows[0].fixture -and $row.mode -eq 'allocated') "Invalid query mapping in $Prefix"
        Assert-True ($row.scope -match '^\d+$' -and [uint64]$row.scope -lt $Nodes) "Invalid scope in $Prefix"
    }
    # A balanced fixture must exercise a real subtree as well as ROOT.
    Assert-True (@($map | Where-Object { $_.query -like '*-root' -and $_.scope -ne '0' }).Count -eq 0) 'ROOT mapping differs'
    Assert-True (@($map | Where-Object { $_.query -like '*-focus' -and $_.scope -eq '0' }).Count -eq 0) 'Focus aliases ROOT'
    return @{ Rows = $rows; Map = $map }
}

try {
    $env:RAYON_NUM_THREADS = '4'
    New-Item -ItemType Directory -Path $baseline -Force | Out-Null
    & git -C $repo archive --format=tar -o (Join-Path $scratch 'baseline.tar') $baselineSha
    Assert-True ($LASTEXITCODE -eq 0) 'Baseline archive failed'
    & tar -xf (Join-Path $scratch 'baseline.tar') -C $baseline
    Assert-True ($LASTEXITCODE -eq 0) 'Baseline extraction failed'

    # Guard the overlay: the baseline lib gains only the optional perf module
    # (inserted into the baseline's own lib, so later candidate modules never
    # leak into it), and the manifest change must be only the perf
    # feature/example registration plus package metadata, which does not
    # affect the build. The baseline manifest is its own plus the
    # registration, because its workspace root has no `repository` to inherit.
    $lib = [IO.File]::ReadAllText((Join-Path $repo 'crates/darak-core/src/lib.rs')).Replace("`r`n", "`n")
    $oldLib = [IO.File]::ReadAllText((Join-Path $baseline 'crates/darak-core/src/lib.rs')).Replace("`r`n", "`n")
    $perfModule = "#[cfg(feature = `"perf`")]`npub mod perf;`n`n"
    Assert-True ($lib.Contains($perfModule)) 'Candidate lib does not declare the perf harness module'
    $firstModule = $oldLib.IndexOf("pub mod ")
    Assert-True ($firstModule -ge 0 -and -not $oldLib.Contains('mod perf;')) 'Baseline lib has an unexpected layout'
    $baselineLib = $oldLib.Insert($firstModule, $perfModule)
    $manifest = [IO.File]::ReadAllText((Join-Path $repo 'crates/darak-core/Cargo.toml')).Replace("`r`n", "`n")
    $oldManifest = [IO.File]::ReadAllText((Join-Path $baseline 'crates/darak-core/Cargo.toml')).Replace("`r`n", "`n")
    $registration = "`n[features]`nperf = []`n`n[[example]]`nname = `"search_bench`"`nrequired-features = [`"perf`"]`n"
    $packageMetadata = '(?m)^(repository\.workspace = true|description = "[^"\n]*")\n'
    Assert-True (($manifest.Replace($registration, '') -replace $packageMetadata, '') -ceq $oldManifest) 'Core manifest overlay includes a non-harness change'
    $baselineManifestPath = Join-Path $baseline 'crates/darak-core/Cargo.toml'
    [IO.File]::WriteAllText($baselineManifestPath, $oldManifest + $registration, [Text.UTF8Encoding]::new($false))
    $metadata.harness += [ordered]@{ path = 'crates/darak-core/Cargo.toml'; sha256 = (Get-FileHash -Algorithm SHA256 $baselineManifestPath).Hash }
    $overlay = @('crates/darak-core/src/perf.rs', 'crates/darak-core/examples/search_bench.rs')
    foreach ($relative in $overlay) {
        $source = Join-Path $repo $relative
        $destination = Join-Path $baseline $relative
        New-Item -ItemType Directory -Force -Path (Split-Path $destination) | Out-Null
        Copy-Item -LiteralPath $source -Destination $destination
        $metadata.harness += [ordered]@{ path = $relative; sha256 = (Get-FileHash -Algorithm SHA256 $source).Hash }
    }
    $baselineLibPath = Join-Path $baseline 'crates/darak-core/src/lib.rs'
    [IO.File]::WriteAllText($baselineLibPath, $baselineLib, [Text.UTF8Encoding]::new($false))
    $metadata.harness += [ordered]@{ path = 'crates/darak-core/src/lib.rs'; sha256 = (Get-FileHash -Algorithm SHA256 $baselineLibPath).Hash }
    Assert-True ((Get-FileHash (Join-Path $repo 'Cargo.lock')).Hash -eq (Get-FileHash (Join-Path $baseline 'Cargo.lock')).Hash) 'Baseline lockfile differs'
    $metadata.baseline_search_sha256 = (Get-FileHash (Join-Path $baseline 'crates/darak-core/src/search.rs')).Hash
    $metadata.candidate_search_sha256 = (Get-FileHash (Join-Path $repo 'crates/darak-core/src/search.rs')).Hash
    $cargo = (Get-Command cargo).Source
    $binaries = @{}
    foreach ($version in @('baseline', 'candidate')) {
        $sourceRoot = if ($version -eq 'baseline') { $baseline } else { $repo }
        $target = Join-Path $scratch "target-$version"
        $arguments = @('build', '--locked', '--release', '-p', 'darak-core', '--features', 'perf', '--example', 'search_bench', '--manifest-path', (Join-Path $sourceRoot 'Cargo.toml'), '--target-dir', $target)
        $metadata["${version}_build_arguments"] = $arguments
        Invoke-Captured $cargo $arguments (Join-Path $outPath "$version-build") 600
        $binaries[$version] = Join-Path $target 'release/examples/search_bench.exe'
    }
    foreach ($nodes in @(100000, 4000000)) {
        $data = @{}
        $arguments = @('--nodes', [string]$nodes, '--shape', 'balanced', '--unicode-percent', '25', '--threads', '4', '--iterations', '3')
        $metadata["n${nodes}_arguments"] = $arguments
        $order = if ($nodes -eq 100000) { @('baseline', 'candidate') } else { @('candidate', 'baseline') }
        $metadata["n${nodes}_execution_order"] = @()
        # Each pair executes on this job's machine with identical release options.
        foreach ($version in $order) {
            $prefix = Join-Path $outPath "$version-n$nodes"
            $metadata["n${nodes}_execution_order"] += $version
            Invoke-Captured $binaries[$version] $arguments $prefix 600
            $data[$version] = Read-Search $prefix $nodes
        }
        Assert-True ($data.baseline.Rows[0].fixture -ceq $data.candidate.Rows[0].fixture) 'Baseline/candidate fixture fingerprint differs'
        $baseMap = @($data.baseline.Map | Sort-Object query | ConvertTo-Csv -NoTypeInformation)
        $nextMap = @($data.candidate.Map | Sort-Object query | ConvertTo-Csv -NoTypeInformation)
        Assert-True (($baseMap -join "`n") -ceq ($nextMap -join "`n")) 'Baseline/candidate query/scope mapping differs'
        foreach ($query in @($data.baseline.Rows.query | Sort-Object -Unique)) {
            $before = @($data.baseline.Rows | Where-Object query -EQ $query)
            $after = @($data.candidate.Rows | Where-Object query -EQ $query)
            foreach ($column in @('nodes', 'hits', 'total_size', 'result_fingerprint')) {
                Assert-True ($before[0].$column -ceq $after[0].$column) "Baseline/candidate $column differs for n$nodes/$query"
            }
            foreach ($version in @('baseline', 'candidate')) {
                $rows = if ($version -eq 'baseline') { $before } else { $after }
                $times = @($rows | ForEach-Object { [uint64]$_.elapsed_us } | Sort-Object)
                $summary += [pscustomobject][ordered]@{
                    version = $version; fixture = $rows[0].fixture; query = $query; samples = $times.Count
                    p50_us = $times[[int][Math]::Ceiling(0.50 * $times.Count) - 1]
                    p95_us = $times[[int][Math]::Ceiling(0.95 * $times.Count) - 1]
                    hits = $rows[0].hits; total_size = $rows[0].total_size; result_fingerprint = $rows[0].result_fingerprint
                    interpretation = 'diagnostic-only; n=3; no CI timing threshold'
                }
            }
        }
        $summary | Export-Csv -NoTypeInformation -Encoding utf8 (Join-Path $outPath 'quantiles.csv')
    }
    $metadata.status = 'passed'
} catch {
    $metadata.status = 'failed'; $metadata.error = $_.Exception.Message
    $scriptFailure = $_
} finally {
    $env:RAYON_NUM_THREADS = $priorThreads
    try { if (Test-Path $scratch) { Remove-Item -Force -Recurse $scratch } }
    catch {
        $captureCleanupErrors += $_.Exception.Message
        if ($null -eq $scriptFailure) { $scriptFailure = $_; $metadata.status = 'failed'; $metadata.error = $_.Exception.Message }
    }
    $metadata.cleanup_errors = $captureCleanupErrors
    try { $metadata | ConvertTo-Json -Depth 6 | Set-Content -Encoding utf8 (Join-Path $outPath 'metadata.json') }
    catch { [Console]::Error.WriteLine($_.Exception.Message); if ($null -eq $scriptFailure) { $scriptFailure = $_ } }
}
if ($null -ne $scriptFailure) { throw $scriptFailure }
