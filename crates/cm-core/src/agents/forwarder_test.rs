//! Executes the emitted JavaScript with a fake hook host and controlled child
//! processes. Node is a test dependency supplied by the Nix toolchain; no agent
//! installation, network connection or real hook subprocess is involved.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::state::HookEvent;

pub(super) fn run(source: &str, scenario: &str) -> Vec<(HookEvent, String)> {
    let mut child = Command::new("node")
        .args([
            "--experimental-vm-modules",
            "--input-type=module",
            "-e",
            DRIVER,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("forwarder tests require Node.js; run through nix develop");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({"source": source, "scenario": scenario})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "generated forwarder failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    messages
        .into_iter()
        .map(|m| {
            (
                HookEvent::from_kebab(m["event"].as_str().unwrap()).unwrap(),
                m["body"].to_string(),
            )
        })
        .collect()
}

const DRIVER: &str = r#"
import vm from 'node:vm';
import { EventEmitter } from 'node:events';
import assert from 'node:assert/strict';

let input = '';
for await (const chunk of process.stdin) input += chunk;
const { source, scenario } = JSON.parse(input);
const watchdog = setTimeout(() => { throw new Error('forwarder did not finish'); }, 5000);
const pending = [];
const delivered = [];
const timers = new Map();
let spawnAttempts = 0;
const context = vm.createContext({
  setTimeout(fn) { const id = {}; timers.set(id, fn); return id; },
  clearTimeout(id) { timers.delete(id); },
});
const spawn = (_exe, args) => {
  assert.deepEqual(Array.from(args.slice(0, 3)), ['hook', '--agent', scenario.split('_')[0]]);
  if (++spawnAttempts === 1 && scenario.endsWith('_spawn_throw')) {
    throw new Error('spawn threw');
  }
  const child = new EventEmitter();
  child.stdin = new EventEmitter();
  child.stdin.end = body => { child.body = JSON.parse(body); };
  child.kill = signal => { assert.equal(signal, 'SIGKILL'); child.killed = true; return true; };
  child.args = args;
  pending.push(child);
  return child;
};
const builtin = new vm.SyntheticModule(['spawn'], function () {
  this.setExport('spawn', spawn);
}, { context });
const module = new vm.SourceTextModule(source, { context });
await module.link(name => { assert.equal(name, 'node:child_process'); return builtin; });
await module.evaluate();
const tick = () => new Promise(setImmediate);
const complete = child => {
  if (child.finished) return;
  child.finished = true;
  delivered.push({ event: child.args.at(-1), body: child.body });
  child.emit('close', 0);
};

if (scenario.startsWith('opencode_')) {
  const hooks = await module.namespace.default({ directory: '/project' });
  const created = { event: { type: 'session.created', properties: {
    info: { id: 'child', parentID: 'root', directory: '/project' },
  } } };
  const idle = { event: { type: 'session.status', properties: {
    sessionID: 'child', status: { type: 'idle' },
  } } };
  // OpenCode discards the bus hook's promise, so call both without awaiting.
  const first = hooks.event(created);
  const directInput = { sessionID: 'child', tool: 'original' };
  const second = scenario === 'opencode_direct'
    ? hooks['tool.execute.before'](directInput, {})
    : hooks.event(idle);
  directInput.tool = 'mutated';
  await tick();
  assert.ok(pending.length > 0);
  if (scenario === 'opencode_order' || scenario === 'opencode_direct') {
    // Hold the lineage subprocess back while a later one can finish first.
    if (pending.length > 1) complete(pending[1]);
    complete(pending[0]);
  } else if (scenario === 'opencode_spawn_error') {
    pending[0].emit('error', new Error('spawn failed'));
    pending[0].finished = true;
  } else if (scenario === 'opencode_stdin_error') {
    pending[0].stdin.emit('error', new Error('EPIPE'));
    complete(pending[0]);
  } else if (scenario === 'opencode_timeout') {
    assert.ok(timers.size > 0, 'a stuck child needs a deadline');
    [...timers.values()][0]();
    assert.equal(pending[0].killed, true);
    pending[0].finished = true;
  } else if (scenario === 'opencode_spawn_throw') {
    assert.equal(pending.length, 1, 'the second event must survive a throwing spawn');
    complete(pending[0]);
  } else throw new Error('unknown scenario');
  await tick();
  assert.equal(spawnAttempts, 2, 'delivery must continue after the first child');
  for (const child of pending) complete(child);
  await Promise.all([first, second]);
  assert.equal(timers.size, 0, 'completed sends must clear their deadlines');
  if (scenario === 'opencode_direct') {
    assert.equal(delivered[1].body.payload[0].tool, 'original');
  }
} else if (scenario.endsWith('_concurrent')) {
  const handlers = new Map();
  module.namespace.default({ on(name, fn) { handlers.set(name, fn); } });
  const rename = scenario.startsWith('pi_') ? 'session_info_changed' : 'session_switch';
  const first = handlers.get(rename)({}, {});
  const event = { prompt: 'Original prompt' };
  const second = handlers.get('before_agent_start')(event, {});
  event.prompt = 'Mutated prompt';
  await tick();
  if (pending.length > 1) complete(pending[1]);
  complete(pending[0]);
  await tick();
  complete(pending[1]);
  await Promise.all([first, second]);
  assert.equal(delivered[0].event, 'session-start', 'unawaited events must keep invocation order');
  assert.equal(delivered[1].body.prompt, 'Original prompt', 'capture payload before queueing');
} else if (scenario === 'pi_metadata') {
  const handlers = new Map();
  const pi = { on(name, fn) { handlers.set(name, fn); } };
  module.namespace.default(pi);
  const ctx = {
    sessionManager: { getSessionId: () => 'root' },
    model: { provider: 'openrouter', id: 'google/gemini-3.8-flash' },
    getContextUsage: () => ({ tokens: null }),
  };
  for (const type of ['model_select', 'session_tree']) {
    assert.ok(handlers.has(type), `missing handler: ${type}`);
    const sent = handlers.get(type)({ type, model: ctx.model }, ctx);
    await tick();
    complete(pending.at(-1));
    await sent;
    ctx.getContextUsage = () => ({ tokens: 4321 });
  }
} else if (/^(pi|omp)_(timeout|spawn_throw|spawn_error|stdin_error)$/.test(scenario)) {
  const handlers = new Map();
  module.namespace.default({ on(name, fn) { handlers.set(name, fn); } });
  let settled = false;
  const sent = handlers.get('before_agent_start')({ prompt: 'Draft an essay' }, {});
  sent.then(value => { assert.equal(value, undefined); settled = true; });
  await tick();
  if (scenario.endsWith('_timeout')) {
    assert.ok(timers.size > 0, 'a stuck hook must not freeze Pi or omp');
    [...timers.values()][0]();
    assert.equal(pending[0].killed, true);
  } else if (scenario.endsWith('_spawn_error')) {
    pending[0].emit('error', new Error('spawn failed'));
  } else if (scenario.endsWith('_stdin_error')) {
    pending[0].stdin.emit('error', new Error('EPIPE'));
  }
  await tick();
  assert.equal(settled, true, 'failed delivery must release the agent');
  assert.equal(timers.size, 0, 'failure must clear the deadline');
  await sent;
  const next = handlers.get('tool_execution_start')({ toolName: 'read' }, {});
  await tick();
  complete(pending.at(-1));
  await next;
  assert.equal(spawnAttempts, 2);
  assert.equal(timers.size, 0);
} else if (scenario.endsWith('_extension')) {
  const handlers = new Map();
  const pi = {
    on(name, fn) { handlers.set(name, fn); },
    getSessionName: () => 'Current title',
    getThinkingLevel: () => 'high',
  };
  module.namespace.default(pi);
  const ctx = {
    sessionManager: { getSessionId: () => 'root' },
    cwd: '/project',
    getContextUsage: () => ({ tokens: 1234.6 }),
    model: { id: 'test-model' },
  };
  const events = [
    { type: 'before_agent_start', prompt: 'Do the work' },
    { type: 'tool_execution_end', toolName: 'bash', isError: true },
    ...(scenario.startsWith('omp_') ? [
      { type: 'agent_end', willContinue: true },
      { type: 'agent_end', willContinue: false },
    ] : [
      { type: 'session_compact_failed', willRetry: true },
      { type: 'session_compact_failed', aborted: true, willRetry: false },
    ]),
  ];
  for (const event of events) {
    assert.ok(handlers.has(event.type), `missing handler: ${event.type}`);
    const sent = handlers.get(event.type)(event, ctx);
    let settled = false;
    sent.then(value => { assert.equal(value, undefined); settled = true; });
    await tick();
    assert.equal(settled, false, 'delivery must await its hook child');
    complete(pending.at(-1));
    await sent;
  }
} else if (scenario === 'pi_compaction') {
  const handlers = new Map();
  const pi = { on(name, fn) { handlers.set(name, fn); } };
  module.namespace.default(pi);
  const ctx = { sessionManager: { getSessionId: () => 'root' } };
  for (const event of [
    { type: 'session_before_compact' },
    { type: 'session_compact_failed', reason: 'manual', aborted: true, willRetry: false },
    { type: 'session_compact_failed', reason: 'manual', aborted: false, willRetry: false, errorMessage: 'summary failed' },
    { type: 'session_compact_failed', reason: 'overflow', aborted: false, willRetry: true, errorMessage: 'retrying' },
  ]) {
    assert.ok(handlers.has(event.type), `missing handler: ${event.type}`);
    const sent = handlers.get(event.type)(event, ctx);
    await tick();
    complete(pending.at(-1));
    await sent;
  }
} else if (scenario === 'omp_prompts') {
  const handlers = new Map();
  const pi = {
    on(name, fn) { handlers.set(name, fn); },
    getSessionName: () => 'Current title',
    getThinkingLevel: () => 'high',
  };
  module.namespace.default(pi);
  let resolveDialog;
  const calls = { select: 0, askDialog: 0 };
  // omp's extension `ui` is a proxy over the interactive UI. Its blocking
  // methods are adapters served from the proxy's own table; each adapter
  // re-reads `target[name]` when called, and the proxy has no set trap, so a
  // write lands on the target. Wrapping the method the proxy shows made the
  // adapter call the wrapper back until the stack overflowed, so the fake host
  // models that shape: only a call to the target's original method succeeds.
  const target = {
    select: () => { calls.select += 1; return new Promise((resolve) => { resolveDialog = resolve; }); },
    askDialog: () => { calls.askDialog += 1; return new Promise((resolve) => { resolveDialog = resolve; }); },
  };
  const adapters = {
    select: (...args) => target.select(...args),
    askDialog: (...args) => target.askDialog(...args),
  };
  const ui = new Proxy(target, {
    get(object, key) {
      return Object.hasOwn(adapters, key) ? adapters[key] : Reflect.get(object, key, object);
    },
  });
  const ctx = {
    sessionManager: { getSessionId: () => 'root' },
    cwd: '/project',
    agent: { kind: 'main' },
    ui,
    getContextUsage: () => ({ tokens: 10 }),
    model: { id: 'test-model' },
  };
  const send = async (type, event, onCtx = ctx) => {
    const sent = handlers.get(type)({ type, ...event }, onCtx);
    await tick();
    complete(pending.at(-1));
    await sent;
    return delivered.at(-1).body;
  };
  const openDialog = async (name) => {
    const dialog = ctx.ui[name]('question');
    await tick();
    complete(pending.at(-1));
    await tick();
    return { dialog, body: delivered.at(-1).body };
  };
  const closeDialog = async (dialog, value) => {
    resolveDialog(value);
    await dialog;
    await tick();
    complete(pending.at(-1));
    await tick();
    return delivered.at(-1).body;
  };

  const start = await send('session_start', {});
  assert.equal(start.blocking_prompt, 'none');

  let dialog = await openDialog('select');
  assert.equal(dialog.body.blocking_prompt, 'decision');
  assert.equal(dialog.body.tool_name, undefined, 'a dialog carries no tool name');
  assert.equal((await closeDialog(dialog.dialog, 'answer')).blocking_prompt, 'none');
  assert.equal(calls.select, 1, 'the wrapping must call omp\'s method once, not itself');

  const approval = await send('tool_approval_requested', { toolCallId: 't1', toolName: 'bash' });
  assert.equal(approval.blocking_prompt, 'approval');
  dialog = await openDialog('askDialog');
  assert.equal(dialog.body.blocking_prompt, 'approval', 'an open approval outranks a dialog');
  assert.equal(
    (await closeDialog(dialog.dialog, { kind: 'chat' })).blocking_prompt,
    'approval',
    'closing the dialog leaves the approval open',
  );
  assert.equal(calls.askDialog, 1, 'the wrapping must call omp\'s method once, not itself');
  const resolved = await send('tool_approval_resolved', { toolCallId: 't1' });
  assert.equal(resolved.blocking_prompt, 'none');

  assert.equal((await send('plan_review_requested', { reviewId: 'r1' })).blocking_prompt, 'decision');
  assert.equal((await send('plan_review_resolved', { reviewId: 'r1' })).blocking_prompt, 'none');

  // omp hands each session its own context over the same `ui`; a dialog opened
  // after a switch must report the new session, not the one it was wrapped in.
  const nextCtx = { ...ctx, sessionManager: { getSessionId: () => 'next' } };
  assert.equal((await send('session_switch', {}, nextCtx)).session_id, 'next');
  dialog = await openDialog('select');
  assert.equal(dialog.body.session_id, 'next', 'a dialog reports the current session');
  assert.equal((await closeDialog(dialog.dialog, 'answer')).session_id, 'next');

  // A subagent reports its own snapshot: its approval events still feed the
  // tracker, so the parent row can show a card the user answers in the same
  // terminal. Its other facts stay marked as the child's.
  const subCtx = { ...ctx, agent: { kind: 'sub' }, ui: {} };
  const subBody = await send('tool_execution_start', { toolName: 'read' }, subCtx);
  assert.equal(subBody.session_is_child, true);
  assert.equal(subBody.blocking_prompt, 'none', 'a child reports its own empty snapshot');
  const subApproval = await send(
    'tool_approval_requested', { toolCallId: 'c1', toolName: 'bash' }, subCtx,
  );
  assert.equal(subApproval.blocking_prompt, 'approval');
  assert.equal(
    (await send('tool_approval_resolved', { toolCallId: 'c1' }, subCtx)).blocking_prompt,
    'none',
  );
} else throw new Error('unknown scenario');
clearTimeout(watchdog);
process.stdout.write(JSON.stringify(delivered));
"#;
