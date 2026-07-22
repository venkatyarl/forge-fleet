import { test } from 'node:test';
import assert from 'node:assert/strict';

import { DbFetcher, DbClient, DbQueryResult, MAX_CONTEXT_ROWS } from './db_fetcher';
import {
  CHARS_PER_TOKEN,
  DEFAULT_MAX_CONTEXT_CHARS,
  maxCharsForTokens,
  truncateContext,
} from './context_truncator';

class FakeDbClient implements DbClient {
  lastSql = '';
  lastValues: unknown[] | undefined;

  constructor(private readonly rows: Array<Record<string, unknown>>) {}

  async query(text: string, values?: unknown[]): Promise<DbQueryResult> {
    this.lastSql = text;
    this.lastValues = values;
    return { rows: this.rows };
  }
}

test('fetchContext reads rows from a pg-style { rows } result', async () => {
  const db = new FakeDbClient([{ content: 'alpha' }, { content: 'beta' }]);
  const fetched = await new DbFetcher(db).fetchContext({ name: 'notes', query: 'alp' });
  assert.deepEqual(fetched, ['alpha', 'beta']);
});

test('fetchContext selects only content, never embedding/metadata', async () => {
  const db = new FakeDbClient([]);
  await new DbFetcher(db).fetchContext({ id: 'abc', query: 'x' });
  assert.match(db.lastSql, /SELECT c\.content/);
  assert.doesNotMatch(db.lastSql, /embedding|metadata|\*/);
  assert.deepEqual(db.lastValues, ['abc', null, 'x', MAX_CONTEXT_ROWS]);
});

test('fetchContext truncates results to the maxTokens budget', async () => {
  const db = new FakeDbClient([{ content: 'a'.repeat(10) }, { content: 'b'.repeat(10) }]);
  // 3 tokens * 4 chars/token = 12 chars: first chunk whole, second sliced to 2.
  const fetched = await new DbFetcher(db).fetchContext({ id: 'abc', query: 'a', maxTokens: 3 });
  assert.deepEqual(fetched, ['a'.repeat(10), 'bb']);
});

test('fetchContext skips rows with missing or non-string content', async () => {
  const db = new FakeDbClient([{ content: 'ok' }, { content: null }, { other: 1 }]);
  const fetched = await new DbFetcher(db).fetchContext({ id: 'abc', query: 'x' });
  assert.deepEqual(fetched, ['ok']);
});

test('fetchContext rejects invalid queries without touching the db', async () => {
  const db = new FakeDbClient([{ content: 'never' }]);
  await assert.rejects(
    () => new DbFetcher(db).fetchContext({ query: '' }),
    /invalid context query/,
  );
  assert.equal(db.lastSql, '');
});

test('truncateContext keeps whole chunks and slices the boundary chunk', () => {
  assert.deepEqual(truncateContext(['abcd', 'efgh', 'ijkl'], 6), ['abcd', 'ef']);
  assert.deepEqual(truncateContext(['abcd'], 0), []);
  assert.deepEqual(truncateContext([], 10), []);
});

test('maxCharsForTokens falls back to the default budget', () => {
  assert.equal(maxCharsForTokens(undefined), DEFAULT_MAX_CONTEXT_CHARS);
  assert.equal(maxCharsForTokens(-5), DEFAULT_MAX_CONTEXT_CHARS);
  assert.equal(maxCharsForTokens(100), 100 * CHARS_PER_TOKEN);
});
