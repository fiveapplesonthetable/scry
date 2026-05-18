#!/bin/bash
# Drive build_kzip.bash's xref targets (xref_cxx / xref_java /
# xref_kotlin / xref_rust + merge_zips) with safe defaults:
#
#  * Capped ninja parallelism. Each Kythe extractor is a JVM with
#    a default max heap of ~25% of RAM, so unconstrained -j on a
#    high-core host trivially OOMs. Try -j 24, then -j 12, then -j 6.
#
#  * Symlinked OUT_DIR support. Soong refuses an OUT_DIR outside
#    the source tree, so the recommended pattern is to symlink the
#    in-tree `out/` directory at a large-disk location:
#        ln -sfn /mnt/agent/aosp-out ~/dev/aosp/out
#    `find out ...` will then return zero matches (GNU find won't
#    descend through a symlink given as the starting path). The
#    `find -L out ...` form below follows symlinks and reports
#    every per-CU .kzip.
#
#  * Explicit merge_zips fallback. If the build crashes after
#    extraction but before the final merge, re-running this script
#    re-uses the existing per-CU .kzip files and only re-runs the
#    merge step.
#
# Environment overrides (all optional):
#   AOSP_ROOT       default: ~/dev/aosp
#   TARGET_PRODUCT  default: aosp_cf_x86_64_phone
#   TARGET_RELEASE  default: trunk_staging
#   TARGET_BUILD_VARIANT default: userdebug
#   DIST_DIR        default: /mnt/agent/scry-kzip
#   KZIP_NAME       default: $TARGET_PRODUCT
#   GOCACHE         default: /mnt/agent/tmp/go-build-cache
#   XREF_CORPUS     default: android.googlesource.com/platform/superproject
#   LOG             default: /mnt/agent/tmp/aosp-build-kzip.log

set -uo pipefail

AOSP_ROOT="${AOSP_ROOT:-$HOME/dev/aosp}"
cd "$AOSP_ROOT"

export TARGET_PRODUCT="${TARGET_PRODUCT:-aosp_cf_x86_64_phone}"
export TARGET_RELEASE="${TARGET_RELEASE:-trunk_staging}"
export TARGET_BUILD_VARIANT="${TARGET_BUILD_VARIANT:-userdebug}"
export XREF_CORPUS="${XREF_CORPUS:-android.googlesource.com/platform/superproject}"
export DIST_DIR="${DIST_DIR:-/mnt/agent/scry-kzip}"
export KZIP_NAME="${KZIP_NAME:-$TARGET_PRODUCT}"
export GOCACHE="${GOCACHE:-/mnt/agent/tmp/go-build-cache}"
LOG="${LOG:-/mnt/agent/tmp/aosp-build-kzip.log}"
mkdir -p "$GOCACHE" "$DIST_DIR" "$(dirname "$LOG")"

kzip_targets="merge_zips xref_cxx xref_java xref_kotlin xref_rust"

# Try -j 24, then -j 12, then -j 6.
for jobs in 24 12 6; do
  echo "=== $(date -Iseconds) attempt with -j $jobs ===" | tee -a "$LOG"
  if build/soong/soong_ui.bash --build-mode --all-modules --dir="$PWD" -k \
        --skip-soong-tests --ninja_weight_source=not_used \
        -j "$jobs" $kzip_targets 2>&1 | tee -a "$LOG"; then
    echo "=== $(date -Iseconds) xref targets built with -j $jobs ===" | tee -a "$LOG"
    break
  fi
  echo "=== $(date -Iseconds) -j $jobs failed; retrying lower ===" | tee -a "$LOG"
done

# Count per-CU .kzip files. Note `-L`: `out/` is typically a symlink
# to a large-disk path, and GNU find won't descend a symlink-as-root
# without -L.
kzip_count=$(find -L out -name '*.kzip' 2>/dev/null | wc -l)
echo "=== $(date -Iseconds) $kzip_count kzips on disk ===" | tee -a "$LOG"
if (( kzip_count < 100 )); then
  echo "ERROR: only $kzip_count kzips produced; xref targets didn't run" | tee -a "$LOG"
  exit 1
fi

# Pack everything into one all.kzip. merge_zips is the same tool
# build_kzip.bash uses; it accepts @file with one path per line.
allkzip="$DIST_DIR/$KZIP_NAME.kzip"
echo "=== $(date -Iseconds) merging $kzip_count kzips into $allkzip ===" | tee -a "$LOG"
out/host/linux-x86/bin/merge_zips "$allkzip" @<(find -L out -name '*.kzip') 2>&1 | tee -a "$LOG"
echo "=== $(date -Iseconds) done: $(ls -lh "$allkzip" | awk '{print $5}') $allkzip ===" | tee -a "$LOG"
