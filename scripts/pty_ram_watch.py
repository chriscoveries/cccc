#!/usr/bin/env python3
"""Sample RSS and swap for existing isolated PTY experiment processes."""
import csv
from datetime import datetime, timezone
import os
from pathlib import Path
import sys
import time


def main():
    output = Path(sys.argv[1])
    pids = [int(value) for value in sys.argv[2:]]
    ticks = os.sysconf("SC_CLK_TCK")
    with output.open("x") as stream:
        writer = csv.writer(stream)
        writer.writerow(["utc", "pid", "process_seconds", "rss_kib", "swap_kib",
                         "rss_plus_swap_kib", "python_children", "mem_available_kib"])
        while pids:
            uptime = float(Path("/proc/uptime").read_text().split()[0])
            mem = dict(line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())
            for pid in list(pids):
                proc = Path(f"/proc/{pid}")
                try:
                    stat = (proc / "stat").read_text().rsplit(")", 1)[1].split()
                    status = dict(line.split(":", 1) for line in (proc / "status").read_text().splitlines())
                    rss = int(status["VmRSS"].split()[0])
                    swap = int(status["VmSwap"].split()[0])
                    children = (proc / "task" / str(pid) / "children").read_text().split()
                    python = sum((Path(f"/proc/{child}/comm").read_text().strip() == "python3")
                                 for child in children if Path(f"/proc/{child}/comm").exists())
                    elapsed = uptime - int(stat[19]) / ticks
                    writer.writerow([datetime.now(timezone.utc).isoformat(), pid, round(elapsed, 2),
                                     rss, swap, rss + swap, python,
                                     int(mem["MemAvailable"].split()[0])])
                except (FileNotFoundError, ProcessLookupError, KeyError):
                    pids.remove(pid)
            stream.flush()
            if pids:
                time.sleep(10)


if __name__ == "__main__":
    main()
