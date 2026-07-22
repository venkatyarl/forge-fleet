// TaskService — dedup-aware creation and parent/child linking for fleet_tasks.
//
// create() calls TaskDedup.findSimilar before inserting: when an active
// duplicate exists it returns the existing task (linking it as a child of the
// requested parent via addParentTask when a parentId was supplied) instead of
// enqueuing a second copy. All steps run inside one transaction, and the
// insert is guarded by the partial unique index on dedup_signature so a
// concurrent creator cannot slip a duplicate row in between check and insert.

import { DbClient, TaskDedup, TaskRow } from "./TaskDedup";

export interface PoolClient extends DbClient {
  release(): void;
}

export interface Pool extends DbClient {
  connect(): Promise<PoolClient>;
}

export interface CreateTaskInput {
  taskType: string;
  summary: string;
  payload?: Record<string, unknown>;
  priority?: number;
  taskClass?: string | null;
  requiresCapability?: string[];
  /** Optional parent task: populates fleet_tasks.parent_task_id. */
  parentId?: string | null;
}

export interface CreateTaskResult {
  task: TaskRow;
  /** True when an existing similar task was returned instead of a new row. */
  deduped: boolean;
}

const TASK_COLUMNS =
  "id, parent_task_id, task_type, summary, payload, priority, status, task_class, dedup_signature, created_at";

export class TaskService {
  constructor(private readonly pool: Pool) {}

  /**
   * Create a task, deduplicating against active similar tasks.
   *
   * Runs in a single transaction: findSimilar (FOR UPDATE) → return the
   * existing task (optionally linked under `parentId`) or insert a new row
   * with dedup_signature / parent_task_id populated.
   */
  async create(input: CreateTaskInput): Promise<CreateTaskResult> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");

      const duplicates = await TaskDedup.findSimilar(client, input);
      if (duplicates.length > 0) {
        let existing = duplicates[0];
        if (input.parentId && input.parentId !== existing.id && !existing.parent_task_id) {
          existing = await this.addParentTask(existing.id, input.parentId, client);
        }
        await client.query("COMMIT");
        return { task: existing, deduped: true };
      }

      const inserted = await this.insertTask(client, input);
      if (inserted) {
        await client.query("COMMIT");
        return { task: inserted, deduped: false };
      }

      // A concurrent transaction inserted the same signature between our
      // findSimilar and insert; ON CONFLICT DO NOTHING returned no row, so
      // surface the winner instead of failing.
      const [winner] = await TaskDedup.findSimilar(client, input);
      if (!winner) {
        throw new Error("task insert conflicted on dedup_signature but no duplicate row is visible");
      }
      await client.query("COMMIT");
      return { task: winner, deduped: true };
    } catch (err) {
      await client.query("ROLLBACK");
      throw err;
    } finally {
      client.release();
    }
  }

  /**
   * Link `taskId` as a child of `parentId` (sets fleet_tasks.parent_task_id).
   * Rejects self-parenting and cycles. Pass `db` to join an open transaction;
   * otherwise the update runs directly on the pool.
   */
  async addParentTask(taskId: string, parentId: string, db?: DbClient): Promise<TaskRow> {
    if (taskId === parentId) {
      throw new Error(`task ${taskId} cannot be its own parent`);
    }
    const conn = db ?? this.pool;

    const cycle = await conn.query(
      `WITH RECURSIVE ancestors(id, parent_task_id) AS (
           SELECT id, parent_task_id FROM fleet_tasks WHERE id = $1
           UNION
           SELECT t.id, t.parent_task_id
             FROM fleet_tasks t
             JOIN ancestors a ON t.id = a.parent_task_id
       )
       SELECT 1 FROM ancestors WHERE id = $2 LIMIT 1`,
      [parentId, taskId],
    );
    if (cycle.rows.length > 0) {
      throw new Error(`linking task ${taskId} under ${parentId} would create a cycle`);
    }

    const { rows } = await conn.query(
      `UPDATE fleet_tasks
          SET parent_task_id = $2
        WHERE id = $1
          AND EXISTS (SELECT 1 FROM fleet_tasks WHERE id = $2)
        RETURNING ${TASK_COLUMNS}`,
      [taskId, parentId],
    );
    if (rows.length === 0) {
      throw new Error(`cannot link task ${taskId} under ${parentId}: task or parent not found`);
    }
    return rows[0] as TaskRow;
  }

  /**
   * Insert a fleet_tasks row with the schema fields populated
   * (dedup_signature, parent_task_id, task_class, requires_capability).
   * Returns null when the partial unique index on dedup_signature rejects
   * the row (a concurrent duplicate won).
   */
  private async insertTask(db: DbClient, input: CreateTaskInput): Promise<TaskRow | null> {
    const signature = TaskDedup.computeSignature(input);
    const { rows } = await db.query(
      `INSERT INTO fleet_tasks
           (task_type, summary, payload, priority, requires_capability,
            status, task_class, dedup_signature, parent_task_id)
       VALUES ($1, $2, $3::jsonb, $4, $5::jsonb, 'pending', $6, $7, $8)
       ON CONFLICT (dedup_signature) WHERE dedup_signature IS NOT NULL DO NOTHING
       RETURNING ${TASK_COLUMNS}`,
      [
        input.taskType,
        input.summary,
        JSON.stringify(input.payload ?? {}),
        input.priority ?? 50,
        JSON.stringify(input.requiresCapability ?? []),
        input.taskClass ?? null,
        signature,
        input.parentId ?? null,
      ],
    );
    return (rows[0] as TaskRow | undefined) ?? null;
  }
}
