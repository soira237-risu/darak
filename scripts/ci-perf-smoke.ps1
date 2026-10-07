param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$OutDir
)
$ErrorActionPreference = 'Stop'
$runner = Join-Path $PSScriptRoot 'perf-run.ps1'
$binaryPath = (Resolve-Path $Binary).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = (Resolve-Path $OutDir).Path
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('darak CI fixture ' + [Guid]::NewGuid().ToString('N'))
$priorThreads = $env:RAYON_NUM_THREADS
$summary = [ordered]@{ powershell = $PSVersionTable.PSVersion.ToString(); walk = $false; fallback = $false; missing_root = $false }

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Unlocked([string]$Path) {
    $file = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $file.Dispose()
}
function Read-Run([string]$Directory) {
    Assert-Unlocked (Join-Path $Directory 'run-001.csv')
    Assert-Unlocked (Join-Path $Directory 'run-001.stderr.txt')
    return Get-Content -Raw -Encoding utf8 (Join-Path $Directory 'run-001.metadata.json') | ConvertFrom-Json
}

try {
    New-Item -ItemType Directory -Path (Join-Path $fixture 'nested directory') -Force | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $fixture 'one file.bin'), (New-Object byte[] 17))
    [IO.File]::WriteAllBytes((Join-Path $fixture 'nested directory/two file.bin'), (New-Object byte[] 31))
    foreach ($method in @('walk', 'fallback')) {
        $directory = Join-Path $outPath $method
        $env:RAYON_NUM_THREADS = 'ci-parent-sentinel'
        $parameters = @{ Binary = $binaryPath; Root = $fixture; Threads = 2; Runs = 1; OutDir = $directory }
        if ($method -eq 'walk') { $parameters.Walk = $true }
        & $runner @parameters
        Assert-True ($env:RAYON_NUM_THREADS -eq 'ci-parent-sentinel') 'Runner did not restore Rayon environment after success'
        $metadata = Read-Run $directory
        $csvPath = Join-Path $directory 'run-001.csv'
        $rows = @(Import-Csv -Encoding utf8 $csvPath)
        Assert-True (@(Get-Content -Encoding utf8 $csvPath).Count -eq 2) 'Scan stdout must contain only header and one CSV row'
        Assert-True ($rows.Count -eq 1 -and $rows[0].schema -eq '1') 'Missing schema-1 scan row'
        Assert-True ($rows[0].method -eq 'Walk' -and $metadata.actual_method -eq 'Walk') 'Expected actual Walk method'
        Assert-True ($rows[0].files -eq '2' -and $rows[0].dirs -eq '2' -and $rows[0].nodes -eq '4') 'Known fixture node accounting differs'
        Assert-True ($rows[0].errors -eq '0' -and $rows[0].hardlink_dups -eq '0' -and $rows[0].cancelled -eq '0') 'Known fixture has errors, duplicate links, or cancellation'
        Assert-True ($metadata.exit_code -eq 0 -and $null -eq $metadata.error) 'Successful scan metadata differs'
        Assert-True ($metadata.rayon_num_threads -eq 2 -and @($metadata.cleanup_errors).Count -eq 0) 'Unexpected runner environment or cleanup errors'
        $stderr = Get-Content -Raw -Encoding utf8 (Join-Path $directory 'run-001.stderr.txt')
        Assert-True (-not [string]::IsNullOrWhiteSpace($stderr)) 'Native progress stderr was not retained'
        if ($method -eq 'fallback') {
            Assert-True ($metadata.requested_method -eq 'Mft' -and @($metadata.fallback).Count -gt 0) 'Subdirectory MFT fallback was not recorded'
            Assert-True ($stderr -match '(?m)^fallback:') 'Raw fallback message was not retained'
        } else {
            Assert-True ($metadata.requested_method -eq 'Walk') 'Walk request metadata differs'
        }
        $summary[$method] = $true
    }
    $directory = Join-Path $outPath 'missing-root'
    $env:RAYON_NUM_THREADS = 'ci-parent-sentinel'
    $failed = $false
    try {
        & $runner -Binary $binaryPath -Root (Join-Path $fixture 'does not exist') -Threads 2 -Runs 1 -Walk -OutDir $directory
    } catch { $failed = $true }
    Assert-True $failed 'Missing-root scan unexpectedly succeeded'
    Assert-True ($env:RAYON_NUM_THREADS -eq 'ci-parent-sentinel') 'Runner did not restore Rayon environment after failure'
    $metadata = Read-Run $directory
    Assert-True ($metadata.exit_code -eq 1 -and -not [string]::IsNullOrWhiteSpace($metadata.error)) 'Missing-root exit/error metadata was not retained'
    Assert-True (@($metadata.cleanup_errors).Count -eq 0) 'Missing-root cleanup did not settle normally'
    Assert-True ((Get-Item (Join-Path $directory 'run-001.stderr.txt')).Length -gt 0) 'Missing-root native error stderr was lost'
    $summary.missing_root = $true
} finally {
    $env:RAYON_NUM_THREADS = $priorThreads
    if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture }
    $summary | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $outPath 'summary.json')
}
