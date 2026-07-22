// Initial implementation for TaskDedup.ts
//
// Instruction hashing + similarity check for the fleet_tasks dispatch queue.
// A task's dedup_signature is a stable hash of its (task_type, summary, payload);
// fleet_tasks enforces uniqueness via a partial unique index
// (idx_fleet_tasks_dedup_signature, WHERE dedup_signature IS NOT NULL).
// Terminal rows release their signature, so only active rows can match.

import { createHash } from "node:crypto";

/** Minimal structural subset of a pg client — a real `pg.Pool`/`pg.PoolClient` satisfies it. */
export interface DbClient {
  query(text: string, values?: unknown[]): Promise<{ rows: any[]; rowCount?: number | null }>;
}

/** A row of the fleet_tasks table (subset of columns this service touches). */
export interface TaskRow {
  id: string;
  parent_task_id: string | null;
  task_type: string;
  summary: string;
  payload: Record<string, unknown>;
  priority: number;
  status: string;
  task_class: string | null;
  dedup_signature: string | null;
  created_at: string;
}

export interface TaskFingerprint {
  taskType: string;
  summary: string;
  payload?: Record<string, unknown>;
}

/** Statuses whose rows no longer hold their dedup signature. */
export const TERMINAL_STATUSES = ["completed", "failed", "cancelled"] as const;

const TASK_COLUMNS =
  "id, parent_task_id, task_type, summary, payload, priority, status, task_class, dedup_signature, created_at";

function canonicalize(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>)
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([k, v]) => [k, canonicalize(v)]),
    );
  }
  return value;
}

export const TaskDedup = {
  /** Stable sha256 signature over the normalized instruction. */
  computeSignature(fp: TaskFingerprint): string {
    const material = JSON.stringify({
      task_type: fp.taskType.trim().toLowerCase(),
      summary: fp.summary.trim().toLowerCase().replace(/\s+/g, " "),
      payload: canonicalize(fp.payload ?? {}),
    });
    return createHash("sha256").update(material).digest("hex");
  },

  /**
   * Find active (non-terminal) fleet_tasks similar to the given instruction:
   * exact dedup_signature match first, then normalized (task_type, summary) match.
   * Rows are locked FOR UPDATE so the caller's transaction can act on them safely.
   */
  async findSimilar(db: DbClient, fp: TaskFingerprint): Promise<TaskRow[]> {
    const signature = TaskDedup.computeSignature(fp);
    const { rows } = await db.query(
      `SELECT ${TASK_COLUMNS}
               FROM fleet_tasks
              WHERE status NOT IN ('completed', 'failed', 'cancelled')
                AND (dedup_signature = $1
                     OR (lower(task_type) = lower($2)
                         AND lower(btrim(summary)) = lower(btrim($3))))
              ORDER BY (dedup_signature = $1) DESC, created_at ASC
              FOR UPDATE`,
      [signature, fp.taskType, fp.summary],
    );
    return rows as TaskRow[];
  },
};
