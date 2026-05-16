#!/usr/bin/env bash
# Runs `scry index` with --resume. Exit code propagates to systemd: a cgroup
# OOM-kill is reported as a non-zero exit, which trips Restart=on-failure,
# which re-runs us with --resume, which picks up from progress.json. The loop
# runs until the index finalizes (exit 0).
set -euo pipefail
. /mnt/agent/scry/env.sh

# jemalloc: aggressive return-to-OS so RSS tracks workload instead of
# accumulating a high-water mark across batches. narenas defaults to
# 4*ncpu — fine for our worker pool size; previous narenas:1 caused
# allocator-side serialization under multi-worker pressure.
export MALLOC_CONF="dirty_decay_ms:100,muzzy_decay_ms:100"
# tree-sitter parse timeout per file. VERY generous (60 s) so legitimate
# parses are never the cause of a timeout — if this fires, it's a real
# pathology worth investigating, and scry-lang logs the file path loudly
# ([ts-TIMEOUT] or [ts-ABORT]) so you can root-cause it the next day.
# Set to 0 to disable entirely.
export SCRY_PARSE_TIMEOUT_MS=60000

ROOTS=(
  /home/zim/dev/aosp
  /mnt/agent/dev/linux
)

# Drop any roots that don't exist on this host.
USE_ROOTS=()
for r in "${ROOTS[@]}"; do
  if [ -d "$r" ]; then USE_ROOTS+=("$r"); fi
done
if [ ${#USE_ROOTS[@]} -eq 0 ]; then
  echo "no source roots present" >&2
  exit 2
fi

# Per-batch flush keeps in-RAM accumulation small. mem-cap is the soft jemalloc
# backpressure ceiling. big-file-bytes routes anything > 64KiB to the serial
# big-bucket so a single pathological tree-sitter parse can't pile up across
# workers. Resume + cgroup MemoryMax is the OOM safety net beneath all of that.
exec /mnt/agent/scry/target/release/scry index \
  "${USE_ROOTS[@]}" \
  --resume \
  --workers 16 \
  --flush-bytes 1024 \
  --flush-every 5000 \
  --mem-cap 40 \
  --big-file-bytes 65536 \
  --max-file-bytes 5242880 \
  -o /mnt/agent/scry-index
