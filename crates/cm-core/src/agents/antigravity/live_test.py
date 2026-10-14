"""Optional real-agy probe. All inference uses this process's loopback fixture."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
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
delayed_request = threading.Event()
release_response = threading.Event()


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        title = 'title generator' in json.dumps(body.get('systemInstruction', {}))
        parts = [p for c in body.get('contents', []) for p in c.get('parts', [])]
        latest_prompt = next((p.get('text', '') for c in reversed(body.get('contents', []))
                              if c.get('role') == 'user' for p in reversed(c.get('parts', []))
                              if 'text' in p), '')
        if 'cancel_fixture' in latest_prompt and not title:
            delayed_request.set()
            release_response.wait(45)
        response_parts = [{'text': 'Local fixture response.'}]
        if 'approval_fixture' in latest_prompt and not title and not any('functionResponse' in p for p in parts):
            response_parts = [{'functionCall': {'name': 'run_command', 'args': {
                'CommandLine': 'python3 -c "print(42)"', 'Cwd': str(work),
                'WaitMsBeforeAsync': 1000, 'toolAction': 'Running command',
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
    def __init__(self, extra=()):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        self.process = subprocess.Popen([config['binary'], *extra], cwd=work, env=env,
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


def checkpoint(phase):
    global offset
    all_events = events()
    checkpoints.append({'phase': phase, 'events': all_events[offset:]})
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
    checkpoint('complete')
    terminal.prompt('cancel_fixture')
    terminal.wait('delayed request in flight', lambda: delayed_request.is_set() and status('working'))
    terminal.send(b'\x1b')
    terminal.wait('idle after Escape', lambda: status('idle'))
    checkpoint('cancel')
    release_response.set()
    terminal.prompt('approval_fixture')
    terminal.wait('native permission prompt', lambda: b'Run this command?' in terminal.output
                  and any(e['event'] == 'status-update' and e['body'].get('tool_confirmation_pending')
                          for e in current()))
    checkpoint('approval')
    terminal.send(b'\x1b')
    terminal.wait('idle after cancelling permission', lambda: status('idle'))
    checkpoint('cancel-approval')
finally:
    release_response.set()
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
finally:
    terminal.close()
    server.shutdown()
    server.server_close()
print(json.dumps(checkpoints))
