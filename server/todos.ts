import type { TodoItem, TodoStatus } from '../shared/types.js';

const object = (value: unknown): Record<string, unknown> | undefined =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
const item = (value: unknown): TodoItem | undefined => {
  const todo = object(value);
  return todo && typeof todo.content === 'string' && ['pending', 'in_progress', 'completed', 'cancelled'].includes(String(todo.status))
    ? { content: todo.content, status: todo.status as TodoStatus } : undefined;
};
const signature = (todo: TodoItem) => JSON.stringify([todo.content, todo.status]);

/** Project the pinned native tool's successful full state without importing prior-turn IDs.
 * The engine owns partial merges. Its raw output supplies IDs; ordinary ACP Plans do not.
 */
export class TodoTracker {
  private calls = new Map<string, string[]>();
  private active = new Set<string>();
  native = false;

  project(update: { toolCallId: string; rawInput?: unknown; rawOutput?: unknown }): TodoItem[] | undefined {
    const input = object(update.rawInput);
    if (input?.variant === 'TodoWrite' && Array.isArray(input.todos)) {
      const ids = input.todos.map(todo => object(todo)?.id);
      if (ids.every(id => typeof id === 'string')) this.calls.set(update.toolCallId, ids as string[]);
    }
    const output = object(update.rawOutput);
    const success = output?.type === 'Todo' ? object(output.TodosUpdated) : undefined;
    const state = object(object(success?.state)?.todos);
    const ids = this.calls.get(update.toolCallId);
    if (!ids || !success || !state || !Array.isArray(success.todos)) return;
    const ordered = success.todos.map(item);
    if (ordered.some(todo => !todo)) return;
    const active = new Set([...this.active, ...ids].filter(id => Object.hasOwn(state, id)));
    const counts = new Map<string, number>();
    for (const id of active) {
      const todo = item(state[id]);
      if (!todo) return;
      const key = signature(todo);
      counts.set(key, (counts.get(key) ?? 0) + 1);
    }
    // Match by value/count, preserving the authoritative array order even for numeric IDs.
    // Identical old/current values are visually indistinguishable; retain only current counts.
    const todos = (ordered as TodoItem[]).filter(todo => {
      const key = signature(todo);
      const count = counts.get(key) ?? 0;
      if (!count) return false;
      counts.set(key, count - 1);
      return true;
    });
    if ([...counts.values()].some(count => count !== 0)) return;
    this.active = active;
    this.native = true;
    return todos;
  }
}
