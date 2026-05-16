#!/usr/bin/env bash
# Benchmark scry grep vs ripgrep vs POSIX grep -r over the indexed
# corpus. Demonstrates the "100x rg" claim quantitatively AND the much
# larger gap against the standard grep most users still default to.
#
# Three tools, three different runtimes:
#   scry grep   — trigram pre-filter, best-of-3
#   rg          — best-of-3 (rg is fast enough that 3 runs is cheap)
#   grep -r     — single run; POSIX grep on 70 GB of source can take
#                 minutes, and a best-of-N sweep would dominate the
#                 total benchmark time without changing the message
set -uo pipefail

INDEX=${SCRY_INDEX_DIR:-/mnt/agent/scry-index}
SCRY=/mnt/agent/scry/target/release/scry
RG=$(command -v rg || echo "/usr/bin/rg")
GREP=$(command -v grep || echo "/bin/grep")
ROOTS=(/home/zim/dev/aosp /mnt/agent/dev/linux)
# Set BENCH_INCLUDE_GREP=0 to skip POSIX grep (saves minutes per pattern).
INCLUDE_GREP=${BENCH_INCLUDE_GREP:-1}

if ! command -v rg >/dev/null 2>&1; then
  echo "rg not found — install ripgrep for the comparison."
  exit 1
fi

if [ ! -d "$INDEX" ]; then
  echo "no index at $INDEX — run scry index first."
  exit 1
fi

# A spread of literal patterns:
#   - rare exact symbol     (best case for trigram filtering)
#   - common substring      (moderate case)
#   - very common word      (worst case — most files contain it)
PATTERNS=(
  "Z3ProcessStateController"
  "frameworks/base/services"
  "ParcelFile"
  "ActivityManagerService"
  "TODO("
)

# Time one command, return its elapsed seconds. /usr/bin/time prepends
# a "Command exited with non-zero status N" line when the child exits
# non-zero (rg + grep both do this on no-match); take tail -1 so we
# always get the bare "%e" line.
time_one() {
  local cmd="$1"
  { /usr/bin/time -f "%e" bash -c "$cmd >/dev/null 2>&1" ; } 2>&1 | tail -1
}

bench_best3() {
  local label="$1" cmd="$2"
  eval "$cmd" >/dev/null 2>&1 || true   # warm page cache
  local best=999999
  for _ in 1 2 3; do
    local t=$(time_one "$cmd")
    if awk -v a="$t" -v b="$best" 'BEGIN { exit !(a + 0 < b + 0) }' ; then
      best=$t
    fi
  done
  printf "  %-32s best=%7ss\n" "$label" "$best"
}

bench_one() {
  local label="$1" cmd="$2"
  local t=$(time_one "$cmd")
  printf "  %-32s once=%7ss\n" "$label" "$t"
}

for p in "${PATTERNS[@]}"; do
  echo "=== $p ==="
  bench_best3 "scry grep (trigram)"      "$SCRY grep \"$p\" --index $INDEX --limit 100"
  bench_best3 "rg -j4 (whole tree)"      "$RG -j 4 --no-heading -F \"$p\" ${ROOTS[*]}"
  if [ "$INCLUDE_GREP" = "1" ]; then
    bench_one "grep -rF (whole tree)"    "$GREP -rF -- \"$p\" ${ROOTS[*]}"
  fi
  echo
done
