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

# PGDATA defaults come from the postgres image; we set it in compose to
# /var/lib/postgresql/data/pgdata (subdir of the volume mount, which
# sidesteps lost+found issues and matches the image's default layout).
: "${PGDATA:=/var/lib/postgresql/data/pgdata}"
export PGDATA

# If we're running as root, create the dir, chown to postgres, and
# re-exec ourselves as postgres. This mirrors what docker-entrypoint.sh
# does, but for our custom bootstrap path.
if [ "$(id -u)" = "0" ]; then
  mkdir -p "$PGDATA"
  chown -R postgres:postgres "$(dirname "$PGDATA")"
  chmod 0700 "$PGDATA" || true
  exec gosu postgres "$0" "$@"
fi

if [ ! -s "$PGDATA/PG_VERSION" ]; then
  echo "Replica bootstrap: PGDATA empty — pg_basebackup from ${POSTGRES_PRIMARY_HOST}:${POSTGRES_PRIMARY_PORT}"

  # Make sure target dir is actually empty (pg_basebackup refuses otherwise).
  rm -rf "$PGDATA"/* "$PGDATA"/.[!.]* 2>/dev/null || true

  PGPASSWORD="${POSTGRES_REPLICATION_PASSWORD}" pg_basebackup \
    -h "${POSTGRES_PRIMARY_HOST}" \
    -p "${POSTGRES_PRIMARY_PORT}" \
    -U "${POSTGRES_REPLICATION_USER}" \
    -D "$PGDATA" \
    -Fp -Xs -P -R

  chmod 0700 "$PGDATA"
  echo "Replica bootstrap: pg_basebackup complete."
else
  echo "Replica bootstrap: PGDATA already has PG_VERSION — skipping pg_basebackup."
fi

# Hand off to the real postgres entrypoint. Because PG_VERSION now
# exists, the entrypoint will skip initdb and go straight to `exec postgres`.
exec docker-entrypoint.sh postgres
