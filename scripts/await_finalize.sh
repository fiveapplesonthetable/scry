#!/usr/bin/env bash
# Watches the scry-index systemd unit and fires post_finalize.sh once
# the index has finalized cleanly.
#
# Why this exists separately from run_index.sh: the indexer is run under
# systemd-run with Restart=on-failure, so exit 0 (success) leaves the
# unit in 'inactive (dead)' and there is no built-in "ExecStopPost" path
# we can reuse without rewriting the unit. This script is the post-stop
# hook, run as a regular user process. Start with `nohup ... &` (or any
# detached form) and let it sleep-loop until the conditions hold.
#
# Fires when ALL of:
#   1. systemctl --user is-active scry-index → "inactive"
#   2. $INDEX/manifest.json exists (true after the final atomic rename)
# Then runs post_finalize.sh, logs to $WATCH_LOG, exits.
#
# An older watcher checked for a "DONE:" log line that scry never emits,
# so it would have hung forever — fixed here by using the durable signal
# (the finalized manifest) rather than a fragile log marker.

set -uo pipefail

INDEX=${SCRY_INDEX_DIR:-/mnt/agent/scry-index}
WATCH_LOG=${SCRY_WATCH_LOG:-/mnt/agent/scry-watcher.log}
POST=/mnt/agent/scry/scripts/post_finalize.sh

log() { echo "[$(date -Is)] $*" >> "$WATCH_LOG"; }

log "await_finalize started; watching unit=scry-index, index=$INDEX"

while true; do
  state=$(systemctl --user is-active scry-index 2>/dev/null || true)
  if [ "$state" = "inactive" ] && [ -f "$INDEX/manifest.json" ]; then
    log "unit inactive + manifest.json present — firing post_finalize"
    "$POST" >> "$WATCH_LOG" 2>&1
    rc=$?
    log "post_finalize exited code $rc"
    exit 0
  fi
  sleep 20
done
