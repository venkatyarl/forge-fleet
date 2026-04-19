# ForgeFleet Windows daemon installer.
#
# Installs `ff daemon` as a proper Windows service via NSSM
# (Non-Sucking Service Manager). The service survives reboots and
# user log-off, which the naive "Task Scheduler at logon" approach
# cannot guarantee.
#
# Status: UNTESTED. Windows is future hardware — when a Windows box
# joins the fleet, run this script as Administrator after placing
# ff.exe in %USERPROFILE%\.local\bin\.
#
# Usage (PowerShell, elevated):
#   cd C:\path\to\forge-fleet\scripts
#   Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass
#   .\install-windows-service.ps1
#
# Idempotent: re-running simply reconfigures the existing service.

[CmdletBinding()]
param(
    [string]$NssmUrl     = "https://nssm.cc/release/nssm-2.24.zip",
    [string]$InstallDir  = "$env:USERPROFILE\.local\bin",
    [string]$ServiceName = "forgefleet-daemon",
    [string]$LogDir      = "$env:USERPROFILE\.forgefleet\logs"
)

$ErrorActionPreference = "Stop"

function Ensure-Directory($path) {
    if (-not (Test-Path $path)) {
        New-Item -ItemType Directory -Path $path -Force | Out-Null
        Write-Host "Created directory: $path"
    }
}

Ensure-Directory $InstallDir
Ensure-Directory $LogDir

# ── Find (or install) NSSM ─────────────────────────────────────
$nssmExe = Join-Path $InstallDir "nssm.exe"
if (-not (Test-Path $nssmExe)) {
    Write-Host "NSSM not found at $nssmExe — downloading from $NssmUrl"
    $tmpZip = Join-Path $env:TEMP "nssm.zip"
    $tmpDir = Join-Path $env:TEMP "nssm-extracted"

    Invoke-WebRequest -Uri $NssmUrl -OutFile $tmpZip
    if (Test-Path $tmpDir) { Remove-Item $tmpDir -Recurse -Force }
    Expand-Archive -Path $tmpZip -DestinationPath $tmpDir -Force

    # NSSM's zip contains win32/nssm.exe and win64/nssm.exe — pick by arch.
    $arch = if ([Environment]::Is64BitOperatingSystem) { "win64" } else { "win32" }
    $extracted = Get-ChildItem -Path $tmpDir -Recurse -Filter "nssm.exe" |
        Where-Object { $_.FullName -like "*\$arch\*" } |
        Select-Object -First 1
    if (-not $extracted) {
        throw "could not locate nssm.exe ($arch) inside $tmpDir"
    }
    Copy-Item $extracted.FullName $nssmExe -Force
    Write-Host "Installed NSSM to $nssmExe"
}

# ── Verify ff.exe is present ───────────────────────────────────
$ffExe = Join-Path $InstallDir "ff.exe"
if (-not (Test-Path $ffExe)) {
    throw "ff.exe not found at $ffExe — build ff for Windows (cargo build --release -p ff) and copy it there first."
}

# ── Install or reconfigure the service ─────────────────────────
$existing = & $nssmExe status $ServiceName 2>$null
if ($LASTEXITCODE -eq 0 -and $existing) {
    Write-Host "Service '$ServiceName' already exists — reconfiguring."
    & $nssmExe stop $ServiceName | Out-Null
    & $nssmExe set  $ServiceName Application "$ffExe"
    & $nssmExe set  $ServiceName AppParameters "daemon"
} else {
    Write-Host "Installing service '$ServiceName'..."
    & $nssmExe install $ServiceName "$ffExe" "daemon"
}

# Log rotation + stdout/stderr paths
& $nssmExe set $ServiceName AppStdout  (Join-Path $LogDir "daemon.out.log")
& $nssmExe set $ServiceName AppStderr  (Join-Path $LogDir "daemon.err.log")
& $nssmExe set $ServiceName AppRotateFiles 1
& $nssmExe set $ServiceName AppRotateBytes 10485760   # 10 MiB per rotation
& $nssmExe set $ServiceName AppStopMethodSkip 0
& $nssmExe set $ServiceName AppExit Default Restart
& $nssmExe set $ServiceName AppRestartDelay 30000     # 30s — matches XML manifest

# Run under the current user so %USERPROFILE% resolves the same way ff does
# (LocalSystem would flip it to C:\Windows\system32\config\systemprofile\...).
# Future work: make this configurable via a -RunAsUser param + gMSA.
& $nssmExe set $ServiceName ObjectName "$env:USERDOMAIN\$env:USERNAME"

& $nssmExe start $ServiceName

Write-Host ""
Write-Host "ForgeFleet service installed and started:"
Write-Host "  Name:        $ServiceName"
Write-Host "  Executable:  $ffExe daemon"
Write-Host "  Stdout log:  $LogDir\daemon.out.log"
Write-Host "  Stderr log:  $LogDir\daemon.err.log"
Write-Host ""
Write-Host "Manage via:"
Write-Host "  sc.exe stop  $ServiceName"
Write-Host "  sc.exe start $ServiceName"
Write-Host "  nssm status  $ServiceName"
