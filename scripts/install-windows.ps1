<#
.SYNOPSIS
  Build Seam from source and install it, or build the setup program.

.EXAMPLE
  # From an elevated PowerShell in the repository:
  powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1

.EXAMPLE
  # Build target\installer\Seam-<version>-windows-setup.exe (needs Inno Setup 6):
  powershell -ExecutionPolicy Bypass -File scripts\install-windows.ps1 -Installer
#>
param([switch]$Installer)
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
Push-Location $repo
try {
    # Panic messages name source files by full path. Keep this computer's
    # folders, and with them the user name, out of the programs. (Encoded
    # flags are separated by 0x1F, so paths with spaces stay intact.)
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $env:CARGO_ENCODED_RUSTFLAGS = @(
        "--remap-path-prefix=$cargoHome=cargo",
        "--remap-path-prefix=$repo=seam"
    ) -join [char]0x1F
    try {
        cargo build --release --features gui --bins
        if ($LASTEXITCODE -ne 0) { throw 'Build failed.' }
    } finally {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS
    }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

    if ($Installer) {
        $iscc = @(
            (Get-Command iscc.exe -ErrorAction SilentlyContinue).Source,
            "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
            "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
            # Where winget installs it for the current user.
            "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
        ) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
        if (-not $iscc) { throw 'Install Inno Setup 6 (winget install JRSoftware.InnoSetup), then retry.' }
        & $iscc /Qp "/DVersion=$version" "/DSource=$repo\target\release" installer\windows\seam.iss
        if ($LASTEXITCODE -ne 0) { throw 'Installer build failed.' }
        Write-Host "Built target\installer\Seam-$version-windows-setup.exe"
        return
    }

    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this from an elevated PowerShell window (Run as administrator).'
    }
    $install = Join-Path $env:ProgramFiles 'Seam'
    $agent = Join-Path $install 'seam-agent.exe'
    # The agent is a windowless program, so wait for it explicitly.
    function Invoke-Agent([string]$Command) {
        (Start-Process -FilePath $agent -ArgumentList $Command -Wait -PassThru -WindowStyle Hidden).ExitCode
    }
    if (Test-Path $agent) {
        # Stop the sign-in service first so it cannot restart the agent.
        Invoke-Agent stop-service | Out-Null
        Invoke-Agent stop | Out-Null
    }
    Get-Process seam, seam-agent -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    New-Item -ItemType Directory -Force $install | Out-Null
    Copy-Item target\release\seam.exe, target\release\seam-agent.exe $install -Force
    Copy-Item LICENSE-APACHE (Join-Path $install 'LICENSE-APACHE.txt') -Force
    Copy-Item LICENSE-MIT (Join-Path $install 'LICENSE-MIT.txt') -Force
    Copy-Item assets\fonts\OFL.txt (Join-Path $install 'Inter-LICENSE.txt') -Force
    # The MIT-only license file earlier versions installed.
    Remove-Item (Join-Path $install 'LICENSE.txt') -Force -ErrorAction SilentlyContinue

    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Seam.lnk'))
    $shortcut.TargetPath = Join-Path $install 'seam.exe'
    $shortcut.WorkingDirectory = $install
    $shortcut.Description = 'Share a keyboard and mouse between computers'
    $shortcut.Save()

    if ((Invoke-Agent setup) -ne 0) {
        throw "Setup failed; see $env:ProgramData\Seam\setup.log (or $env:TEMP\seam-setup.log)."
    }
    Write-Host "Installed Seam $version in $install. Open Seam from the Start menu."
} finally {
    Pop-Location
}
