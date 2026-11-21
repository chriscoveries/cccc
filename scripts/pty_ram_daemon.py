#!/usr/bin/env python3
"""Reproduce chatty PTY retention in an isolated foreground CCCC daemon.

Run on a development host: pty_ram_daemon.py BINARY CASE_DIR [SECONDS]
Set HEAPTRACK=1 to profile the daemon. CASE_DIR must be new; no production home
is loaded or copied. The daemon and all fake actors stop at the end.
"""
import csv
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time


def main():
    binary = Path(sys.argv[1]).resolve()
    root = Path(sys.argv[2]).resolve()
    seconds = int(sys.argv[3]) if len(sys.argv) > 3 else 7200
    root.mkdir(parents=True, exist_ok=False)
    home = root / "home"
    home.mkdir()
    workspace = root / "workspace"
    workspace.mkdir()
    env = dict(os.environ, CCCC_HOME=str(home))
    for key in ("CCCC_GROUP_ID", "CCCC_ACTOR_ID", "CCCC_DAEMON_PORT", "CCCC_DAEMON_HOST", "CCCC_DAEMON_TRANSPORT"):
        env.pop(key, None)
    command = [str(binary), "daemon", "run"]
    if os.environ.get("HEAPTRACK") == "1":
        command = ["heaptrack", "-o", str(root / "heap")] + command
    with (root / "daemon.log").open("w") as log:
        daemon = subprocess.Popen(command, env=env, stdout=log, stderr=log)
        endpoint = home / "daemon" / "ccccd.sock"

        def call(op, **args):
            args.setdefault("by", "user")
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(60)
                connection.connect(str(endpoint))
                connection.sendall(json.dumps({"v": 1, "op": op, "args": args}).encode() + b"\n")
                response = json.loads(connection.makefile("rb").readline())
            if not response.get("ok"):
                raise RuntimeError(response)
            return response["result"]

        try:
            deadline = time.monotonic() + 60
            while not endpoint.exists():
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("isolated daemon failed to start; inspect daemon.log")
                time.sleep(0.1)
            pid = int((home / "daemon" / "ccccd.pid").read_text())
            groups = []
            emitter = (
                "import os,time\n"
                f"end=time.monotonic()+{seconds + 120}\n"
                "while time.monotonic()<end:\n"
                " os.write(1,b'x\\n')\n"
                " time.sleep(0.001)\n"
            )
            for number in range(6):
                result = call("group_create", title=f"PTY RAM {number}")
                group = result["group"]["group_id"]
                groups.append(group)
                call("attach", group_id=group, path=str(workspace))
                for actor in range(3):
                    call("actor_add", group_id=group, actor_id=f"a{actor}", runtime="custom",
                         command=["python3", "-u", "-c", emitter], env={"LD_PRELOAD": ""})
                call("group_start", group_id=group)
            (root / "manifest.json").write_text(json.dumps({"binary": str(binary), "pid": pid, "groups": groups, "actors": 18, "seconds": seconds}, indent=2))
            start = time.monotonic()
            with (root / "rss.csv").open("w") as output:
                writer = csv.writer(output)
                writer.writerow(["seconds", "rss_kib", "total_cursor", "ledger_bytes"])
                while True:
                    elapsed = int(time.monotonic() - start)
                    status = Path(f"/proc/{pid}/status").read_text()
                    rss = next(line.split()[1] for line in status.splitlines() if line.startswith("VmRSS:"))
                    cursor = 0
                    for group in groups:
                        for actor in range(3):
                            page = call("terminal_history", group_id=group, actor_id=f"a{actor}", limit_bytes=1)
                            cursor += page["end_cursor"]
                    ledger_bytes = sum(path.stat().st_size for path in home.glob("groups/*/ledger.jsonl"))
                    writer.writerow([elapsed, rss, cursor, ledger_bytes])
                    output.flush()
                    if elapsed >= seconds:
                        break
                    time.sleep(min(10, seconds - elapsed))
            call("shutdown")
            daemon.wait(timeout=60)
        finally:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    main()
