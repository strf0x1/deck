"""Real VM/daemon driver; credentials are synthetic and never enter evidence."""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time


class Evidence:
    def __init__(self, output):
        self.output = output
        self.rows = []

    def check(self, name, passed, observed=None):
        self.rows.append(dict(check=name, passed=bool(passed), observed=observed))
        (self.output / 'results.json').write_text(json.dumps(self.rows, indent=2) + '\n')
        print(('PASS ' if passed else 'FAIL ') + name, flush=True)


class Guest:
    def __init__(self, task, path):
        self.task = task
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(50)
        self.socket.connect(str(path))
        self.reader = self.socket.makefile('rb')

    def call(self, **request):
        self.socket.sendall(json.dumps(request).encode() + b'\n')
        reply = json.loads(self.reader.readline())
        if 'error' in reply:
            raise RuntimeError(reply['error'])
        return reply['result']

    def python(self, code):
        return self.call(python=code)

    def run(self, *argv, timeout=15):
        return self.call(argv=list(argv), timeout=timeout)

    def curl(self, url, *args):
        return self.run('curl', '--noproxy', '', '-x', 'http://127.0.0.1:8082',
                        '--max-time', '12', '-sS', '-i', *args, url)

    def model(self, model='acceptance-model'):
        return self.run('curl', '-sS', '-i', '--max-time', '12',
                        'http://127.0.0.1:8080/v1/chat/completions',
                        '-H', 'Authorization: Bearer guest-decoy',
                        '-H', 'Proxy-Authorization: guest-proxy-decoy',
                        '-H', 'Cookie: guest-cookie-decoy', '-H', 'x-api-key: guest-key-decoy',
                        '-H', 'x-invented: guest-extra', '-H', 'HTTP-Referer: https://deck.example',
                        '-H', 'Content-Type: application/json', '-d',
                        json.dumps(dict(model=model, messages=[dict(role='user', content='probe')])) )

    def close(self):
        self.reader.close()
        self.socket.close()


class Daemon:
    def __init__(self, output, upstream, credential_file, gateway=None, extra_env=None):
        self.output = output
        output.mkdir(parents=True)
        self.data = output / 'data'
        self.data.mkdir()
        self.tasks = []
        self.guests = []
        env = os.environ.copy()
        env.update(DECK_CREDENTIALS=str(credential_file), CONSTRUCT_BASE_URL=upstream,
                   CONSTRUCT_MODEL='acceptance-model')
        env.update(extra_env or {})
        if gateway:
            env['DECK_GATEWAY_EXECUTABLE'] = str(gateway)
        data_hash = 0xcbf29ce484222325
        for byte in str(self.data.resolve()).encode():
            data_hash = ((data_hash ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
        runtime = (Path(env['XDG_RUNTIME_DIR']) / 'deck' if env.get('XDG_RUNTIME_DIR')
                   else Path(f'/tmp/deck-{os.getuid()}'))
        self.sockets = runtime / f'{data_hash:x}'
        self.log = (output / 'daemon.log').open('w')
        self.process = subprocess.Popen([env['DECK_VM_EXECUTABLE'], '--data', str(self.data),
                                         '--idle-ms', '600000'], env=env,
                                        stdout=self.log, stderr=self.log)
        for _ in range(100):
            try:
                self.control(op='tasks')
                break
            except OSError:
                if self.process.poll() is not None:
                    raise RuntimeError('daemon exited; inspect daemon.log')
                time.sleep(.05)
        else:
            self.close()
            raise RuntimeError('daemon did not become ready')

    def control(self, **request):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(60)
            stream.connect(str(self.sockets / 'deckd.sock'))
            stream.sendall(json.dumps(request).encode() + b'\n')
            reply = json.loads(stream.makefile('rb').readline())
            if 'error' in reply:
                raise RuntimeError(reply['error'])
            return reply['ok']

    def guest(self, name):
        project = self.output / name
        project.mkdir()
        shutil.copy(Path(__file__).with_name('guest.py'), project / 'guest.py')
        task = self.control(op='create', spec=dict(argv=['/usr/bin/python3', '-u',
                                                       '/workspace/guest.py'], cwd=str(project)))['id']
        self.tasks.append(task)
        self.control(op='start', id=task)
        guest = Guest(task, self.sockets / 'tasks' / f'{task}.sock')
        self.guests.append(guest)
        return guest

    def allow(self, guest, destination, scope='task'):
        items = self.control(op='approvals', task=guest.task)
        item = next(item for item in items if item['destination'] == destination)
        return self.control(op='decide_approval', id=item['id'], decision=scope)

    def close(self):
        for guest in self.guests:
            guest.close()
        for task in reversed(self.tasks):
            try:
                state = self.control(op='task', id=task)['state']['state']
                if state == 'running':
                    self.control(op='stop', id=task)
            except (OSError, RuntimeError) as error:
                print(f'stop {task}: {error}', flush=True)
            try:
                self.control(op='destroy', id=task)
            except (OSError, RuntimeError) as error:
                print(f'destroy {task}: {error}', flush=True)
        self.process.terminate()
        self.process.wait(timeout=10)
        self.log.close()
