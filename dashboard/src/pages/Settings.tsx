import { type ReactNode, useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { getJson } from '../lib/api'

type RuntimeConfig = {
  loaded?: boolean
  config_path?: string | null
  fleet_name?: string
  api_port?: number
  heartbeat_interval_secs?: number
  heartbeat_timeout_secs?: number
  nodes_configured?: number
  models_configured?: number
  loops?: Record<string, boolean>
}

type TokenState = {
  configured_inline?: boolean
  env_var?: string
  resolved?: boolean
  source?: string
  editable_in_dashboard?: boolean
}

type EnrollmentSettings = {
  default_role?: string | null
  allowed_roles?: string[]
  token?: TokenState
}

type TelegramSettings = {
  configured?: boolean
  enabled?: boolean
  allowed_chat_ids?: number
  polling_interval_secs?: number
  polling_timeout_secs?: number
  token?: TokenState
  runtime?: {
    running?: boolean
    started_at?: string | null
    last_update_id?: number | null
    last_message_at?: string | null
    last_error?: string | null
  }
}

type DatabaseSettings = {
  active_mode?: string
  status?: string
  sqlite?: {
    path?: string
    file_exists?: boolean
    wal_mode?: boolean
    max_connections?: number
    config_kv_entries?: number
  }
  postgres?: {
    config_kv_entries?: number
  }
  error?: string
}

type ExternalDbSettings = {
  url_present?: boolean
  url_scheme?: string | null
  host?: string | null
  port?: number | null
  name?: string | null
}

type SettingsGuidance = {
  secrets_editable_in_dashboard?: boolean
  workflow?: string[]
  activation?: string[]
  onboarding?: string[]
  troubleshooting?: string[]
}

type SettingsResponse = {
  status?: string
  runtime_config?: RuntimeConfig
  enrollment?: EnrollmentSettings
  telegram?: TelegramSettings
  database?: DatabaseSettings
  configured_external_database?: ExternalDbSettings
  guidance?: SettingsGuidance
}

export function Settings() {
  const [data, setData] = useState<SettingsResponse | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setLoading(true)
      setError(null)
      const payload = await getJson<SettingsResponse>('/api/settings/runtime')
      setData(payload)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load settings state')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const guidance = data?.guidance

  const tokenWorkflow = useMemo(
    () =>
      guidance?.workflow ?? [
        'Edit enrollment + Telegram secrets in fleet.toml or environment variables on the host machine.',
        'Restart forgefleet after changing token values so runtime picks up new secrets.',
        'Refresh this page and verify token source, resolved state, and runtime status are healthy.',
      ],
    [guidance],
  )

  const activationChecklist = useMemo(
    () =>
      guidance?.activation ?? [
        'Confirm fleet.toml is loaded from the expected config path.',
        'Verify enrollment token resolves and default role/allowed roles match your security policy.',
        'Verify Telegram transport is enabled and token resolves from the intended source.',
        'Confirm operational store reports status=ready for the active database backend.',
      ],
    [guidance],
  )

  const onboardingChecklist = useMemo(
    () =>
      guidance?.onboarding ?? [
        'Use the Config Editor to review non-secret runtime config before enrolling new nodes.',
        'Enroll workers using the same shared enrollment token source reported on this page.',
        'Validate heartbeat/transport flow by confirming Telegram runtime and fleet heartbeat loops are active.',
        'After first node joins, confirm runtime registry entries increase and mode/state stay healthy.',
      ],
    [guidance],
  )

  const troubleshooting = useMemo(
    () =>
      guidance?.troubleshooting ?? [
        'If enrollment token source is missing, set FORGEFLEET_ENROLLMENT_TOKEN or update fleet.toml then restart.',
        'If Telegram runtime is not running, check bot token source + allowed chat IDs and restart gateway.',
        'If database status is degraded/error, verify active backend connectivity and inspect gateway logs.',
      ],
    [guidance],
  )

  const runtime = data?.runtime_config
  const enrollment = data?.enrollment
  const telegram = data?.telegram
  const database = data?.database
  const externalDb = data?.configured_external_database

  const enrollmentHealthy = Boolean(enrollment?.token?.resolved)
  const telegramTokenHealthy = Boolean(telegram?.token?.resolved)
  const telegramRuntimeHealthy = Boolean(telegram?.enabled && telegram?.runtime?.running)
  const telegramHealthy = telegramTokenHealthy && telegramRuntimeHealthy
  const databaseHealthy = database?.status === 'ready'

  const activeDbMode = (database?.active_mode ?? 'unknown').toLowerCase()
  const sqliteActive = Boolean(database?.sqlite) || activeDbMode.includes('sqlite')
  const postgresActive = Boolean(database?.postgres) || activeDbMode.includes('postgres')

  const secretsEditable = guidance?.secrets_editable_in_dashboard ?? false

  return (
    <section className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Settings & Runtime Config</h1>
          <p className="mt-1 text-sm text-slate-400">
            Runtime visibility for enrollment token state, Telegram transport health, and database backend mode.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Link
            to="/config"
            className="rounded-lg border border-slate-700 bg-slate-900 px-3 py-2 text-sm text-slate-200 transition hover:border-slate-500"
          >
            Open Config Editor
          </Link>
          <button
            onClick={() => void load()}
            disabled={loading}
            className="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:cursor-not-allowed disabled:opacity-50"
            type="button"
          >
            {loading ? 'Refreshing…' : '↻ Refresh'}
          </button>
        </div>
      </div>

      {error ? <Info text={`Error: ${error}`} danger /> : null}

      <Info
        text={
          secretsEditable
            ? 'Secret values are editable in this dashboard environment. Keep browser/session access tightly controlled.'
            : 'Secrets are intentionally read-only in dashboard UI. Token values are never shown in browser payloads.'
        }
        accent="rounded-xl border border-amber-500/30 bg-amber-500/10 px-4 py-3 text-sm text-amber-200"
      />

      <div className="grid gap-4 xl:grid-cols-2">
        <Card title="Runtime Config Visibility" status={<StatePill ok={Boolean(runtime?.loaded)} okLabel="loaded" badLabel="not loaded" />}>
          <Row label="Config loaded" value={runtime?.loaded ? 'yes' : 'no'} />
          <Row label="Config path" value={runtime?.config_path ?? 'unreported'} mono />
          <Row label="Fleet name" value={runtime?.fleet_name ?? 'unreported'} />
          <Row label="API port" value={runtime?.api_port ?? 'unreported'} />
          <Row
            label="Heartbeat interval"
            value={runtime?.heartbeat_interval_secs != null ? `${runtime.heartbeat_interval_secs}s` : 'unreported'}
          />
          <Row
            label="Heartbeat timeout"
            value={runtime?.heartbeat_timeout_secs != null ? `${runtime.heartbeat_timeout_secs}s` : 'unreported'}
          />
          <Row label="Nodes configured" value={runtime?.nodes_configured ?? 0} />
          <Row label="Models configured" value={runtime?.models_configured ?? 0} />
          <div className="mt-3 rounded-lg border border-slate-800 bg-slate-950/70 p-3 text-xs text-slate-300">
            <p className="mb-2 font-medium text-slate-200">Background loops</p>
            {Object.keys(runtime?.loops ?? {}).length > 0 ? (
              <div className="grid grid-cols-2 gap-2">
                {Object.entries(runtime?.loops ?? {}).map(([name, enabled]) => (
                  <div key={name} className="flex items-center justify-between rounded border border-slate-800 px-2 py-1">
                    <span className="text-slate-400">{name}</span>
                    <StatePill ok={Boolean(enabled)} okLabel="enabled" badLabel="disabled" />
                  </div>
                ))}
              </div>
            ) : (
              <p className="text-slate-400">Loop state unavailable until runtime config is loaded.</p>
            )}
          </div>
        </Card>

        <Card
          title="Enrollment Token State"
          status={<StatePill ok={enrollmentHealthy} okLabel="healthy" badLabel="action needed" />}
        >
          <Row label="Default role" value={enrollment?.default_role ?? 'worker'} />
          <Row
            label="Allowed roles"
            value={
              enrollment?.allowed_roles && enrollment.allowed_roles.length > 0
                ? enrollment.allowed_roles.join(', ')
                : 'all roles'
            }
          />
          <Row label="Token source" value={enrollment?.token?.source ?? 'missing'} />
          <Row label="Inline token configured" value={enrollment?.token?.configured_inline ? 'yes' : 'no'} />
          <Row label="Env var" value={enrollment?.token?.env_var ?? 'FORGEFLEET_ENROLLMENT_TOKEN'} mono />
          <Row label="Runtime resolved" value={enrollment?.token?.resolved ? 'yes' : 'no'} />
          <Row label="Editable in dashboard" value={enrollment?.token?.editable_in_dashboard ? 'yes' : 'no'} />
        </Card>

        <Card
          title="Telegram Token + Transport State"
          status={<StatePill ok={telegramHealthy} okLabel="healthy" badLabel="action needed" />}
        >
          <Row label="Config section present" value={telegram?.configured ? 'yes' : 'no'} />
          <Row label="Transport enabled" value={telegram?.enabled ? 'yes' : 'no'} />
          <Row label="Token source" value={telegram?.token?.source ?? 'missing'} />
          <Row label="Inline token configured" value={telegram?.token?.configured_inline ? 'yes' : 'no'} />
          <Row label="Env var" value={telegram?.token?.env_var ?? 'FORGEFLEET_TELEGRAM_BOT_TOKEN'} mono />
          <Row label="Runtime resolved" value={telegram?.token?.resolved ? 'yes' : 'no'} />
          <Row label="Allowed chat IDs" value={telegram?.allowed_chat_ids ?? 0} />
          <Row
            label="Polling"
            value={
              telegram?.polling_interval_secs != null && telegram?.polling_timeout_secs != null
                ? `${telegram.polling_interval_secs}s / timeout ${telegram.polling_timeout_secs}s`
                : 'unreported'
            }
          />
          <Row label="Runtime running" value={telegram?.runtime?.running ? 'yes' : 'no'} />
          <Row label="Last update ID" value={telegram?.runtime?.last_update_id ?? 'unreported'} />
          <Row label="Last message at" value={telegram?.runtime?.last_message_at ?? 'unreported'} />
          <Row label="Last error" value={telegram?.runtime?.last_error ?? 'none'} />
        </Card>

        <Card
          title="Active Database Mode / Status"
          status={<StatePill ok={databaseHealthy} okLabel="ready" badLabel={database?.status ?? 'unavailable'} />}
        >
          <Row label="Active mode" value={database?.active_mode ?? 'unreported'} />
          <Row label="Runtime status" value={database?.status ?? 'unreported'} />

          {sqliteActive ? (
            <>
              <Row label="SQLite path" value={database?.sqlite?.path ?? 'unreported'} mono />
              <Row label="SQLite file exists" value={database?.sqlite?.file_exists ? 'yes' : 'no'} />
              <Row label="SQLite WAL mode" value={database?.sqlite?.wal_mode ? 'enabled' : 'disabled'} />
              <Row label="SQLite max connections" value={database?.sqlite?.max_connections ?? 'unreported'} />
              <Row label="SQLite config_kv entries" value={database?.sqlite?.config_kv_entries ?? 'unreported'} />
            </>
          ) : (
            <Info text="SQLite backend is not active in the current runtime mode." subtle />
          )}

          {postgresActive ? (
            <>
              <Row label="Postgres config_kv entries" value={database?.postgres?.config_kv_entries ?? 'unreported'} />
              <Row label="External DB URL configured" value={externalDb?.url_present ? 'yes' : 'no'} />
              <Row label="External DB scheme" value={externalDb?.url_scheme ?? 'unreported'} />
              <Row label="External DB host" value={externalDb?.host ?? 'unreported'} />
              <Row label="External DB port" value={externalDb?.port ?? 'unreported'} />
              <Row label="External DB name" value={externalDb?.name ?? 'unreported'} />
            </>
          ) : (
            <Info text="Postgres backend is not active in the current runtime mode." subtle />
          )}

          {database?.error ? <Info text={`DB error: ${database.error}`} danger /> : null}
        </Card>
      </div>

      <div className="grid gap-4 xl:grid-cols-3">
        <Card title="Activation Checklist">
          <ol className="list-decimal space-y-2 pl-5 text-sm text-slate-300">
            {activationChecklist.map((step, index) => (
              <li key={`${index}-${step}`}>{step}</li>
            ))}
          </ol>
        </Card>

        <Card title="Onboarding Runbook">
          <ol className="list-decimal space-y-2 pl-5 text-sm text-slate-300">
            {onboardingChecklist.map((step, index) => (
              <li key={`${index}-${step}`}>{step}</li>
            ))}
          </ol>
        </Card>

        <Card title="Token & Config Recovery Quick-Fixes">
          <ul className="list-disc space-y-2 pl-5 text-sm text-slate-300">
            {troubleshooting.map((step, index) => (
              <li key={`${index}-${step}`}>{step}</li>
            ))}
          </ul>
        </Card>
      </div>

      <Card title="Token Change Workflow">
        <ol className="list-decimal space-y-2 pl-5 text-sm text-slate-300">
          {tokenWorkflow.map((step, index) => (
            <li key={`${index}-${step}`}>{step}</li>
          ))}
        </ol>
        <p className="mt-3 text-xs text-slate-500">
          This keeps secret values out of browser logs/history while still exposing accurate runtime state.
        </p>
      </Card>
    </section>
  )
}

function Card({ title, status, children }: { title: string; status?: ReactNode; children: ReactNode }) {
  return (
    <article className="rounded-xl border border-slate-800 bg-slate-900/60 p-4">
      <div className="mb-3 flex items-center justify-between gap-3">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-300">{title}</h2>
        {status}
      </div>
      <div className="space-y-2">{children}</div>
    </article>
  )
}

function Row({
  label,
  value,
  mono = false,
}: {
  label: string
  value: string | number | null | undefined
  mono?: boolean
}) {
  return (
    <div className="flex items-start justify-between gap-3 border-b border-slate-800/60 pb-2 text-sm last:border-b-0 last:pb-0">
      <dt className="text-slate-500">{label}</dt>
      <dd className={`text-right text-slate-200 ${mono ? 'font-mono text-xs md:text-sm' : ''}`}>
        {value == null || value === '' ? 'unreported' : String(value)}
      </dd>
    </div>
  )
}

function StatePill({ ok, okLabel, badLabel }: { ok: boolean; okLabel: string; badLabel: string }) {
  return (
    <span
      className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
        ok ? 'bg-emerald-500/20 text-emerald-300' : 'bg-amber-500/20 text-amber-300'
      }`}
    >
      {ok ? okLabel : badLabel}
    </span>
  )
}

function Info({
  text,
  danger = false,
  subtle = false,
  accent,
}: {
  text: string
  danger?: boolean
  subtle?: boolean
  accent?: string
}) {
  return (
    <div
      className={
        accent ??
        `rounded-xl border px-4 py-3 text-sm ${
          danger
            ? 'border-rose-500/30 bg-rose-500/10 text-rose-200'
            : subtle
              ? 'border-slate-800 bg-slate-950/60 text-slate-400'
              : 'border-slate-800 bg-slate-900/50 text-slate-300'
        }`
      }
    >
      {text}
    </div>
  )
}
