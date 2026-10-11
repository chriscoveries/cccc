#!/usr/bin/env python3
"""Isolated Linux ACP/TUI RSS probe; no credentials or inference service required."""
import argparse
import json
import os
import pathlib
import pty
import queue
import signal
import statistics
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Model(BaseHTTPRequestHandler):
    read_file = None
    requests = []
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        tools = {tool["function"]["name"] for tool in request.get("tools", [])}
        read = self.read_file and "read" in tools and request.get("messages", [{}])[-1].get("role") != "tool"
        self.requests.append({"last_role": request.get("messages", [{}])[-1].get("role"), "read_requested": bool(read)})
        delta = {"role": "assistant", "content": "MEMORY_PROBE_FINAL"}
        if read:
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "read-probe", "type": "function",
                "function": {"name": "read", "arguments": json.dumps({"filePath": self.read_file, "limit": 40})}}]}
        chunks = [
            {"id": "probe", "object": "chat.completion.chunk", "created": 1,
             "model": "first", "choices": [{"index": 0, "delta": delta,
                 "finish_reason": None}]},
            {"id": "probe", "object": "chat.completion.chunk", "created": 1,
             "model": "first", "choices": [{"index": 0, "delta": {},
                 "finish_reason": "tool_calls" if read else "stop"}]},
        ]
        body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks)
        body += "data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode())


def process_tree(root):
    records = {}
    for directory in pathlib.Path("/proc").iterdir():
        if not directory.name.isdigit():
            continue
        try:
            fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
            records[int(directory.name)] = (int(fields[1]), int(fields[21]) * os.sysconf("SC_PAGE_SIZE"),
                                           (directory / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace"),
                                           (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"))
        except (OSError, ValueError, IndexError):
            continue
    selected = {root}
    while True:
        children = {pid for pid, (parent, _, _, _) in records.items() if parent in selected}
        if children <= selected:
            break
        selected |= children
    return [{"pid": pid, "rss_mb": round(records[pid][1] / 1e6, 3), "command": records[pid][2], "cpu_seconds": records[pid][3]}
            for pid in sorted(selected) if pid in records]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True, type=pathlib.Path)
    parser.add_argument("--repo", required=True, type=pathlib.Path)
    parser.add_argument("--executable", required=True)
    parser.add_argument("--runtime", choices=["kilo", "opencode"], required=True)
    parser.add_argument("--case", required=True)
    parser.add_argument("--viewer", action="store_true")
    parser.add_argument("--config", default="{}")
    parser.add_argument("--smol", action="store_true")
    parser.add_argument("--no-watcher", action="store_true")
    parser.add_argument("--read-file", help="Read a checkout file through the native tool to exercise lazy LSP startup")
    parser.add_argument("--turns", type=int, default=1, help="Synthetic turns for CPU/GC comparison")
    parser.add_argument("--settle", type=int, default=30)
    args = parser.parse_args()
    if args.turns < 1 or args.settle < 10:
        parser.error("--turns must be positive and --settle must be at least 10")
    Model.read_file = args.read_file
    trial = args.root / args.case
    trial.mkdir(parents=True, exist_ok=False)
    model = ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    env = os.environ.copy()
    prefix = args.runtime.upper()
    for key in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"]:
        env[key] = str(trial / key.lower())
    config = {"model": "cccc-test/first", "small_model": "cccc-test/first",
              "provider": {"cccc-test": {"npm": "@ai-sdk/openai-compatible", "name": "Local RSS fixture",
                "options": {"baseURL": f"http://127.0.0.1:{model.server_port}/v1", "apiKey": "local-test"},
                "models": {"first": {"name": "first", "limit": {"context": 32000, "output": 4096}}}}}}
    config.update(json.loads(args.config))
    env[prefix + "_CONFIG_CONTENT"] = json.dumps(config)
    env[prefix + "_SERVER_USERNAME"] = args.runtime
    env[prefix + "_SERVER_PASSWORD"] = "isolated-memory-fixture"
    env["KILO_NO_DAEMON"] = "1"
    env["TERM"] = "xterm-256color"
    if args.smol:
        env["BUN_OPTIONS"] = "--smol"
    if args.no_watcher:
        env[prefix + "_EXPERIMENTAL_DISABLE_FILEWATCHER"] = "true"
    import socket
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    command = [args.executable, "acp", "--hostname", "127.0.0.1", "--port", str(port), "--cwd", str(args.repo)]
    if args.runtime == "kilo":
        command += ["--pure"]
    stderr = (trial / "server.stderr").open("w")
    server = subprocess.Popen(command, cwd=args.repo, env=env, stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True)
    messages = queue.Queue()
    def reader():
        for line in server.stdout:
            try:
                messages.put(json.loads(line))
            except ValueError:
                (trial / "non-json.log").open("a").write(line)
    threading.Thread(target=reader, daemon=True).start()
    request_id = 0
    assistant_text = []
    def request(method, params):
        nonlocal request_id
        request_id += 1
        server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}) + "\n")
        server.stdin.flush()
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            try:
                message = messages.get(timeout=1)
            except queue.Empty:
                if server.poll() is not None:
                    raise RuntimeError("ACP exited; see " + str(trial / "server.stderr"))
                continue
            if message.get("method") == "session/request_permission":
                choice = next(option for option in message["params"]["options"] if option["kind"] == "allow_once")
                server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": message["id"],
                    "result": {"outcome": {"outcome": "selected", "optionId": choice["optionId"]}}}) + "\n")
                server.stdin.flush()
                continue
            update = message.get("params", {}).get("update", {})
            if update.get("sessionUpdate") == "agent_message_chunk":
                assistant_text.append(update.get("content", {}).get("text", ""))
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise RuntimeError(message)
                return message["result"]
        raise TimeoutError(method)
    viewer = None
    master = slave = None
    try:
        initialized = request("initialize", {"protocolVersion": 1, "clientCapabilities": {},
                              "clientInfo": {"name": "cccc-memory-probe", "version": "1"}})
        session_id = request("session/new", {"cwd": str(args.repo), "mcpServers": []})["sessionId"]
        if args.viewer:
            master, slave = pty.openpty()
            viewer_command = [args.executable, "attach", f"http://127.0.0.1:{port}", "--session", session_id, "--dir", str(args.repo)]
            if args.runtime == "kilo":
                viewer_command += ["--pure"]
            viewer = subprocess.Popen(viewer_command, cwd=args.repo, env=env, stdin=slave, stdout=slave, stderr=slave,
                                      start_new_session=True)
            os.close(slave)
            slave = None
            def drain():
                while True:
                    try:
                        os.read(master, 65536)
                    except OSError:
                        return
            threading.Thread(target=drain, daemon=True).start()
        def lane_cpu():
            return sum(process["cpu_seconds"] for process in process_tree(server.pid) + (process_tree(viewer.pid) if viewer else []))
        prompt_cpu_before = lane_cpu()
        prompt_started = time.monotonic()
        for _ in range(args.turns):
            prompt = request("session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": "Reply with MEMORY_PROBE_FINAL."}]})
        prompt_elapsed = time.monotonic() - prompt_started
        prompt_cpu = lane_cpu() - prompt_cpu_before
        if "MEMORY_PROBE_FINAL" not in "".join(assistant_text):
            raise RuntimeError("model result missing from ACP output")
        samples = []
        sampled_at = []
        for _ in range(args.settle):
            time.sleep(1)
            samples.append({"server": process_tree(server.pid), "viewer": process_tree(viewer.pid) if viewer else []})
            sampled_at.append(time.monotonic())
        totals = [sum(p["rss_mb"] for processes in sample.values() for p in processes) for sample in samples[-10:]]
        def sampled_cpu(sample):
            return sum(process["cpu_seconds"] for processes in sample.values() for process in processes)
        cpu_percent = 100 * (sampled_cpu(samples[-1]) - sampled_cpu(samples[-10])) / (sampled_at[-1] - sampled_at[-10])
        result = {"turns": args.turns, "prompt_cpu_seconds": round(prompt_cpu, 3),
                  "prompt_elapsed_seconds": round(prompt_elapsed, 3), "settled_cpu_percent_one_core": round(cpu_percent, 3),
                  "case": args.case, "runtime": args.runtime, "session_id": session_id,
                  "agent_info": initialized.get("agentInfo"), "prompt_result": prompt,
                  "model_requests": Model.requests,
                  "median_total_rss_mb": round(statistics.median(totals), 3), "samples": samples}
        (trial / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps({k: v for k, v in result.items() if k != "samples"}), flush=True)
    finally:
        for process in [viewer, server]:
            if process is not None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=5)
                except ProcessLookupError:
                    pass
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
        if master is not None:
            os.close(master)
        stderr.close()
        model.shutdown()


if __name__ == "__main__":
    main()
