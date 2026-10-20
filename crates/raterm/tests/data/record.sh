#!/bin/sh
# Records a session as raw pty output for the snapshot tests, on an 80x24 pty.
# Usage: record.sh NAME COMMAND
set -e
out="$(dirname "$0")/$1.in"
script -q -E never -O "$out.tmp" -c "stty cols 80 rows 24; $2" > /dev/null
# Drop the header and footer lines that script adds around the session.
sed '1d;$d' "$out.tmp" > "$out"
rm "$out.tmp"
