# Builds a release executable and (optionally) the installer.
# Run from the project folder (the one with Cargo.toml):
#   powershell -ExecutionPolicy Bypass -File .\package.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

function Check($what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)" }
}

cargo test --release
Check "cargo test"
cargo build --release
Check "cargo build"

New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\swiftpdf.exe dist\ -Force

if (Test-Path pdfium.dll) {
    Write-Host "pdfium.dll found in the project root -> it is embedded in swiftpdf.exe (single-file build)."
} else {
    Write-Host "No pdfium.dll in the project root -> ship pdfium.dll next to swiftpdf.exe."
}

# Look for Inno Setup in the usual places (system-wide and per-user installs)
$candidates = @(
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
)
$iscc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) {
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { $iscc = $cmd.Source }
}

if ($iscc) {
    & $iscc installer\swiftpdf.iss
    Check "Inno Setup"
    Write-Host "Installer written to dist\"
} else {
    Write-Host "Inno Setup 6 not found - skipped the installer (https://jrsoftware.org/isinfo.php)."
}

Write-Host ""
Write-Host "Output folder: $PSScriptRoot\dist"
Get-ChildItem dist | Format-Table Name, Length -AutoSize
