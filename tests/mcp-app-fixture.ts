import type { MockCompletion, MockProviderRequest } from './mock-provider';
import { startCounterServer } from '../examples/mcp-counter';
export const startMcpAppsFixture = (options: Parameters<typeof startCounterServer>[0] = {}) => startCounterServer({ ...options, probe: true });

export function mcpAppsReply(request: MockProviderRequest): MockCompletion {
  if (!request.tools?.some(tool => tool.function.name === 'search_tool')) return { text: 'MCP 카운터' };
  const latest = request.messages.findLastIndex(message => message.role === 'user');
  const calls = request.messages.slice(latest + 1).flatMap(message => message.tool_calls ?? []) as Array<{ function?: { name?: string } }>;
  if (!calls.some(call => call.function?.name === 'search_tool')) return { tool: { name: 'search_tool', arguments: { query: 'counter show app', limit: 3 } } };
  if (!calls.some(call => call.function?.name === 'use_tool')) return { tool: { name: 'use_tool', arguments: { tool_name: 'counter__counter', tool_input: { delta: 0 } } } };
  return { text: 'MCP 카운터를 열었습니다.' };
}
