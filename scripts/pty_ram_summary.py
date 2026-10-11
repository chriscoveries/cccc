#!/usr/bin/env python3
"""Summarize completed PTY soak CSVs and optional RSS/swap observations.

Run beside the case directories on the development host. Each case directory
has the same name as its CSV without the .csv suffix.
"""
import argparse
import csv
import hashlib
import json
from pathlib import Path
import statistics


def distribution(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values)}


def summarize(path, watch_rows, hot_cap):
    lines = path.read_text().splitlines()
    metadata = dict(field.split("=", 1) for field in lines[0].split(","))
    header = next(i for i, line in enumerate(lines) if line.startswith("seconds,"))
    rows = [{key: int(value) for key, value in row.items()}
            for row in csv.DictReader(lines[header:])]
    end = rows[-1]["seconds"]
    tail = [row for row in rows if row["seconds"] >= end - 900]
    archives = sorted(path.with_suffix("").glob("a*/session.pty"))
    sizes = [archive.stat().st_size - 16 for archive in archives]
    result = {
        "csv": path.name,
        "csv_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "metadata": metadata,
        "samples": len(rows),
        "final_seconds": end,
        "completed_requested_seconds": end >= int(metadata["duration"]),
        "final_rss_kib": rows[-1]["rss_kib"],
        "peak_sampled_rss_kib": max(row["rss_kib"] for row in rows),
        "last_900_seconds_samples": len(tail),
        "last_900_seconds_rss_kib": distribution([row["rss_kib"] for row in tail]),
        "final_total_cursor": rows[-1]["total_cursor"],
        "final_snapshot_bytes": rows[-1]["snapshot_bytes"],
        "archive_actors": len(archives),
        "min_archive_payload_bytes": min(sizes) if sizes else None,
        "max_archive_payload_bytes": max(sizes) if sizes else None,
        "all_actors_passed_hot_cap": len(sizes) == int(metadata["actors"])
        and min(sizes, default=0) >= hot_cap,
    }
    if watch_rows is not None:
        duration = int(metadata["duration"])
        observations = [row for row in watch_rows if row["pid"] == metadata["pid"]
                        and duration - 900 <= float(row["process_seconds"]) <= duration]
        result["watch_last_900_process_seconds"] = {
            "requested_start_seconds": duration - 900,
            "requested_end_seconds": duration,
            "samples": len(observations),
            "first_utc": observations[0]["utc"] if observations else None,
            "last_utc": observations[-1]["utc"] if observations else None,
            "all_actors_alive": bool(observations) and all(
                int(row["python_children"]) == int(metadata["actors"])
                for row in observations),
            "metrics": {key: distribution([int(row[key]) for row in observations])
                        for key in ("rss_kib", "swap_kib", "rss_plus_swap_kib", "mem_available_kib")}
            if observations else {},
        }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", type=Path, nargs="+")
    parser.add_argument("--watch", type=Path)
    parser.add_argument("--hot-cap", type=int, default=2_000_000)
    args = parser.parse_args()
    watch_rows = list(csv.DictReader(args.watch.read_text().splitlines())) if args.watch else None
    print(json.dumps([summarize(path, watch_rows, args.hot_cap) for path in args.csv], indent=2))


if __name__ == "__main__":
    main()
