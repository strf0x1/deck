"""Controlled HTTP upstream with in-memory observations and no credential logs."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading


class Fixture:
    def __init__(self):
        self.requests = []
        self.lock = threading.Lock()
        self.tool_step = None
        self.streaming = threading.Event()
        self.release = threading.Event()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get('content-length', 0)))
                with fixture.lock:
                    fixture.requests.append(dict(path=self.path, headers=dict(self.headers), body=body))
                if fixture.tool_step is not None:
                    step = fixture.tool_step
                    fixture.tool_step += 1
                    delta = dict(content='done')
                    reason = 'stop'
                    if step == 0:
                        delta = dict(tool_calls=[dict(index=0, id='call_boundary_probe', type='function',
                            function=dict(name='shell', arguments=json.dumps(dict(command=
                                "curl --noproxy '' -x http://127.0.0.1:8082 --max-time 5 -I https://example.com"))))])
                        reason = 'tool_calls'
                    chunks = [dict(id='scripted', choices=[dict(index=0, delta=delta, finish_reason=None)]),
                              dict(id='scripted', choices=[dict(index=0, delta={}, finish_reason=reason)])]
                    payload = ''.join('data: '+json.dumps(chunk)+'\n\n' for chunk in chunks)+'data: [DONE]\n\n'
                    self.reply(200, payload.encode(), **{'Content-Type': 'text/event-stream'})
                elif fixture.streaming.is_set():
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.end_headers()
                    self.wfile.write(b'data: {"choices":[{"delta":{"content":"started"}}]}\n\n')
                    self.wfile.flush()
                    fixture.release.wait(20)
                else:
                    self.reply(200, json.dumps(dict(id='probe', object='chat.completion',
                               choices=[dict(index=0, message=dict(role='assistant', content='ok'),
                                             finish_reason='stop')], usage=dict(total_tokens=3))).encode())

            def do_GET(self):
                with fixture.lock:
                    fixture.requests.append(dict(path=self.path, headers=dict(self.headers), body=b''))
                if self.path == '/v1/models':
                    self.reply(200, json.dumps(dict(data=[dict(id='acceptance-model', object='model')])).encode())
                    return
                targets = {
                    '/relative': '/mirror',
                    '/denied': f'http://other.example:{fixture.port}/target',
                    '/https': f'https://mirror.example:{fixture.port}/target',
                    '/missing': 'http://deck-acceptance-no-such-host.invalid/target',
                    '/private': f'http://127.0.0.1:{fixture.port}/target',
                    '/loop': '/loop',
                }
                if self.path in targets:
                    self.reply(302, b'', Location=targets[self.path])
                else:
                    self.reply(200, b'mirror-ok')

            def reply(self, status, body, **headers):
                self.send_response(status)
                self.send_header('Content-Type', headers.pop('Content-Type', 'application/json'))
                self.send_header('Content-Length', str(len(body)))
                for key, value in headers.items():
                    self.send_header(key, value)
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.port = self.server.server_port

    def snapshot(self):
        with self.lock:
            return list(self.requests)

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
