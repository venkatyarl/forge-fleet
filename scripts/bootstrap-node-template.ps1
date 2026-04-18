# ForgeFleet Windows bootstrap script.
# Rendered at serve time from /onboard/bootstrap.ps1; placeholders:
#   {{LEADER_HOST}}, {{LEADER_PORT}}, {{TOKEN}}, {{NODE_NAME}}, {{NODE_IP}},
#   {{SSH_USER}}, {{ROLE}}, {{RUNTIME}}, {{GITHUB_OWNER}},
#   {{GITHUB_PAT_SECRET_KEY}}, {{IS_TAYLOR}}
#
# Run in an ELEVATED PowerShell 7+ / Windows PowerShell:
#   iwr -useb "http://leader:51002/onboard/bootstrap.ps1?..." | iex
#
# The script installs choco or winget-based prerequisites, rust, git, gh,
# clones forge-fleet, builds ff, registers the daemon as a Windows service,
# and self-enrolls.

$ErrorActionPreference = 'Stop'

# ─── Config (filled in by the server) ────────────────────────────────────
$LEADER_HOST           = "{{LEADER_HOST}}"
$LEADER_PORT           = "{{LEADER_PORT}}"
$TOKEN                 = "{{TOKEN}}"
$NAME                  = "{{NODE_NAME}}"
$IP                    = "{{NODE_IP}}"
$SSH_USER              = "{{SSH_USER}}"
$ROLE                  = "{{ROLE}}"
$RUNTIME_HINT          = "{{RUNTIME}}"
$GITHUB_OWNER          = "{{GITHUB_OWNER}}"
$GITHUB_PAT_SECRET_KEY = "{{GITHUB_PAT_SECRET_KEY}}"
$IS_TAYLOR             = "{{IS_TAYLOR}}"
$LEADER                = "http://$LEADER_HOST`:$LEADER_PORT"

# ─── Helpers ─────────────────────────────────────────────────────────────

function Say([string]$msg) { Write-Host "[+] $msg" -ForegroundColor Cyan }
function Die([string]$msg) { Write-Host "[!] $msg" -ForegroundColor Red; exit 1 }

function Report([string]$step, [string]$status, [string]$detail = "") {
    try {
        $body = @{ step = $step; status = $status; detail = $detail; at = (Get-Date).ToUniversalTime().ToString("o") } | ConvertTo-Json
        Invoke-RestMethod -Method Post -Uri "$LEADER/api/fleet/enrollment-progress?name=$NAME" `
            -ContentType "application/json" -Body $body -TimeoutSec 4 | Out-Null
    } catch { }
}

# ─── 0. Require elevated session ─────────────────────────────────────────

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Die "This script must run in an ELEVATED PowerShell. Right-click → 'Run as Administrator'."
}

# ─── 1. OS + hardware detect ─────────────────────────────────────────────

Report "detect_os" "running"
$OS_FULL = (Get-CimInstance Win32_OperatingSystem).Caption
$OS_VERSION = (Get-CimInstance Win32_OperatingSystem).Version
$CORES = (Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum
$RAM_GB = [math]::Round(((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB), 0)
$HAS_NVIDIA = $false
try {
    $gpu = (Get-CimInstance Win32_VideoController | Where-Object { $_.Name -match 'nvidia|geforce|quadro|tesla|rtx' } | Select-Object -First 1)
    if ($gpu) { $HAS_NVIDIA = $true }
} catch { }

$RUNTIME = $RUNTIME_HINT
if ($RUNTIME -eq 'auto') {
    if ($HAS_NVIDIA) { $RUNTIME = 'vllm' } else { $RUNTIME = 'llama.cpp' }
}
Say "OS: $OS_FULL ($OS_VERSION), cores=$CORES, ram=${RAM_GB}GB, nvidia=$HAS_NVIDIA, runtime=$RUNTIME"
Report "detect_os" "ok" "$OS_FULL / $RUNTIME"

# ─── 2. Package manager (winget preferred, fallback chocolatey) ──────────

Report "pkgmgr" "running"
$pkgMgr = $null
if (Get-Command winget -ErrorAction SilentlyContinue) { $pkgMgr = "winget" }
elseif (Get-Command choco -ErrorAction SilentlyContinue) { $pkgMgr = "choco" }
else {
    Say "Installing chocolatey..."
    Set-ExecutionPolicy Bypass -Scope Process -Force
    [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
    iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
    $pkgMgr = "choco"
}
Report "pkgmgr" "ok" $pkgMgr

# ─── 3. Prerequisites: git, gh, OpenSSH ──────────────────────────────────

Report "prereqs" "running"
function Install-IfMissing([string]$cmd, [string]$wingetId, [string]$chocoId) {
    if (Get-Command $cmd -ErrorAction SilentlyContinue) { return }
    Say "Installing $cmd..."
    if ($pkgMgr -eq "winget") {
        winget install --id $wingetId --accept-source-agreements --accept-package-agreements --silent | Out-Null
    } else {
        choco install -y $chocoId | Out-Null
    }
}
Install-IfMissing "git"  "Git.Git"       "git"
Install-IfMissing "gh"   "GitHub.cli"    "gh"
# OpenSSH Server as a Windows feature (not a choco package).
try {
    $caps = Get-WindowsCapability -Online -Name 'OpenSSH.Server~*'
    if ($caps.State -ne 'Installed') {
        Add-WindowsCapability -Online -Name $caps.Name | Out-Null
    }
    Set-Service -Name sshd -StartupType Automatic
    Start-Service sshd
    # Firewall rule for port 22.
    if (-not (Get-NetFirewallRule -Name 'forgefleet-sshd' -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -Name 'forgefleet-sshd' -DisplayName 'ForgeFleet sshd' `
            -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
    }
} catch { Say "OpenSSH Server setup warning: $_" }
Report "prereqs" "ok"

# ─── 4. Rust toolchain ───────────────────────────────────────────────────

Report "rust" "running"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Say "Installing rustup..."
    $rustup = "$env:TEMP\rustup-init.exe"
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustup
    & $rustup -y --default-toolchain stable --profile minimal
    $env:PATH += ";$env:USERPROFILE\.cargo\bin"
}
Report "rust" "ok"

# ─── 5. gh auth login via PAT ────────────────────────────────────────────

Report "gh_auth" "running"
$PAT = ""
try {
    $resp = Invoke-RestMethod -Uri "$LEADER/api/fleet/secret-peek?token=$TOKEN&key=$GITHUB_PAT_SECRET_KEY" -TimeoutSec 10
    $PAT = $resp.value
} catch { }
if ($PAT) {
    $PAT | gh auth login --with-token 2>&1 | Out-Null
    try {
        gh auth status --hostname github.com 2>&1 | Out-Null
        $ghUser = (gh api user -q .login 2>&1 | Out-String).Trim()
        Report "gh_auth" "ok" "logged in as $ghUser"
    } catch {
        Report "gh_auth" "failed" "auth status verification failed"
    }
} else {
    Report "gh_auth" "ok" "no PAT on fleet (public repo clone will still work)"
}

# ─── 6. Clone forge-fleet + build ff ─────────────────────────────────────

Report "clone" "running"
$REPO_DIR = Join-Path $env:USERPROFILE "projects\forge-fleet"
New-Item -ItemType Directory -Force -Path (Split-Path $REPO_DIR) | Out-Null
if (-not (Test-Path (Join-Path $REPO_DIR ".git"))) {
    git clone --depth 50 "https://github.com/$GITHUB_OWNER/forge-fleet.git" $REPO_DIR
} else {
    Push-Location $REPO_DIR
    git fetch origin main
    git reset --hard origin/main
    Pop-Location
}
Report "clone" "ok"

Report "build" "running"
Push-Location $REPO_DIR
cargo build -p ff-terminal --release 2>&1 | Select-Object -Last 3 | ForEach-Object { Say $_ }
$FF_BIN = Join-Path $env:USERPROFILE ".local\bin\ff.exe"
New-Item -ItemType Directory -Force -Path (Split-Path $FF_BIN) | Out-Null
Copy-Item -Force (Join-Path $REPO_DIR "target\release\ff.exe") $FF_BIN
Pop-Location
Report "build" "ok"

# Add ~/.local/bin to user PATH (idempotent).
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$want = "$env:USERPROFILE\.local\bin"
if ($userPath -notlike "*$want*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$want", "User")
    $env:PATH += ";$want"
}

# ─── 7. SSH keypair + host keys ──────────────────────────────────────────

Report "sshkey" "running"
$SSH_DIR = Join-Path $env:USERPROFILE ".ssh"
New-Item -ItemType Directory -Force -Path $SSH_DIR | Out-Null
$KEY_PATH = Join-Path $SSH_DIR "id_ed25519"
if (-not (Test-Path $KEY_PATH)) {
    ssh-keygen -t ed25519 -N '""' -f $KEY_PATH -C "$SSH_USER@$NAME"
}
$USER_PUBKEY = (Get-Content "${KEY_PATH}.pub" -Raw).Trim()

# Host keys (created by sshd on first start). Windows sshd typically writes
# them to ProgramData\ssh\.
$HOST_KEYS = @()
$hostKeyDir = "C:\ProgramData\ssh"
if (Test-Path $hostKeyDir) {
    Get-ChildItem "$hostKeyDir\ssh_host_*_key.pub" | ForEach-Object {
        $HOST_KEYS += (Get-Content $_.FullName -Raw).Trim()
    }
}
Report "sshkey" "ok"

# ─── 8. Sub-agent workspace layout ───────────────────────────────────────

$CountFromCores = [math]::Floor($CORES / 2)
$CountFromRam   = [math]::Floor($RAM_GB / 16)
$SUB_AGENTS = [math]::Max(1, [math]::Min([math]::Min($CountFromCores, $CountFromRam), 4))
if ($HAS_NVIDIA -and $RAM_GB -ge 64) { $SUB_AGENTS = [math]::Min($CountFromCores, 8) }
Say "Sub-agents: $SUB_AGENTS (cores=$CORES, ram=${RAM_GB}G)"

$FF_HOME = Join-Path $env:USERPROFILE ".forgefleet"
New-Item -ItemType Directory -Force -Path (Join-Path $FF_HOME "logs") | Out-Null
for ($i = 0; $i -lt $SUB_AGENTS; $i++) {
    New-Item -ItemType Directory -Force -Path (Join-Path $FF_HOME "sub-agent-$i\scratch") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $FF_HOME "sub-agent-$i\checkpoints") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $FF_HOME "sub-agent-$i\cache") | Out-Null
}
Report "sub_agents" "ok" "count=$SUB_AGENTS"

# ─── 9. Self-enroll ──────────────────────────────────────────────────────

Report "enroll" "running"
$enrollPayload = @{
    token    = $TOKEN
    name     = $NAME
    hostname = (hostname)
    ip       = $IP
    os       = "$OS_FULL $OS_VERSION"
    os_id    = "windows"
    runtime  = $RUNTIME
    ram_gb   = [int]$RAM_GB
    cpu_cores = [int]$CORES
    role     = $ROLE
    ssh_user = $SSH_USER
    sub_agent_count = [int]$SUB_AGENTS
    gh_account = $GITHUB_OWNER
    has_nvidia = $HAS_NVIDIA
    ssh_identity = @{
        user_public_key = $USER_PUBKEY
        host_public_keys = $HOST_KEYS
    }
} | ConvertTo-Json -Depth 5

$ENROLL_RESP = Invoke-RestMethod -Method Post -Uri "$LEADER/api/fleet/self-enroll" `
    -ContentType "application/json" -Body $enrollPayload -TimeoutSec 30
Say "Enrolled: $($ENROLL_RESP.assigned_name)"
Report "enroll" "ok"

# ─── 10. Import peer SSH identities ──────────────────────────────────────

Report "mesh_import" "running"
$authzPath = Join-Path $SSH_DIR "authorized_keys"
$knownPath = Join-Path $SSH_DIR "known_hosts"
if (-not (Test-Path $authzPath)) { New-Item -ItemType File -Path $authzPath | Out-Null }
if (-not (Test-Path $knownPath)) { New-Item -ItemType File -Path $knownPath | Out-Null }
$existingAuthz = Get-Content $authzPath -Raw -ErrorAction SilentlyContinue
$existingKnown = Get-Content $knownPath -Raw -ErrorAction SilentlyContinue
$addedUser = 0; $addedHost = 0
foreach ($p in $ENROLL_RESP.peer_ssh_identities) {
    $upk = $p.user_public_key
    if ($upk -and ($existingAuthz -notlike "*$upk*")) {
        Add-Content -Path $authzPath -Value $upk
        $addedUser++
    }
    foreach ($hk in $p.host_public_keys) {
        if (-not $hk) { continue }
        $line = "$($p.ip),$($p.name) $hk"
        if ($existingKnown -notlike "*$line*") {
            Add-Content -Path $knownPath -Value $line
            $addedHost++
        }
    }
}
Report "mesh_import" "ok" "+$addedUser authorized_keys, +$addedHost known_hosts"

# ─── 11. Windows service registration ────────────────────────────────────

Report "service" "running"
$svcName = "forgefleet-daemon"
if (Get-Service $svcName -ErrorAction SilentlyContinue) {
    Stop-Service $svcName -ErrorAction SilentlyContinue
    & sc.exe delete $svcName | Out-Null
    Start-Sleep -Seconds 1
}
# Simple service wrapper: the ff binary has an `ff daemon` subcommand.
# Windows service manager needs a long-running process; ff daemon qualifies.
# If nssm is available we use it (better stdout/stderr handling); else sc.exe.
$ffCmd = "$env:USERPROFILE\.local\bin\ff.exe daemon --as-node $NAME --scheduler"
if (Get-Command nssm -ErrorAction SilentlyContinue) {
    nssm install $svcName (Join-Path $env:USERPROFILE ".local\bin\ff.exe") "daemon --as-node $NAME --scheduler"
    nssm set $svcName AppStdout (Join-Path $FF_HOME "logs\daemon.out.log")
    nssm set $svcName AppStderr (Join-Path $FF_HOME "logs\daemon.err.log")
    nssm set $svcName Start SERVICE_AUTO_START
    Start-Service $svcName
    Report "service" "ok" "nssm-managed"
} else {
    # Fallback: Task Scheduler at-logon task (service mode requires nssm for reliability).
    $action = New-ScheduledTaskAction -Execute (Join-Path $env:USERPROFILE ".local\bin\ff.exe") `
        -Argument "daemon --as-node $NAME --scheduler"
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -StartWhenAvailable -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskName "ForgeFleet-Daemon" -Action $action -Trigger $trigger `
        -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName "ForgeFleet-Daemon"
    Report "service" "ok" "scheduled-task (install nssm for proper service mode)"
}

# ─── Done ────────────────────────────────────────────────────────────────

Report "done" "ok" "$NAME is now a ForgeFleet node"
Say "✓ Onboarding complete: $NAME"
