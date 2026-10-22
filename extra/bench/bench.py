#!/usr/bin/env python3
"""Black-box benchmark of raterm against other terminals on Hyprland.

Each run opens a floating test window through `hyprctl dispatch`, with this
script running inside it (`inner` mode) and taking orders from a FIFO. What
the screen shows is read back with `grim`, one pixel at a time: a run is over
when the pixel at the center of the window turns to the expected color. One
capture takes one compositor frame, so every time is precise to one frame.

    BENCH_MIB=4 bench.py run [--only startup,latency,throughput] [--runs N] [--out DIR]
    bench.py report DIR     # writes DIR/*.csv and prints Markdown tables
    charts/build.sh DIR OUT # both, plus the charts in OUT
    page.py DIR WIKI        # the Performance pages of the wiki

Never kills anything but the windows it opened, found by their unique title.
"""

import argparse
import base64
import json
import os
import random
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RATERM = ROOT / "target/release/raterm"
FONT, SIZE = "UbuntuMono Nerd Font", 11
WIDTH, HEIGHT, X, Y = 1200, 800, 60, 60
WORKLOAD_MIB = int(os.environ.get("BENCH_MIB", 8))
MARK, BLACK = (255, 0, 255), (0, 0, 0)
# Scrollback sizes of the scrollback benchmark; alacritty stops at 100,000.
TERMINALS = ["raterm", "gnome-terminal", "terminator", "alacritty"]
SCROLLBACK_LINES = [10_000, 100_000, 1_000_000]
# Rich blocks of the blocks benchmark: about 20 fit in the window.
BLOCKS = [20, 200]
LATENCY_COLORS = [(255, 128, 0), (0, 160, 255)]


# --- inside the terminal ---------------------------------------------------


def paint(color, rows=None, cols=None, top=1, left=1):
    """Fills a block (the whole screen by default) with a background color."""
    size = os.get_terminal_size()
    rows, cols = rows or size.lines, cols or size.columns
    out = f"\x1b[?25l\x1b[48;2;{color[0]};{color[1]};{color[2]}m"
    for r in range(rows):
        out += f"\x1b[{top + r};{left}H" + " " * cols
    write(out + "\x1b[0m")


def write(data):
    data = data.encode() if isinstance(data, str) else data
    while data:
        data = data[os.write(1, data) :]


def inner(ctl):
    ctl = Path(ctl)
    # raterm starts the pty at 80x24 and resizes it once the view has measured
    # its cells: repaint the background at each resize.
    background = MARK if (ctl / "startup").exists() else BLACK
    signal.signal(signal.SIGWINCH, lambda *_: paint(background))
    paint(background)
    (ctl / "ready").write_text(json.dumps({"pid": os.getpid()}))
    with open(ctl / "fifo") as fifo:
        for line in fifo:
            cmd, *args = line.split()
            size = os.get_terminal_size()
            if cmd == "black":
                paint(BLACK)
            elif cmd == "size":
                (ctl / "size").write_text(json.dumps([size.columns, size.lines]))
            elif cmd == "cat":
                t0 = time.time_ns()
                with open(args[0], "rb") as f:
                    while chunk := f.read(1 << 16):
                        write(chunk)
                t1 = time.time_ns()
                paint(MARK)
                (ctl / "drained").write_text(json.dumps([t0, t1]))
            elif cmd == "cell":
                # A 12x5 block around the center of the grid, which covers the
                # center of the window whatever the padding.
                rows, cols = 5, 12
                top, left = size.lines // 2 - 1, size.columns // 2 - 5
                paint(LATENCY_COLORS[int(args[0])], rows, cols, top, left)
            elif cmd == "blocks":
                # Small Markdown blocks, two rows each, as raterm's OSC 7117.
                for i in range(int(args[0])):
                    md = f"**Block {i}**: some text with `code` and a [link](https://example.com)."
                    write(f"\x1b]7117;md;{base64.b64encode(md.encode()).decode()}\x07")
                (ctl / "blocks").write_text("done")
            elif cmd == "quit":
                return


# --- workloads --------------------------------------------------------------


def workloads(dir):
    """Generates the outputs to cat, each about WORKLOAD_MIB MiB."""
    rnd = random.Random(7117)
    size = WORKLOAD_MIB << 20
    words = ["".join(rnd.choices("abcdefghijklmnopqrstuvwxyz", k=rnd.randint(1, 10))) for _ in range(2000)]

    def lines(make):
        out, n = [], 0
        while n < size:
            line = make()
            out.append(line)
            n += len(line.encode())
        return "\n".join(out) + "\n"

    def text():
        return " ".join(rnd.choices(words, k=rnd.randint(2, 20)))

    def color():
        return "".join(f"\x1b[38;5;{rnd.randint(0, 255)}m{w} " for w in rnd.choices(words, k=rnd.randint(2, 12))) + "\x1b[0m"

    def truecolor():
        return "".join(
            f"\x1b[38;2;{rnd.randint(0, 255)};{rnd.randint(0, 255)};{rnd.randint(0, 255)}m"
            f"\x1b[48;2;{rnd.randint(0, 60)};{rnd.randint(0, 60)};{rnd.randint(0, 60)}m{w} "
            for w in rnd.choices(words, k=rnd.randint(2, 12))
        ) + "\x1b[0m"

    cjk = [chr(c) for c in range(0x4E00, 0x4E00 + 500)] + ["é", "ß", "ø", "λ", "→", "✓", "😀", "🦀"]

    def unicode():
        return "".join(rnd.choices(cjk, k=rnd.randint(5, 40)))

    def tui():
        # Random cursor moves inside 24x80 with short colored writes, like a
        # full-screen program redrawing: no scrolling at all.
        return f"\x1b[{rnd.randint(1, 24)};{rnd.randint(1, 70)}H\x1b[3{rnd.randint(0, 7)}m{rnd.choice(words)}"

    gens = {
        "ascii": lambda: lines(text),
        "short lines": lambda: "".join(f"{i}\n" for i in range(1, size // 7)),
        "256 colors": lambda: lines(color),
        "truecolor": lambda: lines(truecolor),
        "unicode": lambda: lines(unicode),
        "cursor motion": lambda: "\x1b[2J" + "".join(tui() for _ in range(size // 16)),
    }
    files = {}
    for name, gen in gens.items():
        path = dir / (name.replace(" ", "-") + ".txt")
        dir.mkdir(parents=True, exist_ok=True)
        if not path.exists():
            path.write_text(gen())
        files[name] = path
    return files


# --- outside: windows, pixels, processes ------------------------------------


def hypr_clients():
    return json.loads(subprocess.run(["hyprctl", "clients", "-j"], capture_output=True, check=True).stdout)


def refresh_rate():
    monitors = json.loads(subprocess.run(["hyprctl", "monitors", "-j"], capture_output=True, check=True).stdout)
    return next(m for m in monitors if m["focused"])["refreshRate"]


def pixel(x, y):
    ppm = subprocess.run(["grim", "-g", f"{x},{y} 1x1", "-t", "ppm", "-"], capture_output=True, check=True).stdout
    return tuple(ppm[-3:])


def close(color, want):
    return all(abs(a - b) <= 8 for a, b in zip(color, want))


def wait_pixel(x, y, want, timeout, interval=0):
    """Polls until the pixel has the color `want`; returns when it was seen."""
    end = time.time_ns() + int(timeout * 1e9)
    while time.time_ns() < end:
        color = pixel(x, y)
        seen = time.time_ns()
        if close(color, want):
            return seen
        time.sleep(interval)
    raise TimeoutError(f"pixel {x},{y} never turned {want}")


def tree(pid, exclude):
    """The pid and its descendants, without the subtree of `exclude`."""
    children = {}
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                ppid = int((p / "stat").read_text().rsplit(")", 1)[1].split()[1])
            except (OSError, IndexError):
                continue
            children.setdefault(ppid, []).append(int(p.name))
    out, todo = [], [pid]
    while todo:
        p = todo.pop()
        if p != exclude:
            out.append(p)
            todo += children.get(p, [])
    return out


def cpu_seconds(pids):
    total = 0
    for p in pids:
        try:
            fields = Path(f"/proc/{p}/stat").read_text().rsplit(")", 1)[1].split()
            total += int(fields[11]) + int(fields[12])  # utime, stime
        except OSError:
            pass
    return total / os.sysconf("SC_CLK_TCK")


def rss_mib(pids):
    """Resident memory, shared pages counted in full in each process."""
    total = 0
    for p in pids:
        try:
            for line in Path(f"/proc/{p}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total / 1024


def pss_mib(pids):
    total = 0
    for p in pids:
        try:
            for line in Path(f"/proc/{p}/smaps_rollup").read_text().splitlines():
                if line.startswith("Pss:"):
                    total += int(line.split()[1])
        except OSError:
            pass
    return total / 1024


class Window:
    """A test terminal running this script in `inner` mode."""

    def __init__(self, term, scratch, startup=False):
        self.title = f"raterm-test-{uuid.uuid4()}"
        self.ctl = Path(tempfile.mkdtemp(dir=scratch))
        os.mkfifo(self.ctl / "fifo")
        if startup:
            (self.ctl / "startup").touch()
        inner = f"{sys.executable} -I {Path(__file__).resolve()} inner {self.ctl}"
        self.term = term
        if term == "raterm":
            cmd = f"env XDG_CONFIG_HOME={scratch}/config {RATERM} --title {self.title} -e {inner}"
        elif term == "alacritty":
            cmd = f"alacritty --config-file {scratch}/config/alacritty.toml --title {self.title} -e {inner}"
        elif term == "terminator":
            # Without D-Bus, the window is a process of its own.
            cmd = f"terminator -u -g {scratch}/config/terminator.conf -T {self.title} -x {inner}"
        else:
            # A server of our own, which no other window shares, with its
            # settings in a keyfile of the scratch directory, not in dconf.
            app = f"fr.tforgione.RatermBench{uuid.uuid4().hex}"
            (self.ctl / "launch.sh").write_text(
                f"export GSETTINGS_BACKEND=keyfile XDG_CONFIG_HOME={scratch}/config\n"
                f"/usr/lib/gnome-terminal-server --app-id {app} &\n"
                f"gdbus wait --session --timeout 10 {app}\n"
                f"exec gnome-terminal --app-id {app} --title {self.title} -- {inner}\n"
            )
            cmd = f"sh {self.ctl}/launch.sh"
        rules = f"{{ float = true, size = '{WIDTH} {HEIGHT}', move = '{X} {Y}', no_anim = true, no_initial_focus = true }}"
        self.launched = time.time_ns()
        subprocess.run(["hyprctl", "dispatch", f"hl.dsp.exec_cmd('{cmd}', {rules})"], capture_output=True, check=True)
        self.pid = None
        end = time.time() + 20
        while self.pid is None and time.time() < end:
            client = next((c for c in hypr_clients() if c["title"] == self.title), None)
            if client:
                self.mapped = time.time_ns()
                self.pid = client["pid"]
                (x, y), (w, h) = client["at"], client["size"]
                self.center = (x + w // 2, y + h // 2)
        if self.pid is None:
            raise TimeoutError(f"{term} window never showed up")

    def ready(self, timeout=20):
        end = time.time() + timeout
        while not (self.ctl / "ready").exists() or not (self.ctl / "ready").read_text():
            if time.time() > end:
                raise TimeoutError("inner program never started")
            time.sleep(0.01)
        info = json.loads((self.ctl / "ready").read_text())
        self.fifo = open(self.ctl / "fifo", "w", buffering=1)
        self.inner = info["pid"]
        return self

    def size(self):
        self.send("size")
        while not (self.ctl / "size").exists() or not (self.ctl / "size").read_text():
            time.sleep(0.01)
        return json.loads((self.ctl / "size").read_text())

    def send(self, cmd):
        self.fifo.write(cmd + "\n")

    def procs(self):
        return tree(self.pid, self.inner)

    def close(self):
        try:
            self.send("quit")
            self.fifo.close()
        except (AttributeError, BrokenPipeError):
            pass
        time.sleep(0.2)
        # The window closes when the inner program exits; kill only if not.
        # gnome-terminal's server outlives its windows: it is ours, kill it.
        if self.term == "gnome-terminal" or any(c["pid"] == self.pid for c in hypr_clients()):
            try:
                os.kill(self.pid, 15)
            except ProcessLookupError:
                pass


def setup(scratch, history=10_000):
    """Same font and scrollback for both terminals."""
    config = scratch / "config"
    (config / "raterm").mkdir(parents=True, exist_ok=True)
    (config / "raterm/config.toml").write_text(
        f'scrollback = {history}\n[font]\nfamily = "{FONT}"\nsize = {SIZE}\n'
    )
    (config / "alacritty.toml").write_text(
        f'[font]\nsize = {SIZE}\nnormal = {{ family = "{FONT}" }}\n'
        f"[colors.primary]\nbackground = '#000000'\n[scrolling]\nhistory = {history}\n"
    )
    (config / "terminator.conf").write_text(
        "[global_config]\n  dbus = False\n[profiles]\n  [[default]]\n"
        f"    use_system_font = False\n    font = {FONT} {SIZE}\n    scrollback_lines = {history}\n"
        '    use_theme_colors = False\n    background_color = "#000000"\n'
        '    foreground_color = "#eaeaea"\n    show_titlebar = False\n'
    )
    (config / "glib-2.0/settings").mkdir(parents=True, exist_ok=True)
    (config / "glib-2.0/settings/keyfile").write_text(
        "[org/gnome/terminal/legacy]\ndefault-show-menubar=false\n"
        "[org/gnome/terminal/legacy/profiles:/:b1dcc9dd-5262-4d8d-a863-c897e6d979b9]\n"
        f"use-system-font=false\nfont='{FONT} {SIZE}'\nscrollback-lines={history}\n"
        "use-theme-colors=false\nbackground-color='#000000'\nforeground-color='#eaeaea'\n"
    )


# --- benchmarks --------------------------------------------------------------


def bench_startup(term, scratch, record):
    w = Window(term, scratch, startup=True)
    try:
        seen = wait_pixel(*w.center, MARK, 20)
        w.ready()
        time.sleep(1)
        record(
            mapped_ms=(w.mapped - w.launched) / 1e6,
            first_frame_ms=(seen - w.launched) / 1e6,
            idle_pss_mib=pss_mib(w.procs()),
            idle_rss_mib=rss_mib(w.procs()),
            processes=len(w.procs()),
        )
    finally:
        w.close()


def bench_throughput(term, scratch, record, files):
    for name, path in files.items():
        w = Window(term, scratch).ready()
        try:
            wait_pixel(*w.center, BLACK, 20)
            time.sleep(0.5)
            cols, rows = w.size()
            procs, idle = w.procs(), pss_mib(w.procs())
            cpu0 = cpu_seconds(procs)
            w.send(f"cat {path}")
            seen = wait_pixel(*w.center, MARK, 600, interval=0.02)
            cpu = cpu_seconds(procs) - cpu0
            t0, t1 = json.loads((w.ctl / "drained").read_text())
            time.sleep(1)
            record(
                workload=name,
                mib=path.stat().st_size / (1 << 20),
                cols=cols,
                rows=rows,
                drain_s=(t1 - t0) / 1e9,
                display_s=(seen - t0) / 1e9,
                cpu_s=cpu,
                idle_pss_mib=idle,
                after_pss_mib=pss_mib(w.procs()),
            )
        finally:
            w.close()


def bench_latency(term, scratch, record, samples=60):
    w = Window(term, scratch).ready()
    try:
        wait_pixel(*w.center, BLACK, 20)
        time.sleep(0.5)
        for i, latency in enumerate(latencies(w, samples)):
            record(sample=i, latency_ms=latency)
    finally:
        w.close()


def latencies(w, samples):
    out = []
    for i in range(samples):
        # Random phase against the compositor's frames.
        time.sleep(random.uniform(0.05, 0.1))
        t0 = time.time_ns()
        w.send(f"cell {i % 2}")
        out.append((wait_pixel(*w.center, LATENCY_COLORS[i % 2], 10) - t0) / 1e6)
    return out


def bench_scrollback(term, record, out, extra):
    """Fills a scrollback, then measures memory, latency and 4 MiB more."""
    for lines in SCROLLBACK_LINES:
        if term == "alacritty" and lines > 100_000:
            continue
        fill = out / f"fill-{lines}.txt"
        if not fill.exists():
            rnd = random.Random(lines)
            words = ["".join(rnd.choices("abcdefghijklmnopqrstuvwxyz", k=rnd.randint(1, 10))) for _ in range(2000)]
            fill.write_text("".join(" ".join(rnd.choices(words, k=rnd.randint(4, 16))) + "\n" for _ in range(lines)))
        scratch = Path(tempfile.mkdtemp(prefix="raterm-bench-"))
        setup(scratch, history=lines)
        w = Window(term, scratch).ready()
        try:
            wait_pixel(*w.center, BLACK, 20)
            time.sleep(0.5)
            t0 = time.time_ns()
            w.send(f"cat {fill}")
            fill_s = (wait_pixel(*w.center, MARK, 900, interval=0.05) - t0) / 1e9
            # Most of the garbage of the output is collected by then.
            time.sleep(15)
            pss = pss_mib(w.procs())
            w.send("black")
            wait_pixel(*w.center, BLACK, 120)
            time.sleep(0.5)
            t0 = time.time_ns()
            w.send(f"cat {extra}")
            extra_s = (wait_pixel(*w.center, MARK, 900, interval=0.05) - t0) / 1e9
            time.sleep(5)
            ls = latencies(w, 30)
            record(lines=lines, fill_s=fill_s, pss_mib=pss, extra_s=extra_s, latency_ms=statistics.median(ls))
        finally:
            w.close()


def bench_blocks(term, scratch, record):
    """Memory with rich blocks in view (raterm only): a web view each."""
    if term != "raterm":
        return
    for n in BLOCKS:
        w = Window(term, scratch).ready()
        try:
            wait_pixel(*w.center, BLACK, 20)
            time.sleep(0.5)
            idle = pss_mib(w.procs())
            w.send(f"blocks {n}")
            while not (w.ctl / "blocks").exists():
                time.sleep(0.05)
            # The web views load their documents and measure them.
            time.sleep(3)
            procs = w.procs()
            record(blocks=n, idle_pss_mib=idle, pss_mib=pss_mib(procs), rss_mib=rss_mib(procs), processes=len(procs))
        finally:
            w.close()


def run(args):
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="raterm-bench-"))
    setup(scratch)
    files = workloads(out / f"workloads-{WORKLOAD_MIB}") if {"throughput", "scrollback"} & set(args.only) else {}
    hz = refresh_rate()
    results = out / "results.jsonl"
    with open(results, "a") as log:
        for n in range(args.runs):
            for bench in args.only:
                for term in args.terminals:

                    def record(**fields):
                        row = {"bench": bench, "terminal": term, "run": n, "hz": hz, **fields}
                        log.write(json.dumps(row) + "\n")
                        log.flush()
                        print(json.dumps(row), flush=True)

                    # A window hidden by a workspace switch times out: skip.
                    try:
                        if bench == "startup":
                            bench_startup(term, scratch, record)
                        elif bench == "throughput":
                            bench_throughput(term, scratch, record, files)
                        elif bench == "latency":
                            bench_latency(term, scratch, record)
                        elif bench == "scrollback":
                            bench_scrollback(term, record, out, files["ascii"])
                        elif bench == "blocks":
                            bench_blocks(term, scratch, record)
                    except TimeoutError as e:
                        print(f"{bench} {term} run {n}: {e}", file=sys.stderr, flush=True)
    print(f"results in {results}")


# --- report ------------------------------------------------------------------


def report(args):
    """Writes the CSVs of the charts in DIR and prints Markdown tables."""
    out = Path(args.dir)
    rows = [json.loads(line) for line in open(out / "results.jsonl")]
    terms = list(dict.fromkeys(r["terminal"] for r in rows))

    def write_csv(name, header, lines):
        with open(out / f"{name}.csv", "w") as f:
            f.write(",".join(header) + "\n")
            f.writelines(",".join(str(v) for v in line) + "\n" for line in lines)

    def table(header, lines):
        print("| " + " | ".join(header) + " |")
        print("|" + "---|" * len(header))
        for line in lines:
            print("| " + " | ".join(str(v) for v in line) + " |")
        print()

    def med(rs, key):
        return statistics.median(r[key] for r in rs)

    def pick(bench, **match):
        return [r for r in rows if r["bench"] == bench and all(r[k] == v for k, v in match.items())]

    tp = pick("throughput")
    workloads = list(dict.fromkeys(r["workload"] for r in tp))
    csv, display, detail, memory = [], [], [], []
    for wl in workloads:
        by = {t: pick("throughput", terminal=t, workload=wl) for t in terms}
        by = {t: rs for t, rs in by.items() if rs}
        mib = next(iter(by.values()))[0]["mib"]
        for t, rs in by.items():
            d = med(rs, "display_s")
            csv.append([t, wl, round(mib / d, 3), round(mib / med(rs, "drain_s"), 3)])
            memory.append([t, f"after {wl}", round(med(rs, "after_pss_mib"))])
            detail.append([wl, t, f"{med(rs, 'drain_s'):.3f}", f"{d:.3f}", f"{med(rs, 'cpu_s'):.2f}", f"{med(rs, 'after_pss_mib'):.0f}"])
        ds = [med(rs, "display_s") for rs in by.values()]
        # How many times the first terminal is slower than each other one.
        ratios = [f"×{ds[0] / d:.1f}" for d in ds[1:]]
        display.append([wl, f"{mib:.1f}", *(f"{d:.3f} s ({mib / d:.3g} MiB/s)" for d in ds), *ratios])
    write_csv("throughput", ["terminal", "workload", "displayed_mib_per_s", "drained_mib_per_s"], csv)

    lat = pick("latency")
    setup = lambda r: f"{r['terminal']} @ {round(r['hz'])} Hz"
    write_csv("latency", ["setup", "latency_ms"], [[setup(r), round(r["latency_ms"], 2)] for r in lat])

    st = pick("startup")
    write_csv("startup", ["terminal", "first_frame_ms"], [[r["terminal"], round(r["first_frame_ms"], 1)] for r in st])
    for t in terms:
        if ss := pick("startup", terminal=t):
            memory.insert(0, [t, "idle", round(med(ss, "idle_pss_mib"))])
    write_csv("memory", ["terminal", "state", "pss_mib"], memory)

    runs = max(r["run"] for r in rows) + 1
    print(f"Medians over {runs} runs.\n\n### Throughput\n")
    table(["workload", "MiB", *terms, *(f"{terms[0]} / {t}" for t in terms[1:])], display)
    table(["workload", "terminal", "drained (s)", "displayed (s)", "CPU (s)", "PSS after (MiB)"], detail)
    print("### Latency\n")
    lines = []
    for s in dict.fromkeys(map(setup, lat)):
        ls = sorted(r["latency_ms"] for r in lat if setup(r) == s)
        lines.append([s, f"{ls[0]:.1f}", f"{statistics.median(ls):.1f}", f"{ls[int(len(ls) * 0.9)]:.1f}", f"{ls[-1]:.1f}", len(ls)])
    table(["setup", "min (ms)", "median (ms)", "p90 (ms)", "max (ms)", "samples"], lines)
    print("### Startup and memory\n")
    lines = []
    for t in terms:
        if ss := pick("startup", terminal=t):
            lines.append([t, f"{med(ss, 'mapped_ms'):.0f}", f"{med(ss, 'first_frame_ms'):.0f}", f"{med(ss, 'idle_pss_mib'):.0f}", f"{med(ss, 'idle_rss_mib'):.0f}", ss[0]["processes"]])
    table(["terminal", "window mapped (ms)", "first frame (ms)", "idle PSS (MiB)", "idle RSS (MiB)", "processes"], lines)

    csv, lines = [], []
    for t in terms:
        for n in SCROLLBACK_LINES:
            if rs := pick("scrollback", terminal=t, lines=n):
                csv.append([t, n, round(med(rs, "latency_ms"), 1), round(med(rs, "pss_mib"))])
                lines.append(
                    [t, f"{n:,}", f"{med(rs, 'fill_s'):.1f}", f"{med(rs, 'pss_mib'):.0f}",
                     f"{med(rs, 'latency_ms'):.1f}", f"{med(rs, 'extra_s'):.2f}", len(rs)]
                )
    if lines:
        write_csv("scrollback", ["terminal", "lines", "latency_ms", "pss_mib"], csv)
        print("### Scrollback\n")
        table(["terminal", "lines", "filled in (s)", "PSS (MiB)", "latency (ms)", "4 MiB more (s)", "runs"], lines)

    lines = []
    for n in BLOCKS:
        if rs := pick("blocks", blocks=n):
            lines.append([f"{n}", f"{med(rs, 'idle_pss_mib'):.0f}", f"{med(rs, 'pss_mib'):.0f}", f"{med(rs, 'rss_mib'):.0f}", rs[0]["processes"], len(rs)])
    if lines:
        print("### Blocks\n")
        table(["blocks", "PSS before (MiB)", "PSS with blocks (MiB)", "RSS (MiB)", "processes", "runs"], lines)


def main():
    if sys.argv[1:2] == ["inner"]:
        return inner(sys.argv[2])
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--only", type=lambda s: s.split(","), default=["startup", "latency", "throughput", "scrollback"])
    r.add_argument("--terminals", type=lambda s: s.split(","), default=TERMINALS)
    r.add_argument("--runs", type=int, default=3)
    r.add_argument("--out", default="bench-results")
    sub.add_parser("report").add_argument("dir")
    args = parser.parse_args()
    run(args) if args.cmd == "run" else report(args)


if __name__ == "__main__":
    main()
