#!/usr/bin/env python3
"""Offline `codex app-server` fixture: prints a loopback WebSocket endpoint
and answers the JSON-RPC methods the managed actor launch uses. No provider,
network or real tool execution. `F615-CODEX-HOLD` in a turn/start parks the
turn until <fixture dir>/release-turn appears."""
import base64
import hashlib
import json
import os
import socket
import struct
import threading
import time

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
fixture_dir = os.environ.get("CODEX_FIXTURE_DIR", os.getcwd())
turn_counter = 0
turn_lock = threading.Lock()
send_lock = threading.Lock()


def ws_send(sock, opcode, payload):
    data = payload.encode() if isinstance(payload, str) else payload
    header = bytearray([0x80 | opcode])
    length = len(data)
    if length < 126:
        header.append(length)
    elif length < 65536:
        header.append(126)
        header += struct.pack(">H", length)
    else:
        header.append(127)
        header += struct.pack(">Q", length)
    with send_lock:
        sock.sendall(bytes(header) + data)


def ws_recv_exact(sock, count):
    chunks = bytearray()
    while len(chunks) < count:
        part = sock.recv(count - len(chunks))
        if not part:
            raise ConnectionError("ws closed")
        chunks += part
    return bytes(chunks)


def ws_recv(sock):
    fragments = bytearray()
    while True:
        head = ws_recv_exact(sock, 2)
        fin = head[0] & 0x80
        opcode = head[0] & 0x0F
        masked = head[1] & 0x80
        length = head[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", ws_recv_exact(sock, 2))[0]
        elif length == 127:
            length = struct.unpack(">Q", ws_recv_exact(sock, 8))[0]
        mask = ws_recv_exact(sock, 4) if masked else b""
        payload = ws_recv_exact(sock, length) if length else b""
        if masked:
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        if opcode == 0x8:
            raise ConnectionError("ws close")
        if opcode == 0x9:
            ws_send(sock, 0xA, payload)
            continue
        if opcode in (0x0, 0x1, 0x2):
            fragments += payload
            if fin:
                return bytes(fragments)


def send_result(sock, request_id, result):
    ws_send(sock, 0x1, json.dumps(
        {"jsonrpc": "2.0", "id": request_id, "result": result}))


def send_notification(sock, method, params):
    ws_send(sock, 0x1, json.dumps(
        {"jsonrpc": "2.0", "method": method, "params": params}))


def complete_turn(sock, thread_id, turn_id, delay_file=None):
    def runner():
        if delay_file:
            deadline = time.monotonic() + 20
            while not os.path.exists(delay_file):
                if time.monotonic() > deadline:
                    break
                time.sleep(0.02)
        send_notification(sock, "turn/completed", {
            "threadId": thread_id,
            "turn": {"id": turn_id, "status": "completed"}})

    threading.Thread(target=runner, daemon=True).start()


def handle(sock):
    global turn_counter
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        chunk = sock.recv(4096)
        if not chunk:
            return
        buffer += chunk
    head = buffer.split(b"\r\n\r\n", 1)[0].decode("latin1")
    key = next(
        (line.split(":", 1)[1].strip() for line in head.split("\r\n")
         if line.lower().startswith("sec-websocket-key:")),
        None,
    )
    if key is None:
        return
    accept = base64.b64encode(
        hashlib.sha1((key + GUID).encode()).digest()).decode()
    sock.sendall((
        "HTTP/1.1 101 Switching Protocols\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
    ).encode())
    while True:
        try:
            frame = ws_recv(sock)
        except (ConnectionError, OSError):
            return
        try:
            message = json.loads(frame)
        except ValueError:
            continue
        if not isinstance(message, dict) or "method" not in message:
            continue
        method = message["method"]
        params = message.get("params", {})
        request_id = message.get("id")
        if method == "initialize":
            result = {}
        elif method == "thread/start":
            result = {"thread": {"id": "thread-1"}}
        elif method == "thread/resume":
            result = {"thread": {"id": params.get("threadId", "thread-1")}}
        elif method == "thread/read":
            result = {"thread": {"id": "thread-1", "turns": []}}
        elif method == "turn/start":
            with turn_lock:
                turn_counter += 1
                turn_id = f"turn-{turn_counter}"
            result = {"turn": {"id": turn_id}}
            if request_id is not None:
                send_result(sock, request_id, result)
            send_notification(sock, "turn/started", {
                "threadId": "thread-1", "turn": {"id": turn_id}})
            text = json.dumps(params)
            if "F615-CODEX-HOLD" in text:
                complete_turn(sock, "thread-1", turn_id,
                              os.path.join(fixture_dir, "release-turn"))
            else:
                complete_turn(sock, "thread-1", turn_id)
            continue
        elif method == "turn/steer":
            result = {"turnId": params.get("expectedTurnId", "turn-1")}
        elif method == "turn/interrupt":
            result = {}
        elif request_id is None:
            continue
        else:
            result = {}
        if request_id is not None:
            send_result(sock, request_id, result)


server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", 0))
server.listen(4)
port = server.getsockname()[1]
print(f"codex app-server listening on: ws://127.0.0.1:{port}/", flush=True)

while True:
    conn, _ = server.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
