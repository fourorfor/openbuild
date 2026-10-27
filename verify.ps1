$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$tui = Join-Path $root "tui\grok-build"
$kernel = Join-Path $root "dist\openbuild-kernel.exe"

if (-not (Test-Path $kernel)) { throw "missing $kernel; run .\build.ps1 first" }

$env:PROTOC = if ($env:PROTOC) { $env:PROTOC } else { Join-Path $root "..\.yanagent\tools\protoc\bin\protoc.exe" }

$bun = if ($env:OPENBUILD_BUN) {
    $env:OPENBUILD_BUN
} elseif (Get-Command bun -ErrorAction SilentlyContinue) {
    (Get-Command bun).Source
} else {
    $candidate = Join-Path $root "..\.yanagent\tools\bun\bun-windows-x64\bun.exe"
    if (Test-Path $candidate) { $candidate } else { $null }
}

Write-Host "== backend unit tests ==" -ForegroundColor Cyan
Push-Location $tui
try {
    cargo test -p openbuild-backend
    if ($LASTEXITCODE -ne 0) { throw "backend tests failed" }

    if ($bun) {
        Write-Host "== uncensored plugin tests ==" -ForegroundColor Cyan
        & $bun test (Join-Path $root "tests\uncensored-plugin.test.ts")
        if ($LASTEXITCODE -ne 0) { throw "uncensored plugin tests failed" }
    } else {
        Write-Host "bun not found; skipping the uncensored plugin test" -ForegroundColor Yellow
    }

    Write-Host "== ACP handshake ==" -ForegroundColor Cyan
    cargo run -p openbuild-backend --example handshake -- $kernel $root
    if ($LASTEXITCODE -ne 0) { throw "handshake failed" }
} finally { Pop-Location }