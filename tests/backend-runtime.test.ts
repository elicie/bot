import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { WorkspaceFiles } from '../server/files.js';
import type { AddressInfo } from 'node:net';
import { createBackend } from '../server/app.js';
import type { AppState, Conversation } from '../shared/types.js';

const fakeAgent = `#!/usr/bin/env node
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { writeFile } from 'node:fs/promises';
if (process.argv.includes('--version')) { console.log('grok 1.0.62 (test fixture)'); process.exit(0); }
const send = value => process.stdout.write(JSON.stringify({jsonrpc:'2.0',...value})+'\\n');
let sessionId;
let broker;
let permissionResolve;
let loadCount = 0;
const pending = new Map();
const response = (id,result) => send({id,result});
const update = update => send({method:'session/update',params:{sessionId,update}});
async function rpc(method,params) {
 const id = randomUUID();
 const result = new Promise(resolve=>pending.set(id,resolve));
 send({id,method,params});return result;
}
async function mcp(name,args) {
 const result = await fetch(broker.url,{method:'POST',headers:{'Content-Type':'application/json',Authorization:broker.headers[0].value},body:JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/call',params:{name,arguments:args}})});
 return (await result.json()).result;
}
createInterface({input:process.stdin}).on('line',async line=>{
 const message = JSON.parse(line);
 if(message.result && pending.has(message.id)){pending.get(message.id)(message.result);pending.delete(message.id);return;}
 const {id,method,params}=message;
 if(method==='initialize'&&process.argv.includes('--stall-initialize'))return;
 if(method==='initialize'){response(id,{protocolVersion:1,agentCapabilities:{loadSession:true,mcpCapabilities:{http:true}},agentInfo:{name:'fake',version:'1'}});return;}
 if(method==='session/new'||method==='session/load'){
  sessionId=params.sessionId||randomUUID();broker=params.mcpServers[0];loadCount=method==='session/load'?1:0;
  if(method==='session/load')update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'HISTORICAL REPLAY MUST NOT DUPLICATE'}});
  response(id,{sessionId});return;
 }
 if(method==='session/cancel'){return;}
 if(method==='session/prompt'){
  const prompt=params.prompt[0].text;
  if(prompt==='finish-during-preimage'){
   process.once('SIGUSR1',()=>response(id,{stopReason:'end_turn'}));
   await writeFile(process.argv[process.argv.indexOf('--fixture-root')+1]+'/broker.json',JSON.stringify(broker));
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'PID '+process.pid}});
   void mcp('write_file',{path:'notes.txt',content:'Must not be saved after the turn ends'});
   return;
  }
  if(prompt==='silent-hang'){setInterval(()=>{},1000);return;}
  if(prompt==='active-long'){
   for(let i=0;i<6;i++){
    update({sessionUpdate:'agent_thought_chunk',content:{type:'text',text:'private activity'}});
    await new Promise(resolve=>setTimeout(resolve,60));
   }
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'Active task completed.'}});
   response(id,{stopReason:'end_turn'});return;
  }
  if(prompt==='hang'||prompt==='chatty-hang'){
   process.on('SIGTERM',()=>{});
   const child=spawn(process.execPath,['-e','process.on("SIGTERM",()=>{});setInterval(()=>{},1000)'],{stdio:'ignore'});
   await new Promise(resolve=>setTimeout(resolve,50));
   const pidFileIndex=process.argv.indexOf('--pid-file');
   if(pidFileIndex!==-1)await writeFile(process.argv[pidFileIndex+1],JSON.stringify([process.pid,child.pid]));
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'PID '+process.pid+' CHILD '+child.pid}});
   if(prompt==='chatty-hang')setInterval(()=>update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'.'}}),10);
   setInterval(()=>{},1000);return;
  }
  if(prompt==='permission'||prompt==='permission-hang'){
   const result=await rpc('session/request_permission',{sessionId,toolCall:{toolCallId:'fixture-tool',title:'use_tool',kind:'other',rawInput:{tool_name:'workspace__read_file',path:'notes.txt'}},options:[{optionId:'allow-exact-id',name:'Allow once',kind:'allow_once'},{optionId:'deny-exact-id',name:'Deny',kind:'reject_once'},{optionId:'unsafe-persistent',name:'Always allow',kind:'allow_always'}]});
   if(prompt==='permission-hang'){setInterval(()=>{},1000);return;}
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:JSON.stringify(result)}});
  }else if(prompt==='redact'){
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'Provider returned '+process.env.GROK_BOT_PROVIDER_KEY}});
  }else if(prompt==='redact-split'||prompt==='redact-prefix'){
   const key=process.env.GROK_BOT_PROVIDER_KEY;
   const toolCallId='untrusted-'+key;
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'Provider returned '+key.slice(0,-1)}});
   update({sessionUpdate:'tool_call',toolCallId,title:'Read '+key,kind:'read',status:'in_progress',rawInput:{path:key},locations:[{path:key}]});
   await new Promise(resolve=>setTimeout(resolve,75));
   if(prompt==='redact-split')update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:key.slice(-1)+' safely.'}});
   update({sessionUpdate:'tool_call_update',toolCallId,title:'Finished '+key,status:'completed',rawOutput:key,locations:[{path:key}]});
  }else if(prompt==='large-write'||prompt==='unicode-write'){
   const content=(prompt==='unicode-write'?'😀'.repeat(50000):'x'.repeat(200100))+'HIDDEN_SUFFIX_MUST_NOT_BE_WRITTEN';
   const result=await mcp('write_file',{path:'notes.txt',content});
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:JSON.stringify(result)}});
  }else if(prompt==='write'||prompt==='write-hang'){
   const result=await mcp('write_file',{path:'notes.txt',content:'AI approved contents\\n'});
   if(prompt==='write-hang'){setInterval(()=>{},1000);return;}
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:JSON.stringify(result)}});
  }else{
   update({sessionUpdate:'agent_thought_chunk',content:{type:'text',text:'private chain of thought'}});
   update({sessionUpdate:'tool_call',toolCallId:'read-id',title:'Read notes',kind:'read',status:'in_progress',rawInput:{path:'notes.txt'}});
   update({sessionUpdate:'tool_call_update',toolCallId:'read-id',status:'completed',rawOutput:'read complete'});
   update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:loadCount?'Continued session.':'Hello from fake ACP.'}});
  }
  response(id,{stopReason:'end_turn'});
 }
});
`;

const scratch = tmpdir();

async function fixture(options: { args?: string[]; startupTimeoutMs?: number; promptIdleTimeoutMs?: number } = {}) {
  await mkdir(scratch, { recursive: true });
  const root = await mkdtemp(path.join(scratch, 'grok-bot-backend-runtime-'));
  const workspace = path.join(root, 'workspace');
  await mkdir(workspace);
  await writeFile(path.join(workspace, 'notes.txt'), 'original\n');
  const command = path.join(root, 'fake-agent.mjs');
  await writeFile(command, fakeAgent, { mode: 0o700 });
  const backend = await createBackend({ dataDir: path.join(root, 'data'), serveWeb: false, runtime: { command, args: ['--fixture-root', root, ...(options.args ?? [])], cancelDeadlineMs: 50, startupTimeoutMs: options.startupTimeoutMs, promptIdleTimeoutMs: options.promptIdleTimeoutMs } });
  await new Promise<void>(resolve => backend.server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${(backend.server.address() as AddressInfo).port}`;
  async function api(route: string, body?: unknown, method = body === undefined ? 'GET' : 'POST') {
    return fetch(origin + route, { method, headers: { Origin: origin, 'X-Grok-Bot': '1', 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  }
  await api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace }, 'PUT');
  const { conversation } = await (await api('/api/conversations', { botId: backend.store.bots[0].id })).json() as { conversation: Conversation };
  async function state() { return await (await api('/api/state')).json() as AppState; }
  async function wait(predicate: (conversation: Conversation) => boolean): Promise<Conversation> {
    const deadline = Date.now() + 5000;
    let current: Conversation;
    do {
      current = (await state()).conversations.find(c => c.id === conversation.id)!;
      if (predicate(current)) return current;
      await new Promise(resolve => setTimeout(resolve, 10));
    } while (Date.now() < deadline);
    throw new Error(`Run did not reach expected state. ${JSON.stringify(current)}`);
  }
  return {
    root, workspace, backend, api, conversation, wait, state,
    send: (text: string) => api(`/api/conversations/${conversation.id}/messages`, { text }),
    close: async () => { await backend.close(); await rm(root, { recursive: true, force: true }); },
  };
}

test('model-only chat leaves file access unavailable until a workspace is connected', async () => {
  const f = await fixture();
  try {
    const configured = await f.api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: '' }, 'PUT');
    assert.equal(configured.status, 200);
    assert.equal((await f.api('/api/files')).status, 400);
    assert.equal((await f.api('/api/file?path=notes.txt')).status, 400);
    assert.equal((await f.send('write')).status, 202);
    const completed = await f.wait(conversation => conversation.run.status === 'completed');
    assert.match(completed.messages.find(message => message.role === 'assistant')?.text ?? '', /Choose a workspace/);
    assert.equal(completed.run.permission, undefined);
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'original\n');
  } finally { await f.close(); }
});

test('ACP text and tools stream to the selected conversation, thought streams stay private, and sessions continue without replay duplication', async () => {
  const f = await fixture();
  try {
    assert.equal((await f.send('hello')).status, 202);
    const first = await f.wait(c => c.run.status === 'completed');
    assert.equal(first.messages[1].text, 'Hello from fake ACP.');
    assert.equal(first.messages[1].tools[0].status, 'completed');
    assert.equal(JSON.stringify(first).includes('private chain of thought'), false);
    const sessionId = first.sessionId;
    assert.ok(sessionId);
    assert.equal((await f.send('continue')).status, 202);
    const continued = await f.wait(c => c.run.status === 'completed' && c.messages.length === 4);
    assert.equal(continued.sessionId, sessionId);
    assert.equal(continued.messages[3].text, 'Continued session.');
    assert.equal(JSON.stringify(continued).includes('HISTORICAL'), false);
    const update = await f.api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'changed-model', workspace: f.workspace }, 'PUT');
    assert.equal(update.status, 200);
    assert.equal(((await update.json()) as AppState).conversations[0].sessionId, undefined);
    await f.send('hello again');
    const changed = await f.wait(c => c.run.status === 'completed' && c.messages.length === 6);
    assert.notEqual(changed.sessionId, sessionId);
  } finally { await f.close(); }
});

test('ACP permissions preserve option IDs, exclude persistent approval modes, and can only be resolved once', async () => {
  const f = await fixture();
  try {
    await f.send('permission');
    const pending = await f.wait(c => c.run.status === 'awaiting_permission');
    assert.deepEqual(pending.run.permission!.options.map(o => o.id), ['allow-exact-id', 'deny-exact-id']);
    const route = `/api/conversations/${f.conversation.id}/permissions/${pending.run.permission!.id}`;
    assert.equal((await f.api(route, { optionId: 'not-a-real-option' })).status, 400);
    assert.equal((await f.api(route, { optionId: 'allow-exact-id' })).status, 200);
    assert.equal((await f.api(route, { optionId: 'allow-exact-id' })).status, 409);
    const completed = await f.wait(c => c.run.status === 'completed');
    assert.deepEqual(JSON.parse(completed.messages[1].text), { outcome: { outcome: 'selected', optionId: 'allow-exact-id' } });
  } finally { await f.close(); }
});

test('broker mutations wait for one-use browser approval, reject manual writes during a run, and deny or detect external changes', async () => {
  const f = await fixture();
  try {
    await f.send('write');
    const pending = await f.wait(c => c.run.status === 'awaiting_permission');
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'original\n');
    assert.equal((await f.api('/api/file', { path: 'notes.txt', content: 'manual bypass', hash: '' }, 'PUT')).status, 409);
    assert.equal((await f.api('/api/bots', { name: 'Blocked' })).status, 409);
    assert.equal((await f.send('second simultaneous run')).status, 409);
    const permission = pending.run.permission!;
    await f.api(`/api/conversations/${pending.id}/permissions/${permission.id}`, { optionId: permission.options.find(o => o.kind === 'reject_once')!.id });
    await f.wait(c => c.run.status === 'completed');
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'original\n');
    await f.send('write');
    const allowed = (await f.wait(c => c.run.status === 'awaiting_permission')).run.permission!;
    await f.api(`/api/conversations/${pending.id}/permissions/${allowed.id}`, { optionId: allowed.options.find(o => o.kind === 'allow_once')!.id });
    await f.wait(c => c.run.status === 'completed');
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'AI approved contents\n');
    await f.send('write');
    const conflicted = (await f.wait(c => c.run.status === 'awaiting_permission')).run.permission!;
    await writeFile(path.join(f.workspace, 'notes.txt'), 'external change\n');
    await f.api(`/api/conversations/${pending.id}/permissions/${conflicted.id}`, { optionId: conflicted.options.find(o => o.kind === 'allow_once')!.id });
    const finished = await f.wait(c => c.run.status === 'completed');
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'external change\n');
    assert.match(finished.messages.at(-1)!.text, /File changed on disk/);
  } finally { await f.close(); }
});

test('cancel settles pending writes and terminates a stubborn child process group', async () => {
  const f = await fixture();
  try {
    await f.send('write');
    const pending = await f.wait(c => c.run.status === 'awaiting_permission');
    assert.equal((await f.api(`/api/conversations/${pending.id}/cancel`, {})).status, 200);
    const cancelled = await f.wait(c => c.run.status === 'cancelled');
    assert.equal(cancelled.run.permission, undefined);
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'original\n');
    await f.send('hang');
    const running = await f.wait(c => c.messages.at(-1)!.text.includes('CHILD'));
    const [pid, childPid] = running.messages.at(-1)!.text.match(/\d+/g)!.map(Number);
    assert.equal((await f.api(`/api/conversations/${running.id}/cancel`, {})).status, 200);
    await f.wait(c => c.run.status === 'cancelled');
    await new Promise(resolve => setTimeout(resolve, 50));
    assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
    const childAlive = await readFile(`/proc/${childPid}/stat`, 'utf8').then(value => value.split(' ')[2] !== 'Z').catch(() => false);
    assert.equal(childAlive, false);
  } finally { await f.close(); }
});


test('a nonresponsive ACP child reaches a useful startup error instead of hanging forever', async () => {
  const f = await fixture({ args: ['--stall-initialize'], startupTimeoutMs: 100 });
  try {
    assert.equal((await f.send('hello')).status, 202);
    const failed = await f.wait(c => c.run.status === 'error');
    assert.match(failed.run.error!, /startup deadline/);
    assert.equal(failed.run.permission, undefined);
    assert.equal(failed.messages[1].text, '');
  } finally { await f.close(); }
});


async function waitUntil(predicate: () => boolean | Promise<boolean>, description: string): Promise<void> {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  assert.fail(description);
}

async function processAlive(pid: number): Promise<boolean> {
  try { process.kill(pid, 0); } catch { return false; }
  return readFile(`/proc/${pid}/stat`, 'utf8').then(value => value.split(' ')[2] !== 'Z').catch(() => false);
}

test('a write preimage completing after the turn ends cannot create a late approval or block finalization', async () => {
  const f = await fixture();
  const original = WorkspaceFiles.prototype.expected;
  let release!: () => void;
  let entered = false;
  const blocked = new Promise<void>(resolve => { release = resolve; });
  WorkspaceFiles.prototype.expected = async function (filename) {
    if (this.root === f.workspace) { entered = true; await blocked; }
    return original.call(this, filename);
  };
  const snapshots: AppState[] = [];
  const unsubscribe = f.backend.store.subscribe(state => { snapshots.push(state); });
  try {
    await f.send('finish-during-preimage');
    await waitUntil(() => entered, 'The broker did not reach its controlled preimage read.');
    const running = await f.wait(c => c.messages.at(-1)!.text.startsWith('PID '));
    process.kill(Number(running.messages.at(-1)!.text.match(/\d+/)![0]), 'SIGUSR1');
    const broker = JSON.parse(await readFile(path.join(f.root, 'broker.json'), 'utf8')) as { url: string; headers: { value: string }[] };
    await waitUntil(async () => {
      const response = await fetch(broker.url, { method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: broker.headers[0].value }, body: JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'ping' }) });
      await response.body?.cancel();
      return response.status === 403;
    }, 'The turn did not finish while its preimage was blocked.');
    release();
    const completed = await f.wait(c => c.run.status === 'completed');
    assert.equal(completed.run.permission, undefined);
    assert.equal(snapshots.some(state => state.conversations.some(c => c.run.permission !== undefined)), false);
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'original\n');
  } finally {
    release();
    WorkspaceFiles.prototype.expected = original;
    unsubscribe();
    await f.close();
  }
});

test('every emitted snapshot hides streamed credential prefixes and untrusted tool identifiers', async () => {
  const f = await fixture();
  const apiKey = 'private-split-fixture-credential-793515';
  const snapshots: string[] = [];
  try {
    await f.api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: f.workspace, apiKey }, 'PUT');
    const unsubscribe = f.backend.store.subscribe(state => { snapshots.push(JSON.stringify(state)); });
    try {
      await f.send('redact-split');
      const completed = await f.wait(c => c.run.status === 'completed');
      assert.equal(completed.messages[1].text, 'Provider returned [redacted] safely.');
      assert.equal(completed.messages[1].tools.length, 1);
      const tool = completed.messages[1].tools[0];
      assert.match(tool.id, /^[a-f\d]{8}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{4}-[a-f\d]{12}$/i);
      assert.equal(tool.status, 'completed');
      assert.equal(tool.title, 'Finished [redacted]');
      assert.equal(tool.input, '{"path":"[redacted]"}');
      assert.equal(tool.output, '"[redacted]"');
      assert.deepEqual(tool.paths, ['[redacted]']);
      await f.send('redact-prefix');
      const prefix = await f.wait(c => c.run.status === 'completed' && c.messages.length === 4);
      assert.equal(prefix.messages[3].text, 'Provider returned [redacted]');
      snapshots.push(JSON.stringify(await f.state()));
      assert.ok(snapshots.some(snapshot => snapshot.includes('Provider returned ')));
      for (const snapshot of snapshots) {
        assert.equal(snapshot.includes(apiKey), false);
        assert.equal(snapshot.includes(apiKey.slice(0, -1)), false);
        assert.equal(snapshot.includes(apiKey.slice(0, 16)), false);
      }
    } finally { unsubscribe(); }
  } finally { await f.close(); }
});

test('oversized ASCII and UTF-8 write proposals fail before approval and never write an unreviewed suffix', async () => {
  const f = await fixture();
  const snapshots: AppState[] = [];
  const unsubscribe = f.backend.store.subscribe(state => { snapshots.push(state); });
  try {
    for (const prompt of ['large-write', 'unicode-write']) {
      await f.send(prompt);
      const finished = await f.wait(c => c.run.status === 'completed' || c.run.status === 'awaiting_permission');
      if (finished.run.permission) {
        await f.api(`/api/conversations/${finished.id}/permissions/${finished.run.permission.id}`, { optionId: finished.run.permission.options.find(o => o.kind === 'allow_once')!.id });
      }
      const completed = await f.wait(c => c.run.status === 'completed');
      assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'original\n');
      assert.match(completed.messages.at(-1)!.text, /review.*160 KiB|160 KiB.*review/i);
      assert.equal(snapshots.some(state => state.conversations.some(c => c.run.permission !== undefined)), false);
    }
  } finally { unsubscribe(); await f.close(); }
});

test('a silent inference hits its backend inactivity deadline and kills stubborn child processes', async () => {
  const f = await fixture({ promptIdleTimeoutMs: 100 });
  try {
    await f.send('silent-hang');
    const silent = await f.wait(c => c.run.status === 'error');
    assert.match(silent.run.error!, /inactiv|no (?:session )?updates/i);
    assert.equal(silent.run.permission, undefined);
    await f.send('hang');
    const running = await f.wait(c => c.messages.at(-1)!.text.includes('CHILD'));
    const [pid, childPid] = running.messages.at(-1)!.text.match(/\d+/g)!.map(Number);
    const failed = await f.wait(c => c.run.status === 'error');
    assert.match(failed.run.error!, /inactiv|no (?:session )?updates/i);
    await waitUntil(async () => !await processAlive(pid) && !await processAlive(childPid), 'Inactivity left a child process alive.');
  } finally { await f.close(); }
});

test('inactivity pauses for ACP and broker approval and resets on actual session updates', async () => {
  const f = await fixture({ promptIdleTimeoutMs: 100 });
  try {
    for (const prompt of ['permission', 'write']) {
      await f.send(prompt);
      const pending = await f.wait(c => c.run.status === 'awaiting_permission');
      await new Promise(resolve => setTimeout(resolve, 250));
      const stillPending = (await f.state()).conversations.find(c => c.id === pending.id)!;
      assert.equal(stillPending.run.status, 'awaiting_permission');
      assert.equal(stillPending.run.permission!.id, pending.run.permission!.id);
      await f.api(`/api/conversations/${pending.id}/permissions/${pending.run.permission!.id}`, { optionId: pending.run.permission!.options.find(o => o.kind === 'allow_once')!.id });
      const completed = await f.wait(c => c.run.status === 'completed');
      assert.equal(completed.run.error, undefined);
    }
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'AI approved contents\n');
    await f.send('active-long');
    const completed = await f.wait(c => c.run.status === 'completed');
    assert.equal(completed.messages.at(-1)!.text, 'Active task completed.');
    assert.equal(JSON.stringify(completed).includes('private activity'), false);
  } finally { await f.close(); }
});

test('the inactivity deadline resumes after ACP and broker approval decisions', async () => {
  const f = await fixture({ promptIdleTimeoutMs: 100 });
  try {
    for (const prompt of ['permission-hang', 'write-hang']) {
      await f.send(prompt);
      const pending = await f.wait(c => c.run.status === 'awaiting_permission');
      await new Promise(resolve => setTimeout(resolve, 250));
      const permission = pending.run.permission!;
      assert.equal((await f.api(`/api/conversations/${pending.id}/permissions/${permission.id}`, { optionId: permission.options.find(o => o.kind === 'allow_once')!.id })).status, 200);
      const failed = await f.wait(c => c.run.status === 'error');
      assert.match(failed.run.error!, /inactiv|no (?:session )?updates/i);
      assert.equal(failed.run.permission, undefined);
    }
    assert.equal(await readFile(path.join(f.workspace, 'notes.txt'), 'utf8'), 'AI approved contents\n');
  } finally { await f.close(); }
});

test('a killed backend cannot orphan its Grok process group', async () => {
  await mkdir(scratch, { recursive: true });
  const root = await mkdtemp(path.join(scratch, 'grok-bot-parent-death-'));
  const command = path.join(root, 'fake-agent.mjs');
  const pidFile = path.join(root, 'agent-pids.json');
  const ownerFile = path.join(root, 'owner.mts');
  const appUrl = new URL('../server/app.ts', import.meta.url).href;
  const tsx = fileURLToPath(import.meta.resolve('tsx/cli'));
  await writeFile(command, fakeAgent, { mode: 0o700 });
  await writeFile(ownerFile, `
    import { mkdir, writeFile } from 'node:fs/promises';
    import { createBackend } from ${JSON.stringify(appUrl)};
    const root = ${JSON.stringify(root)};
    const workspace = root + '/workspace';
    await mkdir(workspace);
    await writeFile(workspace + '/notes.txt', 'original');
    const backend = await createBackend({ dataDir: root + '/data', serveWeb: false, runtime: { command: ${JSON.stringify(command)}, args: ['--pid-file', ${JSON.stringify(pidFile)}], cancelDeadlineMs: 50 } });
    await new Promise(resolve => backend.server.listen(0, '127.0.0.1', resolve));
    await backend.store.updateSettings({ baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace });
    const conversation = await backend.store.addConversation(backend.store.bots[0].id);
    await backend.runtime.start(conversation.id, 'chatty-hang');
    await writeFile(root + '/owner-pid', String(process.pid));
  `);
  const launcher = spawn(process.execPath, [tsx, ownerFile], { stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  launcher.stderr.on('data', chunk => { stderr += String(chunk); });
  let ownerPid: number | undefined;
  let pids: number[] = [];
  try {
    await waitUntil(() => readFile(pidFile, 'utf8').then(value => { pids = JSON.parse(value) as number[]; return true; }).catch(() => false), `The parent-death fixture did not start. ${stderr}`);
    ownerPid = Number(await readFile(path.join(root, 'owner-pid'), 'utf8'));
    assert.ok(pids.every(pid => pid > 1));
    assert.equal(await processAlive(pids[0]), true);
    assert.equal(await processAlive(pids[1]), true);
    const supervisorPid = Number((await readFile(`/proc/${pids[0]}/stat`, 'utf8')).split(' ')[3]);
    process.kill(ownerPid, 'SIGKILL');
    await waitUntil(async () => !await processAlive(pids[0]) && !await processAlive(pids[1]) && !await processAlive(supervisorPid), 'Backend death left Grok, its child, or the supervisor alive.');
  } finally {
    if (ownerPid) { try { process.kill(ownerPid, 'SIGKILL'); } catch {} }
    if (pids[0]) { try { process.kill(-pids[0], 'SIGKILL'); } catch {} }
    launcher.kill('SIGKILL');
    await rm(root, { recursive: true, force: true });
  }
});

test('provider text that echoes a backend credential is redacted from public conversation snapshots', async () => {
  const f = await fixture();
  try {
    const apiKey = 'private-fixture-credential-65785';
    await f.api('/api/settings', { baseUrl: 'http://127.0.0.1:12345/v1', model: 'fixture', workspace: f.workspace, apiKey }, 'PUT');
    await f.send('redact');
    const completed = await f.wait(c => c.run.status === 'completed');
    assert.equal(completed.messages[1].text, 'Provider returned [redacted]');
    assert.equal(JSON.stringify(await f.state()).includes(apiKey), false);
  } finally { await f.close(); }
});
