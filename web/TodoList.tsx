import { CheckCircle2, ChevronDown, Circle, ListTodo, LoaderCircle, XCircle } from 'lucide-react';
import type { TodoItem } from '../shared/types';
import './todos.css';

const labels = { pending: '대기', in_progress: '진행 중', completed: '완료', cancelled: '취소됨' } as const;

/** Read-only engine progress, above the current answer; disclosure stays put during live updates. */
export function TodoList({ todos, running }: { todos: TodoItem[]; running: boolean }) {
  const completed = todos.filter(todo => todo.status === 'completed').length;
  return (
    <section className="todo-list" aria-label="할 일 목록">
      <details open>
        <summary>
          <ListTodo size={15} aria-hidden="true" />
          <strong>할 일</strong>
          <span className="todo-count" role="status">{completed}/{todos.length} 완료</span>
          <ChevronDown className="todo-chevron" size={14} aria-hidden="true" />
        </summary>
        <ul aria-label="작업 단계">
          {todos.map((todo, index) => {
            const StatusIcon = todo.status === 'completed' ? CheckCircle2 : todo.status === 'cancelled' ? XCircle : todo.status === 'in_progress' ? LoaderCircle : Circle;
            return (
              <li key={index} className={`todo-${todo.status}`} aria-current={todo.status === 'in_progress' ? 'step' : undefined}>
                <StatusIcon size={14} aria-hidden="true" className={running && todo.status === 'in_progress' ? 'spin' : undefined} />
                <span className="todo-content">{todo.content}</span>
                <small>{labels[todo.status]}</small>
              </li>
            );
          })}
        </ul>
      </details>
    </section>
  );
}
