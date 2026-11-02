import assert from 'node:assert/strict';
import { test } from 'node:test';
import { TodoTracker } from '../server/todos.js';
import type { TodoItem } from '../shared/types.js';

const todo = (content: string, status: TodoItem['status'] = 'pending'): TodoItem => ({ content, status });
const input = (tracker: TodoTracker, call: string, ids: string[]) => tracker.project({ toolCallId: call, rawInput: { variant: 'TodoWrite', todos: ids.map(id => ({ id })) } });
const output = (tracker: TodoTracker, call: string, state: Record<string, TodoItem>, ordered = Object.values(state)) => tracker.project({ toolCallId: call, rawOutput: { type: 'Todo', TodosUpdated: { state: { todos: state }, todos: ordered } } });

test('native plan projection uses successful canonical state and keeps only this turn IDs', () => {
  const tracker = new TodoTracker();
  input(tracker, 'first', ['new']);
  assert.equal(tracker.native, false);
  assert.deepEqual(output(tracker, 'first', { old: todo('Previous task'), new: todo('New task', 'in_progress') }), [todo('New task', 'in_progress')]);
  assert.equal(tracker.native, true);
  input(tracker, 'partial', ['new', 'next']);
  const merged = { old: todo('Previous task'), new: todo('New task', 'completed'), next: todo('Next task') };
  assert.deepEqual(output(tracker, 'partial', merged), [merged.new, merged.next]);
  assert.deepEqual(output(tracker, 'partial', merged), [merged.new, merged.next]);
  input(tracker, 'bad', ['rejected']);
  assert.equal(tracker.project({ toolCallId: 'bad', rawOutput: { type: 'Todo', DuplicateId: 'Duplicate' } }), undefined);
  input(tracker, 'after-bad', ['next']);
  assert.deepEqual(output(tracker, 'after-bad', { ...merged, rejected: todo('Do not admit') }), [merged.new, merged.next]);
  input(tracker, 'replace', ['only']);
  assert.deepEqual(output(tracker, 'replace', { only: todo('Replacement', 'cancelled') }), [todo('Replacement', 'cancelled')]);
  input(tracker, 'clear', []);
  assert.deepEqual(output(tracker, 'clear', {}), []);
});

test('numeric IDs and identical values preserve canonical array order and current counts', () => {
  const tracker = new TodoTracker();
  input(tracker, 'first', ['10', '2', 'duplicate']);
  const state = { old: todo('Same'), '2': todo('Second'), '10': todo('First'), duplicate: todo('Same') };
  assert.deepEqual(output(tracker, 'first', state, [state['10'], state.old, state['2'], state.duplicate]), [todo('First'), todo('Same'), todo('Second')]);
});

test('unknown or malformed output does not disable ordinary Plan fallback', () => {
  const tracker = new TodoTracker();
  assert.equal(output(tracker, 'unknown', { one: todo('Unrequested') }), undefined);
  input(tracker, 'one', ['one']);
  assert.equal(tracker.project({ toolCallId: 'one', rawOutput: { type: 'Todo', TodosUpdated: { state: { todos: { one: todo('One') } }, todos: [] } } }), undefined);
  assert.equal(tracker.native, false);
  assert.deepEqual(output(tracker, 'one', { one: todo('One') }), [todo('One')]);
});
