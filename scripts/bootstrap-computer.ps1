# bootstrap-computer.ps1 — enroll a Windows 11 computer into ForgeFleet.
#
# Usage:
#   PowerShell> .\bootstrap-computer.ps1 -FleetLeaderIp 192.168.5.100 -Name tony -Role member -Runtime llama.cpp
#
# Drafted by Marcus (Qwen3-Coder-30B via ff run) and cleaned up on Taylor.

param(
    [Parameter(Mandatory=$true)]
    [string]$FleetLeaderIp,

    [Parameter(Mandatory=$true)]
    [string]$Name,

    [string]$Role    = 'member',
    [string]$Runtime = 'llama.cpp',
    [string]$Token   = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# 1. Generate ed25519 SSH key if missing.
$sshPath = Join-Path $env:USERPROFILE '.ssh'
if (-not (Test-Path $sshPath)) {
    New-Item -ItemType Directory -Path $sshPath -Force | Out-Null
}
$keyPath = Join-Path $sshPath 'id_ed25519'
if (-not (Test-Path $keyPath)) {
    ssh-keygen -t ed25519 -N '""' -f $keyPath | Out-Null
}
$pubKey = (Get-Content "$keyPath.pub" -Raw).Trim()

# 2. Detect hardware.
$memSum   = Get-CimInstance Win32_PhysicalMemory | Measure-Object -Property Capacity -Sum
$proc     = Get-CimInstance Win32_Processor     | Select-Object -First 1
$gpu      = Get-CimInstance Win32_VideoController | Select-Object -First 1

# 3. Primary IPv4 — skip link-local / loopback / APIPA 169.254.
$ip = Get-NetIPAddress -AddressFamily IPv4 `
    | Where-Object { $_.PrefixOrigin -in 'Dhcp','Manual' -and $_.IPAddress -notmatch '^(127\.|169\.254\.)' } `
    | Select-Object -First 1 -ExpandProperty IPAddress

if (-not $ip) { throw "could not detect a routable IPv4 address" }

# 4. Build enrollment body matching /api/fleet/self-enroll schema.
$body = @{
    token    = $Token
    name     = $Name
    ip       = $ip
    role     = $Role
    runtime  = $Runtime
    ssh_user = $env:USERNAME
    os       = 'windows'
    hardware = @{
        memory_gb  = [math]::Round($memSum.Sum / 1GB, 1)
        cpu        = $proc.Name
        cpu_cores  = $proc.NumberOfLogicalProcessors
        gpu        = $gpu.Name
    }
    ssh_public_key = $pubKey
} | ConvertTo-Json -Depth 4

# 5. POST.
$uri = "http://${FleetLeaderIp}:51002/api/fleet/self-enroll"
Write-Host "POST $uri" -ForegroundColor Cyan
try {
    $resp = Invoke-RestMethod -Uri $uri -Method Post -Body $body -ContentType 'application/json' -TimeoutSec 30
    Write-Host "✓ Enrolled as '$Name'" -ForegroundColor Green
    $resp | ConvertTo-Json -Depth 4
} catch {
    Write-Error "✗ Enrollment failed: $($_.Exception.Message)"
    exit 1
}
