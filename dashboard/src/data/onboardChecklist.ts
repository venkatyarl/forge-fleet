/**
 * Pre-flight checklist items for new-node onboarding.
 * See plan: /Users/venkat/.claude/plans/gentle-questing-valley.md §4 checklist.
 */

export type Applies =
  | 'all'
  | 'linux'
  | 'mac'
  | 'apple-silicon'
  | 'intel-mac'
  | 'linux-gpu'
  | 'dgx-os'

export interface VerifyAction {
  kind: 'tcp' | 'ip_ping' | 'manual'
  ip?: string
  port?: number
}

export interface ChecklistItem {
  id: string
  group: string
  title: string
  applies_to: Applies[]
  detail_md: string
  verify?: VerifyAction
}

/**
 * Match an item against the selected machine-kind. An item's `applies_to`
 * list is OR — any matching tag means the item renders.
 */
export function itemApplies(
  item: ChecklistItem,
  machineKind: string,
  osFamily: string
): boolean {
  if (item.applies_to.includes('all')) return true
  for (const tag of item.applies_to) {
    if (tag === machineKind) return true
    if (tag === osFamily) return true
  }
  return false
}

export const CHECKLIST: ChecklistItem[] = [
  // ─── BIOS / firmware (Linux workers) ──────────────────────────────
  {
    id: 'bios_ac_restore',
    group: 'BIOS / firmware',
    title: 'Enable "Restore on AC Power Loss" in BIOS',
    applies_to: ['linux', 'linux-gpu', 'dgx-os'],
    detail_md:
      'Reboot the machine and enter BIOS (usually F2 / Del / F10 during boot).\n\nFind the setting named one of:\n  - Restore on AC Power Loss\n  - AC Power Recovery\n  - After Power Failure\n\nSet it to **Power On** (not "Last State" or "Stay Off"). This makes the node auto-recover after a power blip so ForgeFleet stays up unattended.',
  },
  {
    id: 'bios_wake_on_lan',
    group: 'BIOS / firmware',
    title: 'Enable Wake-on-LAN (optional)',
    applies_to: ['linux', 'linux-gpu', 'dgx-os'],
    detail_md:
      'In BIOS, under **Power Management**, enable **Wake-on-LAN**.\n\nLets the leader wake sleeping nodes via magic packet. Not required for ForgeFleet to work, but handy.',
  },

  // ─── Account / permissions ────────────────────────────────────────
  {
    id: 'user_account',
    group: 'Account',
    title: 'Create a user account with sudo access',
    applies_to: ['all'],
    detail_md:
      '**Linux:**\n```\nsudo adduser newuser\nsudo usermod -aG sudo newuser\n```\n\n**macOS:** System Settings → Users & Groups → add a user, tick "Allow user to administer this computer".',
  },

  // ─── Network ──────────────────────────────────────────────────────
  {
    id: 'static_ip',
    group: 'Network',
    title: 'Assign a static IP in 192.168.5.0/24',
    applies_to: ['all'],
    detail_md:
      'Pick an unused IP in `192.168.5.100–250`. Check ForgeFleet\'s fleet page for what\'s taken.\n\n**Linux (netplan):** edit `/etc/netplan/01-network.yaml`, set `dhcp4: false` and `addresses: [192.168.5.XXX/24]`, then `sudo netplan apply`.\n\n**macOS:** System Settings → Network → interface → Details → TCP/IP → Configure IPv4: **Manually**, set IP/subnet/router.\n\nClick [Verify] below once set.',
    verify: { kind: 'ip_ping' },
  },
  {
    id: 'network_hostname',
    group: 'Network',
    title: 'Set the hostname to match the desired node name',
    applies_to: ['all'],
    detail_md:
      '**Linux:** `sudo hostnamectl set-hostname <name>`\n\n**macOS:** `sudo scutil --set HostName <name> && sudo scutil --set LocalHostName <name>`',
  },

  // ─── Power & uptime ──────────────────────────────────────────────
  {
    id: 'disable_sleep',
    group: 'Power & uptime',
    title: 'Disable sleep / automatic suspend',
    applies_to: ['all'],
    detail_md:
      '**Ubuntu:**\n```\nsudo systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target\n```\n\n**macOS:**\n```\nsudo pmset -a sleep 0\nsudo pmset -a disablesleep 1\n```',
  },
  {
    id: 'auto_login',
    group: 'Power & uptime',
    title: 'Enable auto-login (so daemon starts without manual login)',
    applies_to: ['linux', 'linux-gpu', 'dgx-os'],
    detail_md:
      'On Ubuntu desktop: Settings → Users → Automatic Login (ON). On server (headless): not needed — systemd starts the daemon at boot regardless.',
  },

  // ─── SSH ────────────────────────────────────────────────────────
  {
    id: 'ssh_server_linux',
    group: 'SSH',
    title: 'Install & enable OpenSSH server',
    applies_to: ['linux', 'linux-gpu', 'dgx-os'],
    detail_md:
      '```\nsudo apt install -y openssh-server\nsudo systemctl enable --now ssh\n```\nVerify port 22 is listening with the button below.',
    verify: { kind: 'tcp', port: 22 },
  },
  {
    id: 'ssh_remote_login_mac',
    group: 'SSH',
    title: 'Enable Remote Login',
    applies_to: ['mac', 'apple-silicon', 'intel-mac'],
    detail_md:
      'System Settings → General → Sharing → **Remote Login**: ON. Allow access for the administrator user.',
    verify: { kind: 'tcp', port: 22 },
  },

  // ─── Firewall ────────────────────────────────────────────────────
  {
    id: 'fw_ssh',
    group: 'Firewall',
    title: 'Allow inbound on port 22 (SSH)',
    applies_to: ['all'],
    detail_md:
      '**Linux (ufw):** `sudo ufw allow 22/tcp`\n\n**macOS:** automatic when Remote Login is enabled.',
    verify: { kind: 'tcp', port: 22 },
  },
  {
    id: 'fw_daemon',
    group: 'Firewall',
    title: 'Allow inbound on 51000–51004 (ForgeFleet daemon)',
    applies_to: ['all'],
    detail_md:
      '**Linux (ufw):** `sudo ufw allow 51000:51004/tcp`\n\n**macOS:** System Settings → Network → Firewall → add an exception (or `sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp /path/to/ff`).',
  },
  {
    id: 'fw_llm',
    group: 'Firewall',
    title: 'Allow inbound on 55000–55010 (LLM ports)',
    applies_to: ['all'],
    detail_md: '`sudo ufw allow 55000:55010/tcp` (Linux) or Firewall exception (mac).',
  },

  // ─── GPU (NVIDIA nodes only) ────────────────────────────────────
  {
    id: 'gpu_nvidia_smi',
    group: 'GPU',
    title: 'nvidia-smi reports a GPU',
    applies_to: ['linux-gpu', 'dgx-os'],
    detail_md:
      'Run `nvidia-smi -L` on the node. Should list at least one GPU.\n\nIf "command not found", install the NVIDIA driver first:\n```\nsudo apt install -y nvidia-driver-535\nsudo reboot\n```',
  },
  {
    id: 'gpu_cuda',
    group: 'GPU',
    title: 'CUDA 12+ installed (driver ≥ 535)',
    applies_to: ['linux-gpu', 'dgx-os'],
    detail_md: '`nvidia-smi --query-gpu=driver_version --format=csv,noheader`\n\nShould show 535 or newer.',
  },
  {
    id: 'gpu_vllm_installable',
    group: 'GPU',
    title: 'pip install vllm succeeds',
    applies_to: ['linux-gpu', 'dgx-os'],
    detail_md:
      'Bootstrap script installs this automatically into `~/.forgefleet/vllm-venv/`. If you want to test manually:\n```\npython3 -m venv ~/.forgefleet/vllm-venv\nsource ~/.forgefleet/vllm-venv/bin/activate\npip install vllm\nvllm serve --help\n```',
  },

  // ─── DGX OS specifics ────────────────────────────────────────────
  {
    id: 'dgx_os_release',
    group: 'DGX OS specifics',
    title: 'Confirm DGX OS via /etc/os-release',
    applies_to: ['dgx-os'],
    detail_md: '`grep ^ID /etc/os-release` — should show `dgx-os` or similar.',
  },
  {
    id: 'dgx_container_toolkit',
    group: 'DGX OS specifics',
    title: 'NVIDIA container toolkit installed (optional)',
    applies_to: ['dgx-os'],
    detail_md: '`which nvidia-ctk`. Used for multi-model process isolation. Not required for basic operation.',
  },

  // ─── Manual desktop apps ─────────────────────────────────────────
  {
    id: 'manual_os_update',
    group: 'Manual desktop apps',
    title: 'Update the operating system to the latest',
    applies_to: ['all'],
    detail_md:
      '**Linux:** `sudo apt update && sudo apt upgrade -y`\n**macOS:** System Settings → General → Software Update → Update Now',
  },
  {
    id: 'manual_telegram',
    group: 'Manual desktop apps',
    title: 'Install Telegram desktop',
    applies_to: ['all'],
    detail_md: 'Download from https://telegram.org/apps',
  },
  {
    id: 'manual_bitwarden',
    group: 'Manual desktop apps',
    title: 'Install Bitwarden desktop',
    applies_to: ['all'],
    detail_md: 'Download from https://bitwarden.com/download/',
  },
  {
    id: 'manual_1password',
    group: 'Manual desktop apps',
    title: 'Install 1Password desktop',
    applies_to: ['all'],
    detail_md: 'Download from https://1password.com/downloads/',
  },

  // ─── Tooling (ForgeFleet installs these automatically) ──────────
  {
    id: 'tooling_gh',
    group: 'Tooling (auto)',
    title: 'GitHub CLI installed',
    applies_to: ['all'],
    detail_md: 'Bootstrap script installs `gh`. Verifies with `gh --version`.',
  },
  {
    id: 'tooling_gh_auth',
    group: 'Tooling (auto)',
    title: 'gh auth login with Venkat\'s GitHub account',
    applies_to: ['all'],
    detail_md:
      'Bootstrap script runs `gh auth login --with-token < ~/.forgefleet/gh_pat.txt`. Verifies with `gh auth status` showing `venkat-oclaw`.',
  },
  {
    id: 'tooling_1password',
    group: 'Tooling (auto)',
    title: '1Password CLI installed',
    applies_to: ['all'],
    detail_md: 'Bootstrap script installs `op`. Verifies with `op --version`.',
  },
  {
    id: 'tooling_codex',
    group: 'Tooling (auto)',
    title: 'Codex CLI installed',
    applies_to: ['all'],
    detail_md: 'Bootstrap script installs `codex`. Verifies with `codex --version`.',
  },
  {
    id: 'tooling_claude',
    group: 'Tooling (auto)',
    title: 'Claude Code CLI installed',
    applies_to: ['all'],
    detail_md: 'Bootstrap script installs `claude`. Verifies with `claude --version`.',
  },
  {
    id: 'tooling_openclaw',
    group: 'Tooling (auto)',
    title: 'OpenClaw installed + registered',
    applies_to: ['all'],
    detail_md: 'Bootstrap script installs `openclaw` and runs `openclaw node register`. If OpenClaw is unreachable, a deferred retry task is enqueued.',
  },

  // ─── Sub-agents ─────────────────────────────────────────────────
  {
    id: 'sub_agents_workspaces',
    group: 'Sub-agents',
    title: '~/.forgefleet/sub-agent-{N}/ workspaces created',
    applies_to: ['all'],
    detail_md:
      'Bootstrap script computes N = max(1, min(cores/2, ram/16, 4 or 8)) and creates N workspaces each with `scratch/`, `checkpoints/`, `cache/`.',
  },

  // ─── Permissions ────────────────────────────────────────────────
  {
    id: 'perm_passwordless_sudo',
    group: 'Permissions',
    title: 'Passwordless sudo configured (skipped on Taylor)',
    applies_to: ['linux', 'linux-gpu', 'dgx-os'],
    detail_md:
      'Bootstrap script writes `/etc/sudoers.d/forgefleet-<user>` with NOPASSWD:ALL.\n\nTaylor is explicitly excluded from this — the leader keeps human-confirmed sudo.',
  },

  // ─── Run bootstrap ─────────────────────────────────────────────
  {
    id: 'run_copy_command',
    group: 'Run bootstrap',
    title: 'Copy the curl command',
    applies_to: ['all'],
    detail_md: 'Click "Copy" on the command at the top of this page.',
  },
  {
    id: 'run_paste_and_sudo',
    group: 'Run bootstrap',
    title: 'Paste into terminal with sudo',
    applies_to: ['all'],
    detail_md: 'Open Terminal on the new machine, paste the command, hit Enter. It\'ll prompt for the user\'s sudo password once.',
  },
  {
    id: 'run_wait_daemon_active',
    group: 'Run bootstrap',
    title: 'Wait for "daemon active" event',
    applies_to: ['all'],
    detail_md: 'The enrollment-progress panel below will tick through steps: prereqs → rust → sshkey → enroll → sub_agents → service. When the last step flips to ✓, you\'re done.',
  },

  // ─── Final verify ───────────────────────────────────────────────
  {
    id: 'final_verify',
    group: 'Final verify',
    title: 'Run full verification battery',
    applies_to: ['all'],
    detail_md:
      'After enrollment completes, click the "Run full verify" button below. It calls POST /api/fleet/verify-node and runs all 12 checks.',
  },
]
