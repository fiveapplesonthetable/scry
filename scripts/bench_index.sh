#!/usr/bin/env bash
# Index-time + peak-RSS benchmark. Sweeps --workers (and optionally
# --mem-cap) on a fixed sub-corpus to make the scaling characteristic
# visible. Reports wall time, peak RSS, throughput in files/sec.
#
# Uses the Linux kernel as the test corpus by default — ~85 k files,
# ~10–60 s per indexing run depending on workers, so a 4-configuration
# sweep fits in single-digit minutes. Full AOSP+Linux is too slow for
# a matrix sweep; one full-corpus run is reported separately in
# docs/BENCHMARKS.md as the headline number.
#
# Env knobs:
#   BENCH_ROOT       — corpus to index (default /mnt/agent/dev/linux)
#   BENCH_INDEX_DIR  — where to write the throwaway test index
#   BENCH_WORKERS    — space-separated worker counts to sweep
#   BENCH_MEM_CAP    — single mem-cap value in GiB (default 8)

set -uo pipefail
. /mnt/agent/scry/env.sh

SCRY=/mnt/agent/scry/target/release/scry
ROOT=${BENCH_ROOT:-/mnt/agent/dev/linux}
TEST_INDEX=${BENCH_INDEX_DIR:-/tmp/scry-bench-index}
WORKERS=${BENCH_WORKERS:-"2 8 16 32"}
MEM_CAP=${BENCH_MEM_CAP:-8}

if [ ! -d "$ROOT" ]; then
  echo "bench root $ROOT not found" >&2
  exit 1
fi

# File count — what we'll divide wall time by for files/sec.
echo "[bench-index] corpus root: $ROOT"
echo "[bench-index] counting source files..."
NFILES=$(find "$ROOT" -type f 2>/dev/null | wc -l)
echo "[bench-index] $NFILES files (rough — pre-walker classification)"
echo

printf "%-10s %-10s %10s %10s %12s   peak RSS\n" \
       "workers" "mem-cap" "wall(s)" "files/s" "MB-out"
printf "%-10s %-10s %10s %10s %12s   --------\n" \
       "-------" "-------" "-------" "-------" "------"

for w in $WORKERS; do
  rm -rf "$TEST_INDEX" "${TEST_INDEX}.tmp" 2>/dev/null
  # /usr/bin/time -v prints "Maximum resident set size (kbytes): N" on
  # GNU coreutils — capture it alongside the wall time.
  TMPLOG=$(mktemp)
  /usr/bin/time -v "$SCRY" index "$ROOT" \
      -o "$TEST_INDEX" \
      --workers "$w" --mem-cap "$MEM_CAP" \
      --flush-bytes 1024 --flush-every 5000 \
      --big-file-bytes 65536 --max-file-bytes 5242880 \
      > /dev/null 2> "$TMPLOG"
  wall=$(awk -F: '/Elapsed \(wall clock\)/ {gsub(/^[ \t]+/, "", $0); print $NF}' "$TMPLOG" | tail -1)
  # wall format is "m:ss.xx" or "h:mm:ss" — normalize to seconds.
  wall_s=$(echo "$wall" | awk -F: '{ if (NF==3) print $1*3600+$2*60+$3; else if (NF==2) print $1*60+$2; else print $1 }')
  peak_kb=$(awk -F': ' '/Maximum resident set size/ {print $2}' "$TMPLOG")
  peak_mb=$(awk -v k="$peak_kb" 'BEGIN { printf "%.1f", k/1024 }')
  out_mb=$(du -sm "$TEST_INDEX" 2>/dev/null | awk '{print $1}')
  fps=$(awk -v n="$NFILES" -v s="$wall_s" 'BEGIN { if (s+0>0) printf "%.0f", n/s; else print "?" }')
  printf "%-10s %-10s %10s %10s %12s   %s MB\n" \
         "$w" "${MEM_CAP}G" "$wall_s" "$fps" "$out_mb" "$peak_mb"
  rm -f "$TMPLOG"
done

echo
echo "[bench-index] cleanup: rm -rf $TEST_INDEX"
rm -rf "$TEST_INDEX" "${TEST_INDEX}.tmp" 2>/dev/null
