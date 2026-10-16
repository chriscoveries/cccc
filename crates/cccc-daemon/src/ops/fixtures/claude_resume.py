#!/usr/bin/env python3
"""Offline Agent View fixture: exact resume, copied IDs, missing history and trust."""
import hashlib
import json
import os
import pathlib
import socket
import sys
import time
import threading
import tty

SAVED = "52b41c61-e23c-4b7c-8b60-809c347451b5"
FRESH = "ca52e69a-6596-4abd-a0ec-3e8690dc70e1"
SHORT = "abcdef12"
FRESH_SHORT = "fedcba21"


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value))
    temporary.replace(path)


def read_request(connection):
    # A canceled readiness probe may close before writing its whole frame.
    # Reject that connection without taking down the sole cleanup/list server.
    try:
        with connection.makefile("rb", buffering=0) as reader:
            line = reader.readline(512 * 1024 + 1)
    except ConnectionError:
        return None
    if not line.endswith(b"\n") or len(line) > 512 * 1024:
        return None
    try:
        request = json.loads(line)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    return request if isinstance(request, dict) and isinstance(request.get("op"), str) else None


def send_response(connection, response):
    try:
        connection.sendall(json.dumps(response).encode() + b"\n")
        return True
    except ConnectionError:
        # Native clients may abandon an outstanding has/attach at their timeout.
        # The next owned kill/list request must still reach this server.
        return False


def fresh_model_flags(arguments):
    # New screen fixtures must describe the model actually passed by CCCC,
    # rather than manufacturing the expected target in durable native state.
    for index, argument in enumerate(arguments):
        if argument in ("--model", "-m") and index + 1 < len(arguments):
            return ["--model", arguments[index + 1]]
        if argument.startswith("--model="):
            return ["--model", argument.split("=", 1)[1]]
    return []


if len(sys.argv) > 1 and sys.argv[1] == "server":
    root = pathlib.Path(sys.argv[2]).resolve()
    directory = pathlib.Path("/tmp") / f"cc-daemon-{os.getuid()}" / hashlib.sha256(str(root).encode()).hexdigest()[:8]
    directory.mkdir(parents=True, exist_ok=True)
    directory.chmod(0o700)
    daemon = root / "daemon"
    daemon.mkdir(exist_ok=True)
    key = daemon / "control.key"
    key.write_text("0123456789abcdef0123456789abcdef")
    key.chmod(0o600)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(directory / "control.sock"))
    listener.listen()
    (root / "ready").touch()
    while True:
        connection, _ = listener.accept()
        with connection:
            request = read_request(connection)
            if request is None:
                continue
            op = request["op"]
            with (root / "control_operations").open("a") as operations:
                operations.write(json.dumps({"op": op, "short": request.get("short")}) + "\n")
            version_path = root / "fixture_worker_version"
            if not version_path.exists():
                version_path = root / "fixture_cli_version"
            version = version_path.read_text().strip() if version_path.exists() else "2.1.286"
            jobs = []
            for active, short in [("active", SHORT), ("fresh_active", FRESH_SHORT)]:
                if (root / active).exists():
                    state = json.loads((root / "jobs" / short / "state.json").read_text())
                    jobs.append({"short": short, "sessionId": state["sessionId"],
                                 "cwd": state["cwd"], "cliVersion": version})
            selected_job = next((job for job in jobs if job["short"] == request.get("short")), None)
            if op == "list":
                if (root / "fixture_cleanup_failed").exists():
                    result = {"ok": False, "op": op, "code": "EPROTO", "error": "fixture cleanup unknown"}
                else:
                    result = {"ok": True, "op": op, "jobs": jobs}
            elif op == "attach" and (root / "mode").read_text().startswith("model_adapter_screen_"):
                # New screen tests alone use a threaded attachment so real has
                # probes and owned cleanup remain serviced while replay streams.
                if selected_job is None:
                    send_response(connection, {"ok": False, "op": op, "code": "ENOJOB", "error": "fixture job is not active"})
                    continue
                if request.get("auth") != key.read_text().strip():
                    send_response(connection, {"ok": False, "op": op, "code": "EAUTH", "error": "fixture authentication failed"})
                    continue
                mode = (root / "mode").read_text()
                attached = connection.dup()
                if not send_response(connection, {"ok": True, "op": op, "imarkNonce": "0123456789abcdef0123456789abcdef"}):
                    attached.close()
                    continue

                def serve_attachment(stream, attachment_mode):
                    with stream:
                        try:
                            stream.sendall(b"x" * (384 * 1024))
                            stream.sendall(b'\x1b_cc-d-imark;{"kind":"prompt_idle","nonce":"0123456789abcdef0123456789abcdef"}\x1b\\')
                            prompt = "❯\r\n⏵⏵ bypass permissions on".encode()
                            if attachment_mode.endswith("post_input"):
                                stream.sendall(b"\x1b[?2026h\x1b[2J\x1b[H" + prompt + b"\x1b[?2026l")
                            elif attachment_mode.endswith("draft"):
                                stream.sendall(b"\x1b[?2026h\x1b[2J\x1b[H" + "❯ existing draft\r\n⏵⏵ bypass permissions on".encode() + b"\x1b[?2026l")
                            else:
                                # Complete incremental paint, but no retained
                                # reconstruction anchor after actual eviction.
                                stream.sendall(b"\x1b[?2026h" + prompt + b"\x1b[?2026l")
                            while True:
                                data = stream.recv(4096)
                                if not data:
                                    break
                                with (root / "model_native_input").open("ab") as trace:
                                    trace.write(data)
                                if attachment_mode.endswith("post_input"):
                                    stream.sendall(b"x" * (384 * 1024))
                                    stream.sendall(b"\x1b[?2026hincremental output\x1b[?2026l")
                        except (BrokenPipeError, ConnectionResetError):
                            pass

                threading.Thread(target=serve_attachment, args=(attached, mode), daemon=True).start()
                continue
            elif op == "has" and (root / "mode").read_text().startswith("model_adapter_screen_"):
                # Match this request's short, not another active job or stale
                # state.json. A successful kill removes its active marker;
                # deliberately refused cleanup leaves that marker present.
                alive = selected_job is not None
                result = {"ok": True, "op": op, "alive": alive, "ready": alive}
            elif op == "kill":
                if selected_job is None:
                    result = {"ok": False, "op": op, "code": "ENOJOB", "error": "fixture job is not active"}
                elif (root / "mode").read_text() in ("model_adapter_cleanup_failure", "model_adapter_screen_unreadable_cleanup_failure"):
                    (root / "fixture_cleanup_failed").touch()
                    result = {"ok": False, "op": op, "code": "EPROTO", "error": "fixture stop refused"}
                else:
                    active = "fresh_active" if request.get("short") == FRESH_SHORT else "active"
                    (root / active).unlink(missing_ok=True)
                    result = {"ok": True, "op": op}
            else:
                result = {"ok": False, "op": op, "code": "EPROTO", "error": "unexpected fixture operation"}
            send_response(connection, result)
elif "--version" in sys.argv:
    root = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
    version_path = root / "fixture_launcher_version"
    if not version_path.exists():
        version_path = root / "fixture_cli_version"
    version = version_path.read_text().strip() if version_path.exists() else "2.1.286"
    print(f"{version} (Claude Code)")
else:
    root = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
    mode = (root / "mode").read_text()
    if "--bg" in sys.argv:
        with (root / "launch_arguments").open("a") as log:
            log.write(json.dumps(sys.argv[1:]) + "\n")
        with (root / "launches").open("a") as log:
            log.write("resume\n" if "--resume" in sys.argv else "fresh\n")
        if mode == "untrusted":
            print("Workspace not trusted", file=sys.stderr)
            sys.exit(1)
        if mode == "delayed_resume" and "--resume" in sys.argv:
            (root / "resume_pending").touch()
            deadline = time.monotonic() + 10
            while not (root / "release_resume").exists() and time.monotonic() < deadline:
                time.sleep(0.01)
        session = SAVED if "--resume" in sys.argv and mode != "copied" else FRESH
        path = root / "projects" / "workspace" / f"{session}.jsonl"
        if mode != "missing_history" and not path.exists():
            path.parent.mkdir(parents=True, exist_ok=True)
            # Fresh launches follow this transcript from offset zero. A user record
            # requires promptId, and the completed fixture turn must settle.
            records = [
                {"type": "user", "sessionId": session, "cwd": os.getcwd(),
                 "promptId": f"fixture-{session}", "message": {"content": "fixture history"}},
                {"type": "system", "sessionId": session, "cwd": os.getcwd(),
                 "subtype": "turn_duration"},
            ]
            path.write_text("".join(json.dumps(record) + "\n" for record in records))
        # Survivor tests preserve the old live job while a distinct fresh
        # job starts. Existing fixture modes retain their original shape.
        distinct = mode == "surviving_model" and "--resume" not in sys.argv
        short = FRESH_SHORT if distinct else SHORT
        active = "fresh_active" if distinct else "active"
        state = {
            "sessionId": session, "daemonShort": short, "cwd": os.getcwd(),
            "tempo": "idle", "inFlight": {"tasks": 0, "queued": 0},
            "intent": "history-bearing", "linkScanOffset": 0,
            "linkScanPath": str(path) if path.exists() else None, "outcome": None,
        }
        if mode.startswith("model_adapter"):
            model = "sonnet" if "--resume" in sys.argv else "opus"
            state.update({"resumeSessionId": session, "respawnFlags": ["--model", model]})
            if mode.startswith("model_adapter_screen_") and "--resume" not in sys.argv:
                state["respawnFlags"] = fresh_model_flags(sys.argv[1:])
        write_json(root / "jobs" / short / "state.json", state)
        (root / active).touch()
        print(f"started · {short}")
    else:
        # Native attach or the temporary trust prompt. No model task is executed.
        tty.setraw(0)
        os.write(1, b"\x1b[?2004hOffline Claude fixture\r\n>")
        (root / "terminal_ready").touch()
        pending = b""
        while True:
            data = os.read(0, 65536)
            if not data:
                break
            pending += data
            while b"\r" in pending:
                message, pending = pending.split(b"\r", 1)
                with (root / "received").open("a") as received:
                    received.write(json.dumps(message.decode()) + "\n")
