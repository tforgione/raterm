#!/usr/bin/env python3
"""Writes the Performance pages of the wiki from a campaign: Performance.md,
the text and the charts, and Performance-tables.md, the tables. The tables
come from DIR/summary.md (see charts/build.sh), the numbers of the text from
DIR/results.jsonl, and the before/after table from the raterm measures of
the first campaign (baseline-2026-10-20.jsonl). The setup section describes
the machine of these measures: edit it when that changes.

    page.py DIR WIKI [COMMIT]     # COMMIT: raterm's, HEAD by default
"""

import json
import re
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

new_dir, wiki = Path(sys.argv[1]), Path(sys.argv[2])
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
names = {
    "raterm": "raterm",
    "gnome-terminal": "GNOME Terminal",
    "terminator": "Terminator",
    "alacritty": "Alacritty",
    "kitty": "kitty",
    "foot": "foot",
}
others = [t for t in names if t != "raterm" and t in grids]


def against(values, f="{:.0f}", unit=""):
    """`values` of the other terminals, as "a for X, b for Y and c for Z"."""
    parts = [f"{f.format(values[t])}{unit} for {names[t]}" for t in others]
    return ", ".join(parts[:-1]) + " and " + parts[-1]

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
# KiB per line of the terminals that keep their scrollback in memory, from
# their largest measure (alacritty stops at 100,000).
in_memory = {
    t: (sb(t, n) - sb(t, 10_000)) * 2**10 / (n - 10_000)
    for t, n in [("alacritty", 100_000), ("kitty", 1_000_000), ("foot", 1_000_000)]
}
others_display = [display(t, wl) for t in others for wl in wls]
runs = max(r["run"] for r in rows if r["bench"] == "latency") + 1
sb_runs = max(r["run"] for r in rows if r["bench"] == "scrollback") + 1
blocks = {n: m("blocks", "pss_mib", terminal="raterm", blocks=n) for n in (20, 200)}
tables = lambda anchor: f"Tables: [Performance tables](Performance-tables#{anchor})."

page = f"""# Performance

raterm draws its grid with Pango on GTK, and its rich blocks in web views
(see [Spec-View](Spec-View)): the specification accepts that it
will never match a GPU terminal. This page measures by how much, against
two GPU-accelerated terminals, Alacritty and kitty, and three that are not,
GNOME Terminal and Terminator (both on VTE, GNOME's terminal widget) and
foot, with the script `extra/bench/bench.py`.

## In short

- **Throughput**: raterm takes {span(gt_ratio, "{:.1f}")} times GNOME Terminal's time to
  display a large output, at {span(speeds("raterm"))} MiB/s against {span(speeds("gnome-terminal"))} MiB/s. The others
  display it in {span(others_display, "{:.2f}")} s; each time is precise to a frame (17 ms),
  so the ratios of the shortest ones are rough. Since the first measures, the fixes of the page,
  M5, those of the core and the native grid (M6) made raterm {span(gains)} times faster (see
  [below](#since-the-first-measures)).
- **Latency** from a program's output to the screen, at 60 Hz: {lmed["raterm"]:.0f} ms median
  for raterm, {against(lmed, unit=" ms")}; at most {lat["raterm"][-1]:.0f} ms for raterm,
  {max(lat[t][-1] for t in others):.0f} ms for the others.
- **Startup**: {first["raterm"]:.0f} ms to the first frame, against {against(first, unit=" ms")}.
- **Memory**: {idle["raterm"]:.0f} MiB idle (PSS, in one process: WebKit only starts with
  the first block), against {against(idle, unit=" MiB")}. With 20 small Markdown blocks in view,
  {blocks[20]:.0f} MiB: their web views share one WebKit process.
- **Scrollback**: since M5, a full scrollback no longer slows raterm down, up
  to 1,000,000 lines; it costs about {per_line:.0f} bytes per line of memory, where VTE
  keeps its scrollback in files and stays flat, and Alacritty, kitty and foot
  take {span(in_memory.values(), "{:.1f}")} KiB per line.

## Setup

- AMD Ryzen 9 5900X, 62 GiB of RAM, NVIDIA RTX 5060 Ti, Arch Linux, Hyprland
  0.56.2, one 2560x1440 screen at 60 Hz.
- raterm at commit `{commit}` (release build, GTK 3 and Pango, WebKitGTK
  2.54.1 for the blocks), GNOME Terminal 3.60.0 and Terminator 2.1.6 on VTE
  0.84.1, Alacritty 0.17.0, kitty 0.49.2, foot 1.28.0.
- All six with the same font (UbuntuMono Nerd Font, 11 pt), a scrollback of
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
(17 ms at 60 Hz), except latency: a small Wayland client of the bench
(`extra/bench/probe`) captures the pixel frame after frame, and the
compositor tells when each frame was presented, so latency is exact to the
frame, without the time grim takes to start.

- **Throughput**: the program writes a 4 MiB file to the terminal, then fills
  the screen with a marker color. *Drained* is when its write returns (the
  terminal has read nearly everything); *displayed* is when the marker shows
  up. The workloads are plain text, numbered short lines (`seq`), text with
  256 colors, truecolor text with backgrounds, CJK with emoji, and random
  cursor moves without scrolling (like a full-screen program).
- **Latency**: the program paints a small block in another color when it is
  told to; the time is from the order to the presentation of the frame
  showing the block, with a random pause between samples to spread them over
  the frame. Typing is not measured. Measures before Oct 22, 2026 polled with
  grim, which added its own 10 to 30 ms.
- **Startup**: from the launch request to the first frame painted by the
  program, which fills the screen as soon as it starts. Hyprland's own time
  to open a window is included, the same for all.
- **Memory**: PSS (shared pages split between the processes that map them) of
  the terminal and its children, without the program running in it. PSS
  depends on what else runs: GTK is shared with the other GTK applications of
  the desktop. The idle RSS
  (shared pages counted in full) is given too. **CPU** is the time used by the
  same processes during a throughput run.
- **Scrollback**: the program fills a scrollback of 10,000, 100,000 or
  1,000,000 lines of about 60 characters (Alacritty stops at 100,000); 15
  seconds later the memory is measured, then 4 MiB more output and the
  latency.
- **Blocks** (raterm only): the program writes 20 or 200 small Markdown
  blocks, two rows each, about 20 of which fit in the window; 3 seconds
  later the memory is measured.

Each terminal runs isolated, without touching the user's own: GNOME Terminal
on a server of its own (`--app-id`) whose settings live in a GSettings
keyfile instead of dconf, Terminator without D-Bus (`-u`) on its own config
file, raterm, Alacritty, kitty and foot on their own config files (kitty
with a fixed window size: otherwise it takes back the last size it had,
after Hyprland placed it).

## Results

The tables of these results are on [Performance tables](Performance-tables).

### Throughput

{img("throughput")}

raterm drains 4 MiB in {span(drains, "{:.2f}")} s, {span(drain_ratio)} times slower than the fastest of the
others (the terminal state), and the screen catches up {span(catch, "{:.2f}")} s later. The
CPU time is about {cpu_ratio:.1f} times the time to display.

{tables("throughput")}

### Latency

{img("latency")}

Whatever the terminal, the output waits for the compositor's next frame, so
the samples spread over about one frame (17 ms at 60 Hz). raterm hands its
frame to the compositor about 1.4 ms after the output reaches it, on a
screen full of code (`extra/bench/trace.py`); the rest is the wait.

{tables("latency")}

### Startup and memory

{img("startup")}

{img("memory")}

{tables("startup-and-memory")}

### Scrollback

{img("scrollback")}

raterm keeps its scrollback in memory in the core and only draws the rows on
screen (see [Spec-Scrollback](Spec-Scrollback)): the latency and
the output speed do not depend on its size, and a line costs about {per_line:.0f} bytes.
Alacritty, kitty and foot keep their whole scrollback in memory too, at about
{in_memory["alacritty"]:.1f}, {in_memory["kitty"]:.1f} and {in_memory["foot"]:.1f} KiB per line: kitty takes
{sb("kitty", 1_000_000) / 1024:.1f} GiB for 1,000,000 lines, foot {sb("foot", 1_000_000) / 1024:.1f} GiB. VTE writes it to
temporary files, so the memory of GNOME Terminal and Terminator does not grow.

{tables("scrollback")}

### Blocks

With 20 small Markdown blocks in view, raterm takes {blocks[20]:.0f} MiB; with 200, most of
them in the scrollback, {blocks[200]:.0f} MiB. The web views of the blocks share one
WebKit process, and only the blocks in view have one: a block that leaves
the view only keeps its HTML and its height. Before they shared it, each
block in view started a process of its own, about 200 MiB each.

{tables("blocks")}

## Since the first measures

The first campaign, on Oct 20, 2026, found raterm 25 to 380 times slower than
Alacritty. Profiling showed that the page did the work, not the core: counting
the scrollback after each line walked the whole of it, every screen update of
a batch was rebuilt even though only the last one is painted, and every line
of a large output became a DOM element. Two fixes and M5 (the scrollback kept
by the core, a window of it in the page), then work on the core (styles built
only when attributes change, all the pending output in one batch, the page
taking all the pending events at once), the native grid, which replaced the
page with Pango drawing, and last a core that renders the screen only when
it is drawn made raterm {span(gains)} times faster to display 4 MiB
(see [the table](Performance-tables#since-the-first-measures)).

## Running it

The windows open floating at the top left of the current workspace and do not
take the focus; the screen must stay on that workspace until the end, and
must not lock, as a hidden window cannot be read back (its run times out and
is skipped). The script needs `grim`, Hyprland and cargo (it builds
`extra/bench/probe`), and only ever closes the windows it opened.
`extra/bench/trace.py` splits raterm's latency into the stages of a redraw
(see [Development](Development)).

```sh
cargo build --release
BENCH_MIB=4 extra/bench/bench.py run --only startup,latency,throughput,blocks --runs 5 --out bench-results
BENCH_MIB=4 extra/bench/bench.py run --only scrollback --runs 2 --out bench-results
extra/bench/charts/build.sh bench-results .wiki/bench   # tables and charts
extra/bench/page.py bench-results .wiki                 # these pages
```

`--terminals raterm,gnome-terminal,terminator,alacritty,kitty,foot` selects the
terminals; results append to `bench-results/results.jsonl`, so a run on
another screen (at 120 Hz for instance) adds to the same report.
"""
(wiki / "Performance.md").write_text(page)

annex = f"""# Performance tables

The measures behind the charts of [Performance](Performance), where the
setup and the method are: medians over {runs} runs ({sb_runs} for the scrollback).
"""
for name in ["Throughput", "Latency", "Startup and memory", "Scrollback", "Blocks"]:
    annex += f"\n## {name}\n\n{sections[name].strip()}\n"
annex += f"\n## Since the first measures\n\nraterm's time to display 4 MiB, on Oct 20, 2026 and now:\n\n{chr(10).join(before)}\n"
(wiki / "Performance-tables.md").write_text(annex)
