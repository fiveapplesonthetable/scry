#!/usr/bin/env bash
# Runs after `scry index` finalizes:
#   1. build-offsets (~30 sec — enables lazy/mmap reader)
#   2. build-trigrams (~15-20 min — enables 100× faster grep)
#   3. validate.sh — sanity-check def/ref/grep on real AOSP symbols
#   4. bench_grep.sh — quantify the 100× rg claim
#   5. Email the user with results
#
# Invoke once the scry-index.service has finished. Designed to be run
# by hand OR by a poller that watches for the unit to deactivate.

set -uo pipefail
. /mnt/agent/scry/env.sh

INDEX=${SCRY_INDEX_DIR:-/mnt/agent/scry-index}
SCRY=/mnt/agent/scry/target/release/scry
LOG=/mnt/agent/scry-post-finalize.log
TO=zimuzostanley@gmail.com
FROM=fiveapplesonthetable@gmail.com

mkdir -p "$(dirname "$LOG")"
exec > >(tee -a "$LOG") 2>&1

date -Is
echo "=== post-finalize: starting ==="

if [ ! -d "$INDEX" ]; then
  echo "no index at $INDEX — nothing to do"
  exit 1
fi

echo "=== step 1: build-offsets (lazy reader sidecars) ==="
t1=$(date +%s)
$SCRY build-offsets --index "$INDEX"
echo "step 1 took $(($(date +%s) - t1)) sec"

echo "=== step 2: build-trigrams ==="
t2=$(date +%s)
$SCRY build-trigrams --index "$INDEX" --workers 16
echo "step 2 took $(($(date +%s) - t2)) sec"

echo "=== final index layout ==="
ls -la "$INDEX"

echo "=== step 3: validate.sh ==="
t3=$(date +%s)
SCRY_INDEX_DIR="$INDEX" /mnt/agent/scry/scripts/validate.sh || true
echo "step 3 took $(($(date +%s) - t3)) sec"

echo "=== step 4: bench_grep.sh ==="
t4=$(date +%s)
SCRY_INDEX_DIR="$INDEX" /mnt/agent/scry/scripts/bench_grep.sh || true
echo "step 4 took $(($(date +%s) - t4)) sec"

DURATION=$(($(date +%s) - t1))
SUMMARY=$($SCRY stats --index "$INDEX" 2>/dev/null | head -10)

msmtp -t <<EMAIL_END
To: $TO
From: $FROM
Subject: [scry] FULL INDEX COMPLETE — post-finalize done in ${DURATION}s

Index has finalized. Post-finalize pipeline ran:
  1. build-offsets
  2. build-trigrams
  3. validate.sh
  4. bench_grep.sh

Index at $INDEX.

STATS:
$SUMMARY

Last 120 lines of post-finalize log:
$(tail -120 "$LOG")
EMAIL_END
echo "=== email sent ==="
echo "post-finalize: DONE"
