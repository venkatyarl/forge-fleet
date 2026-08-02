#!/bin/sh
# Replica bootstrap — pg_basebackup from primary on first start,
# then exec the real postgres entrypoint.
#
# This script is the container's entrypoint. It runs BEFORE the normal
# postgres initdb path. If PGDATA is empty, we do pg_basebackup (which
# populates PGDATA + writes standby.signal). After that,
# docker-entrypoint.sh sees PG_VERSION and skips initdb entirely, going
# straight to `exec postgres` as a hot standby.
#
# pg_basebackup -R writes `standby.signal` and sets `primary_conninfo`
# in postgresql.auto.conf, so the replica starts in standby mode.
set -e

: "${POSTGRES_PRIMARY_HOST:?set POSTGRES_PRIMARY_HOST}"
: "${POSTGRES_PRIMARY_PORT:?set POSTGRES_PRIMARY_PORT}"
: "${POSTGRES_REPLICATION_USER:?set POSTGRES_REPLICATION_USER}"
: "${POSTGRES_REPLICATION_PASSWORD:?set POSTGRES_REPLICATION_PASSWORD}"
: "${POSTGRES_REPLICATION_SLOT:?set POSTGRES_REPLICATION_SLOT}"
: "${FORGEFLEET_REPLICA_BACKUP_ID:?set FORGEFLEET_REPLICA_BACKUP_ID}"

# PGDATA defaults come from the postgres image; we set it in compose to
# /var/lib/postgresql/data/pgdata (subdir of the volume mount, which
# sidesteps lost+found issues and matches the image's default layout).
: "${PGDATA:=/var/lib/postgresql/data/pgdata}"
export PGDATA
BOOTSTRAP_MARKER="$(dirname "$PGDATA")/.forgefleet-replica-bootstrap"
BOOTSTRAP_EVIDENCE="${FORGEFLEET_REPLICA_BACKUP_ID}|${POSTGRES_PRIMARY_HOST}|${POSTGRES_REPLICATION_SLOT}"

# If we're running as root, create the dir, chown to postgres, and
# re-exec ourselves as postgres. This mirrors what docker-entrypoint.sh
# does, but for our custom bootstrap path.
if [ "$(id -u)" = "0" ]; then
  mkdir -p "$PGDATA"
  chown -R postgres:postgres "$(dirname "$PGDATA")"
  chmod 0700 "$PGDATA" || true
  exec gosu postgres "$0" "$@"
fi

if [ -s "$PGDATA/PG_VERSION" ]; then
  if [ ! -f "$PGDATA/standby.signal" ]; then
    echo "Replica bootstrap: refusing existing non-standby PGDATA; reseed requires an explicit operator workflow." >&2
    exit 1
  fi
  echo "Replica bootstrap: healthy standby PGDATA already present — skipping pg_basebackup."
elif [ -n "$(find "$PGDATA" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]; then
  if [ ! -f "$BOOTSTRAP_MARKER" ] || [ "$(cat "$BOOTSTRAP_MARKER")" != "$BOOTSTRAP_EVIDENCE" ]; then
    echo "Replica bootstrap: refusing partial/non-empty PGDATA without matching apply/backup evidence; no files were removed." >&2
    exit 1
  fi
  echo "Replica bootstrap: retrying an interrupted, evidenced basebackup."
  find "$PGDATA" -depth -mindepth 1 -delete
else
  echo "Replica bootstrap: PGDATA empty — pg_basebackup from ${POSTGRES_PRIMARY_HOST}:${POSTGRES_PRIMARY_PORT}"

  printf '%s' "$BOOTSTRAP_EVIDENCE" > "$BOOTSTRAP_MARKER"
fi

if [ ! -s "$PGDATA/PG_VERSION" ]; then
  PGPASSWORD="${POSTGRES_REPLICATION_PASSWORD}" pg_basebackup \
    -h "${POSTGRES_PRIMARY_HOST}" \
    -p "${POSTGRES_PRIMARY_PORT}" \
    -U "${POSTGRES_REPLICATION_USER}" \
    -S "${POSTGRES_REPLICATION_SLOT}" \
    -D "$PGDATA" \
    -Fp -Xs -P -R

  chmod 0700 "$PGDATA"
  test -f "$PGDATA/standby.signal"
  grep -q 'primary_conninfo' "$PGDATA/postgresql.auto.conf"
  rm -f "$BOOTSTRAP_MARKER"
  echo "Replica bootstrap: pg_basebackup complete."
fi

# Hand off to the real postgres entrypoint. Because PG_VERSION now
# exists, the entrypoint will skip initdb and go straight to `exec postgres`.
exec docker-entrypoint.sh postgres -c max_connections=500
