import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';

/** Self-contained example app: no SDK/CDN, credentials or external dependencies. */
export function counterAppHtml(blockedOrigin: string): string {
  return `<!DOCTYPE html><html lang="ko"><head><title>MCP 카운터</title><style>
  body{margin:0;padding:24px;background:#202020;color:#fcfcfc;font:14px/1.6 system-ui,sans-serif}p{margin:0;color:#b7b7b7}h2{margin:8px 0 18px;font-size:20px}output{display:block;margin-bottom:18px;font-size:40px;font-weight:600;font-variant-numeric:tabular-nums}button{padding:9px 16px;border:0;border-radius:7px;background:#1084fe;color:#fff;font:inherit;cursor:pointer}button:disabled{opacity:.6}button:focus-visible{outline:2px solid white;outline-offset:3px}#status{margin-top:12px;font-size:12px}
  </style></head><body><main><p>MCP 앱 예제</p><h2>승인으로 움직이는 카운터</h2><output id="value" aria-label="카운터 값">0</output><button id="increment" disabled>1 더하기</button><p id="status" role="status">연결 중…</p><pre id="checks" hidden></pre></main><script>
  let sequence=0;const pending=new Map();const send=(method,params)=>new Promise((resolve,reject)=>{const id=++sequence;pending.set(id,{resolve,reject});parent.postMessage({jsonrpc:'2.0',id,method,params},'*');});
  function show(result){const text=(result.content||[]).map(item=>item.text||'').join(' ');const match=/counter: (\\d+)/.exec(text);if(match)document.querySelector('#value').textContent=match[1];}
  window.addEventListener('message',event=>{if(event.source!==parent||!event.data||event.data.jsonrpc!=='2.0')return;const data=event.data;if(data.method==='ui/notifications/tool-result')show(data.params);const waiter=pending.get(data.id);if(!waiter)return;pending.delete(data.id);data.error?waiter.reject(new Error(data.error.message)):waiter.resolve(data.result);});
  (async()=>{await send('ui/initialize',{protocolVersion:'2026-01-26',appInfo:{name:'counter-example',version:'1.0.0'},appCapabilities:{availableDisplayModes:['inline','fullscreen']}});parent.postMessage({jsonrpc:'2.0',method:'ui/notifications/initialized'},'*');document.querySelector('#increment').disabled=false;document.querySelector('#status').textContent='준비됨';})();
  document.querySelector('#increment').onclick=async()=>{const button=document.querySelector('#increment');button.disabled=true;document.querySelector('#status').textContent='승인 기다리는 중';try{const result=await send('tools/call',{name:'counter',arguments:{delta:1}});if(result.isError){document.querySelector('#status').textContent='거절됨';}else{show(result);document.querySelector('#status').textContent='완료';}}catch(error){document.querySelector('#status').textContent=error.message;}finally{button.disabled=false;}};
  ${blockedOrigin ? `(async()=>{const checks={};try{checks.parent=parent.document.title;}catch{checks.parent='blocked';}try{checks.storage=localStorage.length;}catch{checks.storage='blocked';}for(const [name,url] of [['api','/api/state'],['network',${JSON.stringify(`${blockedOrigin}/blocked-fetch`)}]]){try{await fetch(url);checks[name]='allowed';}catch{checks[name]='blocked';}}document.querySelector('#checks').textContent=JSON.stringify(checks);})();` : ''}
  </script>${blockedOrigin ? `<img src="${blockedOrigin}/blocked-image" alt="" hidden>` : ''}</body></html>`;
}

export async function startCounterServer(options: { html?: string; mimeType?: string; resourceUri?: string; probe?: boolean } = {}) {
  let origin = '';
  let count = 0;
  const calls: Array<Record<string, unknown>> = [];
  const blocked: string[] = [];
  const resourceReads: string[] = [];
  const server = createServer(async (request, response) => {
    if (request.url !== '/mcp') { blocked.push(request.url ?? ''); response.end('blocked target'); return; }
    if (request.method !== 'POST') { response.writeHead(405).end(); return; }
    let raw = '';
    for await (const chunk of request) {
      raw += chunk.toString();
      if (Buffer.byteLength(raw) > 32 * 1024) { response.writeHead(413).end(); return; }
    }
    let body: { id?: number | string; method: string; params?: Record<string, unknown> };
    try {
      const parsed: unknown = JSON.parse(raw);
      if (!parsed || typeof parsed !== 'object' || !('method' in parsed) || typeof parsed.method !== 'string') throw new Error('Invalid request');
      body = parsed as typeof body;
    } catch { response.writeHead(400).end(); return; }
    if (body.id === undefined) { response.writeHead(202).end(); return; }
    let result: unknown;
    switch (body.method) {
      case 'initialize': result = { protocolVersion: body.params?.protocolVersion, capabilities: { tools: {}, resources: {} }, serverInfo: { name: 'counter-example', version: '1.0.0' } }; break;
      case 'ping': result = {}; break;
      case 'tools/list': result = { tools: [{ name: 'counter', description: 'Show the approval-controlled counter app and optionally increment by one.', inputSchema: { type: 'object', properties: { delta: { type: 'integer', minimum: 0, maximum: 1 } }, required: ['delta'], additionalProperties: false }, _meta: { ui: { resourceUri: 'ui://counter/app', visibility: ['model', 'app'] } } }] }; break;
      case 'tools/call': {
        calls.push(body.params ?? {});
        const delta = (body.params?.arguments as Record<string, unknown> | undefined)?.delta;
        if (body.params?.name !== 'counter' || (delta !== 0 && delta !== 1)) result = { isError: true, content: [{ type: 'text', text: 'Invalid counter request' }] };
        else { count += delta; result = { content: [{ type: 'text', text: `counter: ${count}` }] }; }
        break;
      }
      case 'resources/read': resourceReads.push(String(body.params?.uri)); result = { contents: [{ uri: options.resourceUri ?? 'ui://counter/app', mimeType: options.mimeType ?? 'text/html;profile=mcp-app', text: options.html ?? counterAppHtml(options.probe ? origin : '') }] }; break;
    }
    response.setHeader('Content-Type', 'application/json');
    response.end(JSON.stringify(result === undefined ? { jsonrpc: '2.0', id: body.id, error: { code: -32601, message: 'Method not found' } } : { jsonrpc: '2.0', id: body.id, result }));
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  return { url: `${origin}/mcp`, calls, blocked, resourceReads, count: () => count, close: () => new Promise<void>(resolve => { server.closeAllConnections(); server.close(() => resolve()); }) };
}

// Run with: npx tsx examples/mcp-counter.ts (disposable, loopback-only example).
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const demo = await startCounterServer();
  console.log(JSON.stringify({ mcpServers: { counter: { url: demo.url } } }, null, 2));
  console.log('Add this MCP server JSON to a bot, enable it, then ask the bot to show the counter app.');
  const stop = async () => { await demo.close(); process.exit(0); };
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
}
