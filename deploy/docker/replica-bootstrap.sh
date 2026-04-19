#!/bin/sh
# Replica bootstrap — pg_basebackup from primary on first start.
#
# Mounted into the official postgres:16-alpine image at
#   /docker-entrypoint-initdb.d/00-replica-bootstrap.sh
# The entrypoint runs any file in that directory *before* initializing
# the cluster, so we can pg_basebackup over an empty PGDATA and then
# the normal "start postgres" path resumes with standby.signal present.
#
# pg_basebackup -R creates `standby.signal` and writes
# `primary_conninfo` into postgresql.auto.conf, so the replica starts
# in standby mode automatically.
set -e

if [ -s "$PGDATA/PG_VERSION" ]; then
  echo "Replica bootstrap: PGDATA already initialized, skipping pg_basebackup."
  exit 0
fi

echo "Replica bootstrap: pg_basebackup from ${POSTGRES_PRIMARY_HOST}:${POSTGRES_PRIMARY_PORT}"
PGPASSWORD="${POSTGRES_REPLICATION_PASSWORD}" pg_basebackup \
  -h "${POSTGRES_PRIMARY_HOST}" \
  -p "${POSTGRES_PRIMARY_PORT}" \
  -U "${POSTGRES_REPLICATION_USER}" \
  -D "$PGDATA" \
  -Fp -Xs -P -R

chown -R postgres:postgres "$PGDATA"
echo "Replica bootstrap: complete."
