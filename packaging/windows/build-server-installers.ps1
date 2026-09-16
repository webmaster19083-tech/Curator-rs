[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ServerBinary,

    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory,

    [string]$Version
)

$ErrorActionPreference = 'Stop'
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent (Split-Path -Parent $scriptRoot)
$sourceBinary = (Resolve-Path -LiteralPath $ServerBinary).Path
if (-not (Test-Path -LiteralPath $sourceBinary -PathType Leaf)) {
    throw "Curator Server executable was not found: $ServerBinary"
}

function Find-MakeNSIS {
    # Prefer PATH for local developer installs. Chocolatey's NSIS package
    # writes Program Files but does not refresh the already-running Actions
    # shell's PATH, so also check the standard installation directories.
    foreach ($name in @('makensis.exe', 'makensis')) {
        $command = Get-Command -Name $name -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($command) { return $command.Source }
    }

    $programFilesRoots = @(
        [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFilesX86),
        [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
    ) | Where-Object { $_ }
    foreach ($root in $programFilesRoots) {
        $candidate = Join-Path $root 'NSIS\makensis.exe'
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }

    return $null
}

$nsis = Find-MakeNSIS
if (-not $nsis) {
    throw 'makensis is required to build Curator Server installers. Install NSIS or add makensis.exe to PATH.'
}

if (-not $Version) {
    $cargo = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
    $match = [regex]::Match($cargo, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { throw 'Could not read the workspace version from Cargo.toml.' }
    $Version = $match.Groups[1].Value
}

$resolvedOutput = [System.IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $resolvedOutput | Out-Null
$stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("curator-server-nsis-" + [guid]::NewGuid().ToString('N'))

try {
    foreach ($scope in @('current-user', 'all-users')) {
        $stage = Join-Path $stageRoot $scope
        New-Item -ItemType Directory -Force -Path (Join-Path $stage 'static') | Out-Null
        Copy-Item -LiteralPath $sourceBinary -Destination (Join-Path $stage 'curator.exe')
        Copy-Item -LiteralPath (Join-Path $scriptRoot 'Register-CuratorServer.ps1') -Destination $stage
        Copy-Item -LiteralPath (Join-Path $scriptRoot 'Unregister-CuratorServer.ps1') -Destination $stage
        Copy-Item -Path (Join-Path $repositoryRoot 'static\*') -Destination (Join-Path $stage 'static') -Recurse -Force

        $installer = Join-Path $resolvedOutput ("curator-server-$Version-windows-$scope-setup.exe")
        $arguments = @(
            "/DCURATOR_STAGE=$stage",
            "/DPRODUCT_VERSION=$Version",
            "/DOUTPUT_FILE=$installer"
        )
        if ($scope -eq 'all-users') { $arguments += '/DALL_USERS' }
        & $nsis @arguments (Join-Path $scriptRoot 'curator-server.nsi')
        if ($LASTEXITCODE -ne 0) { throw "makensis failed for the $scope installer." }
    }
} finally {
    if (Test-Path -LiteralPath $stageRoot) {
        Remove-Item -LiteralPath $stageRoot -Recurse -Force
    }
}
