#!/usr/bin/env python3
"""Exercise locked/unavailable Secret Service on a disposable private bus/keyring."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

from support import Evidence

BUS_CONFIG = ('<busconfig><type>session</type><listen>unix:tmpdir=/tmp</listen>'
              '<auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/>'
              '<allow receive_sender="*"/><allow own="*"/></policy></busconfig>')


def worker(output):
    import dbus
    evidence = Evidence(output)
    home = Path(os.environ['HOME'])
    control = home/'control'; control.mkdir(mode=0o700)
    log = (output/'private-keyring.log').open('w')
    keyring = None
    daemon = None
    task = None
    name = 'DECK_ACCEPTANCE_PRIVATE'
    binary = os.environ['DECK_VM_EXECUTABLE']
    data = home/'deck-data'; data.mkdir()
    h = 0xcbf29ce484222325
    for byte in str(data).encode():
        h = ((h ^ byte)*0x100000001b3)&((1<<64)-1)
    address = Path(os.environ['XDG_RUNTIME_DIR'])/'deck'/f'{h:x}'/'deckd.sock'

    def start_keyring():
        process = subprocess.Popen(['gnome-keyring-daemon', '--foreground', '--components=secrets',
            '--control-directory', str(control)], stdout=log, stderr=log)
        for _ in range(100):
            if service_bus.name_has_owner('org.freedesktop.secrets'):
                return process
            if process.poll() is not None:
                raise RuntimeError('private keyring exited')
            time.sleep(.05)
        raise RuntimeError('private Secret Service did not become ready')

    def interfaces():
        obj = service_bus.get_object('org.freedesktop.secrets', '/org/freedesktop/secrets')
        service = dbus.Interface(obj, 'org.freedesktop.Secret.Service')
        internal = dbus.Interface(obj, 'org.gnome.keyring.InternalUnsupportedGuiltRiddenInterface')
        _, session = service.OpenSession('plain', dbus.String('', variant_level=1))
        master = dbus.Struct((session, dbus.ByteArray(b''), dbus.ByteArray(password.encode()),
                              'text/plain'), signature='oayays')
        return service, internal, master

    def request(**body):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(10); stream.connect(str(address))
            stream.sendall(json.dumps(body).encode()+b'\n')
            return json.loads(stream.makefile('rb').readline())

    def fetch():
        return request(op='credential', task=task, name=name)

    def set_value(value):
        reply = subprocess.run([binary, '--data', str(data), 'credential', 'set', name],
            input=value, text=True, capture_output=True, timeout=10)
        if reply.returncode:
            raise RuntimeError('cannot store synthetic value')

    def outcome(reply):
        # Never record raw broker replies, which contain the synthetic value.
        return 'served' if 'ok' in reply else str(reply.get('error'))

    password = secrets.token_hex(24)
    service_bus = dbus.SessionBus()
    try:
        keyring = start_keyring()
        service, internal, master = interfaces()
        collection = internal.CreateWithMasterPassword(
            {'org.freedesktop.Secret.Collection.Label': 'Deck acceptance'}, master)
        service.SetAlias('default', collection)
        value = secrets.token_hex(32)
        set_value(value)
        daemon_log = (output/'private-deckd.log').open('w')
        daemon = subprocess.Popen([binary, '--data', str(data), '--idle-ms', '600000'],
            stdout=daemon_log, stderr=daemon_log)
        for _ in range(100):
            try:
                request(op='tasks'); break
            except OSError:
                time.sleep(.05)
        task = request(op='create',spec={'argv':['/bin/cat'],'cwd':str(home)})['ok']['id']
        request(op='start',id=task)
        config = dict(task=task,data=str(data),lock_fd=-1,disk='/dev/null',journal='/dev/null',
            socket='/tmp/x',forward='/tmp/x',console='/tmp/x',guest='/bin/true',argv=[],env=[],vcpus=1,
            ram_mib=64,library='/dev/null',gateway=dict(executable='/bin/true',model_socket='/tmp/x',
            extension_socket='/tmp/x',model_port=1026,extension_port=1027,github_scratch='/tmp/x',
            upstream='http://127.0.0.1:1/v1',models=['m'],budget_file='/tmp/x',secret=name,
            credentials=[name, name+'_ABSENT'],control='/tmp/x',task=task))
        (data/'tasks'/task/'vmm.json').write_text(json.dumps(config))
        evidence.check('private Secret Service: unlocked broker read matches synthetic value',
                       fetch().get('ok',{}).get('value')==value)
        missing = request(op='credential',task=task,name=name+'_ABSENT')
        search = subprocess.run(['secret-tool','search','--all','service','com.sprawlcommons.deck',
                                 'name',name+'_ABSENT'],capture_output=True,timeout=8)
        evidence.check('private Secret Service: genuinely absent item remains missing',
            'deck_credential_missing' in outcome(missing),
            dict(outcome=outcome(missing),metadata_search_exit=search.returncode))
        locked_outcomes=[]
        for cycle in range(20):
            service.Lock([collection])
            locked = dbus.Interface(service_bus.get_object('org.freedesktop.secrets',collection),
                                     'org.freedesktop.DBus.Properties').Get('org.freedesktop.Secret.Collection','Locked')
            if not locked:
                raise RuntimeError('private test collection did not lock')
            start = time.monotonic(); reply = fetch()
            locked_outcomes.append(dict(outcome=outcome(reply),elapsed=time.monotonic()-start))
            internal.UnlockWithMasterPassword(collection,master)
            if fetch().get('ok',{}).get('value')!=value:
                raise RuntimeError('unlock did not restore the synthetic value')
        evidence.check('private Secret Service: test collection actually locked',True)
        evidence.check('private Secret Service: locked collection is unavailable rather than missing',
            all('deck_credential_unavailable' in row['outcome'] for row in locked_outcomes),
            dict(cycles=locked_outcomes))
        evidence.check('private Secret Service: unlock restores same daemon broker read',True)
        # Stop this private service to simulate a pending OS read. Four callers
        # must join one real secret-tool process, all bounded at five seconds.
        os.kill(keyring.pid,signal.SIGSTOP)
        try:
            start = time.monotonic()
            with ThreadPoolExecutor(max_workers=4) as pool:
                pending = [pool.submit(fetch) for _ in range(4)]
                time.sleep(.4)
                children = set()
                for thread in Path(f'/proc/{daemon.pid}/task').glob('*/children'):
                    children.update(thread.read_text().split())
                lookups = sum(b'secret-tool' in Path(f'/proc/{pid}/cmdline').read_bytes()
                              and b'lookup' in Path(f'/proc/{pid}/cmdline').read_bytes() for pid in children)
                replies = [future.result(timeout=8) for future in pending]
            evidence.check('private Secret Service: concurrent blocked reads share one lookup',lookups==1,dict(lookups=lookups))
            evidence.check('private Secret Service: all blocked reads bounded as unavailable',
                all('deck_credential_unavailable' in outcome(reply) for reply in replies)
                and 4.5<time.monotonic()-start<8,dict(elapsed=time.monotonic()-start))
        finally:
            os.kill(keyring.pid,signal.SIGCONT)
        time.sleep(.2)
        evidence.check('private Secret Service: pending lookup recovers after service resumes',
                       fetch().get('ok',{}).get('value')==value)
        value2 = secrets.token_hex(32); set_value(value2)
        evidence.check('private Secret Service: recovered reads still observe rotation',
                       fetch().get('ok',{}).get('value')==value2)
        keyring.terminate(); keyring.wait(timeout=10);keyring=None
        reply=fetch()
        evidence.check('private Secret Service: service disappearance is unavailable',
                       'deck_credential_unavailable' in outcome(reply),dict(outcome=outcome(reply)))
        keyring=start_keyring()
        service,internal,master=interfaces()
        collection=service.ReadAlias('default')
        internal.UnlockWithMasterPassword(collection,master)
        evidence.check('private Secret Service: service restart restores read without restarting deckd',
                       fetch().get('ok',{}).get('value')==value2)
    finally:
        if daemon:
            if task:
                request(op='stop',id=task);request(op='destroy',id=task)
            daemon.terminate();daemon.wait(timeout=10);daemon_log.close()
        if keyring:
            os.kill(keyring.pid,signal.SIGCONT);keyring.terminate();keyring.wait(timeout=10)
        log.close()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--dbus-python',default='/usr/bin/python3.14')
    parser.add_argument('--worker',action='store_true')
    args=parser.parse_args()
    output=args.output.resolve()
    if args.worker:
        return worker(output)
    output.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix='deck-ss-') as temporary:
        home=Path(temporary);(home/'run').mkdir(mode=0o700)
        config=home/'bus.conf';config.write_text(BUS_CONFIG)
        env={k:v for k,v in os.environ.items() if not k.startswith('DECK_VM_')
             and k not in ['DISPLAY','WAYLAND_DISPLAY','GNOME_KEYRING_CONTROL','DECK_CREDENTIALS']}
        env.update(DECK_VM_EXECUTABLE=os.environ['DECK_VM_EXECUTABLE'],HOME=str(home),
            XDG_DATA_HOME=str(home/'data'),XDG_CONFIG_HOME=str(home/'config'),XDG_RUNTIME_DIR=str(home/'run'))
        child=subprocess.run(['dbus-run-session','--config-file',str(config),'--',args.dbus_python,
            str(Path(__file__).resolve()),'--worker','--output',str(output)],env=env,timeout=55)
        return child.returncode


if __name__=='__main__':
    raise SystemExit(main())
