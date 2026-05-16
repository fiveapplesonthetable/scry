#!/usr/bin/env bash
# Runs `scry index` with --resume. Exit code propagates to systemd: a cgroup
# OOM-kill is reported as a non-zero exit, which trips Restart=on-failure,
# which re-runs us with --resume, which picks up from progress.json. The loop
# runs until the index finalizes (exit 0).
set -euo pipefail
. /mnt/agent/scry/env.sh

# jemalloc: aggressive return-to-OS so RSS tracks workload instead of
# accumulating a high-water mark across batches.
export MALLOC_CONF="dirty_decay_ms:100,muzzy_decay_ms:100,narenas:1"
# tree-sitter parse timeout per file. Adversarial inputs (ctags' own
# kotlin/python grammar fixtures, generated 250 KB Java tests) can
# transiently allocate gigabytes. Capping the parse time bounds the damage
# — files that time out are recorded as parse failures, not OOM kills.
# 2000 ms is comfortably above any legitimate parse (real Kotlin files
# clock in at <50 ms; the largest real Cpp files are ~500 ms).
export SCRY_PARSE_TIMEOUT_MS=2000

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
  --workers 8 \
  --flush-bytes 1024 \
  --flush-every 1000 \
  --mem-cap 40 \
  --big-file-bytes 8192 \
  -o /mnt/agent/scry-index
