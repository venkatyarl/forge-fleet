# Infrastructure Runbook

## Supabase read replica for `traveling-node`

This replica is only for read traffic from `traveling-node`. The Priya database
remains authoritative, and all writes, migrations, leases, and operator actions
must continue to use `FORGEFLEET_DATABASE_URL`. Do not promote the replica URL to
the fleet-wide database URL.

The connection mapping is defined in `config/database.yml`:

- `FORGEFLEET_DATABASE_URL` selects the Priya primary.
- `SUPABASE_REPLICA_URL` selects the Supabase replica with a four-connection
  pool, off-site timeouts, and read-only transactions.

### Provision and verify the subscription

1. In the Supabase project dashboard, confirm the organization is on Pro, Team,
   or Enterprise. Read replicas also require AWS, Postgres 15 or newer, at least
   Small compute, and no legacy logical backups.
2. Open **Database Replication** and create the replica in the region nearest
   `traveling-node`.
3. Wait until the replica is healthy. In its **Replica Information**, confirm
   replication lag is present and stable rather than continuously increasing.
4. Open **Connect**, select the read replica in the **Source** dropdown, and copy
   that replica's database connection string. Do not use the Primary or API load
   balancer connection string.
5. Check the organization invoice/usage page for **Replica Compute Hours** (and
   IPv4 usage if enabled). This confirms that the paid replica is attached to
   the intended project, not merely that a connection string was issued.

### Apply credentials on `traveling-node`

Store the complete replica connection string in the node's existing secret
environment file; never commit it to this repository or paste it into
`fleet.toml`:

```bash
SUPABASE_REPLICA_URL='postgresql://USER:PASSWORD@REPLICA_HOST:5432/postgres?sslmode=require'
```

Ensure only the service account can read the secret file, then restart the
`traveling-node` service so it receives the new environment:

```bash
chmod 600 /path/to/forgefleet.env
sudo systemctl restart forgefleetd
sudo systemctl show forgefleetd --property=EnvironmentFiles
```

For a launchd installation, set `SUPABASE_REPLICA_URL` in the service's existing
secret-loading mechanism and reload that service instead. Do not print the
environment or connection string into logs.

Verify the credential against the replica:

```bash
psql "$SUPABASE_REPLICA_URL" -v ON_ERROR_STOP=1 -c \
  "select current_database(), pg_is_in_recovery(), current_setting('transaction_read_only');"
```

The check must connect successfully, report `pg_is_in_recovery = on`, and report
`transaction_read_only = on`. Then run a representative `SELECT` used by
`traveling-node`. If the replica is unavailable or too stale, fail the read or
explicitly route it to the primary according to the caller's policy; never send
a write to the replica.

### Monitor replica and IPv4 status

- In **Database Replication > Replica Information**, monitor replication lag,
  CPU, memory, disk, and connection count. Investigate sustained or increasing
  lag before trusting time-sensitive reads.
- In **Project Settings > Add-ons**, confirm the IPv4 add-on shows active when
  `traveling-node` requires a direct connection from an IPv4-only network.
  Supabase assigns an IPv4 address to every database, including each read
  replica, so each replica adds IPv4 cost.
- From `traveling-node`, confirm the replica hostname resolves to IPv4:

  ```bash
  getent ahostsv4 REPLICA_HOST
  ```

- For automated status checks, use a scoped Supabase access token and project
  reference:

  ```bash
  curl --fail --silent --show-error \
    -H "Authorization: Bearer $SUPABASE_ACCESS_TOKEN" \
    "https://api.supabase.com/v1/projects/$SUPABASE_PROJECT_REF/billing/addons"
  ```

  Alert if the expected `ipv4` add-on is absent or inactive, DNS stops returning
  an A record, the replica cannot be reached, or replication lag continually
  rises. Keep the access token in the node's secret store.

Enabling the IPv4 add-on does not restart the database, but DNS propagation
affects new connections. Removing or toggling it can briefly interrupt direct
connections and may assign a different address, so treat add-on changes as
planned maintenance and re-run the DNS and SQL checks afterward.

Supabase references:

- <https://supabase.com/docs/guides/platform/read-replicas>
- <https://supabase.com/docs/guides/platform/read-replicas/getting-started>
- <https://supabase.com/docs/guides/platform/ipv4-address>
