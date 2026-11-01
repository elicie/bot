import type { AppState } from '../shared/types.js';

export function projectComputerPermissions(state: AppState, local: boolean): AppState {
  if (local) return state;
  return {
    ...state,
    conversations: state.conversations.map(conversation => {
      const permission = conversation.run.permission;
      const messages = conversation.messages.map(message => ({ ...message, tools: message.tools.map(tool => tool.localOnly ? { id: tool.id, title: '로컬 브라우저 작업', kind: tool.kind, status: tool.status, localOnly: true } : tool) }));
      if (!permission?.localOnly) return { ...conversation, messages };
      return { ...conversation, messages, run: { ...conversation.run, permission: {
        id: permission.id, title: '로컬 컴퓨터 승인이 필요합니다', kind: 'other',
        description: '서버 컴퓨터의 localhost 연결에서 요청을 확인하고 승인하세요.',
        paths: [], options: [], localOnly: true,
      } } };
    }),
  };
}

export function projectResponse(body: unknown, local: boolean): unknown {
  if (local || !body || typeof body !== 'object') return body;
  if ('conversations' in body && Array.isArray(body.conversations)) return projectComputerPermissions(body as AppState, false);
  if ('state' in body && body.state && typeof body.state === 'object' && 'conversations' in body.state) return { ...body, state: projectComputerPermissions(body.state as AppState, false) };
  return body;
}
