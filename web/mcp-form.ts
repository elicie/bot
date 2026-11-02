import type { PublicMcpServer, PublicSecret, SecretInput } from '../shared/vault';

export interface McpCredential {
  id: string;
  name: string;
  source: 'vault' | 'new';
  secret: string;
  value: string;
  prefix: string;
}
export interface McpDraft {
  name: string;
  transport: 'stdio' | 'http';
  command: string;
  args: string[];
  url: string;
  env: McpCredential[];
  headers: McpCredential[];
}
export const emptyMcpDraft = (): McpDraft => ({ name: '', transport: 'stdio', command: '', args: [], url: '', env: [], headers: [] });
const control = /\p{Cc}/u;
const secretName = /^[A-Z][A-Z0-9_]{0,63}$/;
export const credentialRows = (draft: McpDraft) => draft.transport === 'stdio' ? draft.env : draft.headers;

/** A bot-scoped secret shadows the same global name, just as it does in the runtime. */
export function availableMcpSecrets(secrets: PublicSecret[], botId: string) {
  const available = new Map<string, PublicSecret>();
  for (const secret of secrets.filter(secret => !secret.botId)) available.set(secret.name, secret);
  for (const secret of secrets.filter(secret => secret.botId === botId)) available.set(secret.name, secret);
  return [...available.values()].sort((a, b) => a.name.localeCompare(b.name));
}

/** Client feedback mirrors the vault's limits; the storage/probe APIs remain authoritative. */
export function mcpDraftProblems(draft: McpDraft, servers: PublicMcpServer[], secrets: PublicSecret[]) {
  const problems: Record<string, string> = {};
  const name = draft.name.trim();
  if (!/^[a-z0-9][a-z0-9_-]{0,31}$/.test(name) || name.includes('__') || name.startsWith('workspace')) {
    problems.name = '소문자·숫자로 시작하는 32자 이하 이름을 입력하세요. -, _는 쓸 수 있지만 __와 workspace로 시작하는 이름은 쓸 수 없어요.';
  } else if (servers.some(server => server.name === name)) problems.name = '같은 이름의 서버가 있어요. 다른 이름을 입력하세요.';
  if (servers.length >= 10) problems.name = 'Bot 하나에 서버는 10개까지 추가할 수 있어요. JSON 편집기에서 기존 서버를 정리해 주세요.';
  if (draft.transport === 'stdio') {
    const command = draft.command.trim();
    if (!command || command.length > 1024 || control.test(command) || command.includes('${secret:') || (command.includes('/') && !command.startsWith('/'))) {
      problems.command = 'PATH에서 찾을 명령 이름이나 절대 경로를 입력하세요. 시크릿 참조·제어 문자 없이 1,024자 이하여야 해요.';
    }
    if (draft.args.length > 64 || draft.args.some(arg => arg.length > 4096 || arg.includes('\0') || arg.includes('${secret:'))) {
      problems.args = '인자는 64개까지, 각각 4,096자까지 입력할 수 있어요. 시크릿은 환경 변수에서 선택하세요.';
    }
  } else {
    try {
      const url = new URL(draft.url.trim());
      if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password || url.hash || draft.url.trim().includes('#') || draft.url.trim().includes('${secret:') || draft.url.trim().length > 2048) throw Error();
    } catch { problems.url = '인증 정보와 # 없이 http:// 또는 https://로 시작하는 2,048자 이하 URL을 입력하세요.'; }
  }
  const rows = credentialRows(draft);
  const names = new Set<string>();
  const newSecrets = new Set<string>();
  for (const row of rows) {
    const key = `pair-${row.id}`;
    const invalid = (field: 'name' | 'secret' | 'value' | 'prefix', message: string) => {
      if (problems[key]) return;
      problems[key] = message;
      problems[`${key}-${field}`] = message;
    };
    const name = row.name.trim();
    const normalized = draft.transport === 'http' ? name.toLowerCase() : name;
    const validName = draft.transport === 'http' ? /^[A-Za-z0-9!#$%&'*+.^_`|~-]{1,128}$/ : /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
    if (!validName.test(name)) invalid('name', draft.transport === 'http' ? '올바른 HTTP 헤더 이름을 입력하세요.' : '영문·_로 시작하는 환경 변수 이름을 입력하세요.');
    else if (names.has(normalized)) invalid('name', '같은 이름의 항목이 있어요. HTTP 헤더는 대소문자를 구분하지 않아요.');
    names.add(normalized);
    if (row.source === 'vault') {
      if (!secrets.some(secret => secret.name === row.secret)) invalid('secret', '보관함 시크릿을 선택하세요.');
    } else {
      if (!secretName.test(row.secret)) invalid('secret', '새 시크릿 이름은 대문자 영문으로 시작하고 A-Z, 0-9, _를 사용해 64자 이내로 입력하세요.');
      else if (secrets.some(secret => secret.name === row.secret) || newSecrets.has(row.secret)) invalid('secret', '같은 이름의 시크릿이 있어요. 보관함에서 선택하거나 새 이름을 입력하세요.');
      const value = row.value.trim();
      if (value.length < 8 || value.length > 8192 || control.test(value)) invalid('value', '시크릿 값은 줄바꿈·제어 문자 없이 8~8,192자로 입력하세요.');
      newSecrets.add(row.secret);
    }
    if (draft.transport === 'http' && (control.test(row.prefix) || row.prefix.includes('${secret:') || row.prefix.length + row.secret.length + 10 > 8192)) invalid('prefix', '접두사는 시크릿 참조·제어 문자 없이 입력하세요. 전체 헤더는 8,192자 이하여야 해요.');
  }
  if (rows.length > 64) problems.pairs = '환경 변수 또는 헤더는 64개까지 입력할 수 있어요.';
  return problems;
}

export function buildMcpDraft(draft: McpDraft, botId: string) {
  const rows = credentialRows(draft);
  const pairs = Object.fromEntries(rows.map(row => [row.name.trim(), `${draft.transport === 'http' ? row.prefix : ''}\${secret:${row.secret}}`]));
  const config = draft.transport === 'stdio'
    ? { command: draft.command.trim(), args: draft.args, env: pairs }
    : { url: draft.url.trim(), headers: pairs };
  const secrets: SecretInput[] = rows.filter(row => row.source === 'new').map(row => ({ name: row.secret, value: row.value.trim(), botId }));
  return { name: draft.name.trim(), config, secrets, json: JSON.stringify({ mcpServers: { [draft.name.trim()]: config } }) };
}

/** Append to a fresh canonical GET, never a stale form-opening snapshot. */
export function appendMcpDraft(json: string, draft: ReturnType<typeof buildMcpDraft>) {
  const document = JSON.parse(json) as { mcpServers: Record<string, unknown> };
  if (!document.mcpServers || typeof document.mcpServers !== 'object' || Array.isArray(document.mcpServers)) throw Error('최신 MCP 서버 JSON을 읽을 수 없어요. 창을 다시 열어 주세요.');
  if (Object.hasOwn(document.mcpServers, draft.name)) throw Error('같은 이름의 서버가 이미 저장됐어요. 다른 이름을 입력하세요.');
  if (Object.keys(document.mcpServers).length >= 10) throw Error('Bot 하나에 서버는 10개까지 추가할 수 있어요.');
  const merged = JSON.stringify({ mcpServers: { ...document.mcpServers, [draft.name]: draft.config } });
  if (new TextEncoder().encode(merged).length > 64 * 1024) throw Error('MCP 서버 JSON은 64 KiB 이하여야 해요. 인자나 환경 변수·헤더를 줄여 주세요.');
  return merged;
}
