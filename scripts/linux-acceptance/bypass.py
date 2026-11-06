#!/usr/bin/env python3
"""Exercise broader ADR 0004 bypass cases against real Linux/KVM guests."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import socket
import subprocess
import hashlib
import time

from fixture import Fixture
from support import Daemon, Evidence


def native_connect(guest, destination):
    return guest.python('import socket\n'
        's=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM);s.settimeout(12);s.connect((2,1028))\n'
        f's.sendall({("CONNECT " + destination + " HTTP/1.1\r\n\r\n").encode()!r})\n'
        'result=s.recv(4096).decode();s.close()')


def production(output, fixture, credential_file, token, evidence):
    daemon = Daemon(output / 'production', f'http://127.0.0.1:{fixture.port}/v1', credential_file)
    sentinel = socket.socket()
    sentinel.bind(('127.0.0.1', 0))
    sentinel.listen()
    sentinel.settimeout(.1)
    try:
        a, b = daemon.guest('project-a'), daemon.guest('project-b')
        for guest in [a, b]:
            before = len(fixture.snapshot())
            response = guest.model('unapproved-model')
            evidence.check('disallowed model never reaches provider ' + guest.task,
                           '403' in response['out'] and len(fixture.snapshot()) == before)
            response = guest.model()
            captured = fixture.snapshot()[-1]
            headers = {key.lower(): value for key, value in captured['headers'].items()}
            evidence.check('host credential replaces guest headers ' + guest.task,
                response['rc'] == 0 and '200 OK' in response['out']
                and headers.get('authorization') == 'Bearer ' + token
                and all(key not in headers for key in ['cookie', 'proxy-authorization', 'x-api-key', 'x-invented'])
                and headers.get('http-referer') == 'https://deck.example')
            evidence.check('credential absent from guest environment ' + guest.task,
                           token not in guest.run('/usr/bin/env')['out'])
            evidence.check('credential absent from task artifacts ' + guest.task,
                all(token.encode() not in path.read_bytes() for path in
                    (daemon.data / 'tasks' / guest.task).glob('*')
                    if path.is_file() and path.suffix in ['.json', '.jsonl', '.log']))
        port = sentinel.getsockname()[1]
        response = a.run('curl', '--noproxy', '*', '--max-time', '2', f'http://127.0.0.1:{port}')
        try:
            connection, _ = sentinel.accept()
            connection.close()
            contacted = True
        except socket.timeout:
            contacted = False
        evidence.check('old host loopback sentinel receives no guest dial', response['rc'] != 0 and not contacted)
        destination = 'ssh.github.com:443'
        evidence.check('task A initially denied on native vsock', '403' in native_connect(a, destination))
        daemon.allow(a, destination)
        evidence.check('task A grant opens its native vsock tunnel', '200 Connection Established' in native_connect(a, destination))
        evidence.check('task B cannot reuse task A grant on same vsock port', '403' in native_connect(b, destination))
        response = b.python('import socket\ns=socket.socket(socket.AF_UNIX);s.settimeout(2)\n'
            f'target={str(daemon.sockets / "tasks" / (a.task + ".web"))!r}\n'
            'try:\n s.connect(target);result="connected"\nexcept OSError as e:result=str(e)\ns.close()')
        evidence.check('task A host socket path is absent from task B guest', 'No such file' in response, response)
        for cid in [3, 4]:
            response = b.python('import socket\ns=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM);s.settimeout(2)\n'
                f'try:\n s.connect(({cid},1028));result="connected"\n'
                'except OSError as e:result=str(e)\ns.close()')
            evidence.check(f'guest cannot address another guest CID {cid}', response != 'connected', response)
        address_classes = ['0.0.0.0', '127.0.0.1', '10.0.0.1', '169.254.169.254',
                           '100.64.0.1', '192.0.2.1', '198.51.100.1', '203.0.113.1',
                           '224.0.0.1', '255.255.255.255', '[::]', '[::1]', '[fd00::1]',
                           '[fe80::1]', '[ff02::1]', '[2001:db8::1]',
                           '[::ffff:127.0.0.1]', '[64:ff9b::7f00:1]']
        for address in address_classes:
            before = len(daemon.control(op='approvals', task=b.task))
            reply = native_connect(b, address + ':8443')
            after = len(daemon.control(op='approvals', task=b.task))
            evidence.check('refused address class with no approval: ' + address, '403' in reply and before == after)
        for kind, family, target in [('DNS', 'AF_INET', ('1.1.1.1', 53)),
                                     ('QUIC-port', 'AF_INET', ('1.1.1.1', 443)),
                                     ('IPv6 UDP', 'AF_INET6', ('2606:4700:4700::1111', 443))]:
            packet = (b'\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00'
                      b'\x07example\x03com\x00\x00\x01\x00\x01') if kind == 'DNS' else b'\xc0\x00\x00\x00\x01'+bytes(1195)
            response = b.python('import socket\n'
                f's=socket.socket(socket.{family},socket.SOCK_DGRAM);s.settimeout(2)\n'
                f'try:\n s.sendto({packet!r},{target!r});result="sent"\n'
                'except OSError as e:result=str(e)\ns.close()')
            evidence.check(kind + ' datagram has no route', 'unreachable' in response.lower(), response)
        response = b.python('import socket\n'
            'try:\n s=socket.socket(socket.AF_INET,socket.SOCK_RAW,socket.IPPROTO_ICMP);'
            's.sendto(b"\\x08\\0\\xf7\\xff\\0\\0\\0\\0",("1.1.1.1",0));result="sent";s.close()\n'
            'except OSError as e:result=str(e)')
        evidence.check('raw ICMP socket cannot reach public IP', 'unreachable' in response.lower(), response)
        # Hold a real VM model stream, then revoke its synthetic host credential.
        fixture.streaming.set()
        response = a.python('import subprocess,builtins\n'
            'builtins.model=subprocess.Popen(["curl","-sS","--max-time","15",'
            '"http://127.0.0.1:8080/v1/chat/completions","-H","Content-Type: application/json",'
            '"-d",\'{"model":"acceptance-model","messages":[]}\'],stdout=subprocess.PIPE,stderr=subprocess.PIPE)\n'
            'result=builtins.model.pid')
        time.sleep(.5)
        start = time.monotonic()
        daemon.control(op='revoke_credential', name='OPENAI_API_KEY')
        response = a.python('import builtins\nout,err=builtins.model.communicate(timeout=8);'
                            'result={"rc":builtins.model.returncode,"started":b"started" in out}')
        evidence.check('revocation terminates live VM model stream', time.monotonic()-start < 8 and response['started'], response)
        before = len(fixture.snapshot())
        response = b.model()
        evidence.check('revoked credential blocks next VM request before provider',
                       'deck_credential_revoked' in response['out'] and len(fixture.snapshot()) == before)
        fixture.streaming.clear()
        fixture.release.set()
        rotated = secrets.token_hex(32)
        credential_file.write_text(json.dumps({'OPENAI_API_KEY': rotated}))
        response = b.model()
        headers = {key.lower(): value for key, value in fixture.snapshot()[-1]['headers'].items()}
        evidence.check('credential rotation restores live VM request without restart',
                       '200 OK' in response['out'] and headers.get('authorization') == 'Bearer '+rotated)
        fixture.tool_step = 0
        before = len(fixture.snapshot())
        response = b.run('/usr/local/bin/construct', 'run', '--no-learn',
                         'Run the scripted boundary test', timeout=35)
        requests = fixture.snapshot()[before:]
        notes = []
        for request in requests:
            if request['body']:
                messages = json.loads(request['body']).get('messages', [])
                notes.extend(str(message.get('content', '')) for message in messages if message.get('role') == 'tool')
        pending = daemon.control(op='approvals', task=b.task)
        blocked = [item for item in pending if item['destination'] == 'example.com:443']
        evidence.check('real Construct tool result names blocked destination and approval',
            response['rc'] == 0 and blocked and any('[deck boundary]' in note
                and 'example.com:443' in note and blocked[-1]['id'] in note for note in notes),
            dict(rc=response['rc'], tool_results=len(notes), approvals=len(blocked), error=response['err']))
        fixture.tool_step = None
        # Identify only this test task's gateway; never signal an unrelated process.
        task = daemon.control(op='task', id=a.task)
        pid = task['holder']['pid']
        children = Path(f'/proc/{pid}/task/{pid}/children').read_text().split()
        gateways = [int(child) for child in children
                    if b'deck-gateway' in Path(f'/proc/{child}/cmdline').read_bytes()
                    and a.task.encode() in Path(f'/proc/{child}/cmdline').read_bytes()]
        if len(gateways) != 1:
            raise RuntimeError('cannot uniquely identify test gateway')
        start = time.monotonic()
        os.kill(gateways[0], signal.SIGKILL)
        while time.monotonic()-start < 8:
            state = daemon.control(op='task', id=a.task)['state']
            if state.get('state') != 'running':
                break
            time.sleep(.05)
        evidence.check('gateway crash ends owning VM task', state.get('state') != 'running', state)
        evidence.check('gateway crash closes guest task stream', a.reader.readline() == b'')
        response = b.model()
        evidence.check('unrelated VM survives peer gateway crash', '200 OK' in response['out'])
    finally:
        sentinel.close()
        daemon.close()


def mirrors(output, fixture, credential_file, evidence):
    hosts = output / 'fixture-hosts'
    hosts.write_text('140.82.112.3 mirror.example other.example mixed.example\n127.0.0.1 mixed.example private.example\n')
    wrapper = output / 'fixture-gateway'
    wrapper.write_text('#!/bin/sh\nexec ' + shlex.quote(os.environ['DECK_GATEWAY_EXECUTABLE'])
                       + ' "$@" --web-hosts ' + shlex.quote(str(hosts)) + ' --web-test-connect 127.0.0.1\n')
    wrapper.chmod(0o700)
    daemon = Daemon(output / 'fixtures', f'http://127.0.0.1:{fixture.port}/v1', credential_file, wrapper)
    try:
        guest = daemon.guest('mirror-project')
        destination = f'mirror.example:{fixture.port}'
        base = 'http://' + destination
        for name in ['mixed.example', 'private.example']:
            before = len(fixture.snapshot())
            approvals = len(daemon.control(op='approvals', task=guest.task))
            response = guest.curl(f'http://{name}:{fixture.port}/target')
            evidence.check('fixture: private/mixed DNS answer denied without dial or prompt '+name,
                '403' in response['out'] and len(fixture.snapshot()) == before
                and len(daemon.control(op='approvals', task=guest.task)) == approvals)
        before = len(fixture.snapshot())
        response = guest.curl(base + '/mirror')
        evidence.check('fixture: ungranted mirror never dialed', '403' in response['out'] and len(fixture.snapshot()) == before)
        daemon.allow(guest, destination)
        response = guest.curl(base + '/mirror', '-H', 'Proxy-Authorization: guest-decoy')
        seen = fixture.snapshot()[-1]
        evidence.check('fixture: granted mirror uses origin form and strips proxy credential',
                       '200 OK' in response['out'] and seen['path'] == '/mirror'
                       and 'proxy-authorization' not in {k.lower(): v for k,v in seen['headers'].items()}, response)
        evidence.check('fixture: mirror body is unchanged (#62)', response['out'].endswith('mirror-ok'), response)
        response = guest.curl(base + '/relative')
        evidence.check('fixture: relative redirect reaches held mirror', '200 OK' in response['out'] and fixture.snapshot()[-1]['path']=='/mirror', response)
        for path in ['/denied', '/https', '/missing', '/private', '/loop']:
            before = len(fixture.snapshot())
            response = guest.curl(base + path)
            contacted_target = any(item['path'] == '/target' for item in fixture.snapshot()[before:])
            evidence.check('fixture: redirect refused before second destination ' + path,
                           '403' in response['out'] and not contacted_target, response)
        grants = daemon.control(op='grants', task=guest.task)
        evidence.check('fixture: task mirror grant has finite expiry',
                       bool(grants) and all(item.get('expires_ms', 0) > time.time()*1000 for item in grants))
        destinations = [destination, f'other.example:{fixture.port}']
        request = dict(op='web_profile', task=guest.task, profile='acceptance-fixture', destinations=destinations)
        first, second = daemon.control(**request), daemon.control(**request)
        evidence.check('fixture: whole-profile approval persists once per destination',
                       first['changed'] == 2 and second['changed'] == 0)
        for grant in grants:
            daemon.control(op='revoke_grant', task=guest.task, id=grant['id'])
        project_rules = daemon.control(op='access_grants', task=guest.task)
        evidence.check('fixture: saved project rules name both profile destinations',
            all(any(rule['destination']==target and rule['scope']=='project' for rule in project_rules)
                for target in destinations))
        peer = daemon.guest('different-project')
        response = peer.curl(base + '/mirror')
        evidence.check('fixture: saved profile does not authorize another project', '403' in response['out'])
        response = guest.curl(base + '/denied')
        evidence.check('fixture: redirect to newly held second destination is admitted',
                       '200 OK' in response['out'] and fixture.snapshot()[-1]['path'] == '/target')
        daemon.control(op='web_profile', task=guest.task, profile='acceptance-fixture',
                       destinations=[destinations[1]], revoke=True)
        refused = guest.curl('http://' + destinations[1] + '/target')
        allowed = guest.curl(base + '/mirror')
        evidence.check('fixture: revoking one profile destination preserves sibling rule',
                       '403' in refused['out'] and '200 OK' in allowed['out'])
    finally:
        daemon.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True, help='fresh evidence directory')
    parser.add_argument('--section', choices=['all', 'production', 'mirrors'], default='all')
    args = parser.parse_args()
    for key in ['DECK_VM_IMAGE', 'DECK_VM_JOURNAL', 'DECK_VM_KRUN',
                'DECK_VM_EXECUTABLE', 'DECK_GATEWAY_EXECUTABLE']:
        if not Path(os.environ[key]).is_file():
            raise RuntimeError(key + ' must name an existing file')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    evidence = Evidence(output)
    manifest = dict(revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                    section=args.section, host=os.uname().machine,
                    fixture_dial_override='mirror phase only', binaries={})
    for key in ['DECK_VM_EXECUTABLE', 'DECK_GATEWAY_EXECUTABLE', 'DECK_VM_KRUN']:
        binary = Path(os.environ[key])
        manifest['binaries'][key] = dict(path=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    token = secrets.token_hex(32)
    credentials = output / 'synthetic-credentials.json'
    credentials.touch(mode=0o600)
    credentials.write_text(json.dumps({'OPENAI_API_KEY': token}))
    fixture = Fixture()
    try:
        if args.section in ['all', 'production']:
            production(output, fixture, credentials, token, evidence)
        if args.section in ['all', 'mirrors']:
            mirrors(output, fixture, credentials, evidence)
    finally:
        fixture.close()
        credentials.unlink()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


if __name__ == '__main__':
    raise SystemExit(main())
