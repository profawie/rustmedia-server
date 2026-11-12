<#
.SYNOPSIS
    Build rustmedia.exe (release) and copy it to the project folder.

.EXAMPLE
    .\build.ps1                 # test + release build
    .\build.ps1 -SkipTests      # build only
    .\build.ps1 -Gnu            # use the GNU toolchain instead of MSVC
    .\build.ps1 -Output D:\Movies   # also copy the exe to another folder

    If PowerShell blocks the script, run once:
    powershell -ExecutionPolicy Bypass -File .\build.ps1
#>
param(
    [switch]$SkipTests,
    [switch]$Gnu,
    [string]$Output = ""
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

function Fail([string]$msg) {
    Write-Host "ERROR: $msg" -ForegroundColor Red
    exit 1
}

# 1. Rust toolchain
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail "cargo not found. Install Rust from https://rustup.rs (or: winget install Rustlang.Rustup), then open a new terminal."
}
Write-Host "Using $(cargo --version)" -ForegroundColor Cyan

# -Gnu builds with the GNU toolchain, which bundles its own linker, so the
# Visual Studio Build Tools are not needed.
$toolchain = @()
$exeDir = "target\release"
if ($Gnu) {
    $tc = "stable-x86_64-pc-windows-gnu"
    rustup toolchain install $tc --profile minimal
    if ($LASTEXITCODE -ne 0) { Fail "could not install toolchain $tc" }
    $toolchain = @("+$tc")
}

# 2. Tests
if (-not $SkipTests) {
    Write-Host "`nRunning tests..." -ForegroundColor Cyan
    cargo @toolchain test --release
    if ($LASTEXITCODE -ne 0) { Fail "tests failed" }
}

# 3. Build
Write-Host "`nBuilding release..." -ForegroundColor Cyan
cargo @toolchain build --release
if ($LASTEXITCODE -ne 0) {
    Fail ("build failed. With the default MSVC toolchain you need 'Visual Studio Build Tools' " +
          "(C++ workload): winget install Microsoft.VisualStudio.2022.BuildTools. " +
          "Or try: .\build.ps1 -Gnu")
}

# 4. Copy the exe
$exe = Join-Path $exeDir "rustmedia.exe"
if (-not (Test-Path $exe)) { Fail "$exe not found after build" }
Copy-Item $exe -Destination (Join-Path $PSScriptRoot "rustmedia.exe") -Force
if ($Output -ne "") {
    if (-not (Test-Path $Output -PathType Container)) { Fail "output folder '$Output' does not exist" }
    Copy-Item $exe -Destination (Join-Path $Output "rustmedia.exe") -Force
}

$size = [math]::Round((Get-Item $exe).Length / 1MB, 2)
Write-Host "`nDone: rustmedia.exe ($size MB)" -ForegroundColor Green
if ($Output -ne "") { Write-Host "Also copied to: $Output" -ForegroundColor Green }
