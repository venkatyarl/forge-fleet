#!/bin/sh
set -eu

: "${NODE_NAME:?set NODE_NAME}"
: "${NODE_IP:?set NODE_IP}"
: "${TAYLOR_IP:?set TAYLOR_IP}"
: "${MARCUS_IP:?set MARCUS_IP}"
: "${SOPHIE_IP:?set SOPHIE_IP}"
: "${POSTGRES_PASSWORD:?set POSTGRES_PASSWORD}"
: "${POSTGRES_REPLICATION_PASSWORD:?set POSTGRES_REPLICATION_PASSWORD}"

envsubst < /etc/patroni/patroni.yaml.in > /tmp/patroni.yaml
exec patroni /tmp/patroni.yaml
