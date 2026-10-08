param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$Root,
    [ValidateRange(1, 256)][int]$Threads = 4,
    [ValidateRange(1, 1000)][int]$Runs = 3,
    [switch]$Walk,
    [Parameter(Mandatory = $true)][string]$OutDir
)
$ErrorActionPreference = 'Stop'

# ProcessStartInfo.ArgumentList is unavailable on Windows PowerShell 5.1/.NET
# Framework. Escape each Windows CRT argument, including trailing backslashes.
function ConvertTo-NativeArgument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

# Load before launching any child; a compiler/policy failure cannot interrupt capture cleanup.
if (-not ('DarakPerfDeferredCleanup' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Threading;
using System.Threading.Tasks;
public static class DarakPerfDeferredCleanup {
    public static void DisposeAfterCopies(Task[] copies, IDisposable[] resources) {
        Task.WhenAll(copies).ContinueWith(done => {
            GC.KeepAlive(done.Exception); // Observe aggregate faults from both copies.
            foreach (var resource in resources) {
                if (resource != null) { try { resource.Dispose(); } catch (Exception error) { Console.Error.WriteLine("Deferred capture cleanup: " + error.Message); } }
            }
        }, CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
    }
}
'@
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$binaryPath = (Resolve-Path $Binary).Path
$priorThreads = $env:RAYON_NUM_THREADS
try {
    $env:RAYON_NUM_THREADS = [string]$Threads
    for ($run = 1; $run -le $Runs; $run++) {
        $prefix = Join-Path $OutDir ('run-{0:D3}' -f $run)
        $arguments = @($Root, '--threads', [string]$Threads, '--perf-csv')
        if ($Walk) { $arguments += '--walk' }
        $metadata = [ordered]@{
            schema = 1; run = $run; binary = $binaryPath; root = $Root
            rayon_num_threads = $Threads; requested_method = $(if ($Walk) { 'Walk' } else { 'Mft' })
            arguments = $arguments; exit_code = $null; actual_method = $null; fallback = @(); error = $null
            cleanup_errors = @()
            os = [Environment]::OSVersion.ToString(); process_arch = $env:PROCESSOR_ARCHITECTURE
            machine = $env:COMPUTERNAME; cpu = $env:PROCESSOR_IDENTIFIER
            utc = [DateTime]::UtcNow.ToString('o')
        }
        $process = New-Object System.Diagnostics.Process
        $stdoutFile = $null
        $stderrFile = $null
        $runError = $null
        $started = $false
        $copyTasks = @()
        $copiesObserved = $false
        $deferredCleanup = $false
        $copyCancellation = New-Object System.Threading.CancellationTokenSource
        try {
            $process.StartInfo.FileName = $binaryPath
            $process.StartInfo.Arguments = ($arguments | ForEach-Object { ConvertTo-NativeArgument $_ }) -join ' '
            $process.StartInfo.UseShellExecute = $false
            $process.StartInfo.CreateNoWindow = $true
            $process.StartInfo.RedirectStandardOutput = $true
            $process.StartInfo.RedirectStandardError = $true
            $stdoutFile = [System.IO.File]::Create("$prefix.csv")
            $stderrFile = [System.IO.File]::Create("$prefix.stderr.txt")
            # Each invocation creates a new process inheriting RAYON_NUM_THREADS.
            # Copy raw bytes concurrently, avoiding pipe deadlocks and PS5.1's
            # native-stderr ErrorRecord conversion under ErrorActionPreference Stop.
            if (-not $process.Start()) { throw 'Could not start scan process' }
            $started = $true
            $copyTasks += $process.StandardOutput.BaseStream.CopyToAsync($stdoutFile, 81920, $copyCancellation.Token)
            $copyTasks += $process.StandardError.BaseStream.CopyToAsync($stderrFile, 81920, $copyCancellation.Token)
            # A failed destination stops draining its pipe. Check faults during
            # the child wait so the child cannot deadlock writing into that pipe.
            while (-not $process.WaitForExit(100)) {
                foreach ($copy in $copyTasks) {
                    if ($copy.IsFaulted -or $copy.IsCanceled) { $copy.GetAwaiter().GetResult() }
                }
            }
            $metadata.exit_code = $process.ExitCode
            $drainTimer = [System.Diagnostics.Stopwatch]::StartNew()
            while (@($copyTasks | Where-Object { -not $_.IsCompleted }).Count -ne 0) {
                foreach ($copy in $copyTasks) {
                    if ($copy.IsFaulted -or $copy.IsCanceled) { $copy.GetAwaiter().GetResult() }
                }
                if ($drainTimer.ElapsedMilliseconds -ge 10000) { throw 'Output copies did not finish after child exit' }
                [System.Threading.Thread]::Sleep(100)
            }
            # Observe BOTH copies even if the first one fails after child exit.
            $copyError = $null
            foreach ($copy in $copyTasks) {
                try { $copy.GetAwaiter().GetResult() } catch { if ($null -eq $copyError) { $copyError = $_ } }
            }
            $copiesObserved = $true
            if ($null -ne $copyError) { throw $copyError }
            $stdoutFile.Dispose()
            $stdoutFile = $null
            $stderrFile.Dispose()
            $stderrFile = $null
            # PS5.1 otherwise serializes Get-Content provider properties alongside each line.
            $metadata.fallback = @(Get-Content -Encoding utf8 "$prefix.stderr.txt" | Where-Object { $_ -like 'fallback:*' } | ForEach-Object { $_.ToString() })
            if ($metadata.exit_code -ne 0) {
                throw "Scan run $run failed with exit code $($metadata.exit_code); raw output retained"
            }
            $rows = @(Import-Csv -Encoding utf8 "$prefix.csv")
            if ($rows.Count -ne 1 -or $rows[0].schema -ne '1') { throw "Invalid perf CSV in $prefix.csv" }
            $metadata.actual_method = $rows[0].method
        } catch {
            $runError = $_
            $metadata.error = $_.Exception.Message
        } finally {
            if (-not $copiesObserved) {
                # Capture/launch failure: terminate only the child we created.
                if ($started) {
                    try {
                        if (-not $process.HasExited) { $process.Kill() }
                        if ($process.WaitForExit(5000)) { $metadata.exit_code = $process.ExitCode }
                        else { $metadata.cleanup_errors += 'Owned child did not exit within 5 seconds after termination' }
                    } catch { $metadata.cleanup_errors += $_.Exception.Message }
                }
                # Preserve the healthy pipe's available bytes before cancelling.
                $settleTimer = [System.Diagnostics.Stopwatch]::StartNew()
                while (@($copyTasks | Where-Object { -not $_.IsCompleted }).Count -ne 0 -and $settleTimer.ElapsedMilliseconds -lt 5000) {
                    [System.Threading.Thread]::Sleep(100)
                }
                if (@($copyTasks | Where-Object { -not $_.IsCompleted }).Count -ne 0) {
                    try { $copyCancellation.Cancel() } catch { $metadata.cleanup_errors += $_.Exception.Message }
                    $settleTimer = [System.Diagnostics.Stopwatch]::StartNew()
                    while (@($copyTasks | Where-Object { -not $_.IsCompleted }).Count -ne 0 -and $settleTimer.ElapsedMilliseconds -lt 5000) {
                        [System.Threading.Thread]::Sleep(100)
                    }
                }
                if (@($copyTasks | Where-Object { -not $_.IsCompleted }).Count -eq 0) {
                    foreach ($copy in $copyTasks) {
                        try { $copy.GetAwaiter().GetResult() } catch {
                            $metadata.cleanup_errors += $_.Exception.Message
                            if ($null -eq $runError) { $runError = $_; $metadata.error = $_.Exception.Message }
                        }
                    }
                } else {
                    $metadata.cleanup_errors += 'Output copy cleanup timed out; resources retained until both tasks settle'
                    # On an exceptional stalled-I/O timeout, never close a file
                    # still being written. A tiny .NET continuation owns final
                    # observation/disposal without needing a PowerShell runspace.
                    [DarakPerfDeferredCleanup]::DisposeAfterCopies(
                        [System.Threading.Tasks.Task[]]$copyTasks,
                        [System.IDisposable[]]@($stdoutFile, $stderrFile, $process, $copyCancellation))
                    $deferredCleanup = $true
                }
            }
            if (-not $deferredCleanup) {
                foreach ($resource in @($stdoutFile, $stderrFile, $process, $copyCancellation)) {
                    if ($null -ne $resource) {
                        try { $resource.Dispose() } catch {
                            $metadata.cleanup_errors += $_.Exception.Message
                            if ($null -eq $runError) { $runError = $_; $metadata.error = $_.Exception.Message }
                        }
                    }
                }
                # Even a failed stdout copy leaves independently retained stderr.
                if (Test-Path "$prefix.stderr.txt") {
                    try { $metadata.fallback = @(Get-Content -Encoding utf8 "$prefix.stderr.txt" | Where-Object { $_ -like 'fallback:*' } | ForEach-Object { $_.ToString() }) }
                    catch { $metadata.cleanup_errors += $_.Exception.Message }
                }
            }
            # Preserve the first error and native exit status before rethrowing.
            try { $metadata | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 "$prefix.metadata.json" }
            catch {
                [Console]::Error.WriteLine("Could not persist capture metadata: " + $_.Exception.Message)
                if ($null -eq $runError) { $runError = $_ }
            }
        }
        if ($null -ne $runError) { throw $runError }
    }
} finally {
    $env:RAYON_NUM_THREADS = $priorThreads
}
