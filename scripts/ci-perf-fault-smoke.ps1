# Fault injection is confined to temporary runner copies. The production runner
# has no test switches. Each case runs in an owned host with a 30-second bound.
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [ValidateSet('stdout-flood', 'stderr-flood', 'exit-race')][string]$FaultCase,
    [string]$FixtureBinary
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = (Resolve-Path $OutDir).Path

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Unlocked([string]$Path) {
    $file = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $file.Dispose()
}

if ($FaultCase) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

public static class DarakCiCaptureProbe {
    private static readonly HashSet<int> created = new HashSet<int>();
    private static readonly HashSet<int> observed = new HashSet<int>();
    public static Task Track(Task task) { lock (created) { created.Add(task.Id); } return task; }
    public static void Observe(Task task) {
        try { task.GetAwaiter().GetResult(); }
        finally { lock (observed) { observed.Add(task.Id); } }
    }
    public static int CreatedCount { get { lock (created) { return created.Count; } } }
    public static int ObservedCount { get { lock (observed) { return observed.Count; } } }
}

public sealed class DarakCiCaptureStream : Stream {
    private readonly Stream inner;
    private readonly bool fail;
    private readonly int delay;
    private readonly string name;
    private int accepted;
    public DarakCiCaptureStream(Stream inner, bool fail, int delay, string name) {
        this.inner = inner; this.fail = fail; this.delay = delay; this.name = name;
    }
    public override bool CanRead { get { return false; } }
    public override bool CanSeek { get { return false; } }
    public override bool CanWrite { get { return true; } }
    public override long Length { get { return inner.Length; } }
    public override long Position { get { return inner.Position; } set { throw new NotSupportedException(); } }
    public override void Flush() { inner.Flush(); }
    public override int Read(byte[] b, int o, int c) { throw new NotSupportedException(); }
    public override long Seek(long o, SeekOrigin s) { throw new NotSupportedException(); }
    public override void SetLength(long n) { inner.SetLength(n); }
    public override void Write(byte[] b, int o, int c) { WriteAsync(b, o, c, CancellationToken.None).GetAwaiter().GetResult(); }
    public override async Task WriteAsync(byte[] b, int o, int c, CancellationToken token) {
        if (delay != 0) { await Task.Delay(delay).ConfigureAwait(false); }
        int retained = fail ? Math.Min(c, Math.Max(0, 128 - accepted)) : c;
        if (retained != 0) {
            await inner.WriteAsync(b, o, retained, token).ConfigureAwait(false);
            accepted += retained;
        }
        if (retained != c) { throw new IOException("injected " + name + " capture failure"); }
    }
    protected override void Dispose(bool disposing) { if (disposing) { inner.Dispose(); } base.Dispose(disposing); }
}
'@
    $runnerPath = Join-Path $PSScriptRoot 'perf-run.ps1'
    $text = [IO.File]::ReadAllText($runnerPath)
    $injected = $text
    $faultSide = if ($FaultCase -eq 'stderr-flood') { 'stderr' } else { 'stdout' }
    foreach ($side in @('stdout', 'stderr')) {
        $needle = '$' + $side + 'File = [System.IO.File]::Create("$prefix.' + $(if ($side -eq 'stdout') { 'csv' } else { 'stderr.txt' }) + '")'
        Assert-True ($injected.Contains($needle)) "Runner injection point changed: $side destination"
        $fail = if ($side -eq $faultSide) { '$true' } else { '$false' }
        $delay = if ($FaultCase -eq 'exit-race') { if ($side -eq $faultSide) { 700 } else { 1200 } } else { 0 }
        $replacement = '$' + $side + 'File = [DarakCiCaptureStream]::new([System.IO.File]::Create("$prefix.' + $(if ($side -eq 'stdout') { 'csv' } else { 'stderr.txt' }) + '"), ' + $fail + ', ' + $delay + ', "' + $side + '")'
        $injected = $injected.Replace($needle, $replacement)
        $streamName = if ($side -eq 'stdout') { 'StandardOutput' } else { 'StandardError' }
        $needle = '$copyTasks += $process.' + $streamName + '.BaseStream.CopyToAsync($' + $side + 'File, 81920, $copyCancellation.Token)'
        Assert-True ($injected.Contains($needle)) "Runner injection point changed: $side task"
        $injected = $injected.Replace($needle, ('$copyTasks += [DarakCiCaptureProbe]::Track(' + $needle.Substring('$copyTasks += '.Length) + ')'))
    }
    # Observe delegates to the identical GetAwaiter().GetResult() and only counts
    # task identities. No wait/cleanup branches or timeout values are changed.
    $needle = '$copy.GetAwaiter().GetResult()'
    $observationSites = ([regex]::Matches($injected, [regex]::Escape($needle))).Count
    Assert-True ($observationSites -eq 4) 'Runner task-observation sites changed'
    $injected = $injected.Replace($needle, '[DarakCiCaptureProbe]::Observe($copy)')
    $temporaryRunner = Join-Path $outPath 'injected-runner.ps1'
    [IO.File]::WriteAllText($temporaryRunner, $injected, [Text.UTF8Encoding]::new($true))
    [ordered]@{
        original_sha256 = (Get-FileHash -Algorithm SHA256 $runnerPath).Hash
        injected_sha256 = (Get-FileHash -Algorithm SHA256 $temporaryRunner).Hash
        fault_case = $FaultCase; failed_destination = $faultSide; observation_sites = $observationSites
        substitutions = @('two destination stream constructors', 'two identity-only Track calls', 'four equivalent Observe(GetAwaiter.GetResult) calls')
    } | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $outPath 'injection.json')
    $priorThreads = $env:RAYON_NUM_THREADS
    $env:RAYON_NUM_THREADS = 'fault-parent-sentinel'
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $failed = $false
    try {
        $mode = if ($FaultCase -eq 'exit-race') { 'finite' } else { 'flood' }
        try { & $temporaryRunner -Binary $FixtureBinary -Root $mode -Runs 1 -Threads 2 -OutDir $outPath }
        catch { $failed = $true }
        Assert-True $failed 'Injected capture failure unexpectedly succeeded'
        Assert-True ($timer.Elapsed.TotalSeconds -lt 25) 'Capture failure did not return within its bounded cleanup window'
        Assert-True ($env:RAYON_NUM_THREADS -eq 'fault-parent-sentinel') 'Rayon environment was not restored after capture failure'
        $metadata = Get-Content -Raw -Encoding utf8 (Join-Path $outPath 'run-001.metadata.json') | ConvertFrom-Json
        Assert-True ($metadata.error -like "*injected $faultSide capture failure*") 'First capture failure was replaced by a secondary error'
        Assert-True ($null -ne $metadata.exit_code) 'Owned child exit code was not retained'
        Assert-True ([DarakCiCaptureProbe]::CreatedCount -eq 2 -and [DarakCiCaptureProbe]::ObservedCount -eq 2) 'Both capture tasks must be independently observed'
        Assert-Unlocked (Join-Path $outPath 'run-001.csv')
        Assert-Unlocked (Join-Path $outPath 'run-001.stderr.txt')
        $stderr = [IO.File]::ReadAllText((Join-Path $outPath 'run-001.stderr.txt'))
        Assert-True ($stderr -match '(?m)^owned-child-pid:(\d+)') 'Owned child PID was not retained'
        $childId = [int]$Matches[1]
        Assert-True ($null -eq (Get-Process -Id $childId -ErrorAction SilentlyContinue)) 'Owned native child survived capture failure'
        Assert-True ($stderr -match '(?m)^fallback: injected fixture' -and @($metadata.fallback).Count -eq 1 -and $metadata.fallback[0] -eq 'fallback: injected fixture') 'Fallback bytes/metadata were lost during capture cleanup'
        if ($FaultCase -eq 'exit-race') {
            Assert-True ($metadata.exit_code -eq 0) 'Finite child did not exit before the delayed capture fault'
            Assert-True ($stderr.EndsWith("finite-stderr-complete`n")) 'Healthy stream was not completely drained after child exit'
        } else {
            Assert-True ($metadata.exit_code -ne 0) 'Flood child was not terminated by the runner'
        }
        [ordered]@{ case = $FaultCase; passed = $true; elapsed_ms = $timer.ElapsedMilliseconds; copies_observed = [DarakCiCaptureProbe]::ObservedCount; child_pid = $childId; environment_restored = $true } |
            ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $outPath 'summary.json')
    } finally { $env:RAYON_NUM_THREADS = $priorThreads }
    exit 0
}

$scratch = Join-Path ([IO.Path]::GetTempPath()) ('darak-capture-fixture-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$source = Join-Path $scratch 'capture-child.cs'
$fixtureExe = Join-Path $scratch 'capture-child.exe'
[IO.File]::WriteAllText($source, @'
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
public static class CaptureChild {
    public static void Main(string[] args) {
        Stream output = Console.OpenStandardOutput();
        Stream error = Console.OpenStandardError();
        byte[] header = Encoding.UTF8.GetBytes("owned-child-pid:" + Process.GetCurrentProcess().Id + "\nfallback: injected fixture\n");
        error.Write(header, 0, header.Length); error.Flush();
        if (args.Length != 0 && args[0] == "finite") {
            // Both payloads fit the pipes; the child can exit before either
            // destination's intentionally delayed write completes.
            byte[] first = new byte[512]; output.Write(first, 0, first.Length); output.Flush();
            byte[] last = Encoding.UTF8.GetBytes(new string('e', 1024) + "\nfinite-stderr-complete\n");
            error.Write(last, 0, last.Length); error.Flush(); return;
        }
        byte[] block = Encoding.UTF8.GetBytes(new string('x', 4096));
        for (;;) {
            output.Write(block, 0, block.Length); output.Flush();
            error.Write(block, 0, block.Length); error.Flush();
        }
    }
}
'@
)
$hosts = @()
try {
    $compiler = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
    & $compiler /nologo /target:exe ("/out:$fixtureExe") $source > (Join-Path $outPath 'fixture-build.txt')
    Assert-True ($LASTEXITCODE -eq 0 -and (Test-Path $fixtureExe)) 'Could not compile native capture fixture'
    Copy-Item $source (Join-Path $outPath 'capture-child.cs')
    $hostBinary = [Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
    foreach ($case in @('stdout-flood', 'stderr-flood', 'exit-race')) {
        $directory = Join-Path $outPath $case
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
        # All paths are generated/checkout paths without embedded quote characters.
        $arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $PSCommandPath + '" -OutDir "' + $directory + '" -FaultCase ' + $case + ' -FixtureBinary "' + $fixtureExe + '"'
        $hostProcess = Start-Process -FilePath $hostBinary -ArgumentList $arguments -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $directory 'host.stdout.txt') -RedirectStandardError (Join-Path $directory 'host.stderr.txt')
        $hosts += $hostProcess
        # Windows PowerShell 5.1 returns an uncached Process handle. Retain it
        # before the host exits so ExitCode remains available after the wait.
        $null = $hostProcess.Handle
        Assert-True ($hostProcess.WaitForExit(30000)) "Fault case exceeded its 30-second outer bound: $case"
        $hostProcess.Refresh()
        Assert-True ($hostProcess.ExitCode -eq 0) "Fault case failed: $case; see host.stderr.txt"
    }
} finally {
    foreach ($hostProcess in $hosts) {
        if (-not $hostProcess.HasExited) { $hostProcess.Kill(); $hostProcess.WaitForExit(5000) | Out-Null }
        $hostProcess.Dispose()
    }
    # If an assertion/outer timeout interrupts supervision, clean up only fixture
    # children whose PID is in this run's captured output and whose image matches.
    foreach ($stderrFile in @(Get-ChildItem -Path $outPath -Filter 'run-001.stderr.txt' -Recurse)) {
        $content = [IO.File]::ReadAllText($stderrFile.FullName)
        if ($content -match '(?m)^owned-child-pid:(\d+)') {
            $childProcess = Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue
            if ($null -ne $childProcess -and $childProcess.Path -eq $fixtureExe) { $childProcess.Kill(); $childProcess.WaitForExit(5000) | Out-Null }
        }
    }
    if (Test-Path $scratch) { Remove-Item -Force -Recurse $scratch }
}
