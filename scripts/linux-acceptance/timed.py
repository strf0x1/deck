#!/usr/bin/env python3
"""Production-duration timed grants through real Linux guests and gateways."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import time

from adversarial import restart, status
from fixture import Fixture
from support import Daemon, Evidence


def boundary(guest, path):
    reply = guest.run('curl', '-fsS', '--max-time', '5', 'http://127.0.0.1:8081' + path)
    if reply['rc']:
        raise RuntimeError(reply)
    return json.loads(reply['out'])


def open_stream(guest, name, destination=None, model=None):
    if model:
        payload = json.dumps(dict(model=model, stream=True, messages=[])).encode()
        request = (b'POST /v1/chat/completions HTTP/1.1\r\nHost: model\r\n'
                   b'Content-Type: application/json\r\nContent-Length: ' +
                   str(len(payload)).encode() + b'\r\n\r\n' + payload)
        setup = 's=socket.create_connection(("127.0.0.1",8080),4)'
    else:
        request = ('CONNECT ' + destination + ' HTTP/1.1\r\n\r\n').encode()
        setup = 's=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM);s.settimeout(4);s.connect((2,1028))'
    return guest.python('import socket,builtins\n' + setup + '\n'
        f's.sendall({request!r});data=b""\n'
        f'while {bytes([13,10,13,10])!r} not in data:\n'
        ' chunk=s.recv(4096)\n'
        ' if not chunk:raise RuntimeError("stream closed before response headers")\n'
        ' data+=chunk\n'
        's.settimeout(.3)\n'
        'if not hasattr(builtins,"streams"):builtins.streams={}\n'
        f'builtins.streams[{name!r}]=s\nresult=data.decode()')


def stream_state(guest, name):
    return guest.python('import builtins,socket\n'
        f's=builtins.streams[{name!r}];data=b"";state="open"\n'
        'try:\n'
        ' while True:\n'
        '  chunk=s.recv(8192)\n'
        '  if not chunk:state="closed";break\n'
        '  data+=chunk\n'
        'except socket.timeout:pass\n'
        'except OSError:state="closed"\n'
        'result={"state":state,"bytes":len(data)}')


def decide(daemon, guest, destination, evidence):
    before = int(time.time() * 1000)
    item = daemon.allow(guest, destination, 'timed')
    after = int(time.time() * 1000)
    deadline = item.get('grant_expires_ms', 0)
    evidence.check('timed: fixed production five-minute deadline for ' + destination,
        item['state'] == {'allowed': 'timed'} and before + 300000 <= deadline <= after + 300000,
        dict(before_ms=before, after_ms=after, deadline_ms=deadline))
    return item


def run(output, fixture, credentials, evidence):
    gateway = output / 'fixture-gateway'
    hosts = output / 'fixture-hosts'
    hosts.write_text('140.82.112.3 held.example other.example\n')
    gateway.write_text('#!/bin/sh\nexec ' + shlex.quote(os.environ['DECK_GATEWAY_EXECUTABLE']) +
        ' "$@" --web-hosts ' + shlex.quote(str(hosts)) +
        ' --web-test-connect 127.0.0.1\n')
    gateway.chmod(0o700)
    daemon = Daemon(output / 'timed', f'http://127.0.0.1:{fixture.port}/v1', credentials, gateway)
    daemon.test_env = os.environ.copy()
    daemon.test_env.update(DECK_CREDENTIALS=str(credentials), CONSTRUCT_MODEL='acceptance-model',
        CONSTRUCT_BASE_URL=f'http://127.0.0.1:{fixture.port}/v1', DECK_GATEWAY_EXECUTABLE=str(gateway))
    try:
        a, b = daemon.guest('timed-a'), daemon.guest('timed-b')
        evidence.check('timed: merged Construct executes in guest',
            a.run('/usr/local/bin/construct', '--version')['rc'] == 0)
        evidence.check('timed: unapproved model refused', '403' in a.model('timed-model')['out'])
        model = decide(daemon, a, 'timed-model', evidence)
        for index in range(2):
            reply = a.run('curl', '-sS', '--max-time', '8', '-i',
                'http://127.0.0.1:8080/v1/chat/completions', '-H', 'Content-Type: application/json',
                '-d', json.dumps(dict(model='timed-model', messages=[dict(role='user', content=str(index))])))
            evidence.check(f'timed: changed model prompt {index} reuses authority',
                '200 OK' in reply['out'], reply)
        evidence.check('timed: second task cannot reuse model grant', '403' in b.model('timed-model')['out'])
        evidence.check('timed: different model needs authority', '403' in a.model('other-model')['out'])
        for label, fields in [('operation', {'operation':'other_operation'}),
                              ('credential', {'credential':'DIFFERENT_KEY'}),
                              ('gateway', {'gateway':'different_gateway'})]:
            ask = dict(gateway='model', operation='chat_completions', destination='timed-model',
                       credential='OPENAI_API_KEY')
            ask.update(fields)
            reply = daemon.control(op='approval', task=a.task, **ask)
            evidence.check('timed: different ' + label + ' cannot reuse authority', reply['state']=='pending',
                           dict(state=reply['state']))
        status_body = boundary(a, '/approvals/' + model['id'])
        events = boundary(a, '/events?after=0')
        evidence.check('timed: HTTP status preserves exact scope and deadline',
            status_body.get('scope')=='timed' and status_body.get('expires_at_ms')==model['grant_expires_ms'], status_body)
        evidence.check('timed: HTTP event preserves same deadline', any(
            row['approval_id']==model['id'] and row.get('scope')=='timed' and
            row.get('expires_at_ms')==model['grant_expires_ms'] for row in events))
        held = f'held.example:{fixture.port}'
        evidence.check('timed: unapproved web tunnel refused', '403' in status(a, held))
        web = decide(daemon, a, held, evidence)
        evidence.check('timed: repeated mirror paths reuse web authority', all(
            '200 OK' in a.curl('http://' + held + path)['out'] for path in ['/mirror','/another']))
        evidence.check('timed: peer cannot reuse web grant', '403' in status(b, held))
        evidence.check('timed: other destination denied', '403' in status(a, f'other.example:{fixture.port}'))
        evidence.check('timed: changed port denied', '403' in status(a, f'held.example:{fixture.port+1}'))
        evidence.check('timed: opaque web tunnel opened', '200 Connection Established' in open_stream(a,'web',held))
        fixture.streaming.set()
        evidence.check('timed: idle model stream opened', '200 OK' in open_stream(a,'model',model='timed-model'))
        evidence.check('timed: unrelated configured model stream opened', '200 OK' in open_stream(a,'unrelated',model='acceptance-model'))
        # A separate timed operation is revoked well before its deadline.
        a.run('curl','-sS','--max-time','2','-i','http://127.0.0.1:8080/v1/chat/completions',
              '-H','Content-Type: application/json','-d','{"model":"revoked-model","messages":[]}')
        decide(daemon,a,'revoked-model',evidence)
        open_stream(a,'revoked',model='revoked-model')
        revoked=next(g for g in daemon.control(op='grants',task=a.task) if g['destination']=='revoked-model')
        daemon.control(op='revoke_grant',task=a.task,id=revoked['id'])
        time.sleep(.3)
        evidence.check('timed: revocation closes active model operation',stream_state(a,'revoked')['state']=='closed')
        evidence.check('timed: revocation preserves unrelated stream',stream_state(a,'unrelated')['state']=='open')
        grants_path=daemon.data/'tasks'/a.task/'grants.json'
        original=json.loads(grants_path.read_text())
        deadlines={row['destination']:row['expires_ms'] for row in original}
        daemon.process.terminate();daemon.process.wait(timeout=10)
        restart(daemon)
        restored=daemon.control(op='grants',task=a.task)
        evidence.check('timed: restart persists exact deadlines without renewal',
            {row['destination']:row['expires_ms'] for row in restored}==deadlines)
        evidence.check('timed: restarted daemon adopts live guests', all(
            daemon.control(op='task',id=g.task)['state']['state']=='running' for g in [a,b]))
        evidence.check('timed: reconnect uses existing web deadline', '200 Connection Established' in status(a,held)
            and next(g for g in daemon.control(op='grants',task=a.task) if g['destination']==held)['expires_ms']==web['grant_expires_ms'])
        # Prove authority is live immediately before expiry; leave the daemon
        # absent across the actual, unedited production deadline.
        until=max(model['grant_expires_ms'],web['grant_expires_ms'])
        while int(time.time()*1000)<until-5000:
            left=(until-int(time.time()*1000))/1000
            print(f'Waiting for unchanged production deadline: {left:.1f}s remaining',flush=True)
            time.sleep(min(25,max(.01,left-5)))
        evidence.check('timed: model stream survives until near deadline',stream_state(a,'model')['state']=='open')
        evidence.check('timed: web tunnel survives until near deadline',stream_state(a,'web')['state']=='open')
        daemon.process.terminate();daemon.process.wait(timeout=10)
        time.sleep(max(0,(until-int(time.time()*1000))/1000)+.4)
        evidence.check('timed: model closes at real expiry with daemon absent',stream_state(a,'model')['state']=='closed')
        evidence.check('timed: web closes at real expiry with daemon absent',stream_state(a,'web')['state']=='closed')
        evidence.check('timed: unrelated model survives other grants expiry',stream_state(a,'unrelated')['state']=='open')
        evidence.check('timed: expired web admission denied during outage','403' in status(a,held))
        evidence.check('timed: expired model admission denied during outage','403' in a.model('timed-model')['out'])
        restart(daemon)
        evidence.check('timed: expired grant not restored on restart','403' in status(a,held)
            and '403' in a.model('timed-model')['out'])
        evidence.check('timed: host did not edit persisted five-minute deadlines',
            json.loads(grants_path.read_text())==original)
        fixture.streaming.clear();fixture.release.set()
        a.model('stop-model');decide(daemon,a,'stop-model',evidence)
        daemon.control(op='stop',id=a.task)
        evidence.check('timed: stopping task removes its timed authority',
            not daemon.control(op='grants',task=a.task) and not grants_path.exists())
    finally:
        fixture.release.set()
        if daemon.process.poll() is not None:
            restart(daemon)
        daemon.close()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    output=args.output.resolve();output.mkdir(parents=True)
    evidence=Evidence(output)
    credentials=output/'synthetic-credentials.json';credentials.touch(mode=0o600)
    credentials.write_text(json.dumps({'OPENAI_API_KEY':secrets.token_hex(32)}))
    fixture=Fixture(stream_timeout=420)
    try:
        run(output,fixture,credentials,evidence)
    finally:
        fixture.close();credentials.unlink()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


if __name__=='__main__':
    raise SystemExit(main())
