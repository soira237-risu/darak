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
$substExecutable = Join-Path $env:SystemRoot 'System32/subst.exe'
$substDrive = $null
$priorThreads = $env:RAYON_NUM_THREADS
$summary = [ordered]@{ powershell = $PSVersionTable.PSVersion.ToString(); walk = $false; default_subdirectory = $false; fallback = $false; missing_root = $false }

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
    # Subdirectories select Walk without attempting MFT. A SUBST root reaches
    # MFT selection, but its directory alias cannot be opened as a raw volume.
    $usedDrives = [IO.Directory]::GetLogicalDrives()
    $availableRoot = 90..68 | ForEach-Object { '{0}:\' -f [char]$_ } | Where-Object { $usedDrives -notcontains $_ } | Select-Object -First 1
    Assert-True (-not [string]::IsNullOrWhiteSpace($availableRoot)) 'No unused drive letter for MFT fallback fixture'
    & $substExecutable $availableRoot.Substring(0, 2) $fixture
    Assert-True ($LASTEXITCODE -eq 0) 'Could not create MFT fallback fixture drive'
    $substDrive = $availableRoot.Substring(0, 2)
    foreach ($method in @('walk', 'default_subdirectory', 'fallback')) {
        $directory = Join-Path $outPath $method
        $env:RAYON_NUM_THREADS = 'ci-parent-sentinel'
        $scanRoot = if ($method -eq 'fallback') { $availableRoot } else { $fixture }
        $parameters = @{ Binary = $binaryPath; Root = $scanRoot; Threads = 2; Runs = 1; OutDir = $directory }
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
            Assert-True ($metadata.requested_method -eq 'Mft' -and @($metadata.fallback).Count -eq 1) 'Drive-root MFT fallback was not recorded'
            $fallbackLines = @(Get-Content -Encoding utf8 (Join-Path $directory 'run-001.stderr.txt') | Where-Object { $_ -like 'fallback:*' })
            Assert-True ($fallbackLines.Count -eq 1 -and $fallbackLines[0] -match '^fallback:\s+\S' -and $metadata.fallback[0] -eq $fallbackLines[0]) 'Raw fallback reason and metadata differ'
        } else {
            $requestedMethod = if ($method -eq 'walk') { 'Walk' } else { 'Mft' }
            Assert-True ($metadata.requested_method -eq $requestedMethod) 'Scan request metadata differs'
            Assert-True (@($metadata.fallback).Count -eq 0 -and $stderr -notmatch '(?m)^fallback:') 'Subdirectory scan unexpectedly reported MFT fallback'
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
    try {
        if ($null -ne $substDrive) {
            & $substExecutable $substDrive /D
            Assert-True ($LASTEXITCODE -eq 0 -and [IO.Directory]::GetLogicalDrives() -notcontains ($substDrive + '\')) 'Could not remove owned MFT fallback fixture drive'
        }
    } finally {
        if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture }
        $summary | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $outPath 'summary.json')
    }
}
