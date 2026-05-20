#!/usr/bin/env bash
# TB.4 setup — Taylor side. Run as: sudo bash deploy/nfs/setup-taylor-export.sh
#
# Exports /Users/venkat/models read-only over Thunderbolt (10.44.0.0/24)
# so James can mount it without duplicating the 50GB model directory.

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (sudo)" >&2
    exit 1
fi

EXPORTS=/etc/exports
LINE='/Users/venkat/models -ro -alldirs -network 10.44.0.0 -mask 255.255.255.0'

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
  sudo mkdir -p /Volumes/taylor-models
  sudo mount -t nfs -o resvport,ro,nolocks 10.44.0.1:/Users/venkat/models /Volumes/taylor-models
  ls /Volumes/taylor-models

Then on Taylor:
  ff fleet ssh-mesh-check  # confirm james→taylor still reachable
  ls /Volumes/  # NOT expected to show on Taylor; only on James
NEXT
