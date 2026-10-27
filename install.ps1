# Build both binaries and install them for the current user.
#
#   .\install.ps1              # build (debug) + install
#   .\install.ps1 -SkipBuild   # install whatever is already in dist\
#
# Installs to %LOCALAPPDATA%\OpenBuild and adds that directory to the user
# PATH, so `openbuild` works from any terminal afterwards.

param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$dist = Join-Path $root "dist"
$installDir = Join-Path $env:LOCALAPPDATA "OpenBuild"

if (-not $SkipBuild) {
    & (Join-Path $root "build.ps1")
    if ($LASTEXITCODE -ne 0) { throw "build.ps1 failed with exit code $LASTEXITCODE" }
}

$tui = Join-Path $dist "openbuild.exe"
$kernel = Join-Path $dist "openbuild-kernel.exe"
foreach ($path in @($tui, $kernel)) {
    if (-not (Test-Path $path)) {
        throw "missing $path; run .\build.ps1 (or drop -SkipBuild) first"
    }
}

New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item $tui $installDir -Force
Copy-Item $kernel $installDir -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$segments = @($userPath -split ';' | Where-Object { $_ -ne '' })
if ($segments -notcontains $installDir) {
    [Environment]::SetEnvironmentVariable(
        "Path",
        (($segments + $installDir) -join ';'),
        "User"
    )
    Write-Host "added $installDir to the user PATH (restart your terminal to pick it up)"
} else {
    Write-Host "$installDir is already on the user PATH"
}

Write-Host ""
Write-Host "installed to $installDir"
Get-ChildItem $installDir | Select-Object Name, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } }

# Smoke-test the installed copy. The kernel sits next to it, so the TUI finds it
# without OPENBUILD_KERNEL.
$version = & (Join-Path $installDir "openbuild.exe") --version
Write-Host ""
Write-Host "installed version: $version"
