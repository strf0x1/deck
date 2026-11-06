#!/usr/bin/env python3
"""Measure whether trickled GitHub headers have a total request deadline."""
import argparse
import json
from pathlib import Path
import secrets
import time

from fixture import Fixture
from support import Daemon, Evidence


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    output=args.output.resolve();output.mkdir(parents=True)
    evidence=Evidence(output)
    credentials=output/'credentials.json';credentials.touch(mode=0o600)
    credentials.write_text(json.dumps({'OPENAI_API_KEY':secrets.token_hex(32)}))
    fixture=Fixture()
    daemon=Daemon(output/'daemon',f'http://127.0.0.1:{fixture.port}/v1',credentials,
                  extra_env={'DECK_GITHUB_REPOSITORY':'acceptance/fixture'})
    try:
        guest=daemon.guest('slow-header-project')
        guest.python('import socket,time,threading,builtins\n'
            'def probe():\n'
            ' s=socket.create_connection(("127.0.0.1",8084),3);s.settimeout(.2)\n'
            ' start=time.monotonic();s.sendall(b"POST /operations/publish HTTP/1.1\\r\\nX: ");last=start;reply=b"";closed=False\n'
            ' try:\n'
            '  while time.monotonic()-start<66:\n'
            '   if time.monotonic()-last>2:\n'
            '    s.sendall(b"x");last=time.monotonic()\n'
            '   try:\n'
            '    chunk=s.recv(4096);reply+=chunk\n'
            '    if not chunk or reply:closed=True;break\n'
            '   except socket.timeout:pass\n'
            ' except OSError:closed=True\n'
            ' finally:\n'
            '  s.close();builtins.slow_result={"closed":closed,"elapsed":time.monotonic()-start,"reply":reply.decode(errors="replace")}\n'
            'builtins.slow_result=None\nthreading.Thread(target=probe,daemon=True).start();result="started"')
        result=None
        for _ in range(16):
            time.sleep(5)
            result=guest.python('import builtins\nresult=builtins.slow_result')
            if result is not None:break
        evidence.check('GitHub trickled header gets 408 within five-second total budget',
                       result is not None and result['closed'] and result['elapsed']<8 and ' 408 ' in result['reply'],result)
        evidence.check('GitHub slow-header probe leaves model gateway usable','200 OK' in guest.model()['out'])
    finally:
        daemon.close();fixture.close();credentials.unlink()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


if __name__=='__main__':
    raise SystemExit(main())
