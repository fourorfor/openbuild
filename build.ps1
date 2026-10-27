$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force $dist | Out-Null

function Resolve-Tool($name, $envName, $candidates) {
    if ($envName -and (Get-Item "env:$envName" -ErrorAction SilentlyContinue)) {
        return (Get-Item "env:$envName").Value
    }
    $cmd = Get-Command $name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($candidate in $candidates) {
        if (Test-Path $candidate) { return $candidate }
    }
    throw "$name not found. Set $envName or install it."
}

$bun = Resolve-Tool "bun" "OPENBUILD_BUN" @(
    (Join-Path $root "..\.yanagent\tools\bun\bun-windows-x64\bun.exe")
)
$protoc = Resolve-Tool "protoc" "PROTOC" @(
    (Join-Path $root "..\.yanagent\tools\protoc\bin\protoc.exe")
)

$env:PROTOC = $protoc
# The vendored copy has no .git; skip husky's prepare hook.
$env:HUSKY = "0"

Write-Host "== kernel ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "vendor\opencode")
try {
    & $bun install --frozen-lockfile
    if ($LASTEXITCODE -ne 0) { throw "bun install failed" }
    $env:OPENCODE_VERSION = "2.1.4"
    Push-Location "packages\cli"
    try {
        & $bun run script/build.ts --single --skip-web-ui --skip-install --outdir=dist
        if ($LASTEXITCODE -ne 0) { throw "kernel build failed" }
        Copy-Item "dist\cli-windows-x64\bin\opencode.exe" (Join-Path $dist "openbuild-kernel.exe") -Force
    } finally { Pop-Location }
} finally { Pop-Location }

Write-Host "== tui ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "tui\grok-build")
try {
    cargo build -p xai-grok-pager-bin --release
    if ($LASTEXITCODE -ne 0) { throw "tui build failed" }
    $tuiExe = Join-Path $dist "openbuild.exe"
    Copy-Item "target\release\xai-grok-pager.exe" $tuiExe -Force
    # The pager's debug main frame is large; release is usually fine, but a
    # larger PE stack reserve is harmless and keeps debug builds usable too.
    $editbin = Get-ChildItem "C:\Program Files (x86)\Microsoft Visual Studio" -Recurse -Filter editbin.exe -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($editbin) { & $editbin.FullName /STACK:33554432 $tuiExe | Out-Null }

    Write-Host "== acp server ==" -ForegroundColor Cyan
    cargo build -p openbuild-server --release
    if ($LASTEXITCODE -ne 0) { throw "acp server build failed" }
    Copy-Item "target\release\openbuild-server.exe" (Join-Path $dist "openbuild-server.exe") -Force
} finally { Pop-Location }

Write-Host "== verify ==" -ForegroundColor Cyan
& (Join-Path $dist "openbuild-kernel.exe") --version
& (Join-Path $dist "openbuild.exe") --version
& (Join-Path $dist "openbuild-server.exe") --version
Write-Host "artifacts: $dist" -ForegroundColor Green