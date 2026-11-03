import type { AppState, FileDocument, FileListing } from '../shared/types';

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

export async function request<T>(
  path: string,
  options: { method?: string; body?: unknown; signal?: AbortSignal; headers?: Record<string, string> } = {},
): Promise<T> {
  const method = options.method ?? 'GET';
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      credentials: 'same-origin',
      headers: {
        ...(method === 'GET'
          ? { Accept: 'application/json' }
          : {
              Accept: 'application/json',
              'Content-Type': 'application/json',
              'X-Grok-Bot': '1',
            }),
        ...options.headers,
      },
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: options.signal,
    });
  } catch (error) {
    if (error instanceof Error && error.name === 'AbortError') throw error;
    throw new ApiError('로컬 서버에 연결할 수 없습니다. 서버가 실행 중인지 확인해 주세요.', 0);
  }
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const message =
      body && typeof body === 'object' && 'error' in body && typeof body.error === 'string'
        ? body.error
        : `요청을 처리하지 못했습니다. HTTP ${response.status}`;
    throw new ApiError(message, response.status);
  }
  if (body === null)
    throw new ApiError('서버가 올바른 JSON 응답을 보내지 않았습니다.', response.status);
  return body as T;
}

export const getState = (signal?: AbortSignal) => request<AppState>('/api/state', { signal });
export const listFiles = (path: string, signal?: AbortSignal) =>
  request<FileListing>(`/api/files?path=${encodeURIComponent(path)}`, { signal });
export const readDocument = (path: string, signal?: AbortSignal) =>
  request<FileDocument>(`/api/file?path=${encodeURIComponent(path)}`, { signal });
export const errorText = (error: unknown) =>
  error instanceof Error ? error.message : '알 수 없는 오류가 발생했습니다.';
