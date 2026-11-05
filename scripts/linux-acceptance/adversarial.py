#!/usr/bin/env python3
"""Live resolver-cache, daemon-outage expiry and malformed GitHub acceptance."""
import argparse
import json
import os
from pathlib import Path
import secrets
import random
import shlex
import socket
import struct
import subprocess
import time

from fixture import Fixture
from support import Daemon, Evidence


def status(guest, destination):
    return guest.python('import socket\ns=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM);'
        's.settimeout(8);s.connect((2,1028))\n'
        f's.sendall({("CONNECT "+destination+" HTTP/1.1\r\n\r\n").encode()!r})\n'
        'result=s.recv(4096).decode();s.close()')


def restart(daemon):
    daemon.process = subprocess.Popen([os.environ['DECK_VM_EXECUTABLE'], '--data', str(daemon.data),
        '--idle-ms', '600000'], env=daemon.test_env, stdout=daemon.log, stderr=daemon.log)
    for _ in range(100):
        try:
            daemon.control(op='tasks')
            return
        except (OSError, RuntimeError):
            time.sleep(.05)
    raise RuntimeError('test daemon restart failed')


def cache_and_expiry(output, fixture, credentials, evidence):
    hosts, nss = output/'namespace-hosts', output/'namespace-nss'
    hosts.write_text('140.82.112.3 rebind.example held.example other.example\n')
    nss.write_text('hosts: files\n')
    gateway = output/'namespace-gateway'
    # These mounts exist only in a new user/mount namespace for this gateway.
    # The host /etc files and other applications' resolvers are untouched.
    command = ['unshare', '-Urm', '/bin/sh', '-c',
        'mount --bind "$1" /etc/hosts && mount --bind "$2" /etc/nsswitch.conf && '
        'shift 2 && exec "$@"', 'acceptance', str(hosts), str(nss),
        os.environ['DECK_GATEWAY_EXECUTABLE']]
    gateway.write_text('#!/bin/sh\nexec '+shlex.join(command)+' "$@" --web-test-connect 127.0.0.1\n')
    gateway.chmod(0o700)
    daemon = Daemon(output/'cache', f'http://127.0.0.1:{fixture.port}/v1', credentials, gateway)
    daemon.test_env = os.environ.copy()
    daemon.test_env.update(DECK_CREDENTIALS=str(credentials), CONSTRUCT_MODEL='acceptance-model',
        CONSTRUCT_BASE_URL=f'http://127.0.0.1:{fixture.port}/v1', DECK_GATEWAY_EXECUTABLE=str(gateway))
    try:
        guest = daemon.guest('cache-project')
        destination = f'rebind.example:{fixture.port}'
        evidence.check('cache: initial ungranted request refused', '403' in status(guest,destination))
        daemon.allow(guest,destination)
        start = time.monotonic()
        response = guest.curl('http://'+destination+'/mirror')
        evidence.check('cache: public resolved address admitted', '200 OK' in response['out'])
        hosts.write_text('127.0.0.1 rebind.example\n140.82.112.3 held.example other.example\n')
        response = guest.curl('http://'+destination+'/mirror')
        evidence.check('cache: prior public address used before thirty-second expiry', '200 OK' in response['out'])
        time.sleep(max(0,32-(time.monotonic()-start)))
        before = len(fixture.snapshot())
        response = guest.curl('http://'+destination+'/mirror')
        evidence.check('cache: changed private answer refused after cache expiry with no dial',
            '403' in response['out'] and 'loopback' in response['out'] and len(fixture.snapshot())==before, response)
        held = f'held.example:{fixture.port}'
        status(guest,held)
        daemon.allow(guest,held)
        grants_path = daemon.data/'tasks'/guest.task/'grants.json'
        grants = json.loads(grants_path.read_text())
        deadline = int(time.time()*1000)+4000
        for grant in grants:
            if grant['destination']==held:
                grant['expires_ms']=deadline
        # Host-side fixture shortens the persisted 12-hour production grant;
        # the actual gateway deadline/monitor logic remains unchanged.
        grants_path.write_text(json.dumps(grants))
        response = guest.python('import socket,builtins\n'
            's=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM);s.settimeout(8);s.connect((2,1028))\n'
            f's.sendall({("CONNECT "+held+" HTTP/1.1\r\n\r\n").encode()!r})\n'
            'result=s.recv(4096).decode();builtins.held=s')
        evidence.check('expiry: live tunnel admitted before shortened host fixture deadline', '200 Connection Established' in response)
        daemon.process.terminate()
        daemon.process.wait(timeout=10)
        response = guest.python('import builtins,time\nstart=time.monotonic();'
            'data=builtins.held.recv(4096);builtins.held.close();'
            'result={"eof":data==b"","elapsed":time.monotonic()-start}')
        evidence.check('expiry: open tunnel closes while daemon is absent',response['eof'] and response['elapsed']<7,response)
        evidence.check('expiry: retry denied with daemon still absent', '403' in status(guest,held))
        response = guest.run('/bin/echo','guest-still-running')
        evidence.check('expiry: holder and guest survive control-daemon outage', 'guest-still-running' in response['out'])
        restart(daemon)
        evidence.check('expiry: restarted daemon adopts still-running VM',daemon.control(op='task',id=guest.task)['state']['state']=='running')
        evidence.check('expiry: restarted daemon does not restore expired grant', '403' in status(guest,held))
    finally:
        if daemon.process.poll() is not None:
            restart(daemon)
        daemon.close()


def github_malformed(output, fixture, credentials, evidence):
    daemon=Daemon(output/'github',f'http://127.0.0.1:{fixture.port}/v1',credentials,
                  extra_env={'DECK_GITHUB_REPOSITORY':'acceptance/fixture'})
    try:
        guest=daemon.guest('github-project')
        initial=len(fixture.snapshot())
        ask=json.dumps({'operation':'publish','branch':'deck/'+guest.task}).encode()
        bodies=[('not JSON',b'not-json'),('closed operation',b'{"operation":"merge"}'),
                ('missing bundle',ask),('excess bundle length',ask+struct.pack('>I',16*1024*1024+1)),
                ('truncated bundle',ask+struct.pack('>I',8)+b'xx'),
                ('pkt-line is not a bundle',ask+struct.pack('>I',8)+b'0008oops'),
                ('invalid bundle magic',ask+struct.pack('>I',12)+b'not-a-bundle')]
        corpus = [b'0000', b'0001', b'0002', b'ffff', b'0003bad', b'zzzz',
                  b'# v2 git bundle\n\xff\n\nPACK', b'# v2 git bundle\nno-newline']
        generator = random.Random(4)
        corpus += [generator.randbytes(length) for length in [1, 5, 64, 256]]
        bodies += [('malformed git corpus '+str(index), ask+struct.pack('>I',len(payload))+payload)
                   for index,payload in enumerate(corpus)]
        for name,body in bodies:
            request=b'POST /operations/publish HTTP/1.1\r\nHost: github\r\nContent-Length: '+str(len(body)).encode()+b'\r\n\r\n'+body
            response=guest.python('import socket,time\ns=socket.create_connection(("127.0.0.1",8084),3);'
                's.settimeout(9);start=time.monotonic()\n'
                f's.sendall({request!r})\n'
                'parts=[]\nwhile True:\n chunk=s.recv(8192)\n if not chunk:break\n parts.append(chunk)\n'
                'result={"elapsed":time.monotonic()-start,"reply":b"".join(parts).decode()};s.close()')
            evidence.check('github: bounded refusal for '+name,
                response['elapsed']<8 and any((' '+str(code)+' ') in response['reply'] for code in [400,403,404,411,413]),response)
        evidence.check('github: malformed cases create no approval',not daemon.control(op='approvals',task=guest.task))
        # No GitHub token is configured. The malformed paths should not fetch one.
        events=(daemon.data/'tasks'/guest.task/'events.jsonl').read_text()
        evidence.check('github: malformed cases record no credential use','credential_used' not in events)
        evidence.check('github: unrelated model route remains usable', '200 OK' in guest.model()['out'])
        evidence.check('github: local model fixture received only the final model request',len(fixture.snapshot())==initial+1)
    finally:
        daemon.close()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--section',choices=['all','cache','github'],default='all')
    args=parser.parse_args()
    output=args.output.resolve();output.mkdir(parents=True)
    evidence=Evidence(output)
    credentials=output/'synthetic-credentials.json';credentials.touch(mode=0o600)
    credentials.write_text(json.dumps({'OPENAI_API_KEY':secrets.token_hex(32)}))
    fixture=Fixture()
    try:
        if args.section in ['all','cache']:
            cache_and_expiry(output,fixture,credentials,evidence)
        if args.section in ['all','github']:
            github_malformed(output,fixture,credentials,evidence)
    finally:
        fixture.close();credentials.unlink()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


if __name__=='__main__':
    raise SystemExit(main())
