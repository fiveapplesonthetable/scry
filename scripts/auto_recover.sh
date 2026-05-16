#!/usr/bin/env bash
# Periodic auto-recovery for the scry-index systemd unit.
# Runs every ~5 minutes via cron. Checks:
#   - If service is failed (StartLimitBurst exhausted): reset + reissue.
#   - If watermark hasn't moved in 30 minutes: send a flagged email
#     so the human knows to intervene.
#   - If oom_skiplist.txt is non-empty, count entries for the email.
# Does NOT change knobs autonomously — only restarts on failed state.
set -uo pipefail

SVC=scry-index
TMP=/mnt/agent/scry-index.tmp
LOG=/mnt/agent/scry-index.log
MARK=/tmp/scry-watermark-last.txt

now_ts=$(date +%s)

state=$(systemctl --user is-active "$SVC" 2>/dev/null || echo unknown)
if [ "$state" = "failed" ]; then
  # Restart limit exhausted — reset + try again
  systemctl --user reset-failed "$SVC" 2>/dev/null
  systemd-run --user --unit="$SVC" --collect \
    -p MemoryMax=60G -p MemorySwapMax=0 \
    -p Restart=on-failure -p RestartSec=3 \
    -p StartLimitBurst=500 -p StartLimitIntervalSec=0 \
    -p StandardOutput=append:"$LOG" -p StandardError=append:"$LOG" \
    /mnt/agent/scry/scripts/run_index.sh
  echo "$(date -Is) auto-recover: service was failed, restarted" >> /tmp/scry-auto-recover.log
fi

# Watermark stuck detection
cur_water=$(jq -r '.completed_files // 0' "$TMP/progress.json" 2>/dev/null || echo 0)
if [ -f "$MARK" ]; then
  prev_water=$(awk '{print $1}' "$MARK")
  prev_ts=$(awk '{print $2}' "$MARK")
  if [ "$cur_water" = "$prev_water" ] && [ -n "$prev_ts" ]; then
    age=$((now_ts - prev_ts))
    if [ "$age" -ge 1800 ]; then
      # 30 min stuck — flag
      skips=$(wc -l < "$TMP/oom_skiplist.txt" 2>/dev/null || echo 0)
      msmtp -t <<EOF
To: zimuzostanley@gmail.com
From: fiveapplesonthetable@gmail.com
Subject: [scry] WARN — watermark stuck at $cur_water for ${age}s

Auto-recovery noticed the indexer watermark has not advanced in
$((age/60)) minutes (current: $cur_water, last seen at $(date -d @$prev_ts -Is)).

OOM-skiplist size: $skips file(s) auto-excluded.

Last 30 lines of /mnt/agent/scry-index.log:
$(tail -30 "$LOG")
EOF
      # Reset the watermark mark so we don't re-flag every 5 min.
      echo "$cur_water $now_ts" > "$MARK"
    fi
  else
    echo "$cur_water $now_ts" > "$MARK"
  fi
else
  echo "$cur_water $now_ts" > "$MARK"
fi
