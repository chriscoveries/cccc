#!/usr/bin/env python3
"""Isolated fake OpenCode/Kilo backend for the initial-model tests (v2).

Serves exactly what the real managed launch path touches, with no provider,
no credentials and no network beyond loopback:

  * `--version`            -> prints `opencode 1.18.31` and exits WITHOUT
                              dumping: the env dump is an ACP-phase artifact
                              only, so a version-only run cannot leave a
                              stale oracle file behind
  * `acp --port <p> ...`   -> dumps the ACTUAL spawned process environment
                              to env-dump.json in the cwd, STRICTLY
                              allowlisted to the EXACT keys
                              OPENCODE_CONFIG_CONTENT, KILO_CONFIG_CONTENT,
                              OC_FIXTURE_MARKER — never HOME, never
                              SERVER_* credentials, never wildcard matches.
                              Starts an HTTP listener with REAL basic-auth
                              enforcement on /global/health and an SSE stream on /event
                              (real 0x0A newline bytes; AST-counted by the
                              tests), then answers ACP over stdio:
                              initialize (protocol v1 + loadSession),
                              session/new (fixed id), session/set_config_option
                              (appended to config-options.jsonl).
"""
import base64
import http.server
import json
import os
import sys
import threading
import time

# Exact allowlist (no wildcard patterns): the three keys the contract names,
# and nothing else — never HOME, never SERVER_* credentials.
DUMP_ALLOW_KEYS = (
    "OPENCODE_CONFIG_CONTENT",
    "KILO_CONFIG_CONTENT",
    "OC_FIXTURE_MARKER",
)


def dump_env():
    keep = {
        key: value
        for key, value in os.environ.items()
        if key in DUMP_ALLOW_KEYS
    }
    with open("env-dump.json", "w", encoding="utf-8") as handle:
        json.dump(keep, handle)


class Handler(http.server.BaseHTTPRequestHandler):
    expected = ""

    def log_message(self, format, *args):  # silence the fixture (http.server API)
        pass

    def do_GET(self):  # noqa: N802 - http.server API
        auth = self.headers.get("authorization", "")
        if self.path.startswith("/global/health"):
            if auth != Handler.expected:
                self.send_response(401)
                self.end_headers()
                return
            body = b'{"healthy":true}'
            self.send_response(200)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.startswith("/event"):
            if auth != Handler.expected:
                self.send_response(401)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.end_headers()
            try:
                while True:
                    # python source escape -> two REAL 0x0A bytes, never the
                    # literal four characters backslash-n. The tests count
                    # newline bytes via ast.parse of this file.
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
                    time.sleep(30)
            except Exception:  # client went away; the test owns teardown
                return
        self.send_response(404)
        self.end_headers()


def main():
    args = sys.argv[1:]
    if "--version" in args:
        print("opencode 1.18.31")
        sys.stdout.flush()
        return
    dump_env()  # ACP phase only, after the version-probe early exit
    port = int(args[args.index("--port") + 1])
    user = os.environ.get("OPENCODE_SERVER_USERNAME") or os.environ.get(
        "KILO_SERVER_USERNAME", ""
    )
    password = os.environ.get("OPENCODE_SERVER_PASSWORD") or os.environ.get(
        "KILO_SERVER_PASSWORD", ""
    )
    Handler.expected = "Basic " + base64.b64encode(
        f"{user}:{password}".encode()
    ).decode()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except Exception:
            continue
        method = request.get("method")
        request_id = request.get("id")
        result = {}
        if method == "initialize":
            result = {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True},
            }
        elif method == "session/new":
            result = {"sessionId": "fake-session-1"}
        elif method == "session/set_config_option":
            with open("config-options.jsonl", "a", encoding="utf-8") as handle:
                handle.write(json.dumps(request.get("params", {})) + "\n")
        if request_id is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}))
            sys.stdout.flush()


if __name__ == "__main__":
    main()
