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
  if (++spawnAttempts === 1 && scenario === 'opencode_spawn_throw') {
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
} else if (scenario.endsWith('_extension')) {
  const handlers = new Map();
  const pi = {
    on(name, fn) { handlers.set(name, fn); },
    getSessionName: () => 'Current title',
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
} else throw new Error('unknown scenario');
clearTimeout(watchdog);
process.stdout.write(JSON.stringify(delivered));
"#;
