#!/usr/bin/env bash
# Runs after `scry index` finishes:
#   1. build-file-symbols + build-file-refs (outline / uses sidecars)
#   2. build-trigrams (~15-20 min — enables 100× faster grep)
#   3. build-digests (incremental-reindex change detection)
#   4. build-modgraph soong (the --reachable build graph)
#   5. validate.sh — sanity-check def/ref/grep on real AOSP symbols
#   6. bench_grep.sh — quantify the 100× rg claim
#   7. Email the user with results
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

echo "=== sidecar builds + Soong module graph ==="
t1=$(date +%s)
$SCRY build-file-symbols --index "$INDEX"
$SCRY build-file-refs    --index "$INDEX"
$SCRY build-trigrams     --index "$INDEX" --workers 16
$SCRY build-digests      --index "$INDEX" --workers 16
$SCRY build-modgraph --kind soong --root /home/zim/dev/aosp \
  --output "$INDEX/module_graph.json"
echo "sidecar builds took $(($(date +%s) - t1)) sec"

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
SUMMARY=$($SCRY stats --index "$INDEX" 2>/dev/null | head -20)
INDEX_LAYOUT=$(ls -la "$INDEX" 2>/dev/null | tail -n +2)
INDEX_DISK=$(du -sh "$INDEX" 2>/dev/null | awk '{print $1}')

# High-level repo layout — answers "what is scry made of" for someone
# reading this email cold. Pulled live (not hardcoded) so it stays in
# sync as the code evolves.
REPO=/mnt/agent/scry
CRATES=$(ls -d "$REPO"/crates/*/ 2>/dev/null | awk -F/ '{print "  - " $(NF-1)}')
DOC_FILES=$(ls "$REPO"/docs/*.md "$REPO"/README.md 2>/dev/null | awk -F/ '{print "  - " $(NF-1) "/" $NF}' | sed "s|/mnt/agent/scry/||")
LATEST_COMMITS=$(git -C "$REPO" log --oneline -10 2>/dev/null)

# Sample queries to demonstrate the API to whoever reads the email.
SAMPLES=$(
  for q in \
    "def ActivityManagerService" \
    "callers transact --lang Java --limit 5" \
    "grep 'ZygoteInit'" \
    "def libbinder --kind soong" \
    "def zygote --kind init.svc"; do
    printf '\n$ scry %s\n' "$q"
    timeout 5 $SCRY $q --index "$INDEX" 2>&1 | head -5
  done
)

msmtp -t <<EMAIL_END
To: $TO
From: $FROM
Subject: [scry] FULL INDEX COMPLETE — post-finalize done in ${DURATION}s

The scry full AOSP + Linux kernel index has finished. Post-index
pipeline ran:
  1. build-file-symbols + build-file-refs (outline / uses sidecars)
  2. build-trigrams (100× rg grep path)
  3. build-digests + build-modgraph soong
  4. validate.sh (def/ref/grep against real AOSP symbols)
  5. bench_grep.sh (scry vs rg head-to-head)

Index at: $INDEX
Index size on disk: $INDEX_DISK

==================================================================
PROJECT STRUCTURE
==================================================================
Source root: $REPO  (git: github.com/fiveapplesonthetable/scry)

Crates:
$CRATES

Docs:
$DOC_FILES

Latest commits:
$LATEST_COMMITS

==================================================================
STATS
==================================================================
$SUMMARY

==================================================================
INDEX LAYOUT
==================================================================
$INDEX_LAYOUT

==================================================================
SAMPLE QUERIES (live against the just-finalized index)
==================================================================
$SAMPLES

==================================================================
LAST 120 LINES OF POST-FINALIZE LOG
==================================================================
$(tail -120 "$LOG")
EMAIL_END
echo "=== email sent ==="
echo "post-finalize: DONE"
