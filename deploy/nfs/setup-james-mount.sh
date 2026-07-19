#!/usr/bin/env bash
# TB.4 setup — James side. Run as: sudo bash deploy/nfs/setup-james-mount.sh
#
# Mounts Taylor's /Users/venkat/models read-only at /Volumes/taylor-models
# over Thunderbolt (10.44.0.1).

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (sudo)" >&2
    exit 1
fi

MOUNT=/Volumes/taylor-models
SERVER=10.44.0.1
EXPORT=/Users/venkat/models

mkdir -p "$MOUNT"

if mount | grep -qF " on $MOUNT "; then
    echo "$MOUNT already mounted — unmount first if you want to re-mount"
    mount | grep " on $MOUNT "
    exit 0
fi

mount -t nfs -o resvport,ro,nolocks,soft,timeo=600,retrans=3 "${SERVER}:${EXPORT}" "$MOUNT"

echo
echo "Mounted:"
mount | grep " on $MOUNT "
echo
echo "Top of mount:"
ls -lah "$MOUNT" | head -10

cat <<'NEXT'

----
Persist across reboot — install the launchd plist:

  sudo install -m 644 deploy/nfs/com.forgefleet.taylor-models-mount.plist \
       /Library/LaunchDaemons/com.forgefleet.taylor-models-mount.plist
  sudo launchctl bootstrap system /Library/LaunchDaemons/com.forgefleet.taylor-models-mount.plist
NEXT
