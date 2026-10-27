$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$tui = Join-Path $root "tui\grok-build"
$kernel = Join-Path $root "dist\openbuild-kernel.exe"

if (-not (Test-Path $kernel)) { throw "missing $kernel; run .\build.ps1 first" }

$env:PROTOC = if ($env:PROTOC) { $env:PROTOC } else { Join-Path $root "..\.yanagent\tools\protoc\bin\protoc.exe" }

Write-Host "== backend unit tests ==" -ForegroundColor Cyan
Push-Location $tui
try {
    cargo test -p openbuild-backend
    if ($LASTEXITCODE -ne 0) { throw "backend tests failed" }

    Write-Host "== ACP handshake ==" -ForegroundColor Cyan
    cargo run -p openbuild-backend --example handshake -- $kernel $root
    if ($LASTEXITCODE -ne 0) { throw "handshake failed" }
} finally { Pop-Location }