<#
.SYNOPSIS
  The checks every change must pass: formatting, lints for Windows and macOS,
  and tests.

.DESCRIPTION
  The macOS lints run when the aarch64-apple-darwin target is installed
  (rustup target add aarch64-apple-darwin). The clipboard tests use an
  isolated clipboard, which Windows allows only in an elevated PowerShell;
  elsewhere they are skipped. -Release requires both.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\check.ps1
#>
param([switch]$Release)
$ErrorActionPreference = 'Stop'

function Step([string]$Name, [scriptblock]$Command) {
    Write-Host "== $Name"
    & $Command
    if ($LASTEXITCODE -ne 0) { throw "$Name failed." }
}

Push-Location (Join-Path $PSScriptRoot '..')
try {
    Step 'Format' { cargo fmt --all -- --check }
    Step 'Lints (Windows)' { cargo clippy --all-targets --features gui -- -D warnings }

    if (rustup target list --installed | Select-String -SimpleMatch -Quiet 'aarch64-apple-darwin') {
        # blake3 compiles C for Arm unless kept to pure Rust, which a lint pass
        # can be. A separate target folder keeps the normal build intact.
        $saved = @{ CARGO_FEATURE_PURE = $env:CARGO_FEATURE_PURE; CARGO_TARGET_DIR = $env:CARGO_TARGET_DIR }
        $env:CARGO_FEATURE_PURE = '1'
        $env:CARGO_TARGET_DIR = 'target\macos-check'
        try {
            Step 'Lints (macOS)' {
                cargo clippy --target aarch64-apple-darwin --all-targets --features gui -- -D warnings
            }
        } finally {
            foreach ($name in $saved.Keys) {
                [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
            }
        }
    } elseif ($Release) {
        throw 'Install the macOS target first: rustup target add aarch64-apple-darwin'
    } else {
        Write-Warning 'macOS lints skipped: rustup target add aarch64-apple-darwin'
    }

    $required = $env:SEAM_REQUIRE_CLIPBOARD_TESTS
    if ($Release) { $env:SEAM_REQUIRE_CLIPBOARD_TESTS = '1' }
    try {
        Step 'Tests' { cargo test --all-targets --features gui }
    } finally {
        $env:SEAM_REQUIRE_CLIPBOARD_TESTS = $required
    }
} finally {
    Pop-Location
}
