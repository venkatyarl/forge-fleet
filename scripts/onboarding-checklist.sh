#!/usr/bin/env bash
# Per-computer onboarding checklist report.
#
# Output: ~/.forgefleet/reports/onboarding_<date>.md
# Reads live Postgres state + queries each computer's Pulse beat.
#
# Run any time to get a snapshot of fleet readiness.

set -u
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT=~/.forgefleet/reports/onboarding_${STAMP}.md
mkdir -p ~/.forgefleet/reports
PG="docker exec -i forgefleet-postgres psql -U forgefleet -d forgefleet -tA -F|"

# Pull every enrolled computer.
COMPUTERS=$($PG -c "SELECT name FROM computers ORDER BY name" 2>/dev/null)

{
  echo "# ForgeFleet Onboarding Checklist — $STAMP"
  echo
  echo "Per-computer status across 12 onboarding items. Run from Taylor."
  echo
  printf "| %-8s | %-4s | %-4s | %-5s | %-5s | %-6s | %-4s | %-6s | %-4s | %-7s | %-5s | %-7s |\n" \
    computer ident ssh sudo ff daemon pulse runtime llm subagnt disk netscope
  printf "| %-8s | %-4s | %-4s | %-5s | %-5s | %-6s | %-4s | %-6s | %-4s | %-7s | %-5s | %-7s |\n" \
    -------- ---- ---- ----- ----- ------ ---- ------ ---- ------- ----- -------

  for NAME in $COMPUTERS; do
    # 1. identity (always ✓ if we got here)
    IDENT="✓"

    # 2. SSH mesh — at least 3 trust rows out of 5 peers (non-self)
    TRUST_CNT=$($PG -c "SELECT COUNT(*) FROM computer_trust WHERE source_computer_id=(SELECT id FROM computers WHERE name='$NAME')")
    if [[ "${TRUST_CNT:-0}" -ge 3 ]]; then SSH="✓"; else SSH="⚠ $TRUST_CNT"; fi

    # 3. sudo — check metadata (Taylor is intentionally excluded)
    if [[ "$NAME" == "taylor" ]]; then
      SUDO="n/a"
    else
      SUDO_OK=$($PG -c "SELECT (metadata->>'passwordless_sudo') FROM computers WHERE name='$NAME'" 2>/dev/null)
      [[ "$SUDO_OK" == "true" ]] && SUDO="✓" || SUDO="?"
    fi

    # 4. ff binary installed
    FF=$($PG -c "SELECT installed_version FROM computer_software cs JOIN computers c ON c.id=cs.computer_id WHERE c.name='$NAME' AND cs.software_id='ff'")
    [[ -n "$FF" ]] && FF_CELL="✓" || FF_CELL="✗"

    # 5. forgefleetd running — software row or daemon seen via pulse
    DAEMON=$($PG -c "SELECT installed_version FROM computer_software cs JOIN computers c ON c.id=cs.computer_id WHERE c.name='$NAME' AND cs.software_id='forgefleetd'")
    [[ -n "$DAEMON" ]] && DAEMON_CELL="✓" || DAEMON_CELL="?"

    # 6. Pulse fresh — last_seen_at within 60s
    PULSE=$($PG -c "SELECT CASE WHEN last_seen_at > NOW() - INTERVAL '60 seconds' THEN 'y' ELSE 'n' END FROM computers WHERE name='$NAME'")
    [[ "$PULSE" == "y" ]] && PULSE_CELL="✓" || PULSE_CELL="✗"

    # 7. Runtime matches policy for this OS
    OS=$($PG -c "SELECT os_family FROM computers WHERE name='$NAME'")
    RT=$($PG -c "SELECT runtime FROM fleet_members WHERE computer_id=(SELECT id FROM computers WHERE name='$NAME')")
    case "$OS" in
      macos) POLICY="mlx" ;;
      linux*) POLICY="llamacpp" ;;
      *) POLICY="?" ;;
    esac
    if [[ "$RT" == "$POLICY" ]]; then RUNTIME="✓ $RT"; else RUNTIME="⚠ $RT vs $POLICY"; fi

    # 8. At least one LLM server active
    LLM=$($PG -c "SELECT COUNT(*) FROM computer_model_deployments cmd JOIN computers c ON c.id=cmd.computer_id WHERE c.name='$NAME' AND cmd.status='active'")
    [[ "${LLM:-0}" -ge 1 ]] && LLM_CELL="✓ $LLM" || LLM_CELL="✗"

    # 9. Sub-agents seeded
    SA=$($PG -c "SELECT COUNT(*) FROM sub_agents sa JOIN computers c ON c.id=sa.computer_id WHERE c.name='$NAME'")
    [[ "${SA:-0}" -ge 1 ]] && SA_CELL="✓ $SA" || SA_CELL="✗"

    # 10. Disk sample recent (table is fleet_disk_usage, keyed by node_name + sampled_at)
    DISK=$($PG -c "SELECT CASE WHEN MAX(sampled_at) > NOW() - INTERVAL '15 minutes' THEN 'y' ELSE 'n' END FROM fleet_disk_usage WHERE node_name='$NAME'")
    [[ "$DISK" == "y" ]] && DISK_CELL="✓" || DISK_CELL="?"

    # 11. Network scope declared
    NS=$($PG -c "SELECT COALESCE(network_scope, '?') FROM computers WHERE name='$NAME'")

    printf "| %-8s | %-4s | %-4s | %-5s | %-5s | %-6s | %-4s | %-6s | %-4s | %-7s | %-5s | %-7s |\n" \
      "$NAME" "$IDENT" "$SSH" "$SUDO" "$FF_CELL" "$DAEMON_CELL" "$PULSE_CELL" "$RUNTIME" "$LLM_CELL" "$SA_CELL" "$DISK_CELL" "$NS"
  done

  echo
  echo "## Legend"
  echo
  echo "- **ident** — row in \`computers\` table"
  echo "- **ssh** — ≥3 mesh trust rows (out of 5 peers)"
  echo "- **sudo** — passwordless_sudo flag (Taylor excluded by policy)"
  echo "- **ff** — \`ff\` binary tracked in computer_software"
  echo "- **daemon** — forgefleetd tracked in computer_software"
  echo "- **pulse** — last_seen_at < 60s"
  echo "- **runtime** — fleet_members.runtime vs policy (macos=mlx, linux=llamacpp)"
  echo "- **llm** — count of active rows in computer_model_deployments"
  echo "- **subagnt** — count of sub_agents slots"
  echo "- **disk** — computer_disk_usage row within last 15 min"
  echo "- **netscope** — computers.network_scope"
  echo
  echo "## Drift detail"
  echo
  echo "### Software with status='upgrade_available'"
  echo '```'
  $PG -c "SELECT c.name, cs.software_id, cs.installed_version, sr.latest_version FROM computer_software cs JOIN computers c ON c.id=cs.computer_id JOIN software_registry sr ON sr.id=cs.software_id WHERE cs.status='upgrade_available' ORDER BY cs.software_id, c.name" 2>&1 | sed 's/|/\t/g'
  echo '```'
  echo
  echo "### LLM servers per computer"
  echo '```'
  $PG -c "SELECT c.name, cmd.model_id, cmd.runtime, cmd.status, cmd.endpoint FROM computer_model_deployments cmd JOIN computers c ON c.id=cmd.computer_id ORDER BY c.name, cmd.model_id" 2>&1 | sed 's/|/\t/g'
  echo '```'
} > "$OUT"

echo "$OUT"
