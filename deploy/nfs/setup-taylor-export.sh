#!/usr/bin/env bash
# TB.4 setup — Vinny side. Run as: sudo bash deploy/nfs/setup-vinny-export.sh
#
# Exports /Users/vinny/models read-only over Thunderbolt (10.44.0.0/24)
# so James can mount it without duplicating the 50GB model directory.

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (sudo)" >&2
    exit 1
fi

EXPORTS=/etc/exports
LINE='/Users/vinny/models -ro -alldirs -network 10.44.0.0 -mask 255.255.255.0'

if [[ -f $EXPORTS ]] && grep -qF "$LINE" "$EXPORTS"; then
    echo "/etc/exports already contains the line — no edit"
else
    touch "$EXPORTS"
    printf '\n# TB.4 — read-only export to james (10.44.0.0/24) via Thunderbolt\n%s\n' "$LINE" >> "$EXPORTS"
    echo "appended export to $EXPORTS"
fi

# nfsd uses launchd on macOS. Enable + restart.
nfsd enable
nfsd update

echo
echo "Current exports:"
showmount -e 127.0.0.1 || true

cat <<'NEXT'

----
On James, run:
  sudo mkdir -p /Volumes/vinny-models
  sudo mount -t nfs -o resvport,ro,nolocks,soft,intr,timeo=50,retrans=3 10.44.0.1:/Users/vinny/models /Volumes/vinny-models
  ls /Volumes/vinny-models

Then on Vinny:
  ff fleet ssh-mesh-check  # confirm james→vinny still reachable
  ls /Volumes/  # NOT expected to show on Vinny; only on James
NEXT
