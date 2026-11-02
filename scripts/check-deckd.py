#!/usr/bin/env python3
"""Drive detached tasks through real control and ACP sockets using only Python stdlib.

Build first: cargo build --bin deckd --bin deck-fake-agent
Run: python3 scripts/check-deckd.py
"""

import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


class Lines:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(10)
        self.socket.connect(str(path))
        self.file = self.socket.makefile("rwb")
        self.next_id = 0

    def send(self, value):
        self.file.write(json.dumps(value).encode() + b"\n")
        self.file.flush()

    def read(self):
        line = self.file.readline()
        if not line:
            raise AssertionError("socket closed before its reply")
        return json.loads(line)

    def control(self, op, **params):
        self.send({"op": op, **params})
        reply = self.read()
        assert "error" not in reply, reply
        return reply.get("ok")

    def request(self, method, **params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        return self.next_id

    def response(self, request_id):
        before = []
        while True:
            message = self.read()
            if "method" not in message and message.get("id") == request_id:
                assert "error" not in message, message
                return before, message["result"]
            before.append(message)

    def initialize(self):
        _, result = self.response(self.request("initialize", protocolVersion=1, clientCapabilities={}))
        assert result["agentCapabilities"]["loadSession"] is True, result
        assert result["agentCapabilities"]["_meta"]["construct"]["liveAttach"] is True

    def close(self):
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.file.close()
        self.socket.close()


def paths(data, runtime_root):
    value = 0xCBF29CE484222325
    for byte in os.fsencode(data.resolve()):
        value = ((value ^ byte) * 0x100000001B3) & ((1 << 64) - 1)
    directory = runtime_root / "deck" / f"{value:x}"
    return directory / "deckd.sock", directory / "tasks"


def connect_when_ready(path, daemon):
    deadline = time.monotonic() + 10
    while True:
        try:
            return Lines(path)
        except (FileNotFoundError, ConnectionRefusedError):
            assert daemon.poll() is None, "deckd exited before opening its control socket"
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)


def updates(messages):
    return [message["params"]["update"] for message in messages if message.get("method") == "session/update"]


def texts(messages, kind):
    return "".join(update["content"]["text"] for update in updates(messages) if update["sessionUpdate"] == kind)


def prompt(acp, sid, text, finish):
    request_id = acp.request("session/prompt", sessionId=sid, prompt=[{"type": "text", "text": text}])
    while True:
        message = acp.read()
        if message.get("method") == "session/request_permission":
            if not finish:
                return request_id, message
            acp.send({"jsonrpc": "2.0", "id": message["id"], "result": {"outcome": {"outcome": "selected", "optionId": "reject"}}})
        if "method" not in message and message.get("id") == request_id:
            assert finish and message["result"]["stopReason"] == "end_turn", message
            return


def load(acp, sid, cwd):
    acp.initialize()
    replay, result = acp.response(acp.request("session/load", sessionId=sid, cwd=str(cwd), mcpServers=[]))
    assert result["_meta"]["construct"]["turn"]["state"] == "idle", result
    return replay


def check(deckd, fake):
    with tempfile.TemporaryDirectory(prefix="deckd-", dir="/tmp") as scratch:
        root = Path(scratch)
        data, state = root / "data", root / "state"
        data.mkdir()
        environment = dict(os.environ, XDG_RUNTIME_DIR=str(root))
        daemon = subprocess.Popen([str(deckd), "--data", str(data), "--idle-ms", "200"], env=environment)
        control_path, task_paths = paths(data, root)
        control = acp = None
        task_id = None
        try:
            control = connect_when_ready(control_path, daemon)
            spec = {"argv": ["env", f"DECK_FAKE_STATE={state}", "DECK_FAKE_DELAY_MS=0", str(fake)], "cwd": str(root)}
            first = control.control("run", spec=spec)
            assert first["started"] is True, first
            task = first["task"]
            task_id = task["id"]
            acp = Lines(task_paths / f"{task_id}.sock")
            acp.initialize()
            _, created = acp.response(acp.request("session/new", cwd=str(root), mcpServers=[]))
            sid = created["sessionId"]
            control.control("session", id=task_id, session_id=sid)
            prompt(acp, sid, "completed turn", finish=True)
            acp.close()
            acp = None
            control.close()
            control = None
            time.sleep(0.1)
            os.kill(task["pid"], 0)
            assert daemon.poll() is None, "closing Deck ended deckd"
            snapshot = json.loads((data / "tasks" / task_id / "task.json").read_text())
            assert snapshot["state"]["state"] == "running", snapshot

            control = Lines(control_path)
            existing = control.control("run", spec=spec)
            assert existing["started"] is False and existing["task"]["id"] == task_id, existing
            assert existing["task"]["session_id"] == sid, existing
            acp = Lines(task_paths / f"{task_id}.sock")
            replay = load(acp, sid, root)
            assert texts(replay, "user_message_chunk") == "completed turn", replay
            assert texts(replay, "agent_message_chunk") == 'You said "completed turn". Checking the workspace first.\npermission: reject', replay
            assert control.control("task", id=task_id)["pid"] == task["pid"]
            acp.close()
            acp = None
            print("PASS: live reopen restores a completed turn with exact text and the same agent")
            control.control("stop", id=task_id)
            active = control.control("resume", id=task_id)
            acp = Lines(task_paths / f"{task_id}.sock")
            replay = load(acp, sid, root)
            assert texts(replay, "user_message_chunk") == "completed turn", replay
            assert texts(replay, "agent_message_chunk").endswith("permission: reject"), replay
            print("PASS: closing clients leaves the task running; reopen offers the same task; restart loads history")

            request, question = prompt(acp, sid, "survives daemon crash", finish=False)
            daemon.kill()
            daemon.wait(timeout=5)
            control.close()
            control = None
            os.kill(active["pid"], 0)
            os.kill(active["holder"]["pid"], 0)
            acp.send({"jsonrpc": "2.0", "id": question["id"], "result": {"outcome": {"outcome": "selected", "optionId": "reject"}}})
            _, finished = acp.response(request)
            assert finished["stopReason"] == "end_turn", finished
            daemon = subprocess.Popen([str(deckd), "--data", str(data), "--idle-ms", "200"], env=environment)
            control = connect_when_ready(control_path, daemon)
            adopted = control.control("run", spec=spec)
            assert adopted["started"] is False, adopted
            assert adopted["task"]["pid"] == active["pid"], adopted
            assert adopted["task"]["holder"] == active["holder"], adopted
            assert adopted["task"]["session_id"] == sid, adopted
            print("PASS: killing deckd preserves a waiting turn and ACP stream; restart adopts the same holder")

            _, old = prompt(acp, sid, "interrupted turn", finish=False)
            acp.close()
            time.sleep(0.05)
            acp = Lines(task_paths / f"{task_id}.sock")
            acp.initialize()
            acp.send({"jsonrpc": "2.0", "id": old["id"], "result": {"outcome": {"outcome": "selected", "optionId": "reject"}}})
            replay, attached = acp.response(acp.request("session/load", sessionId=sid, cwd=str(root), mcpServers=[]))
            assert attached["_meta"]["construct"]["turn"]["state"] == "waiting", attached
            questions = [v for v in replay if v.get("method") == "session/request_permission"]
            assert len(questions) == 1 and questions[0]["id"] != old["id"], replay
            print("PASS: reconnect restores a waiting card, renews its id, and ignores a retired wire reply")
            acp.close()
            acp = None
            control.control("stop", id=task_id)
            control.control("resume", id=task_id)
            acp = Lines(task_paths / f"{task_id}.sock")
            replay = load(acp, sid, root)
            assert texts(replay, "user_message_chunk") == "completed turnsurvives daemon crashinterrupted turn", replay
            assert "Turn interrupted when the agent stopped" not in texts(replay, "agent_message_chunk"), replay
            assert any(u.get("status") == "failed" for u in updates(replay)), replay
            assert not any(message.get("method") == "session/request_permission" for message in replay), replay
            print("PASS: graceful stop closes the waiting session; restart loads its history without retry")

            acp.close()
            acp = None
            control.control("stop", id=task_id)
            events = [json.loads(line) for line in (data / "tasks" / task_id / "events.jsonl").read_text().splitlines()]
            names = [event["event"] for event in events]
            assert names == ["created", "started", "session", "stopped", "started", "adopted", "stopped", "started", "stopped"], events
            assert all(event["clean"] is True for event in events if event["event"] == "stopped"), events
            print("PASS: durable lifecycle records the session id, all stops and both resumes")
            control.close()
            control = None
            assert daemon.wait(timeout=5) == 0, "idle daemon failed"
            print("PASS: idle daemon exits after the last task and client stop")
        finally:
            for connection in (acp, control):
                if connection:
                    connection.close()
            if task_id is not None:
                try:
                    if daemon.poll() is not None:
                        daemon = subprocess.Popen([str(deckd), "--data", str(data), "--idle-ms", "200"], env=environment)
                    cleanup = connect_when_ready(control_path, daemon)
                    try:
                        cleanup.control("destroy", id=task_id)
                    finally:
                        cleanup.close()
                except (OSError, AssertionError):
                    pass
            if daemon.poll() is None:
                daemon.kill()
                daemon.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    project = Path(__file__).resolve().parents[1]
    parser.add_argument("--deckd", type=Path, default=project / "target/debug/deckd")
    parser.add_argument("--fake", type=Path, default=project / "target/debug/deck-fake-agent")
    args = parser.parse_args()
    check(args.deckd.resolve(), args.fake.resolve())


if __name__ == "__main__":
    main()
