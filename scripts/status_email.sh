#!/usr/bin/env bash
# Hourly status report for the scry indexing job. Sent to the address per
# CLAUDE memory; never to anyone else (see ~/.msmtprc comment block).
set -uo pipefail

INDEX_DIR=/mnt/agent/scry-index
TMP_DIR=${INDEX_DIR}.tmp
LOG=/mnt/agent/scry-index.log
TO=zimuzostanley@gmail.com
FROM=fiveapplesonthetable@gmail.com
SVC=scry-index

state=$(systemctl --user is-active "$SVC" 2>/dev/null || echo unknown)
restarts=$(journalctl --user -u "$SVC" --since "24 hours ago" --no-pager 2>/dev/null \
  | grep -c "^.*Started ${SVC}.service" || echo 0)
oom_kills=$(journalctl --user -u "$SVC" --since "24 hours ago" --no-pager 2>/dev/null \
  | grep -cE "out-of-memory|oom-kill|Result: oom-kill" || echo 0)

# Watermark + chunks
if [ -f "$TMP_DIR/progress.json" ]; then
  completed=$(jq -r '.completed_files // 0' "$TMP_DIR/progress.json" 2>/dev/null)
  sym_chunks=$(jq -r '.symbol_chunks // 0' "$TMP_DIR/progress.json" 2>/dev/null)
  ref_chunks=$(jq -r '.ref_chunks // 0' "$TMP_DIR/progress.json" 2>/dev/null)
  total_files=$(jq -r '[.roots[].n_files] | add' "$TMP_DIR/progress.json" 2>/dev/null)
  pct="-"
  if [ "${total_files:-0}" -gt 0 ]; then
    pct=$(awk -v c="$completed" -v t="$total_files" 'BEGIN{printf "%.1f%%", 100*c/t}')
  fi
else
  completed="-"; sym_chunks="-"; ref_chunks="-"; total_files="-"; pct="-"
fi

tmp_du=$(du -sh "$TMP_DIR" 2>/dev/null | awk '{print $1}')
index_du=$(du -sh "$INDEX_DIR" 2>/dev/null | awk '{print $1}')
mem_now=$(systemctl --user show "$SVC" -p MemoryCurrent --value 2>/dev/null)
mem_h="-"
if [ -n "$mem_now" ] && [ "$mem_now" != "[not set]" ]; then
  mem_h=$(numfmt --to=iec --suffix=B "$mem_now" 2>/dev/null || echo "$mem_now")
fi

last_done=$(grep "^DONE:" "$LOG" 2>/dev/null | tail -1)
done_flag="(none yet)"
if [ -n "$last_done" ]; then done_flag="$last_done"; fi

subject="[scry] $state — watermark $completed/$total_files ($pct), $restarts starts, $oom_kills OOMs"

body=$(cat <<EOF
scry indexing job — hourly status
host: $(hostname)
time: $(date -Is)

systemd unit state: $state
current memory:     $mem_h
restarts (24h):     $restarts
cgroup OOM-kills:   $oom_kills

progress.json watermark: $completed / $total_files files ($pct)
symbol chunks: $sym_chunks
ref chunks:    $ref_chunks
tmp dir size:  $tmp_du
final index:   $index_du
finalize line: $done_flag

------- last 30 lines of log -------
$(tail -30 "$LOG" 2>/dev/null)
EOF
)

msmtp -t <<EOF
To: $TO
From: $FROM
Subject: $subject

$body
EOF
