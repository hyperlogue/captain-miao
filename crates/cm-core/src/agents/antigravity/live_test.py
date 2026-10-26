"""Optional real-agy probe. All inference uses this process's loopback fixture."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import shutil
import sqlite3
import struct
import subprocess
import sys
import termios
import threading
import time

config = json.load(sys.stdin)
root = Path(config['root'])
work = root / 'work'
work.mkdir()
event_file = root / 'events.jsonl'
event_file.touch()
real_settings = Path(config['home']) / '.gemini/antigravity-cli/settings.json'
original_settings = real_settings.read_bytes()
delayed_request = threading.Event()
release_response = threading.Event()
tool_started = threading.Event()
release_tool = threading.Event()
compact_requested = threading.Event()
compact_started = threading.Event()
release_compact = threading.Event()


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        assert self.path == '/held-tool', self.path
        tool_started.set()
        release_tool.wait(45)
        try:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'42\n')
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        title = 'title generator' in json.dumps(body.get('systemInstruction', {}))
        if compact_requested.is_set() and not title:
            compact_started.set()
            release_compact.wait(45)
        contents = body.get('contents', [])
        prompt_index = next((i for i in range(len(contents) - 1, -1, -1)
                             if contents[i].get('role') == 'user'
                             and any('text' in p for p in contents[i].get('parts', []))), 0)
        current_turn = contents[prompt_index:]
        parts = [p for c in current_turn for p in c.get('parts', [])]
        latest_prompt = next((p['text'] for c in current_turn[:1]
                              for p in reversed(c.get('parts', [])) if 'text' in p), '')
        if 'cancel_fixture' in latest_prompt and not title:
            delayed_request.set()
            release_response.wait(45)
        tool_result = any('functionResponse' in p for p in parts)
        if 'accept_fixture' in latest_prompt and not title and tool_result:
            # Agy may background the held command. Keep the follow-up model
            # call active until the test has observed approval being released.
            release_tool.wait(45)
        response_parts = [{'text': 'Local fixture response.'}]
        if any(marker in latest_prompt for marker in ['approval_fixture', 'accept_fixture']) and not title and not tool_result:
            command = 'python3 -c "print(42)"'
            if 'accept_fixture' in latest_prompt:
                command = shlex.join([sys.executable, '-c',
                    "import os, pathlib, urllib.request; "
                    "pathlib.Path('child-home.txt').write_text(os.environ['HOME']); "
                    "pathlib.Path('~/new-top-level.txt').expanduser().write_text('real home'); "
                    f"print(urllib.request.urlopen('http://127.0.0.1:{server.server_port}/held-tool', timeout=50).read().decode())"])
            response_parts = [{'functionCall': {'name': 'run_command', 'args': {
                'CommandLine': command,
                'WaitMsBeforeAsync': 1000,
                'toolSummary': 'Local fixture print',
            }}}]
        response = {
            'candidates': [{'content': {'role': 'model', 'parts': response_parts},
                            'finishReason': 'STOP', 'index': 0}],
            'usageMetadata': {'promptTokenCount': 1234, 'candidatesTokenCount': 12,
                              'totalTokenCount': 1246},
        }
        try:
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            self.wfile.write(('data: ' + json.dumps(response) + '\n\n').encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # The cancellation fixture intentionally closes this request.


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = dict(os.environ, HOME=config['home'], TERM='xterm-256color',
           GEMINI_API_KEY='fake-local-fixture',
           GOOGLE_GEMINI_BASE_URL=f'http://127.0.0.1:{server.server_port}',
           AGY_CLI_DISABLE_AUTO_UPDATE='1', CM_AGY_TEST_EVENTS=str(event_file))
for key in ['CONFIG', 'CACHE', 'STATE', 'DATA', 'RUNTIME']:
    directory = root / key.lower()
    directory.mkdir(mode=0o700)
    env[f'XDG_{key}_HOME' if key != 'RUNTIME' else 'XDG_RUNTIME_DIR'] = str(directory)


def events():
    # Every recorder appends once; ignore a final incomplete write while polling.
    return [json.loads(line) for line in event_file.read_text().splitlines(keepends=True)
            if line.endswith('\n')]


class Terminal:
    def __init__(self, extra=(), managed=True):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        launch_args = config['launch_args'] if managed else []
        self.process = subprocess.Popen([config['binary'], *launch_args, *extra], cwd=work, env=env,
                                        stdin=slave, stdout=slave, stderr=slave,
                                        start_new_session=True)
        os.close(slave)
        self.output = b''
        self.trusted = False

    def send(self, value):
        os.write(self.master, value)

    def prompt(self, text):
        self.send(b"\x1b[200~" + text.encode() + b"\x1b[201~\r")

    def wait(self, phase, predicate):
        # Exceeds agy's configured 10-second hook timeout, with the awaited
        # phase named on failure. Readiness is observed, never timed by sleeps.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], 0.05)[0]:
                try:
                    self.output += os.read(self.master, 65536)
                except OSError:
                    raise AssertionError(f'{phase}: agy exited ({self.process.poll()})')
            if b'Do you trust the contents' in self.output and not self.trusted:
                self.send(b'\r')
                self.trusted = True
            if predicate():
                return
        summary = [(e['event'], e['body'].get('agent_state')) for e in events()]
        raise AssertionError(f'{phase}: timed out; events={summary}; terminal={self.output[-1200:].decode(errors='replace')!r}')

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
        os.close(self.master)


checkpoints = []
offset = 0


def current():
    return events()[offset:]


def status(activity):
    snapshots = [e for e in current() if e['event'] == 'status-update']
    return bool(snapshots) and snapshots[-1]['body'].get('agent_state') == activity


def approval_pending():
    snapshots = [e for e in current() if e['event'] == 'status-update']
    return bool(snapshots) and bool(snapshots[-1]['body'].get('tool_confirmation_pending'))


def checkpoint(phase):
    global offset
    all_events = events()
    # Freeze actual files at the observed phase so Rust can check its reader
    # against startup, in-flight prompts, clear and resume independently.
    cli = Path(config['home']) / '.gemini/antigravity-cli'
    snapshot = root / 'checkpoints' / phase
    snapshot.mkdir(parents=True)
    sid = next((e['body'].get('conversation_id') or e['body'].get('conversationId')
                for e in reversed(all_events)
                if e['body'].get('conversation_id') or e['body'].get('conversationId')), None)
    if sid:
        transcript = cli / 'brain' / sid / '.system_generated/logs/transcript.jsonl'
        if transcript.exists():
            shutil.copyfile(transcript, snapshot / 'transcript.jsonl')
        database = cli / 'conversation_summaries.db'
        if database.exists():
            with sqlite3.connect(f'file:{database}?mode=ro', uri=True) as source:
                with sqlite3.connect(snapshot / 'conversation_summaries.db') as dest:
                    source.backup(dest)
    checkpoints.append({'phase': phase, 'events': all_events[offset:], 'snapshot': str(snapshot)})
    offset = len(all_events)


terminal = Terminal()
try:
    terminal.wait('initial idle and original status display',
                  lambda: status('idle') and b'custom-status-visible' in terminal.output)
    checkpoint('startup')
    terminal.prompt('completion_fixture')
    terminal.wait('completed turn and context snapshot', lambda: any(
        e['event'] == 'status-update' and e['body'].get('agent_state') == 'idle'
        and e['body'].get('context_window', {}).get('total_input_tokens', 0) > 0
        for e in current()) and any(e['event'] == 'stop' for e in current()))
    conversation = next(e['body']['conversation_id'] for e in current()
                        if e['event'] == 'status-update' and e['body'].get('conversation_id'))
    def current_title():
        database = Path(config['home']) / '.gemini/antigravity-cli/conversation_summaries.db'
        if not database.exists():
            return None
        try:
            with sqlite3.connect(f'file:{database}?mode=ro', uri=True) as conn:
                return conn.execute('SELECT title FROM conversation_summaries WHERE conversation_id = ?',
                                    (conversation,)).fetchone()
        except sqlite3.Error:
            return None
    terminal.wait('generated title persisted', lambda: current_title() == ('Local fixture response.',))
    checkpoint('complete')
    terminal.prompt('/rename Renamed fixture')
    terminal.wait('rename persisted without another turn', lambda: current_title() == ('Renamed fixture',))
    checkpoint('rename')
    terminal.prompt('cancel_fixture')
    terminal.wait('delayed request in flight', lambda: delayed_request.is_set() and status('working'))
    checkpoint('working')
    terminal.send(b'\x1b')
    terminal.wait('idle after Escape', lambda: status('idle'))
    checkpoint('cancel')
    release_response.set()
    terminal.prompt('approval_fixture')
    terminal.wait('native permission prompt', lambda: b'Run this command?' in terminal.output
                  and approval_pending())
    checkpoint('approval')
    terminal.send(b'\x1b')
    terminal.wait('idle after cancelling permission', lambda: status('idle'))
    checkpoint('cancel-approval')
    terminal.prompt('accept_fixture')
    terminal.wait('native permission before accepting', lambda: approval_pending())
    checkpoint('accept-approval')
    terminal.send(b'\r')
    terminal.wait('active execution after accepting permission', lambda: tool_started.is_set()
                  and not approval_pending() and (status('working') or status('tool_use')))
    checkpoint('accepted')
    release_tool.set()
    terminal.wait('idle after accepted tool finishes', lambda: status('idle'))
    checkpoint('accepted-complete')
    compact_requested.set()
    terminal.prompt('/compact')
    terminal.wait('compaction request in flight', lambda: compact_started.is_set()
                  and status('working'))
    checkpoint('compacting')
    release_compact.set()
    terminal.wait('idle after compaction', lambda: status('idle'))
    checkpoint('compacted')
    compact_requested.clear()
    terminal.prompt('/clear')
    terminal.wait('new conversation after clear', lambda: status('idle') and any(
        e['event'] == 'status-update' and e['body'].get('conversation_id')
        and e['body']['conversation_id'] != conversation for e in current()))
    checkpoint('clear')
finally:
    release_response.set()
    release_tool.set()
    release_compact.set()
    terminal.close()

offset = len(events())
terminal = Terminal(['--conversation', conversation])
try:
    terminal.wait('resumed conversation identity and context', lambda: any(
        e['event'] == 'status-update' and e['body'].get('agent_state') == 'idle'
        and e['body'].get('conversation_id') == conversation
        and e['body'].get('context_window', {}).get('total_input_tokens', 0) > 0
        for e in current()))
    checkpoint('resume')
    terminal.prompt('after_compact_fixture')
    terminal.wait('fresh usage after compaction', lambda: status('idle')
                  and any(e['event'] == 'stop' for e in current()))
    checkpoint('after-compact')
finally:
    terminal.close()

# A normal agy can update its own settings (e.g. workspace trust). Check our
# preservation contract before starting that independent bare invocation.
assert real_settings.read_bytes() == original_settings, 'managed launches changed global settings'
offset = len(events())
terminal = Terminal(['--conversation', conversation], managed=False)
try:
    terminal.wait('bare resume from shared real-home state', lambda: any(
        e['event'] == 'user-status' and e['body'].get('agent_state') == 'idle'
        and e['body'].get('conversation_id') == conversation
        and e['body'].get('context_window', {}).get('total_input_tokens', 0) > 0
        for e in current()) and b'custom-status-visible' in terminal.output)
    assert all(e['event'] == 'user-status' and e['home'] == config['home']
               for e in current()), 'bare agy must use only the original status command'
finally:
    terminal.close()
    server.shutdown()
    server.server_close()
print(json.dumps(checkpoints))
