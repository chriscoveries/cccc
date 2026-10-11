#!/usr/bin/env python3
"""Exercise one private claw daemon with real ACP, viewer sockets and local inference.

Never point this at a production CCCC_HOME. Each case creates its own home.
"""
import argparse
import importlib.util
import json
import os
import pathlib
import signal
import socket
import statistics
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

spec = importlib.util.spec_from_file_location("rss_probe", pathlib.Path(__file__).with_name("kilo-memory-probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def wait_for(description, predicate, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(.1)
    raise TimeoutError(description)


class Model(BaseHTTPRequestHandler):
    requests = []
    release_first = threading.Event()
    release_cancel = threading.Event()

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        latest = next((message["content"] for message in reversed(body.get("messages", [])) if message["role"] == "user"), "")
        text = latest if isinstance(latest, str) else json.dumps(latest)
        marker = next((value for value in ["FIRST", "SECOND", "THIRD", "CANCEL"] if f"CWF_HEADLESS_{value}" in text), "OTHER")
        self.requests.append({"marker": marker, "at": time.monotonic()})
        if marker == "FIRST":
            self.release_first.wait(90)
        if marker == "CANCEL":
            self.release_cancel.wait(90)
        chunks = [{"id": "proof", "object": "chat.completion.chunk", "created": 1,
            "model": "first", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "MEMORY_PROBE_FINAL"}, "finish_reason": None}]},
            {"id": "proof", "object": "chat.completion.chunk", "created": 1,
            "model": "first", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}]
        data = ("".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks) + "data: [DONE]\n\n").encode()
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--runtime", choices=["kilo", "opencode"], required=True)
    parser.add_argument("--executable", required=True)
    parser.add_argument("--repo", required=True, type=pathlib.Path)
    parser.add_argument("--root", required=True, type=pathlib.Path)
    parser.add_argument("--case", required=True)
    parser.add_argument("--expect-headless", action="store_true")
    parser.add_argument("--rss-only", action="store_true", help="Idle startup before/after comparison without delivery proof")
    parser.add_argument("--settle", type=int, default=30)
    args = parser.parse_args()
    trial = args.root / args.case
    trial.mkdir(parents=True, exist_ok=False)
    cccc_home = trial / "cccc"
    daemon_env = os.environ.copy()
    daemon_env.update(CCCC_HOME=str(cccc_home), CCCC_LAUNCHER_PATH=str(args.binary))
    for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"]:
        daemon_env[key] = str(trial / key.lower())
    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    config = {"model": "cccc-test/first", "small_model": "cccc-test/first",
        "agent": {"title": {"disable": True}},
        "provider": {"cccc-test": {"npm": "@ai-sdk/openai-compatible", "name": "Local daemon proof",
            "options": {"baseURL": f"http://127.0.0.1:{model.server_port}/v1", "apiKey": "local-test"},
            "models": {"first": {"name": "first", "limit": {"context": 64000, "output": 4096}}}}}}
    actor_env = {args.runtime.upper() + "_CONFIG_CONTENT": json.dumps(config), "CCCC_OPENCODE_HEADLESS": "1"}
    daemon = None
    connections = []
    result = {"case": args.case, "runtime": args.runtime, "samples": {}}
    log = (trial / "daemon.log").open("a")

    def connect():
        address = json.loads((cccc_home / "daemon/ccccd.addr.json").read_text())
        connection = socket.socket(socket.AF_UNIX)
        connection.settimeout(90)
        connection.connect(address["path"])
        return connection

    def header(connection, op, **params):
        print(f"IPC {op}", flush=True)
        connection.sendall((json.dumps({"v": 1, "op": op, "args": {"by": "user", **params}}) + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            chunk = connection.recv(1)
            if not chunk:
                raise RuntimeError(f"IPC closed: {op}")
            data += chunk
        response = json.loads(data)
        if not response["ok"]:
            raise RuntimeError(response)
        return response["result"]

    def request(op, **params):
        with connect() as connection:
            return header(connection, op, **params)

    def launch():
        nonlocal daemon
        daemon = subprocess.Popen([str(args.binary), "daemon", "run"], env=daemon_env,
            stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        wait_for("daemon address", lambda: (cccc_home / "daemon/ccccd.addr.json").exists() or
            (daemon.poll() is not None and (_ for _ in ()).throw(RuntimeError("daemon exited"))))
        wait_for("IPC ready", lambda: request("group_list"))

    def stop():
        nonlocal daemon
        if daemon and daemon.poll() is None:
            daemon.send_signal(signal.SIGTERM)
            try:
                daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(daemon.pid, signal.SIGKILL)
                daemon.wait(timeout=10)
        daemon = None

    def sample(name):
        values = []
        for _ in range(args.settle):
            processes = probe.process_tree(daemon.pid)
            values.append({"lane_mb": round(sum(p["rss_mb"] for p in processes if p["pid"] != daemon.pid), 3), "processes": processes})
            time.sleep(1)
        result["samples"][name] = values
        result[name + "_median_mb"] = round(statistics.median(value["lane_mb"] for value in values[-10:]), 3)

    def viewer(mode="viewer", takeover=False):
        connection = connect()
        response = header(connection, "term_attach", group_id=group_id, actor_id="probe", mode=mode, takeover=takeover)
        def drain():
            try:
                while connection.recv(65536):
                    pass
            except OSError:
                pass
        threading.Thread(target=drain, daemon=True).start()
        connections.append(connection)
        return connection, response

    def close_viewer(connection):
        connection.shutdown(socket.SHUT_RDWR)
        connection.close()
        connections.remove(connection)

    try:
        launch()
        group_id = request("attach", path=str(args.repo))["group_id"]
        command = [args.executable] + (["--pure"] if args.runtime == "kilo" else [])
        request("actor_add", group_id=group_id, actor_id="probe", runtime=args.runtime,
            command=command, env=actor_env)
        request("group_start", group_id=group_id)
        actor = lambda: request("actor_list", group_id=group_id)["actors"][0]
        wait_for("actor running", lambda: actor().get("running"))
        session_path = cccc_home / "groups" / group_id / "state/runtime_sessions/probe.json"
        session = lambda: json.loads(session_path.read_text())["provider_session_id"]
        result["session_before"] = wait_for("session recorded", lambda: session() if session_path.exists() else None)
        sample("startup")
        if args.rss_only:
            result["initial_processes"] = probe.process_tree(daemon.pid)
            (trial / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps({key: value for key, value in result.items() if key != "samples"}), flush=True)
            return
        events = []
        def send(marker):
            source = request("message_send", group_id=group_id, to=["probe"], text=f"CWF_HEADLESS_{marker}", message_mode="send")["event"]
            events.append(source["id"])
            return source
        send("FIRST")
        wait_for("first prompt admitted", lambda: any(record["marker"] == "FIRST" for record in Model.requests))
        wait_for("working state", lambda: actor().get("effective_working_state") == "working")
        result["working_while_first"] = True
        send("SECOND")
        time.sleep(2)
        assert [record["marker"] for record in Model.requests] == ["FIRST"], Model.requests
        Model.release_first.set()
        wait_for("queued second prompt", lambda: any(record["marker"] == "SECOND" for record in Model.requests))
        ledger_path = cccc_home / "groups" / group_id / "ledger.jsonl"
        def ledger():
            return [json.loads(line) for line in ledger_path.read_text().splitlines()]
        def accepted():
            return [event["data"]["source_event_id"] for event in ledger()
                if event["kind"] == "runtime.delivery" and event["data"]["state"] == "accepted"]
        wait_for("both delivery receipts", lambda: all(source in accepted() for source in events))
        wait_for("idle state", lambda: actor().get("effective_working_state") == "idle")
        result["idle_after_second"] = True
        sample("closed")
        if args.expect_headless:
            assert not any(" attach " in process["command"] for process in result["samples"]["closed"][-1]["processes"])
        first, _ = viewer()
        second, _ = viewer()
        sample("open")
        close_viewer(first)
        time.sleep(1)
        assert any(" attach " in process["command"] for process in probe.process_tree(daemon.pid))
        close_viewer(second)
        sample("closed_again")
        if args.expect_headless:
            assert not any(" attach " in process["command"] for process in result["samples"]["closed_again"][-1]["processes"])
        stop()
        launch()
        wait_for("restored actor", lambda: actor().get("running"))
        result["session_after"] = session()
        assert result["session_after"] == result["session_before"]
        send("THIRD")
        wait_for("resumed delivery", lambda: any(record["marker"] == "THIRD" for record in Model.requests))
        wait_for("all receipts durable", lambda: all(source in accepted() for source in events))
        wait_for("resumed idle state", lambda: actor().get("effective_working_state") == "idle")
        controller, control_header = viewer("control")
        passive, passive_header = viewer("viewer")
        assert control_header["terminal_writable"] and not passive_header["terminal_writable"]
        send("CANCEL")
        wait_for("cancel turn in progress", lambda: any(record["marker"] == "CANCEL" for record in Model.requests))
        wait_for("cancel working state", lambda: actor().get("effective_working_state") == "working")
        passive.sendall(b"\x03")
        time.sleep(.3)
        assert actor()["effective_working_state"] == "working", "passive viewer must not cancel"
        controller.sendall(b"\x03")
        wait_for("ACP cancelled turn", lambda: actor().get("effective_working_state") == "idle")
        assert actor()["running"] and session() == result["session_before"]
        Model.release_cancel.set()
        result["native_cancel_preserves_session"] = True
        close_viewer(controller)
        close_viewer(passive)
        wait_for("cancel receipt", lambda: all(source in accepted() for source in events))
        assert [record["marker"] for record in Model.requests] == ["FIRST", "SECOND", "THIRD", "CANCEL"], Model.requests
        assert all(accepted().count(source) == 1 for source in events), accepted()
        result["model_requests"] = Model.requests
        result["source_events"] = events
        result["accepted_events"] = accepted()
        (trial / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps({key: value for key, value in result.items() if key != "samples"}), flush=True)
    finally:
        Model.release_first.set()
        Model.release_cancel.set()
        for connection in connections[:]:
            close_viewer(connection)
        stop()
        model.shutdown()
        log.close()


if __name__ == "__main__":
    main()
