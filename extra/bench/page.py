#!/usr/bin/env python3
"""Writes the Performance page of the wiki from a campaign: its tables come
from DIR/summary.md (see charts/build.sh), the numbers of its text from
DIR/results.jsonl, and the before/after table from the raterm measures of
the first campaign (baseline-2026-10-20.jsonl). The setup section describes
the machine of these measures: edit it when that changes.

    page.py DIR OUT.md [COMMIT]     # COMMIT: raterm's, HEAD by default
"""

import json
import re
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

new_dir, out = Path(sys.argv[1]), Path(sys.argv[2])
commit = sys.argv[3] if len(sys.argv) > 3 else subprocess.run(
    ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, check=True
).stdout.strip()
baseline = Path(__file__).parent / "baseline-2026-10-20.jsonl"
summary = (new_dir / "summary.md").read_text()
sections = dict(re.findall(r"### (\w[\w ]*)\n\n(.*?)(?=\n### |\Z)", summary, re.S))
img = lambda name: f"![{name}](https://git.tforgione.fr/tforgione/raterm/wiki/raw/bench/{name}.svg)"


def medians(path, bench, key):
    d = defaultdict(list)
    for line in open(path):
        r = json.loads(line)
        if r["bench"] == bench and r["terminal"] == "raterm":
            d[r.get("workload")].append(r[key])
    return {k: statistics.median(v) for k, v in d.items()}


old, new = medians(baseline, "throughput", "display_s"), medians(new_dir / "results.jsonl", "throughput", "display_s")
before = ["| workload | Oct 20 (s) | now (s) | faster |", "|---|---|---|---|"]
before += [f"| {wl} | {old[wl]:.2f} | {new[wl]:.2f} | ×{old[wl] / new[wl]:.0f} |" for wl in old]

rows = [json.loads(line) for line in open(new_dir / "results.jsonl")]
grids = {r["terminal"]: f"{r['cols']}x{r['rows']}" for r in rows if r["bench"] == "throughput"}
grid = ", ".join(f"{t} {g}" for t, g in grids.items())


def m(bench, key, **match):
    return statistics.median(
        r[key] for r in rows if r["bench"] == bench and all(r.get(k) == v for k, v in match.items())
    )


def span(xs, f="{:.0f}"):
    return f"{f.format(min(xs))} to {f.format(max(xs))}"


wls = list(dict.fromkeys(r["workload"] for r in rows if r["bench"] == "throughput"))
mib = {wl: next(r["mib"] for r in rows if r.get("workload") == wl) for wl in wls}
display = lambda t, wl: m("throughput", "display_s", terminal=t, workload=wl)
drain = lambda t, wl: m("throughput", "drain_s", terminal=t, workload=wl)
speeds = lambda t: [mib[wl] / display(t, wl) for wl in wls]
others = ["gnome-terminal", "terminator", "alacritty"]
gt_ratio = [display("raterm", wl) / display("gnome-terminal", wl) for wl in wls]
drains = [drain("raterm", wl) for wl in wls]
drain_ratio = [drain("raterm", wl) / min(drain(t, wl) for t in others) for wl in wls]
catch = [display("raterm", wl) - drain("raterm", wl) for wl in wls]
cpu_ratio = statistics.median(m("throughput", "cpu_s", terminal="raterm", workload=wl) / display("raterm", wl) for wl in wls)
gains = [old[wl] / new[wl] for wl in old]
lat = {t: sorted(r["latency_ms"] for r in rows if r["bench"] == "latency" and r["terminal"] == t) for t in grids}
lmed = {t: statistics.median(v) for t, v in lat.items()}
first = {t: m("startup", "first_frame_ms", terminal=t) for t in grids}
idle = {t: m("startup", "idle_pss_mib", terminal=t) for t in grids}
sb = lambda t, n: m("scrollback", "pss_mib", terminal=t, lines=n)
per_line = (sb("raterm", 1_000_000) - sb("raterm", 10_000)) * 2**20 / 990_000
ala_line = (sb("alacritty", 100_000) - sb("alacritty", 10_000)) * 2**10 / 90_000
runs = max(r["run"] for r in rows if r["bench"] == "latency") + 1
sb_runs = max(r["run"] for r in rows if r["bench"] == "scrollback") + 1

page = f"""# Performance

raterm draws its screen with a web view: the specification accepts that it
will never match a GPU terminal. This page measures by how much, against
Alacritty, and against two terminals that are not GPU-accelerated, GNOME
Terminal and Terminator (both on VTE, GNOME's terminal widget), with the
script `extra/bench/bench.py`.

## In short

- **Throughput**: raterm displays a large output {span(gt_ratio, "{:.1f}")} times slower than
  GNOME Terminal, at {span(speeds("raterm"))} MiB/s against {span(speeds("gnome-terminal"))} MiB/s. The three other
  terminals are close to the precision of the measure (a few frames), so the
  ratios are lower bounds. Since the first measures, the fixes of the page,
  M5 and those of the core made raterm {span(gains)} times faster (see
  [below](#since-the-first-measures)).
- **Latency** from a program's output to the screen, at 60 Hz: {lmed["raterm"]:.0f} ms median
  for raterm, {lmed["gnome-terminal"]:.0f} ms for GNOME Terminal, {lmed["terminator"]:.0f} ms for Terminator and {lmed["alacritty"]:.0f} ms for
  Alacritty; at most {lat["raterm"][-1]:.0f} ms for raterm, {max(lat[t][-1] for t in others):.0f} ms for the others.
- **Startup**: {first["raterm"]:.0f} ms to the first frame, against {first["gnome-terminal"]:.0f} ms for GNOME
  Terminal, {first["terminator"]:.0f} ms for Terminator and {first["alacritty"]:.0f} ms for Alacritty.
- **Memory** is where raterm is far behind: {idle["raterm"]:.0f} MiB idle (PSS, in three
  processes, as WebKit runs the page in its own process), against {idle["gnome-terminal"]:.0f} MiB for
  GNOME Terminal and {idle["terminator"]:.0f} MiB for Terminator.
- **Scrollback**: since M5, a full scrollback no longer slows raterm down, up
  to 1,000,000 lines; it costs about {per_line:.0f} bytes per line of memory, where VTE
  keeps its scrollback in files and stays flat.

## Setup

- AMD Ryzen 9 5900X, 62 GiB of RAM, NVIDIA RTX 5060 Ti, Arch Linux, Hyprland
  0.56.2, one 2560x1440 screen at 60 Hz.
- raterm at commit `{commit}` (release build, wry 0.57, WebKitGTK 2.54.1),
  GNOME Terminal 3.60.0 and Terminator 2.1.6 on VTE 0.84.1, Alacritty 0.17.0.
- All four with the same font (UbuntuMono Nerd Font, 11 pt), a scrollback of
  10,000 lines (except for the scrollback measures), a black background and a
  1200x800 floating window, without menu bar (GNOME Terminal) or title bar
  (Terminator). The grids differ with each terminal's cells and margins:
  {grid}.
- {runs} runs of everything per terminal ({sb_runs} for the scrollback), on a desktop in
  use: medians are given.

## Method

Everything is measured from outside, the same way for all terminals. A
floating window opens with the script itself running inside it, which takes
orders through a FIFO and paints colored markers; `grim` reads back a single
pixel in the middle of the window until it has the expected color. A capture
takes one compositor frame, so **every time is precise to about one frame**
(17 ms at 60 Hz).

- **Throughput**: the program writes a 4 MiB file to the terminal, then fills
  the screen with a marker color. *Drained* is when its write returns (the
  terminal has read nearly everything); *displayed* is when the marker shows
  up. The workloads are plain text, numbered short lines (`seq`), text with
  256 colors, truecolor text with backgrounds, CJK with emoji, and random
  cursor moves without scrolling (like a full-screen program).
- **Latency**: the program paints a small block in another color when it is
  told to; the time is from the order to the frame showing the block, with a
  random pause between samples to spread them over the frame. Typing goes
  through more steps in raterm (key event in the page, IPC, pty) that this
  does not measure.
- **Startup**: from the launch request to the first frame painted by the
  program, which fills the screen as soon as it starts. Hyprland's own time
  to open a window is included, the same for all.
- **Memory**: PSS (shared pages split between the processes that map them) of
  the terminal and its children, without the program running in it. PSS
  depends on what else runs: GTK is shared with the other GTK applications of
  the desktop, WebKit with the raterm windows already open. The idle RSS
  (shared pages counted in full) is given too. **CPU** is the time used by the
  same processes during a throughput run.
- **Scrollback**: the program fills a scrollback of 10,000, 100,000 or
  1,000,000 lines of about 60 characters (Alacritty stops at 100,000); 15
  seconds later the memory is measured, then 4 MiB more output and the
  latency.

Each terminal runs isolated, without touching the user's own: GNOME Terminal
on a server of its own (`--app-id`) whose settings live in a GSettings
keyfile instead of dconf, Terminator without D-Bus (`-u`) on its own config
file, raterm and Alacritty on their own config files.

## Results

### Throughput

{img("throughput")}

{sections["Throughput"].strip()}

raterm drains 4 MiB in {span(drains, "{:.2f}")} s, {span(drain_ratio)} times slower than the fastest of the
others (terminal state, screen diff and JSON events for the page), and the
page catches up {span(catch, "{:.2f}")} s later. The CPU time is about {cpu_ratio:.1f} times the time to
display, as the core and the web process work at the same time.

### Latency

{img("latency")}

{sections["Latency"].strip()}

### Startup and memory

{img("startup")}

{img("memory")}

{sections["Startup and memory"].strip()}

The memory after an output is measured one second after it is displayed, so
part of it is garbage the web process has not collected yet.

### Scrollback

{img("scrollback")}

{sections["Scrollback"].strip()}

raterm keeps its scrollback in memory in the core and only a window of 5,000
lines in the page (see [Spec-Scrollback](Spec-Scrollback)): the latency and
the output speed do not depend on its size, and a line costs about {per_line:.0f} bytes.
Alacritty keeps its whole scrollback in memory too, at about {ala_line:.1f} KiB per line;
VTE writes it to temporary files, so the memory of GNOME Terminal and
Terminator does not grow.

## Since the first measures

The first campaign, on Oct 20, 2026, found raterm 25 to 380 times slower than
Alacritty. Profiling showed that the page did the work, not the core: counting
the scrollback after each line walked the whole of it, every screen update of
a batch was rebuilt even though only the last one is painted, and every line
of a large output became a DOM element. Two fixes and M5 (the scrollback kept
by the core, a window of it in the page), then work on the core (styles built
only when attributes change, all the pending output in one batch, the page
taking all the pending events at once) changed the time to display 4 MiB:

{chr(10).join(before)}

## Running it

The windows open floating at the top left of the current workspace and do not
take the focus; the screen must stay on that workspace until the end, and
must not lock, as a hidden window cannot be read back (its run times out and
is skipped). The script needs `grim` and Hyprland, and only ever closes the
windows it opened.

```sh
cargo build --release
BENCH_MIB=4 extra/bench/bench.py run --only startup,latency,throughput --runs 5 --out bench-results
BENCH_MIB=4 extra/bench/bench.py run --only scrollback --runs 2 --out bench-results
extra/bench/charts/build.sh bench-results ../raterm.wiki/bench   # tables and charts
extra/bench/page.py bench-results ../raterm.wiki/Performance.md  # this page
```

`--terminals raterm,gnome-terminal,terminator,alacritty` selects the
terminals; results append to `bench-results/results.jsonl`, so a run on
another screen (at 120 Hz for instance) adds to the same report.
"""
out.write_text(page)
