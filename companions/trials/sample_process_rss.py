#!/usr/bin/env python3
"""Sample resident memory for a process and its descendants using `ps`."""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
import time


def process_table() -> dict[int, tuple[int, int, str]]:
    output = subprocess.check_output(
        ["ps", "-axo", "pid=,ppid=,rss=,command="], text=True
    )
    processes: dict[int, tuple[int, int, str]] = {}
    for line in output.splitlines():
        fields = line.strip().split(maxsplit=3)
        if len(fields) != 4:
            continue
        try:
            pid, parent, rss_kib = map(int, fields[:3])
        except ValueError:
            continue
        processes[pid] = (parent, rss_kib, fields[3])
    return processes


def process_tree_rss(root_pids: list[int]) -> tuple[int, int]:
    processes = process_table()
    missing = sorted(set(root_pids) - processes.keys())
    if missing:
        raise RuntimeError(f"root process(es) exited: {missing}")
    children: dict[int, list[int]] = {}
    for pid, (parent, _rss, _command) in processes.items():
        children.setdefault(parent, []).append(pid)
    pending = list(set(root_pids))
    included: set[int] = set()
    while pending:
        pid = pending.pop()
        if pid in included:
            continue
        included.add(pid)
        pending.extend(children.get(pid, ()))
    return sum(processes[pid][1] for pid in included) * 1024, len(included)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root-pid", type=int, action="append", required=True,
        help="process-tree root; repeat for independent roots such as host and browser",
    )
    parser.add_argument("--settle-seconds", type=float, default=10.0)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--interval-seconds", type=float, default=1.0)
    args = parser.parse_args()
    if args.settle_seconds < 0 or args.samples < 1 or args.interval_seconds < 0:
        parser.error("settle and interval must be non-negative; samples must be positive")
    if not (sys.platform.startswith("linux") or sys.platform == "darwin"):
        parser.error("process RSS sampling is supported on Linux and macOS")

    time.sleep(args.settle_seconds)
    rss_samples: list[int] = []
    process_counts: list[int] = []
    for index in range(args.samples):
        rss_bytes, process_count = process_tree_rss(args.root_pid)
        rss_samples.append(rss_bytes)
        process_counts.append(process_count)
        if index + 1 < args.samples:
            time.sleep(args.interval_seconds)
    print(json.dumps({
        "root_pids": args.root_pid,
        "settle_seconds": args.settle_seconds,
        "samples": args.samples,
        "interval_seconds": args.interval_seconds,
        "rss_bytes_samples": rss_samples,
        "process_counts": process_counts,
        "median_rss_bytes": int(statistics.median(rss_samples)),
    }, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
