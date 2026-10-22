#!/usr/bin/env python3
"""Where the latency of raterm goes: runs the latency samples of bench.py on
a raterm built with the `trace` feature, and prints, for each stage of a
redraw it traced, its median time since the order, up to the frame that
showed the output (see probe/).

    trace.py [--code] [--samples N] [--each]

--code fills the screen with colored code first; --each prints every sample.
raterm is built in target/trace, apart from the bench's target/release.
"""

import argparse
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

import bench

STAGES = ["read", "advance", "advanced", "request", "event", "laid", "before-paint", "draw", "drawn", "after-paint"]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--code", action="store_true")
    p.add_argument("--samples", type=int, default=60)
    p.add_argument("--each", action="store_true")
    args = p.parse_args()
    for manifest, extra in [(bench.ROOT / "Cargo.toml", ["--features", "trace", "--target-dir", bench.ROOT / "target/trace"]), (bench.PROBE / "Cargo.toml", [])]:
        subprocess.run(["cargo", "build", "--release", "--quiet", "--manifest-path", manifest, *extra], check=True)
    scratch = Path(tempfile.mkdtemp(prefix="raterm-trace-"))
    bench.setup(scratch)
    log = scratch / "trace.log"
    bench.RATERM = f"RATERM_TRACE={log} {bench.ROOT / 'target/trace/release/raterm'}"
    w = bench.Window("raterm", scratch).ready()
    probe, samples = None, []
    try:
        bench.wait_pixel(*w.center, bench.BLACK, 20)
        if args.code:
            w.send("code")
        time.sleep(1)
        probe = bench.Probe(*w.center)
        for i in range(args.samples):
            time.sleep(bench.random.uniform(0.05, 0.1))
            # The trace has the wall clock, the probe the monotonic one.
            real, t0 = time.time_ns(), time.monotonic_ns()
            w.send(f"cell {i % 2}")
            samples.append((real, (probe.wait(bench.LATENCY_COLORS[i % 2]) - t0) / 1e6))
    finally:
        if probe:
            probe.close()
        w.close()

    events = [line.split(maxsplit=1) for line in log.read_text().splitlines()]
    events = [(int(t), what.split()[0]) for t, what in events]
    since = {s: [] for s in STAGES + ["presented"]}
    for real, shown in samples:
        end = real + shown * 1e6
        first = {}
        for t, what in events:
            if real <= t <= end and what in since:
                first.setdefault(what, (t - real) / 1e6)
        for s, ms in first.items():
            since[s].append(ms)
        since["presented"].append(shown)
        if args.each:
            print(f"{shown:5.1f} ms: " + ", ".join(f"{s} {ms:.2f}" for s, ms in first.items()))
    print(f"{'stage':14} {'median (ms)':>11} {'p90 (ms)':>9} samples")
    for s, ms in since.items():
        if ms:
            ms.sort()
            print(f"{s:14} {statistics.median(ms):11.2f} {ms[int(len(ms) * 0.9)]:9.2f} {len(ms):7}")


if __name__ == "__main__":
    main()
