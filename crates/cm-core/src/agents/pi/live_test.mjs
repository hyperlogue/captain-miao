// Run only by the ignored real-Pi test. Every process and file belongs to this
// fixture; the model is a loopback HTTP server with deterministic responses.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, watch, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';

let input = '';
for await (const chunk of process.stdin) input += chunk;
const { source } = JSON.parse(input);
const root = mkdtempSync(join(tmpdir(), 'miao-pi-live-'));
const children = [];
const checkpoints = [];
let offset = 0;
let holdRequest;
let phase = 'draft';
const server = createServer(async (request, response) => {
  let body = '';
  for await (const chunk of request) body += chunk;
  const payload = JSON.parse(body);
  if (holdRequest) {
    const ready = holdRequest;
    holdRequest = undefined;
    ready();
    return; // Pi's abort must cancel a provider request still awaiting headers.
  }
  const tool = phase === 'draft' && payload.messages.at(-1).role === 'user';
  const delta = tool
    ? { role: 'assistant', tool_calls: [{ index: 0, id: 'read-notes', type: 'function',
        function: { name: 'read', arguments: JSON.stringify({ path: join(root, 'notes.md') }) } }] }
    : { role: 'assistant', content: 'The project helps writers keep their work grounded in the source.' };
  const chunk = (delta, finish_reason) => ({
    id: 'fixture', object: 'chat.completion.chunk', created: 1, model: payload.model,
    choices: [{ index: 0, delta, finish_reason }],
    usage: { prompt_tokens: 120, completion_tokens: 20, total_tokens: 140 },
  });
  response.writeHead(200, { 'Content-Type': 'text/event-stream', Connection: 'close' });
  response.end([chunk(delta, null), chunk({}, tool ? 'tool_calls' : 'stop')]
    .map(value => `data: ${JSON.stringify(value)}\n\n`).join('') + 'data: [DONE]\n\n');
});

function deadline(promise, phase, client) {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timed out awaiting ${phase}; ${client?.stderr ?? ''}`)), 20000);
  })]).finally(() => clearTimeout(timer));
}

async function checkpoint(phase, ready) {
  let watcher;
  const observed = new Promise((resolve, reject) => {
    const check = () => {
      try {
        const lines = readFileSync(join(root, 'hooks.jsonl'), 'utf8').trim().split('\n');
        const events = lines.slice(offset).filter(Boolean).map(line => JSON.parse(line));
        if (events.some(ready)) resolve({ phase, events, count: lines.length });
      } catch (error) {
        if (error.code !== 'ENOENT') reject(error);
      }
    };
    watcher = watch(root, check);
    check();
  });
  try {
    const result = await deadline(observed, `${phase} hook delivery`);
    checkpoints.push(result);
    offset = result.count;
  } finally {
    watcher.close();
  }
}

function start(extra = []) {
  const binary = process.env.CM_TEST_PI_BIN || 'pi';
  const child = spawn(binary.includes('/') ? resolve(binary) : binary, [
    '--mode', 'rpc', '--offline', '--no-extensions', '--no-skills', '--no-prompt-templates',
    '--no-context-files', '--no-mcp', '--provider', 'miao-test', '--model', 'writer',
    '--session-dir', join(root, 'sessions'), '-e', join(root, 'extension.ts'), ...extra,
  ], {
    cwd: root, stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, PI_CODING_AGENT_DIR: join(root, 'agent'),
      PI_CODING_AGENT_SESSION_DIR: join(root, 'sessions'),
      XDG_CONFIG_HOME: join(root, 'config'), XDG_DATA_HOME: join(root, 'data'),
      XDG_STATE_HOME: join(root, 'state'), XDG_CACHE_HOME: join(root, 'cache'),
      CM_TEST_PI_EVENTS: join(root, 'hooks.jsonl') },
  });
  children.push(child);
  const client = { child, stderr: '', queue: [], waiters: [] };
  child.stderr.on('data', data => { client.stderr += data; });
  child.on('error', error => {
    for (const waiter of client.waiters.splice(0)) waiter.reject(error);
  });
  createInterface({ input: child.stdout }).on('line', line => {
    const value = JSON.parse(line);
    const index = client.waiters.findIndex(waiter => waiter.matches(value));
    if (index >= 0) client.waiters.splice(index, 1)[0].resolve(value);
    else client.queue.push(value);
  });
  client.wait = (matches, phase) => deadline(new Promise((resolve, reject) => {
    const index = client.queue.findIndex(matches);
    if (index >= 0) resolve(client.queue.splice(index, 1)[0]);
    else client.waiters.push({ matches, resolve, reject });
  }), phase, client);
  let id = 0;
  client.command = async value => {
    const request = { ...value, id: String(++id) };
    child.stdin.write(JSON.stringify(request) + '\n');
    const reply = await client.wait(value => value.type === 'response' && value.id === request.id, value.type);
    assert.equal(reply.success, true, `${value.type}: ${JSON.stringify(reply)}`);
    return reply.data;
  };
  client.settle = () => client.wait(value => value.type === 'agent_settled', 'agent_settled');
  client.close = async () => {
    const exited = once(child, 'exit');
    child.stdin.end();
    const [code] = await deadline(exited, 'Pi shutdown', client);
    assert.equal(code, 0, client.stderr);
  };
  return client;
}

try {
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  mkdirSync(join(root, 'agent'));
  writeFileSync(join(root, 'agent/models.json'), JSON.stringify({ providers: { 'miao-test': {
    api: 'openai-completions', baseUrl: `http://127.0.0.1:${server.address().port}/v1`, apiKey: 'fixture',
    models: ['writer', 'reviewer'].map(id => ({ id, name: id, contextWindow: 64000, maxTokens: 1024,
      reasoning: false, input: ['text'], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } })),
  } } }));
  writeFileSync(join(root, 'agent/settings.json'), JSON.stringify({
    retry: { enabled: false }, compaction: { enabled: false, reserveTokens: 1024, keepRecentTokens: 1 },
  }));
  writeFileSync(join(root, 'notes.md'), 'The project is a session dashboard.\n');
  writeFileSync(join(root, 'hook.mjs'), `#!/usr/bin/env node
import { appendFileSync } from 'node:fs';
let body = '';
for await (const chunk of process.stdin) body += chunk;
appendFileSync(process.env.CM_TEST_PI_EVENTS, JSON.stringify({ event: process.argv.at(-1), body: JSON.parse(body) }) + '\\n');
`, { mode: 0o700 });
  writeFileSync(join(root, 'extension.ts'), source.replace('"__MIAO_TEST_HOOK__"', JSON.stringify(join(root, 'hook.mjs'))));
  let client = start();
  const initial = await client.command({ type: 'get_state' });
  await checkpoint('startup', event => event.event === 'session-start');
  await client.command({ type: 'prompt', message: 'Read notes.md and draft a paragraph.' });
  await client.settle();
  await checkpoint('draft', event => event.event === 'stop');
  await client.command({ type: 'set_session_name', name: 'Essay draft' });
  await checkpoint('rename', event => event.body.session_title === 'Essay draft');
  await client.command({ type: 'set_model', provider: 'miao-test', modelId: 'reviewer' });
  await checkpoint('model', event => event.event === 'model-switch');
  const requestArrived = new Promise(resolve => { holdRequest = resolve; });
  await client.command({ type: 'prompt', message: 'Revise the paragraph.' });
  await deadline(requestArrived, 'provider request before abort', client);
  await client.command({ type: 'abort' });
  await client.settle();
  await checkpoint('abort', event => event.event === 'stop');
  phase = 'compact';
  await client.command({ type: 'compact' });
  await checkpoint('compact', event => event.event === 'post-compact');
  await client.command({ type: 'new_session' });
  await checkpoint('new', event => event.event === 'session-start' && event.body.session_id !== initial.sessionId);
  await client.close();
  client = start(['--session', initial.sessionId]);
  assert.equal((await client.command({ type: 'get_state' })).sessionId, initial.sessionId);
  await checkpoint('resume', event => event.event === 'session-start');
  await client.close();
  client = start(['--fork', initial.sessionId]);
  assert.notEqual((await client.command({ type: 'get_state' })).sessionId, initial.sessionId);
  await checkpoint('fork', event => event.event === 'session-start');
  await client.close();
  process.stdout.write(JSON.stringify(checkpoints));
} finally {
  await Promise.all(children.map(async child => {
    if (child.pid && child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit');
      child.kill('SIGKILL');
      await deadline(exited, 'fixture process cleanup');
    }
  }));
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
  rmSync(root, { recursive: true, force: true });
}
