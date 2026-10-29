# Build all binaries and install them for the current user.
#
#   .\install.ps1              # build (release) + install
#   .\install.ps1 -SkipBuild   # install whatever is already in dist\
#   .\install.ps1 -Update      # fetch from the update channel and install
#
# Installs to %LOCALAPPDATA%\OpenBuild and adds that directory to the user
# PATH, so `openbuild` and `openbuild-server` work from any terminal afterwards.
#
# `-Update` is what the TUI's Ctrl+U runs. It reads the same channel settings as
# the TUI:
#
#   OPENBUILD_UPDATE_BASE     channel base URL, e.g. https://updates.example/openbuild
#   OPENBUILD_UPDATE_CHANNEL  channel name, default "stable"
#   OPENBUILD_UPDATE_VERSION  exact version to install (set by the TUI)
#
# and downloads `{base}/{channel}/{artifact}` for each of the three binaries.
# A `{base}/{channel}/{artifact}.sha256` sidecar is verified when present.

param(
    [switch]$SkipBuild,
    [switch]$Update
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$dist = Join-Path $root "dist"
$installDir = Join-Path $env:LOCALAPPDATA "OpenBuild"

function Get-ChannelSetting([string]$Name, [string]$Default) {
    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($value)) { return $Default }
    return $value.Trim()
}

function Get-PointerVersion([string]$Url) {
    $body = (Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 30).Content
    $text = $body.Trim()
    # JSON pointer: {"version":"1.2.3"} — otherwise a bare semver line.
    try {
        $json = $text | ConvertFrom-Json -ErrorAction Stop
        if ($json.version) { return ([string]$json.version).Trim() }
    } catch {
        # Not JSON; fall through to the plain-text form.
    }
    return ($text -split "`n")[0].Trim().Trim('"')
}

function Get-ChannelArtifact([string]$Base, [string]$Channel, [string]$Name, [string]$Dest) {
    $url = "$($Base.TrimEnd('/'))/$Channel/$Name"
    Write-Host "  downloading $Name"
    Invoke-WebRequest -Uri $url -OutFile $Dest -UseBasicParsing -TimeoutSec 900

    $sidecar = "$url.sha256"
    try {
        $expected = (Invoke-WebRequest -Uri $sidecar -UseBasicParsing -TimeoutSec 30).Content
        $expected = ($expected -split "\s+")[0].Trim().ToLowerInvariant()
        if ($expected) {
            $actual = (Get-FileHash -Path $Dest -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actual -ne $expected) {
                Remove-Item $Dest -Force -ErrorAction SilentlyContinue
                throw "checksum mismatch for $Name (expected $expected, got $actual)"
            }
        }
    } catch [System.Net.WebException] {
        # No sidecar published: the binaries are still smoke-tested below.
        Write-Host "  no checksum published for $Name"
    } catch [Microsoft.PowerShell.Commands.HttpResponseException] {
        Write-Host "  no checksum published for $Name"
    }
}

if ($Update) {
    $base = Get-ChannelSetting "OPENBUILD_UPDATE_BASE" ""
    if ([string]::IsNullOrWhiteSpace($base)) {
        throw "no update channel configured: set OPENBUILD_UPDATE_BASE (or [update] base in your config)"
    }
    $channel = Get-ChannelSetting "OPENBUILD_UPDATE_CHANNEL" "stable"
    $version = Get-ChannelSetting "OPENBUILD_UPDATE_VERSION" ""
    if ([string]::IsNullOrWhiteSpace($version)) {
        $version = Get-PointerVersion "$($base.TrimEnd('/'))/$channel"
    }
    Write-Host "== update ==" -ForegroundColor Cyan
    Write-Host "channel $base/$channel"
    Write-Host "version $version"

    New-Item -ItemType Directory -Path $dist -Force | Out-Null
    $suffix = if ($IsWindows -or $env:OS -eq "Windows_NT") { "windows-x86_64" } else { "unknown" }
    $map = [ordered]@{
        "openbuild.exe"        = "openbuild-$version-$suffix.exe"
        "openbuild-kernel.exe" = "openbuild-kernel-$version-$suffix.exe"
        "openbuild-server.exe" = "openbuild-server-$version-$suffix.exe"
    }
    foreach ($local in $map.Keys) {
        Get-ChannelArtifact -Base $base -Channel $channel -Name $map[$local] -Dest (Join-Path $dist $local)
    }
} elseif (-not $SkipBuild) {
    & (Join-Path $root "build.ps1")
    if ($LASTEXITCODE -ne 0) { throw "build.ps1 failed with exit code $LASTEXITCODE" }
}

$tui = Join-Path $dist "openbuild.exe"
$kernel = Join-Path $dist "openbuild-kernel.exe"
$server = Join-Path $dist "openbuild-server.exe"
foreach ($path in @($tui, $kernel, $server)) {
    if (-not (Test-Path $path)) {
        throw "missing $path; run .\build.ps1 (or drop -SkipBuild) first"
    }
}

# A running kernel locks its exe on Windows. Stop the copies this install owns
# so the swap below cannot half-succeed.
Get-Process openbuild, openbuild-kernel, openbuild-server -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and (Split-Path -Parent $_.Path) -eq $installDir } |
    ForEach-Object {
        Write-Host "stopping running $($_.ProcessName) (pid $($_.Id))"
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
Start-Sleep -Milliseconds 500

New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item $tui $installDir -Force
Copy-Item $kernel $installDir -Force
Copy-Item $server $installDir -Force

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
$serverVersion = & (Join-Path $installDir "openbuild-server.exe") --version
Write-Host ""
Write-Host "installed version: $version"
Write-Host "installed ACP server: $serverVersion"
