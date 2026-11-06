#!/usr/bin/env python3
"""Drive the real Deck window and VM Construct through timed boundary feedback."""
import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import socket
import subprocess
import time

from fixture import Fixture
from support import Daemon, Evidence


def control(path, **request):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(10);stream.connect(str(path))
        stream.sendall(json.dumps(request).encode()+b'\n')
        reply=json.loads(stream.makefile('rb').readline())
        if not reply['ok']:
            raise RuntimeError(reply)
        return reply


def until(path, predicate, timeout=40):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        try:
            state=control(path,cmd='state')
            if predicate(state):return state
        except (OSError,ValueError):pass
        time.sleep(.1)
    raise RuntimeError('live window did not reach expected state')


def screenshot(path, target):
    geometry=control(path,cmd='geometry')
    clients=json.loads(subprocess.check_output(['hyprctl','clients','-j']))
    window=next(c for c in clients if c['pid']==geometry['pid'])
    region=f"{window['at'][0]},{window['at'][1]} {window['size'][0]}x{window['size'][1]}"
    subprocess.run(['grim','-g',region,str(target)],check=True)
    return geometry


def prompt(path,text):
    control(path,cmd='type',text=text);control(path,cmd='key',key='enter')


def run(output,fixture,credentials,evidence):
    daemon=Daemon(output/'ui',f'http://127.0.0.1:{fixture.port}/v1',credentials)
    project=output/'project';project.mkdir()
    (project/'cwd-proof.txt').write_text('host-project-imported\n')
    path=Path(os.environ['XDG_RUNTIME_DIR'])/f'deck-ta-{os.getpid()}.sock'
    window=None
    try:
        cwd_command='pwd; cat cwd-proof.txt; test ! -e '+shlex.quote(str(project))+' && echo host-path-absent'
        fixture.plan=[dict(call='shell',args=dict(command=cwd_command)),dict(call='shell',args=dict(command=
            "curl --noproxy '' -x http://127.0.0.1:8082 --max-time 3 -I https://example.com")),
            dict(text='Boundary probe finished.')]
        command='cd '+shlex.quote(str(project))+' && exec '+shlex.join([
            'env','DECK_DATA='+str(daemon.data),'DECK_CONTROL='+str(path),
            os.environ['DECK_ACCEPTANCE_UI_EXECUTABLE'],'--','/usr/local/bin/construct','acp'])
        dispatch='hl.dsp.exec_cmd('+json.dumps(shlex.join(['sh','-c',command]))+', { no_initial_focus = true })'
        subprocess.run(['hyprctl','dispatch',dispatch],check=True,capture_output=True)
        until(path,lambda s:s['session']['status']=='ready')
        tasks=daemon.control(op='tasks');task=tasks[0]['id'];daemon.tasks.append(task)
        window=control(path,cmd='geometry')['pid']
        evidence.check('UI: merged Construct ready inside real VM',tasks[0]['backend']=='vm')
        evidence.check('UI: host task identity retains the host project', tasks[0]['spec']['cwd']==str(project))
        prompt(path,'Run the scripted boundary probe.')
        state=until(path,lambda s:s['approval'] is not None and s['session']['status']=='ready')
        first_results=[str(m.get('content','')) for r in fixture.snapshot() if r['body']
            for m in json.loads(r['body']).get('messages',[]) if m.get('role')=='tool']
        evidence.check('UI: new VM session uses workspace and imported host project without alias',
            any('/workspace\nhost-project-imported\nhost-path-absent' in text for text in first_results))
        approval=state['approval']['id']
        evidence.check('UI: actual blocked operation appears on boundary card',state['approval']['destination']=='example.com:443')
        screenshot(path,output/'timed-card.png')
        before=int(time.time()*1000)
        control(path,cmd='key',key='2')
        until(path,lambda s:s['approval'] is None)
        grants=daemon.control(op='grants',task=task)
        grant=next(g for g in grants if g['destination']=='example.com:443')
        expiry=grant.get('expires_ms',0)
        evidence.check('UI: second decision creates five-minute grant',grant['scope']=='timed'
            and before+300000<=expiry<=int(time.time()*1000)+300000,grant)
        geometry=control(path,cmd='geometry')
        control(path,cmd='click',x=geometry['viewport']['w']-170,y=34)
        access=until(path,lambda s:s['access']['open'] and bool(s['access']['grants']))
        evidence.check('UI: Access shows exact timed deadline',any(g['scope']=='timed' and g['expires_ms']==expiry
            for g in access['access']['grants']))
        screenshot(path,output/'timed-access.png')
        control(path,cmd='key',key='escape')
        # The first request after this explicit prompt must contain the late
        # ACP notice. The model then reads the same decision through HTTP tools.
        journal_code=("import glob,json,sqlite3; p=glob.glob('/root/.construct/projects/*/journal.sqlite')[0]; "
            "c=sqlite3.connect(p); rows=[json.loads(r[0]) for r in c.execute(\"select payload from events where type='tool_end'\")]; "
            "print(json.dumps([r.get('data') for r in rows if isinstance(r.get('data'),dict) and 'expires_at_ms' in r['data']]))")
        fixture.plan=[dict(call='boundary_status',args=dict(id=approval)),
            dict(call='boundary_wait',args=dict(id=approval,timeout_s=1)),
            dict(call='shell',args=dict(command='python3 -c '+shlex.quote(journal_code))),
            dict(call='shell',args=dict(command=
                "curl --noproxy '' -x http://127.0.0.1:8082 --max-time 12 -sS -I https://example.com")),
            dict(text='Timed feedback verified.')]
        start=len(fixture.snapshot());prompt(path,'Read the existing decision using the scripted tools.')
        until(path,lambda s:s['session']['status']=='ready' and len(fixture.snapshot())>=start+5)
        requests=fixture.snapshot()[start:]
        messages=[json.loads(r['body']).get('messages',[]) for r in requests if r['body']]
        first='\n'.join(str(m.get('content','')) for m in messages[0])
        evidence.check('UI: late ACP notice carries original timed deadline to Construct',
            'scope: timed' in first and f'expires_at_ms={expiry} (Unix ms)' in first and 'is now allowed' in first)
        evidence.check('UI: late notification is delivered once',first.count('is now allowed')==1)
        tool_results=[str(m.get('content','')) for m in messages[-1] if m.get('role')=='tool']
        evidence.check('UI: status and wait expose the host deadline in model text',sum(
            'allowed: approval '+approval in text and f'expires_at_ms={expiry} (Unix ms)' in text
            for text in tool_results)==2)
        expected=json.dumps([dict(approval=approval,outcome='allowed',scope='timed',expires_at_ms=expiry)]*2)
        evidence.check('UI: journal preserves structured status and wait metadata',any(expected in text for text in tool_results))
        evidence.check('UI: approved public TLS succeeds through guest proxy',any('HTTP/2 200' in text or 'HTTP/1.1 200 OK' in text for text in tool_results))
        evidence.check('UI: observation did not renew timed grant',next(g for g in daemon.control(op='grants',task=task)
            if g['id']==grant['id'])['expires_ms']==expiry)
        (output/'observations.json').write_text(json.dumps(dict(task=task,approval=approval,expires_at_ms=expiry,
            model_requests=len(requests),tool_results=tool_results),indent=2)+'\n')
        screenshot(path,output/'timed-feedback.png')
        session=daemon.control(op='task',id=task)['session_id']
        daemon.control(op='stop',id=task)
        until(path,lambda s:isinstance(s['session']['status'],dict) or s['session']['status']=='detached')
        control(path,cmd='key',key='r',modifiers=['ctrl'])
        until(path,lambda s:s['session']['status']=='ready')
        fixture.plan=[dict(call='shell',args=dict(command=cwd_command)),dict(text='Restored cwd verified.')]
        start=len(fixture.snapshot());prompt(path,'Verify the restored working directory.')
        until(path,lambda s:s['session']['status']=='ready' and len(fixture.snapshot())>=start+2)
        restored=[str(m.get('content','')) for r in fixture.snapshot()[start:] if r['body']
            for m in json.loads(r['body']).get('messages',[]) if m.get('role')=='tool']
        evidence.check('UI: restart loads same VM session with workspace cwd',
            daemon.control(op='task',id=task)['session_id']==session and
            any('/workspace\nhost-project-imported\nhost-path-absent' in text for text in restored))
        control(path,cmd='key',key='q',modifiers=['ctrl'])
        for _ in range(100):
            if not Path(f'/proc/{window}').exists():break
            time.sleep(.05)
        evidence.check('UI: closing window leaves VM running',daemon.control(op='task',id=task)['state']['state']=='running')
    finally:
        observed=[]
        for request in fixture.snapshot():
            if request['body']:
                observed.extend(m.get('content') for m in json.loads(request['body']).get('messages',[])
                                if m.get('role')=='tool')
        (output/'tool-observations.json').write_text(json.dumps(observed,indent=2)+'\n')
        if window is not None and Path(f'/proc/{window}').exists():
            try:control(path,cmd='key',key='q',modifiers=['ctrl'])
            except OSError:os.kill(window,15)
        # Include a task even if startup failed after daemon creation.
        for task in daemon.control(op='tasks'):
            if task['id'] not in daemon.tasks:daemon.tasks.append(task['id'])
        daemon.close()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    output=args.output.resolve();output.mkdir(parents=True)
    credentials=output/'synthetic-credentials.json';credentials.touch(mode=0o600)
    credentials.write_text(json.dumps({'OPENAI_API_KEY':secrets.token_hex(32)}))
    evidence=Evidence(output);fixture=Fixture()
    try:run(output,fixture,credentials,evidence)
    finally:fixture.close();credentials.unlink()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


if __name__=='__main__':raise SystemExit(main())
