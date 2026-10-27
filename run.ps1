$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$kernel = Join-Path $root "dist\openbuild-kernel.exe"
$tui = Join-Path $root "dist\openbuild.exe"

if (-not (Test-Path $kernel)) { throw "missing $kernel; run .\build.ps1 first" }
if (-not (Test-Path $tui)) { throw "missing $tui; run .\build.ps1 first" }

$env:OPENBUILD_BACKEND = "opencode"
$env:OPENBUILD_KERNEL = $kernel

& $tui @args