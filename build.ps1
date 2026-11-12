<#
.SYNOPSIS
    Build rustmedia.exe (release) and copy it to the project folder.

.EXAMPLE
    .\build.ps1                     # test + release build
    .\build.ps1 -SkipTests          # build only
    .\build.ps1 -Gnu                # force the GNU toolchain (no Visual Studio needed)
    .\build.ps1 -Output D:\Movies   # also copy the exe to another folder

    Without Visual Studio Build Tools the script switches to the GNU
    toolchain by itself. "--gnu" style options are accepted too.

    If PowerShell blocks the script, run once:
    powershell -ExecutionPolicy Bypass -File .\build.ps1
#>
[CmdletBinding(PositionalBinding = $false)]
param(
    [switch]$SkipTests,
    [switch]$Gnu,
    [string]$Output = "",
    # Catches GNU-style options such as --gnu / --skip-tests.
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest = @()
)

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

function Fail([string]$msg) {
    Write-Host "ERROR: $msg" -ForegroundColor Red
    exit 1
}

foreach ($a in $Rest) {
    switch -Regex ($a) {
        '^-{1,2}gnu$' { $Gnu = $true; continue }
        '^-{1,2}skip-?tests$' { $SkipTests = $true; continue }
        default { Fail "unknown option '$a'. Use -Gnu, -SkipTests or -Output <folder>." }
    }
}

# 1. Rust toolchain
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail "cargo not found. Install Rust from https://rustup.rs (or: winget install Rustlang.Rustup), then open a new terminal."
}
Write-Host "Using $(cargo --version)" -ForegroundColor Cyan

# The default (MSVC) toolchain needs link.exe from the Visual Studio Build
# Tools. If they aren't installed, use the GNU toolchain instead: it ships
# its own linker, so nothing else has to be installed.
function Test-MsvcLinker {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { return $false }
    $path = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    return [bool]$path
}

$hostTriple = ((rustc -vV) | Select-String '^host:').ToString().Split(' ')[1]
if (-not $Gnu -and $hostTriple -like '*msvc*' -and -not (Test-MsvcLinker)) {
    Write-Host "Visual Studio Build Tools (link.exe) not found: using the GNU toolchain instead." -ForegroundColor Yellow
    $Gnu = $true
}

$toolchain = @()
if ($Gnu) {
    $tc = "stable-x86_64-pc-windows-gnu"
    Write-Host "Installing/updating Rust toolchain $tc (first time only takes a minute)..." -ForegroundColor Cyan
    rustup toolchain install $tc --profile minimal
    if ($LASTEXITCODE -ne 0) { Fail "could not install toolchain $tc" }
    $toolchain = @("+$tc")
}

# 2. Tests (same toolchain as the build)
if (-not $SkipTests) {
    Write-Host "`nRunning tests..." -ForegroundColor Cyan
    cargo @toolchain test --release
    if ($LASTEXITCODE -ne 0) { Fail "tests failed" }
}

# 3. Build
Write-Host "`nBuilding release..." -ForegroundColor Cyan
cargo @toolchain build --release
if ($LASTEXITCODE -ne 0) {
    if ($Gnu) {
        Fail "build failed with the GNU toolchain (see the errors above)."
    }
    Fail ("build failed. The MSVC toolchain needs 'Visual Studio Build Tools' (C++ workload): " +
          "winget install Microsoft.VisualStudio.2022.BuildTools. Or run: .\build.ps1 -Gnu")
}

# 4. Copy the exe
$exe = "target\release\rustmedia.exe"
if (-not (Test-Path $exe)) { Fail "$exe not found after build" }
Copy-Item $exe -Destination (Join-Path $PSScriptRoot "rustmedia.exe") -Force
if ($Output -ne "") {
    if (-not (Test-Path $Output -PathType Container)) { Fail "output folder '$Output' does not exist" }
    Copy-Item $exe -Destination (Join-Path $Output "rustmedia.exe") -Force
}

$size = [math]::Round((Get-Item $exe).Length / 1MB, 2)
Write-Host "`nDone: rustmedia.exe ($size MB)" -ForegroundColor Green
if ($Output -ne "") { Write-Host "Also copied to: $Output" -ForegroundColor Green }
