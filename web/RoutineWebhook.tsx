import { useEffect, useState } from 'react';
import { Check, Clipboard, KeyRound, RefreshCw, Trash2, Webhook } from 'lucide-react';
import type { Routine, RoutineWebhookKey } from '../shared/types';
import { errorText, request } from './api';
import { ErrorNotice } from './ui';
import './webhooks.css';

const MAX_PAYLOAD_KIB = 16;

async function copyText(value: string) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value);
    return;
  }
  const field = document.createElement('textarea');
  field.value = value;
  field.readOnly = true;
  field.className = 'settings-clipboard-field';
  document.body.append(field);
  try {
    field.select();
    if (!document.execCommand('copy')) throw new Error('Clipboard unavailable');
  } finally {
    field.remove();
  }
}

function CopyField({ label, value, description }: { label: string; value: string; description: string }) {
  const [copied, setCopied] = useState<'idle' | 'copied' | 'error'>('idle');
  useEffect(() => setCopied('idle'), [value]);
  return (
    <div className="webhook-field">
      <div className="webhook-field-heading">
        <span>{label}</span>
        <button
          type="button"
          className="small-button"
          aria-label={`${label} 복사`}
          onClick={() => copyText(value).then(() => setCopied('copied'), () => setCopied('error'))}
        >
          {copied === 'copied' ? <Check size={13} /> : <Clipboard size={13} />}
          {copied === 'copied' ? '복사됨' : '복사'}
        </button>
      </div>
      <code aria-label={label}>{value}</code>
      <small>{description}</small>
      {copied === 'error' && <small className="danger-text">클립보드에 접근할 수 없어요. 위 내용을 직접 선택해 복사해 주세요.</small>}
    </div>
  );
}

/** "웹훅이 발생할 때": per-routine webhook key and endpoint. The key is shown only once. */
export function RoutineWebhook({ routine, disabled }: { routine: Routine; disabled: boolean }) {
  const [revealed, setRevealed] = useState<{ routineId: string; key: string } | null>(null);
  const [pending, setPending] = useState<'create' | 'revoke' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const key = revealed?.routineId === routine.id ? revealed.key : null;
  const webhook = routine.webhook;
  useEffect(() => setError(null), [routine.id]);

  const endpoint = `${window.location.origin}/hooks/routines/${routine.id}`;
  const shownKey = key ?? `gbw_…${webhook?.keyHint ?? ''}`;
  const header = `Authorization: Bearer ${key ?? '<key>'}`;
  const example = [
    `curl -X POST '${endpoint}'`,
    `  -H '${header}'`,
    `  -H 'Content-Type: application/json'`,
    `  --data '{"event":"example"}'`,
  ].join(' \\\n');

  const create = async () => {
    if ((webhook || key) && !window.confirm('새 키로 교체할까요? 지금 키를 쓰는 서비스는 더 이상 이 루틴을 실행할 수 없어요.')) return;
    setPending('create');
    setError(null);
    try {
      const result = await request<RoutineWebhookKey>(`/api/routines/${encodeURIComponent(routine.id)}/webhook`, { method: 'POST', body: {} });
      setRevealed({ routineId: routine.id, key: result.key });
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setPending(null);
    }
  };
  const revoke = async () => {
    if (!window.confirm('웹훅 키를 삭제할까요? 이 키로 보내는 요청은 더 이상 루틴을 실행하지 않아요.')) return;
    setPending('revoke');
    setError(null);
    try {
      await request(`/api/routines/${encodeURIComponent(routine.id)}/webhook`, { method: 'DELETE', body: {} });
      setRevealed(null);
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setPending(null);
    }
  };

  return (
    <section className="routine-webhook" aria-label="웹훅">
      <h3><Webhook size={15} />웹훅이 발생할 때</h3>
      <p className="routine-form-note">
        외부 서비스가 POST 요청을 보내면 이 루틴을 한 번 실행해요. 요청 본문(최대 {MAX_PAYLOAD_KIB} KiB 텍스트나 JSON)은
        신뢰하지 않는 데이터로 구분해 지시 뒤에 붙이고, 웹훅으로 시작한 실행에서는 권한 규칙과 관계없이 모든 작업을 먼저 물어봐요.
      </p>
      {webhook || key ? (
        <div className="webhook-fields">
          <CopyField label="POST 대상" value={endpoint} description="외부 서비스가 요청을 보내는 주소예요. 요청마다 이 루틴이 한 번 실행돼요." />
          {key ? (
            <CopyField
              label="웹훅 키"
              value={key}
              description="요청이 정말 보낸 쪽에서 왔는지 Grok Bot이 확인할 수 있도록 보내는 쪽이 함께 넣는 비밀 키예요. 이 루틴을 호출하는 서비스에만 공유해 주세요. 키는 지금 한 번만 표시돼요."
            />
          ) : webhook && (
            <div className="webhook-field">
              <div className="webhook-field-heading"><span>웹훅 키</span></div>
              <code aria-label="웹훅 키">{shownKey}</code>
              <small>
                서버에는 키의 해시만 저장돼서 다시 표시할 수 없어요. 키를 잃어버렸다면 새 키로 교체해 주세요.
                {' '}만든 시각: <time dateTime={webhook.createdAt}>{new Date(webhook.createdAt).toLocaleString()}</time>
              </small>
            </div>
          )}
          <CopyField
            label="헤더"
            value={header}
            description="발신자는 이 키로 인증해요. 보내는 서비스의 각 요청에 이 헤더 줄을 그대로 붙여 넣어 주세요."
          />
          <CopyField label="요청 예시" value={example} description="같은 루틴이 대기 중이거나 실행 중이면 409로 거절돼요. 다른 기기에서 보내려면 서버를 HOST=0.0.0.0과 GROK_BOT_ALLOWED_HOSTS로 실행해야 해요." />
        </div>
      ) : (
        <p className="routine-form-note">웹훅 키가 없어요. 키를 만들면 이 루틴을 외부에서 실행할 수 있는 주소와 키가 표시돼요.</p>
      )}
      {error && <ErrorNotice onDismiss={() => setError(null)}>{error}</ErrorNotice>}
      <div className="routine-actions">
        <button type="button" className="small-button" disabled={disabled || pending !== null} onClick={() => void create()}>
          {webhook || key ? <RefreshCw size={13} /> : <KeyRound size={13} />}
          {pending === 'create' ? '만드는 중…' : webhook || key ? '새 키로 교체' : '웹훅 키 만들기'}
        </button>
        {(webhook || key) && (
          <button type="button" className="small-button danger-text" disabled={disabled || pending !== null} onClick={() => void revoke()}>
            <Trash2 size={13} />{pending === 'revoke' ? '삭제 중…' : '웹훅 키 삭제'}
          </button>
        )}
      </div>
    </section>
  );
}
