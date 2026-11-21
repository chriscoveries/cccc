#!/usr/bin/env python3
"""Offline `kilo acp` fixture: ACP JSON-RPC on stdio plus the authenticated
loopback HTTP backend (health + /event SSE) the managed attach path requires.
No provider, network or real tool execution."""
import base64
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

args = sys.argv[1:]
password = os.environ.get("KILO_SERVER_PASSWORD", "")
port = int(args[args.index("--port") + 1]) if "--port" in args else 0
sid = "kilo-fixture-session"
active = None
events = []
event_lock = threading.Condition()


def emit(**frame):
    print(json.dumps({"jsonrpc": "2.0", **frame}), flush=True)


def publish(kind, properties):
    with event_lock:
        events.append({"type": kind, "properties": properties})
        event_lock.notify_all()


def finish(reason="end_turn"):
    global active
    if active is not None:
        emit(id=active, result={"stopReason": reason})
    active = None
    publish("session.status", {"sessionID": sid, "status": {"type": "idle"}})


def session_result(new=False):
    result = {"models": {"availableModels": [{"modelId": "kilo-fixture", "name": "Fixture model"}]},
              "configOptions": [{"id": "model", "options": [{"value": "kilo-fixture", "name": "Fixture model"}]}],
              "modes": {"availableModes": [{"id": mode, "name": mode} for mode in ["fixture-agent", "default"]]}}
    if new:
        result["sessionId"] = sid
    return result


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def authorized(self):
        expected = "Basic " + base64.b64encode(("kilo:" + password).encode()).decode()
        if self.headers.get("Authorization") == expected:
            return True
        self.send_response(401)
        self.end_headers()
        return False

    def do_GET(self):
        if not self.authorized():
            return
        if self.path == "/global/health":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"healthy":true}')
            return
        if self.path.startswith("/event"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            offset = 0
            while True:
                with event_lock:
                    if len(events) == offset:
                        event_lock.wait(timeout=1)
                    batch = events[offset:]
                    offset = len(events)
                for event in batch:
                    self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                self.wfile.flush()
            return
        self.send_response(404)
        self.end_headers()


server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
server.daemon_threads = True
threading.Thread(target=server.serve_forever, daemon=True).start()

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    if method is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": 1,
                  "agentCapabilities": {"loadSession": True, "sessionCapabilities": {"list": {}}}}
    elif method == "session/list":
        result = {"sessions": [{"sessionId": sid, "cwd": os.getcwd()}]}
    elif method == "session/new":
        result = session_result(new=True)
    elif method == "session/load":
        result = session_result()
    elif method in ("session/set_config_option", "session/set_model", "session/set_mode"):
        result = {}
    elif method == "session/prompt":
        active = request["id"]
        text = params["prompt"][0]["text"]
        publish("session.status", {"sessionID": sid, "status": {"type": "busy"}})
        emit(method="session/update", params={"sessionId": sid, "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "live:" + text}}})
        if text != "HOLD":
            finish()
        continue
    elif method == "session/cancel":
        finish("cancelled")
        continue
    else:
        result = {}
    if "id" in request:
        emit(id=request["id"], result=result)
