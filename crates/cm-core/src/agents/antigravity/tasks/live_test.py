"""Real agy background lifecycle against a loopback model and a private home."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import sqlite3
import struct
import subprocess
import sys
import termios
import threading
import time

config = json.load(sys.stdin)
root = Path(config['root'])
home = root / 'home'
cli = home / '.gemini/antigravity-cli'
(cli / 'cache').mkdir(parents=True)
(cli / 'cache/onboarding.json').write_text(json.dumps({
    'onboardingComplete': True, 'consumerOnboardingComplete': True,
    'enterpriseOnboardingComplete': True,
}))
events_path = root / 'events.jsonl'
events_path.touch()
recorder = root / 'record'
recorder.write_text('''#!/usr/bin/env python3
import json, os, sys
body = json.load(sys.stdin)
fd = os.open(os.environ['CM_AGY_TEST_EVENTS'], os.O_APPEND | os.O_WRONLY)
os.write(fd, (json.dumps({'event': sys.argv[-1], 'body': body}) + '\\n').encode())
os.close(fd)
print('{}')
''')
recorder.chmod(0o700)


def hook(event):
    return {'type': 'command', 'command': shlex.quote(str(recorder)) + ' ' + event, 'timeout': 10}


(cli / 'settings.json').write_text(json.dumps({
    'modelProvider': 'gemini', 'enableTelemetry': False,
    'statusLine': hook('status-update'),
}))
settings_dir = home / '.gemini/config'
settings_dir.mkdir()
(settings_dir / 'hooks.json').write_text(json.dumps({'probe': {
    'SessionStart': [hook('session-start')], 'PreInvocation': [hook('prompt-submit')],
    'Stop': [hook('stop')], 'PostToolUse': [{'matcher': '*', 'hooks': [hook('post-tool-use')]}],
}}))
started = threading.Event()
release = threading.Event()


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        assert self.path == '/held'
        started.set()
        release.wait(45)
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'finished\n')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        title = 'title generator' in json.dumps(body.get('systemInstruction', {}))
        result = any('functionResponse' in part for item in body.get('contents', [])
                     for part in item.get('parts', []))
        parts = [{'text': 'Waiting for the background command.'}]
        if not title and not result:
            command = shlex.join([sys.executable, '-c',
                f"import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:{server.server_port}/held', timeout=50).read().decode())"])
            parts = [{'functionCall': {'name': 'run_command', 'args': {
                'CommandLine': command, 'WaitMsBeforeAsync': 1000,
                'toolSummary': 'Held background fixture',
            }}}]
        response = {'candidates': [{'content': {'role': 'model', 'parts': parts},
                                   'finishReason': 'STOP', 'index': 0}],
                    'usageMetadata': {'promptTokenCount': 100, 'candidatesTokenCount': 10,
                                      'totalTokenCount': 110}}
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()
        self.wfile.write(('data: ' + json.dumps(response) + '\n\n').encode())


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = dict(os.environ, HOME=str(home), TERM='xterm-256color',
           GEMINI_API_KEY='fake-local-fixture', AGY_CLI_DISABLE_AUTO_UPDATE='1',
           GOOGLE_GEMINI_BASE_URL=f'http://127.0.0.1:{server.server_port}',
           CM_AGY_TEST_EVENTS=str(events_path))
for key in ['CONFIG', 'CACHE', 'STATE', 'DATA', 'RUNTIME']:
    directory = root / key.lower()
    directory.mkdir(mode=0o700)
    env[f'XDG_{key}_HOME' if key != 'RUNTIME' else 'XDG_RUNTIME_DIR'] = str(directory)
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
process = subprocess.Popen([config['binary']], cwd=root, env=env, stdin=slave,
                           stdout=slave, stderr=slave, start_new_session=True)
os.close(slave)
output = b''
trusted = False


def events():
    return [json.loads(line) for line in events_path.read_text().splitlines(keepends=True)
            if line.endswith('\n')]


def wait(phase, predicate):
    global output, trusted
    deadline = time.monotonic() + 30  # Exceeds the 10-second production hook deadline.
    while time.monotonic() < deadline:
        if select.select([master], [], [], 0.05)[0]:
            try:
                output += os.read(master, 65536)
            except OSError:
                raise AssertionError(f'{phase}: agy exited ({process.poll()})')
        if b'Do you trust the contents' in output and not trusted:
            os.write(master, b'\r')
            trusted = True
        if predicate():
            return
    raise AssertionError(f'{phase}: timed out; terminal={output[-1600:]!r}')


def task_rows():
    with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as conn:
        return list(conn.execute('SELECT idx,status FROM steps WHERE length(task_details)>0'))


def capture(phase):
    target = root / f'{phase}.db'
    with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as source:
        with sqlite3.connect(target) as dest:
            source.backup(dest)


try:
    wait('startup', lambda: any(e['body'].get('agent_state') == 'idle' for e in events()))
    os.write(master, b'\x1b[200~background fixture\x1b[201~\r')
    wait('permission prompt', lambda: any(e['body'].get('tool_confirmation_pending') for e in events()))
    os.write(master, b'\r')
    wait('background Stop', lambda: started.is_set() and any(
        e['event'] == 'stop' and e['body'].get('fullyIdle') is False for e in events()))
    stopped = events()
    sid = next(e['body']['conversationId'] for e in reversed(stopped) if e['event'] == 'stop')
    database = cli / 'conversations' / f'{sid}.db'
    wait('running task persisted', lambda: any(status == 2 for _, status in task_rows()))
    capture('running')
    # Delay completion beyond the monitor's coalescing window. Synchronization
    # is the held HTTP request, independent of subprocess speed.
    release.set()
    wait('completed task persisted', lambda: all(status == 3 for _, status in task_rows()))
    capture('completed')
    wait('completion Stop hook', lambda: any(
        e['event'] == 'stop' and e['body'].get('fullyIdle') is True
        for e in events()[len(stopped):]))
    print(json.dumps({'background': stopped, 'completed': events()[len(stopped):]}))
finally:
    release.set()
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    os.close(master)
    server.shutdown()
    server.server_close()
