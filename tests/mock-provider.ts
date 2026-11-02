import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';

export interface MockProviderRequest {
  model: string;
  messages: Array<{ role: string; content?: unknown; tool_calls?: unknown[]; tool_call_id?: string }>;
  tools?: Array<{ type: string; function: { name: string; description?: string; parameters?: Record<string, unknown> } }>;
  stream?: boolean;
}
export interface MockProviderOptions {
  reply?: (request: MockProviderRequest) => MockCompletion | Promise<MockCompletion>;
  status?: number;
  delayMs?: number;
}
export interface MockCompletion {
  text?: string;
  tool?: { name: string; arguments: Record<string, unknown> };
  /** Generated images as `data:` URLs, sent OpenRouter-style in `images` (with no `content` when `text` is unset). */
  images?: string[];
}

export async function startMockProvider(options: MockProviderOptions = {}) {
  const requests: MockProviderRequest[] = [];
  const sockets = new Set<import('node:net').Socket>();
  const server = createServer(async (req: IncomingMessage, res: ServerResponse) => {
    if (req.url?.endsWith('/models')) {
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({ object: 'list', data: [{ id: 'local-test-model', object: 'model', created: 1, owned_by: 'local-fixture' }] }));
      return;
    }
    if (!req.url?.split('?')[0].endsWith('/chat/completions')) {
      res.writeHead(404, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { message: `Unknown fixture route: ${req.url}` } }));
      return;
    }
    let body = '';
    for await (const part of req) body += part.toString();
    const request = JSON.parse(body) as MockProviderRequest;
    requests.push(request);
    if (options.status) {
      res.writeHead(options.status, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { message: 'Local fixture provider error', type: 'fixture_error' } }));
      return;
    }
    const result = await options.reply?.(request) ?? { text: '로컬 OpenAI 호환 서버의 응답입니다.' };
    if (options.delayMs) await new Promise(resolve => setTimeout(resolve, options.delayMs));
    if (res.destroyed) return;
    const id = `chatcmpl-local-${requests.length}`;
    const toolCall = result.tool && { index: 0, id: `call_local_${requests.length}`, type: 'function', function: { name: result.tool.name, arguments: JSON.stringify(result.tool.arguments) } };
    const finishReason = result.tool ? 'tool_calls' : 'stop';
    const usage = { prompt_tokens: 100, completion_tokens: 12, total_tokens: 112 };
    if (request.stream) {
      res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' });
      const chunk = (delta: unknown, reason: string | null = null) => ({ id, object: 'chat.completion.chunk', created: 1, model: request.model, choices: [{ index: 0, delta, finish_reason: reason }] });
      const send = (value: unknown) => res.write(`data: ${JSON.stringify(value)}\n\n`);
      const images = result.images?.map((url, index) => ({ type: 'image_url', image_url: { url }, index }));
      send(chunk({ role: 'assistant', ...(images ? { images } : {}) }));
      if (toolCall) send(chunk({ tool_calls: [toolCall] }));
      else if (!images || result.text !== undefined) send(chunk({ content: result.text ?? '' }));
      send({ ...chunk({}, finishReason), usage });
      res.end('data: [DONE]\n\n');
      return;
    }
    res.writeHead(200, { 'Content-Type': 'application/json' });
    const { index: _index, ...completeTool } = toolCall ?? { index: 0 };
    res.end(JSON.stringify({ id, object: 'chat.completion', created: 1, model: request.model, choices: [{ index: 0, message: { role: 'assistant', content: result.text ?? null, ...(toolCall ? { tool_calls: [completeTool] } : {}), ...(result.images ? { images: result.images.map(url => ({ type: 'image_url', image_url: { url } })) } : {}) }, finish_reason: finishReason }], usage }));
  });
  server.on('connection', socket => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const address = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${address.port}/v1`,
    requests,
    close: async () => {
      for (const socket of sockets) socket.destroy();
      await new Promise<void>(resolve => server.close(() => resolve()));
    },
  };
}
