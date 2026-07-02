# pult installer for Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/lonic-software/pult/main/install.ps1 | iex
#
# Environment overrides:
#   PULT_VERSION      install a specific tag, e.g. v0.1.0 (default: latest release)
#   PULT_INSTALL_DIR  where to put the binary (default: %LOCALAPPDATA%\Programs\pult)
#   PULT_REPO         GitHub repo slug        (default: lonic-software/pult)
#
# Note: pult executes commands via sh/bash and fetches git modules via git —
# install Git for Windows and use pult from Git Bash (or ensure bash and git
# are on your PATH).

$ErrorActionPreference = "Stop"

$Repo = if ($env:PULT_REPO) { $env:PULT_REPO } else { "lonic-software/pult" }
$Version = if ($env:PULT_VERSION) { $env:PULT_VERSION } else { "latest" }
$InstallDir = if ($env:PULT_INSTALL_DIR) { $env:PULT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\pult" }

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") {
    throw "no Windows ARM build yet - build from source: cargo install --path ."
}

$Asset = "pult-x86_64-pc-windows-msvc.zip"
$Base = if ($Version -eq "latest") {
    "https://github.com/$Repo/releases/latest/download"
} else {
    "https://github.com/$Repo/releases/download/$Version"
}

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $Tmp | Out-Null
try {
    Write-Host "downloading $Base/$Asset"
    Invoke-WebRequest "$Base/$Asset" -OutFile (Join-Path $Tmp $Asset)

    # Verify against checksums.txt
    try {
        Invoke-WebRequest "$Base/checksums.txt" -OutFile (Join-Path $Tmp "checksums.txt")
        $line = (Get-Content (Join-Path $Tmp "checksums.txt")) | Where-Object { $_ -match [regex]::Escape($Asset) }
        $expected = ($line -split '\s+')[0]
        $actual = (Get-FileHash (Join-Path $Tmp $Asset) -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) { throw "checksum verification FAILED - refusing to install" }
        Write-Host "checksum ok"
    } catch [System.Net.WebException] {
        Write-Host "warning: could not fetch checksums.txt; skipping verification"
    }

    Expand-Archive (Join-Path $Tmp $Asset) -DestinationPath $Tmp
    New-Item -ItemType Directory -Force $InstallDir | Out-Null
    Copy-Item (Join-Path $Tmp "pult.exe") (Join-Path $InstallDir "pult.exe") -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
        Write-Host "added $InstallDir to your user PATH (restart your terminal)"
    }

    Write-Host "installed $InstallDir\pult.exe"

    # Runtime dependencies — warn specifically rather than a blanket reminder.
    $MissingDeps = @("bash", "git") | Where-Object { -not (Get-Command $_ -ErrorAction SilentlyContinue) }
    if ($MissingDeps) {
        Write-Host ""
        Write-Host "warning: $($MissingDeps -join ' and ') not found on PATH. pult executes"
        Write-Host "commands via bash and fetches git modules via git. Git for Windows"
        Write-Host "provides both (https://gitforwindows.org) - install it and either run"
        Write-Host "pult from Git Bash or add its Unix tools to your PATH."
    }
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
