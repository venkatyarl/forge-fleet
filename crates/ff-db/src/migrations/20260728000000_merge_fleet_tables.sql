-- Merge the physical-computer registry and worker configuration into one
-- canonical base table. Renaming `computers` preserves its UUID identity and
-- all foreign keys that already reference it.
DO $$
DECLARE
    fk RECORD;
    dependent_view RECORD;
BEGIN
    -- A completed migration is safe to execute again.
    IF EXISTS (
           SELECT 1 FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = 'public' AND c.relname = 'fleet_nodes' AND c.relkind = 'r'
       )
       AND NOT EXISTS (
           SELECT 1 FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = 'public' AND c.relname IN ('computers', 'fleet_workers')
             AND c.relkind = 'r'
       )
    THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relname = 'computers' AND c.relkind = 'r'
    ) OR NOT EXISTS (
        SELECT 1 FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relname = 'fleet_workers' AND c.relkind = 'r'
    ) OR EXISTS (
        SELECT 1 FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'public' AND c.relname = 'fleet_nodes'
    )
    THEN
        RAISE EXCEPTION
            'cannot merge fleet tables: expected computers and fleet_workers base tables and no fleet_nodes relation';
    END IF;

    ALTER TABLE computers RENAME TO fleet_nodes;
    ALTER TABLE fleet_workers RENAME TO fleet_workers_legacy;

    ALTER TABLE fleet_nodes
        ADD COLUMN ip TEXT,
        ADD COLUMN ram_gb INTEGER,
        ADD COLUMN worker_cpu_cores INTEGER,
        ADD COLUMN os TEXT,
        ADD COLUMN role TEXT,
        ADD COLUMN election_priority INTEGER,
        ADD COLUMN hardware TEXT,
        ADD COLUMN alt_ips JSONB,
        ADD COLUMN capabilities JSONB,
        ADD COLUMN preferences JSONB,
        ADD COLUMN resources JSONB,
        ADD COLUMN worker_status TEXT,
        ADD COLUMN registered_at TIMESTAMPTZ,
        ADD COLUMN updated_at TIMESTAMPTZ,
        ADD COLUMN runtime TEXT,
        ADD COLUMN models_dir TEXT,
        ADD COLUMN disk_quota_pct INTEGER,
        ADD COLUMN sub_agent_count INTEGER,
        ADD COLUMN gh_account TEXT,
        ADD COLUMN tooling JSONB;

    -- Preserve every physical node, even if it never had a worker row.
    UPDATE fleet_nodes
       SET ip = primary_ip,
           ram_gb = COALESCE(total_ram_gb, 0),
           worker_cpu_cores = COALESCE(cpu_cores, 0),
           os = os_family,
           role = 'worker',
           election_priority = 50,
           hardware = '',
           alt_ips = all_ips,
           capabilities = '{}'::jsonb,
           preferences = '{}'::jsonb,
           resources = '{}'::jsonb,
           worker_status = status,
           registered_at = enrolled_at,
           updated_at = COALESCE(last_seen_at, enrolled_at),
           runtime = 'unknown',
           models_dir = '~/models',
           disk_quota_pct = 80,
           sub_agent_count = 1,
           tooling = '{}'::jsonb;

    -- Worker configuration is authoritative for the worker-specific fields.
    UPDATE fleet_nodes n
       SET ip = w.ip,
           ssh_user = w.ssh_user,
           ram_gb = w.ram_gb,
           worker_cpu_cores = w.cpu_cores,
           os = w.os,
           role = w.role,
           election_priority = w.election_priority,
           hardware = w.hardware,
           alt_ips = w.alt_ips,
           capabilities = w.capabilities,
           preferences = w.preferences,
           resources = w.resources,
           worker_status = w.status,
           registered_at = w.registered_at,
           updated_at = w.updated_at,
           runtime = w.runtime,
           models_dir = w.models_dir,
           disk_quota_pct = w.disk_quota_pct,
           sub_agent_count = w.sub_agent_count,
           gh_account = w.gh_account,
           tooling = w.tooling
      FROM fleet_workers_legacy w
     WHERE n.name = w.name;

    -- Retain workers that did not yet have a physical-computer record.
    INSERT INTO fleet_nodes (
        name, primary_ip, all_ips, os_family, cpu_cores, total_ram_gb,
        ssh_user, status, ip, ram_gb, worker_cpu_cores, os, role,
        election_priority, hardware, alt_ips, capabilities, preferences,
        resources, worker_status, enrolled_at, registered_at, updated_at,
        runtime, models_dir, disk_quota_pct, sub_agent_count, gh_account, tooling
    )
    SELECT
        w.name, w.ip, w.alt_ips, COALESCE(NULLIF(w.os, ''), 'unknown'),
        w.cpu_cores, w.ram_gb, w.ssh_user, w.status, w.ip, w.ram_gb,
        w.cpu_cores, w.os, w.role, w.election_priority, w.hardware,
        w.alt_ips, w.capabilities, w.preferences, w.resources, w.status,
        w.registered_at, w.registered_at, w.updated_at, w.runtime,
        w.models_dir, w.disk_quota_pct, w.sub_agent_count, w.gh_account, w.tooling
      FROM fleet_workers_legacy w
     WHERE NOT EXISTS (SELECT 1 FROM fleet_nodes n WHERE n.name = w.name);

    ALTER TABLE fleet_nodes
        ALTER COLUMN ip SET NOT NULL,
        ALTER COLUMN ram_gb SET NOT NULL,
        ALTER COLUMN ram_gb SET DEFAULT 0,
        ALTER COLUMN worker_cpu_cores SET NOT NULL,
        ALTER COLUMN worker_cpu_cores SET DEFAULT 0,
        ALTER COLUMN os SET NOT NULL,
        ALTER COLUMN os SET DEFAULT '',
        ALTER COLUMN role SET NOT NULL,
        ALTER COLUMN role SET DEFAULT 'worker',
        ALTER COLUMN election_priority SET NOT NULL,
        ALTER COLUMN election_priority SET DEFAULT 50,
        ALTER COLUMN hardware SET NOT NULL,
        ALTER COLUMN hardware SET DEFAULT '',
        ALTER COLUMN alt_ips SET NOT NULL,
        ALTER COLUMN alt_ips SET DEFAULT '[]'::jsonb,
        ALTER COLUMN capabilities SET NOT NULL,
        ALTER COLUMN capabilities SET DEFAULT '{}'::jsonb,
        ALTER COLUMN preferences SET NOT NULL,
        ALTER COLUMN preferences SET DEFAULT '{}'::jsonb,
        ALTER COLUMN resources SET NOT NULL,
        ALTER COLUMN resources SET DEFAULT '{}'::jsonb,
        ALTER COLUMN worker_status SET NOT NULL,
        ALTER COLUMN worker_status SET DEFAULT 'online',
        ALTER COLUMN registered_at SET NOT NULL,
        ALTER COLUMN registered_at SET DEFAULT NOW(),
        ALTER COLUMN updated_at SET NOT NULL,
        ALTER COLUMN updated_at SET DEFAULT NOW(),
        ALTER COLUMN runtime SET NOT NULL,
        ALTER COLUMN runtime SET DEFAULT 'unknown',
        ALTER COLUMN models_dir SET NOT NULL,
        ALTER COLUMN models_dir SET DEFAULT '~/models',
        ALTER COLUMN disk_quota_pct SET NOT NULL,
        ALTER COLUMN disk_quota_pct SET DEFAULT 80,
        ALTER COLUMN sub_agent_count SET NOT NULL,
        ALTER COLUMN sub_agent_count SET DEFAULT 1,
        ALTER COLUMN tooling SET NOT NULL,
        ALTER COLUMN tooling SET DEFAULT '{}'::jsonb;

    -- Repoint name-based worker foreign keys, preserving their complete
    -- constraint definitions (delete action, deferrability, and validation).
    FOR fk IN
        SELECT conrelid::regclass AS table_name,
               conname,
               replace(
                   pg_get_constraintdef(oid),
                   'REFERENCES fleet_workers_legacy(name)',
                   'REFERENCES fleet_nodes(name)'
               ) AS definition
          FROM pg_constraint
         WHERE contype = 'f'
           AND confrelid = 'public.fleet_workers_legacy'::regclass
    LOOP
        EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', fk.table_name, fk.conname);
        EXECUTE format(
            'ALTER TABLE %s ADD CONSTRAINT %I %s',
            fk.table_name, fk.conname, fk.definition
        );
    END LOOP;

    -- Compatibility projections keep existing binaries usable while new code
    -- moves to fleet_nodes. They are views, not duplicate storage.
    CREATE VIEW computers AS SELECT
        id, name, primary_ip, all_ips, hostname, mac_addresses, os_family,
        os_distribution, os_version, os_version_latest, os_upgrade_available,
        os_version_checked_at, cpu_cores, total_ram_gb, total_disk_gb, has_gpu,
        gpu_kind, gpu_count, gpu_model, gpu_vram_gb, gpu_total_vram_gb,
        cuda_version, metal_version, rocm_version, gpu_driver_version, ssh_user,
        ssh_port, ssh_public_key, enrolled_at, last_seen_at, offline_since,
        status_changed_at, status, metadata, network_scope, source_tree_path,
        build_archs, connectivity_mode, election_eligibility, reservation_state,
        reserved_reason, reserved_at, reservation_owner, reservation_expires_at,
        dispatch_tick_at
    FROM fleet_nodes;

    CREATE VIEW fleet_workers AS SELECT
        name, ip, ssh_user, ram_gb, worker_cpu_cores AS cpu_cores, os, role,
        election_priority, hardware, alt_ips, capabilities, preferences,
        resources, worker_status AS status, registered_at, updated_at, runtime,
        models_dir, disk_quota_pct, sub_agent_count, gh_account, tooling
    FROM fleet_nodes;

    -- Views followed the old table across its rename. Recreate them against
    -- the compatibility view so the legacy base table can be dropped safely.
    FOR dependent_view IN
        SELECT DISTINCT c.oid::regclass AS view_name, pg_get_viewdef(c.oid, true) AS definition
          FROM pg_depend d
          JOIN pg_rewrite r ON r.oid = d.objid
          JOIN pg_class c ON c.oid = r.ev_class
         WHERE d.refobjid = 'public.fleet_workers_legacy'::regclass
           AND c.relkind = 'v'
           AND c.oid <> d.refobjid
    LOOP
        EXECUTE format(
            'CREATE OR REPLACE VIEW %s AS %s',
            dependent_view.view_name,
            replace(dependent_view.definition, 'fleet_workers_legacy', 'fleet_workers')
        );
    END LOOP;

    DROP TABLE fleet_workers_legacy RESTRICT;
END $$;

ANALYZE fleet_nodes;
