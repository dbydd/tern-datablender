# Build the native NetCDF plugin for one target triple and stage a
# distributable that Tern can install as a plain plugin directory.
[CmdletBinding()]
param(
    [Parameter(Position = 0)][string] $Target,
    [int] $Jobs = 4
)

$ErrorActionPreference = 'Stop'
$PackageName = 'tern-datablender'
$BinName = 'tern-datablender'

if (-not $PSBoundParameters.ContainsKey('Jobs') -and $env:JOBS) {
    $Jobs = [int]$env:JOBS
}
if ($Jobs -lt 1) { throw "jobs must be a positive integer, got '$Jobs'" }

$Root = Split-Path -Parent $PSScriptRoot

function Copy-StageFile {
    param([string] $Source, [string] $Destination)

    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "missing $Source" }
    if (Test-Path -LiteralPath $Destination -PathType Container) {
        throw "$Destination is a directory"
    }
    $Parent = Split-Path -Parent $Destination
    if (-not (Test-Path -LiteralPath $Parent -PathType Container)) {
        New-Item -ItemType Directory -Force -Path $Parent | Out-Null
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

Push-Location -LiteralPath $Root
try {
    if (-not $Target) {
        $rustcInfo = & rustc -vV
        if ($LASTEXITCODE -ne 0) { throw 'rustc is not on PATH' }
        $hostLine = $rustcInfo | Where-Object { $_ -like 'host: *' } | Select-Object -First 1
        if (-not $hostLine) { throw 'could not read the host triple from rustc -vV' }
        $Target = $hostLine.Substring('host: '.Length).Trim()
    }

    $Exe = if ($Target -like '*windows*') { '.exe' } else { '' }

    $TargetRoot = if ($env:CARGO_TARGET_DIR) {
        if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
            $env:CARGO_TARGET_DIR
        } else {
            Join-Path $Root $env:CARGO_TARGET_DIR
        }
    } else {
        Join-Path $Root 'target'
    }

    $StageName = "$PackageName-$Target"
    $DistDir = Join-Path $Root 'dist'
    $StageDir = Join-Path $DistDir $StageName
    $BinDir = Join-Path $StageDir 'bin'
    $Binary = Join-Path $TargetRoot "$Target/release/$BinName$Exe"
    $StagedBinary = Join-Path $BinDir "$BinName$Exe"

    Write-Host "building $PackageName for $Target (jobs=$Jobs)"
    & cargo build --release --locked --target $Target --jobs $Jobs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

    if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
        throw "cargo did not produce $Binary"
    }

    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    if (Test-Path -LiteralPath $StagedBinary -PathType Container) {
        throw "$StagedBinary is a directory"
    }
    Copy-Item -LiteralPath $Binary -Destination $StagedBinary -Force

    Copy-StageFile -Source (Join-Path $Root 'plugin.toml') -Destination (Join-Path $StageDir 'plugin.toml')
    Copy-StageFile -Source (Join-Path $Root 'window.luau') -Destination (Join-Path $StageDir 'window.luau')
    Copy-StageFile -Source (Join-Path $Root 'README.md') -Destination (Join-Path $StageDir 'README.md')
    Copy-StageFile -Source (Join-Path $Root 'README.en.md') -Destination (Join-Path $StageDir 'README.en.md')
    Copy-StageFile -Source (Join-Path $Root 'DESIGN.md') -Destination (Join-Path $StageDir 'DESIGN.md')

    if ($Target -like '*windows*') {
        $Archive = Join-Path $DistDir "$StageName.zip"
    } else {
        $Archive = Join-Path $DistDir "$StageName.tar.gz"
    }
    if (Test-Path -LiteralPath $Archive -PathType Container) { throw "$Archive is a directory" }
    if (Test-Path -LiteralPath $Archive -PathType Leaf) { Remove-Item -LiteralPath $Archive -Force }

    if ($Target -like '*windows*') {
        Compress-Archive -LiteralPath $StageDir -DestinationPath $Archive
    } else {
        & tar -czf $Archive -C $DistDir $StageName
        if ($LASTEXITCODE -ne 0) { throw "tar failed with exit code $LASTEXITCODE" }
    }

    Write-Output "staged  $StageDir"
    Write-Output "archive $Archive"
} finally {
    Pop-Location
}
