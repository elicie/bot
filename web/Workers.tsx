import { CheckCircle2, ChevronDown, Circle, GitBranch, LoaderCircle, XCircle } from 'lucide-react';
import type { AppState, WorkerBatch, WorkerTask } from '../shared/types';
import './workers.css';

const labels: Record<WorkerTask['status'], string> = {
  queued: '대기', running: '실행 중', completed: '완료', cancelled: '취소됨', error: '실패', interrupted: '중단됨',
};

/** The parent owns progress; hidden child conversations never become navigation targets. */
export function WorkerProgress({ batch }: { batch: WorkerBatch }) {
  const finished = batch.workers.filter(worker => worker.status !== 'queued' && worker.status !== 'running').length;
  const stopping = batch.workers.some(worker => worker.status === 'running') || batch.report?.status === 'running';
  const cancelledStatus = batch.report?.status === 'completed'
    ? '중단 요청과 동시에 결과 정리가 완료되었습니다.'
    : stopping ? '하위 작업 중단을 요청했습니다. 실행 중인 작업을 멈추는 중입니다.'
      : '하위 작업을 중단했습니다. 남은 작업과 자동 결과 정리를 실행하지 않습니다.';
  return <section className="worker-progress" aria-label="하위 작업 진행 상황">
    <details open>
      <summary>
        <GitBranch size={15} aria-hidden="true" />
        <strong>하위 작업</strong>
        <span className="worker-count" role="status">{finished}/{batch.workers.length} 종료</span>
        <ChevronDown className="worker-chevron" size={14} aria-hidden="true" />
      </summary>
      <p className="worker-limits">같은 Bot이 한 번에 1개씩 실행 · 최대 3개 · 작업당 최대 5분</p>
      <ol aria-label="위임한 작업">
        {batch.workers.map(worker => {
          const StatusIcon = worker.status === 'completed' ? CheckCircle2 : worker.status === 'running' ? LoaderCircle : worker.status === 'queued' ? Circle : XCircle;
          const longTask = worker.task.length > 180 || worker.task.split('\n').length > 3;
          const preview = worker.task.replace(/\s+/g, ' ').trim();
          const previewText = Array.from(preview).slice(0, 160).join('');
          return <li key={worker.id} className={`worker-${worker.status}`} aria-current={worker.status === 'running' ? 'step' : undefined}>
            <div className="worker-task-heading">
              <StatusIcon size={14} aria-hidden="true" className={worker.status === 'running' ? 'spin' : undefined} />
              <div className="worker-task">{longTask ? <details className="worker-task-detail">
                <summary><span className="worker-task-preview">{previewText.length < preview.length ? `${previewText}…` : previewText}</span><span className="worker-task-disclosure">작업 전문 보기<ChevronDown size={12} aria-hidden="true" /></span></summary>
                <p tabIndex={0}>{worker.task}</p>
              </details> : worker.task}</div>
              <small>{labels[worker.status]}</small>
            </div>
            {worker.error && <p className="worker-error">{worker.error}</p>}
            {worker.answer && <details className="worker-result">
              <summary>결과 보기<ChevronDown size={12} aria-hidden="true" /></summary>
              <small className="worker-result-limit">최종 답변 · 최대 48 KiB</small>
              <p tabIndex={0}>{worker.answer}</p>
            </details>}
          </li>;
        })}
      </ol>
      {batch.cancelled ? <p className="worker-report-status" role="status">{cancelledStatus}</p>
        : batch.report?.status === 'queued' ? <p className="worker-report-status" role="status">결과를 모두 모았습니다. 이어서 한 번에 정리합니다.</p>
          : batch.report?.status === 'running' ? <p className="worker-report-status" role="status">모은 결과를 정리하는 중입니다.</p>
            : batch.report?.error ? <p className="worker-error worker-report-status">결과 정리를 완료하지 못했습니다: {batch.report.error}</p>
              : null}
    </details>
  </section>;
}

export function WorkerReportMarker({ messageId, data }: { messageId: string; data?: AppState }) {
  const batch = data?.workerBatches?.find(batch => batch.reportMessageId === messageId);
  if (!batch) return null;
  return <div className="bot-report-marker worker-report-marker" role="note" aria-label="하위 작업 결과를 모은 답변">
    <GitBranch size={13} aria-hidden="true" />
    <span>{batch.workers.length}개 하위 작업의 결과를 모아 이어서 답함</span>
  </div>;
}
