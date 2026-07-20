# PostgreSQL HA

`docker-compose.postgres-ha.yml` runs one etcd, Patroni/Postgres, HAProxy, and
pgcat member on each of Taylor, Marcus, and Sophie. Run it on all three hosts;
three containers on one host do not provide HA.

```bash
cd deploy
cp postgres-ha.env.example .env.postgres-ha
# Set this host's NODE_NAME/NODE_IP and the same three IPs/secrets everywhere.
docker compose --env-file .env.postgres-ha -f docker-compose.postgres-ha.yml config
docker compose --env-file .env.postgres-ha -f docker-compose.postgres-ha.yml up -d --build
```

Applications use the stable local pgcat DSN and opt into the existing cached
DSN fallback:

```toml
[database]
url = "postgresql://forgefleet:REDACTED@127.0.0.1:56432/forgefleet"
dsn_failover = true
```

HAProxy checks Patroni's `/primary` and `/replica?lag=1MB` endpoints, so pgcat's
primary and replica entries remain correct after promotion. Writes and unknown
statements go to the current primary; eligible reads are round-robin split over
healthy replicas. Migrations, session advisory locks, temp tables, and workflows
that require read-after-write consistency must connect directly to HAProxy's
primary port `55434`, not the transaction-pooled pgcat port `56432`.

Verify and operate the cluster with:

```bash
curl -fsS http://127.0.0.1:58008/cluster
docker compose --env-file .env.postgres-ha -f docker-compose.postgres-ha.yml exec patroni \
  patronictl -c /tmp/patroni.yaml list
docker compose --env-file .env.postgres-ha -f docker-compose.postgres-ha.yml exec patroni \
  patronictl -c /tmp/patroni.yaml switchover
```

To test automatic failover, stop only the current leader's Patroni container and
confirm another member reports HTTP 200 from `/primary`, then write through the
unchanged `:56432` DSN. Restart the old member and confirm it rejoins as a replica.
One failed etcd member retains quorum; never force promotion after losing two.

The existing WAL/base-backup process remains required. Patroni/etcd coordinates
roles but is not a backup. Roll back by stopping this stack and restoring the
previous standalone Postgres DSN; do not delete either Patroni or etcd volumes
until the old primary and backups have been validated.
