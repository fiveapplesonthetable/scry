#!/usr/bin/env bash
# Benchmark scry grep (with and without trigram pre-filter) vs raw rg
# over the indexed corpus. Demonstrates the "100x rg" claim quantitatively.
set -uo pipefail

INDEX=${SCRY_INDEX_DIR:-/mnt/agent/scry-index}
SCRY=/mnt/agent/scry/target/release/scry
RG=$(command -v rg || echo "/usr/bin/rg")
ROOTS=(/home/zim/dev/aosp /mnt/agent/dev/linux)

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

bench() {
  local label="$1" cmd="$2" pattern="$3"
  # warm the page cache once, time three runs, report the best
  eval "$cmd" >/dev/null 2>&1 || true
  local best=999999
  for _ in 1 2 3; do
    # /usr/bin/time prepends "Command exited with non-zero status N" when
    # the child exits non-zero (rg does this on no-match). Take the LAST
    # line so we always get the bare "%e" elapsed value, not the diagnostic.
    local t=$({ /usr/bin/time -f "%e" bash -c "$cmd >/dev/null 2>&1" ; } 2>&1 | tail -1)
    if awk -v a="$t" -v b="$best" 'BEGIN { exit !(a + 0 < b + 0) }' ; then
      best=$t
    fi
  done
  printf "  %-30s pattern=%-30s best=%6ss\n" "$label" "\"$pattern\"" "$best"
}

for p in "${PATTERNS[@]}"; do
  echo "=== $p ==="
  bench "scry grep (trigram)"    "$SCRY grep \"$p\" --index $INDEX --limit 100"      "$p"
  bench "rg (whole tree)"        "$RG -j 4 --no-heading -F \"$p\" ${ROOTS[*]}"        "$p"
  echo
done
