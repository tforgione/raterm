#!/bin/sh
# Usage: build.sh RESULTS_DIR OUT_DIR
# Writes the summary tables to RESULTS_DIR/summary.md and the charts to OUT_DIR.
set -e
here=$(dirname "$0")
python3 "$here/../bench.py" report "$1" > "$1/summary.md"
cp "$1"/*.csv "$here"
mkdir -p "$2"
for chart in throughput latency memory startup scrollback; do
    ~/.claude/skills/vega-charts/scripts/render.sh "$here/$chart.vl.json" > "$2/$chart.svg"
done
