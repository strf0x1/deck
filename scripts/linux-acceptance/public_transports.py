#!/usr/bin/env python3
"""Unauthenticated public TLS and SSH transport through the guest TCP relay."""
import argparse
import json
from pathlib import Path
import secrets

from fixture import Fixture
from support import Daemon, Evidence


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True);args=parser.parse_args()
    output=args.output.resolve();output.mkdir(parents=True)
    evidence=Evidence(output)
    credentials=output/'credentials.json';credentials.touch(mode=0o600)
    credentials.write_text(json.dumps({'OPENAI_API_KEY':secrets.token_hex(32)}))
    fixture=Fixture();daemon=Daemon(output/'daemon',f'http://127.0.0.1:{fixture.port}/v1',credentials)
    try:
        guest=daemon.guest('public-project')
        evidence.check('public: TLS denied before approval','403' in guest.curl('https://example.com','-I')['out'])
        daemon.allow(guest,'example.com:443','timed')
        tls=guest.curl('https://example.com','-I')
        evidence.check('public: certificate-verified TLS through proxy succeeds',tls['rc']==0 and
            ('HTTP/2 200' in tls['out'] or 'HTTP/1.1 200 OK' in tls['out']),tls)
        ssh_code=('import socket\n'
            's=socket.create_connection(("127.0.0.1",8082),4);s.settimeout(8)\n'
            's.sendall(b"CONNECT ssh.github.com:443 HTTP/1.1\\r\\n\\r\\n");data=b""\n'
            'while b"\\r\\n\\r\\n" not in data:\n'
            ' chunk=s.recv(4096)\n'
            ' if not chunk:break\n'
            ' data+=chunk\n')
        denied=guest.python(ssh_code+'result=data.decode();s.close()')
        evidence.check('public: SSH relay denied before approval','403' in denied)
        daemon.allow(guest,'ssh.github.com:443','timed')
        banner=guest.python(ssh_code+
            'head,_,body=data.partition(b"\\r\\n\\r\\n")\n'
            'while b"\\n" not in body:body+=s.recv(4096)\n'
            's.sendall(b"SSH-2.0-DeckAcceptance\\r\\n")\n'
            'result={"status":head.decode(),"banner":body.splitlines()[0].decode()};s.close()')
        evidence.check('public: SSH banner exchanged through TCP relay and CONNECT',
            '200 Connection Established' in banner['status'] and banner['banner'].startswith('SSH-2.0-'),banner)
        other=guest.curl('https://example.org','-I')
        evidence.check('public: other TLS destination remains unapproved','403' in other['out'],other)
    finally:
        daemon.close();fixture.close();credentials.unlink()
    return 0 if all(row['passed'] for row in evidence.rows) else 1


if __name__=='__main__':raise SystemExit(main())
