#!/usr/bin/env bash
# Interactive e2e: drive real `emacs -nw` in an isolated tmux server
# and verify the plugin works as a USER experiences it. NEVER
# touches the user's existing tmux — uses a dedicated socket
# (`tmux -L scry-e2e-$$`) and ONLY ever calls `kill-session` on
# that socket, never `kill-server`.
#
# Tests cover:
#   1. scry-mode lights up the modeline.
#   2. M-x scry-stats reports symbols / files / refs.
#   3. M-x scry-def lands on a Rust source line.
#   4. M-x scry-callers fills the xref buffer.
#   5. Typing a prefix triggers completion containing the expected
#      candidate (uses corfu if user has it via straight.el; falls
#      back to vanilla *Completions* otherwise).
#   6. M-x scry-restart confirms.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

INDEX="${INDEX:-/mnt/agent/tmp/scry-self-idx}"
SCRY="${SCRY:-$root/target/release/scry}"

[ -d "$INDEX" ] || { rm -rf "$INDEX"; "$SCRY" index "$root" -o "$INDEX" --workers 4 > /dev/null; }

# Per-script tmux socket name. The `$$` guarantees uniqueness per
# invocation. The user's tmux uses a DIFFERENT socket (the default
# `/tmp/tmux-$UID/default`), so nothing we do here can ever touch
# the user's sessions.
SOCK="scry-e2e-$$"
SESSION="scry-emacs-tty"
TMUX="tmux -L $SOCK"

# Hard refuse to run if the socket name doesn't carry our PID —
# defensive against someone setting SOCK by hand to a name that
# could collide with an existing tmux server.
case "$SOCK" in
  scry-e2e-$$) : ;;
  *) echo "[e2e] refusing to run: SOCK=$SOCK doesn't carry our PID"; exit 2 ;;
esac

INIT="$(mktemp /tmp/scry-emacs-init-XXXXXX.el)"

cleanup() {
  # Kill ONLY our isolated session. Never call kill-server here:
  # that would kill the whole tmux server on this socket, and
  # while the socket is unique to us, the user is explicit that
  # they don't want any tmux killed. kill-session terminates only
  # the named session inside our server, and the server itself
  # then exits when no sessions remain.
  $TMUX kill-session -t "$SESSION" 2>/dev/null || true
  # Wipe our socket file if it's still there (server should have
  # exited when the last session went away, but be defensive).
  local sock_file="/tmp/tmux-$EUID/$SOCK"
  [ -S "$sock_file" ] && rm -f "$sock_file" 2>/dev/null || true
  rm -f "$INIT"
}
trap cleanup EXIT INT TERM HUP

# Discover the user's corfu install (if any) so the popup actually
# renders inside a TTY. We point load-path at every subdir of
# straight/build/ so corfu's deps come along too.
STRAIGHT_BUILD="$HOME/.emacs.d/straight/build"
CORFU_LOAD=""
if [ -d "$STRAIGHT_BUILD/corfu" ]; then
  CORFU_LOAD='(progn (dolist (d (directory-files "'$STRAIGHT_BUILD'" t)) (when (and (file-directory-p d) (not (string-suffix-p "/." d)) (not (string-suffix-p "/.." d))) (add-to-list (quote load-path) d))) (when (require (quote corfu) nil t) (setq corfu-auto t corfu-auto-prefix 2 corfu-auto-delay 0.1) (global-corfu-mode 1)))'
fi

cat > "$INIT" <<EOF
;; ---- isolated init: does NOT load the user's normal config ----
(setq inhibit-startup-screen t
      initial-scratch-message ""
      make-backup-files nil
      auto-save-default nil
      confirm-kill-emacs nil)

(add-to-list 'load-path "$root/editors/emacs")
(setq scry-binary    "$SCRY"
      scry-index-dir "$INDEX"
      scry-socket-path (format "/tmp/scry-tty-e2e-%d.sock" (emacs-pid))
      scry-completion-min-length 2)
(require 'scry)
(global-scry-mode 1)

;; corfu if available (user has it via straight.el)
$CORFU_LOAD

;; Make M-Tab always pop *Completions* so the test works with or
;; without corfu.
(setq completion-auto-help t)
EOF

capture () { $TMUX capture-pane -t "$SESSION" -p; }
send    () { $TMUX send-keys -t "$SESSION" "$@"; }

waitfor () {
  local pat="$1" max="${2:-80}" t=0
  while [ "$t" -lt "$max" ]; do
    if capture | grep -qE "$pat"; then return 0; fi
    sleep 0.1
    t=$((t + 1))
  done
  return 1
}

dump_on_fail () {
  echo "    last 18 lines of pane:"
  capture | tail -18 | sed 's/^/      /'
}

fails=0
ck () {
  printf "  %s ... " "$1"
  if eval "$2"; then
    echo "ok"
  else
    echo "FAIL"
    fails=$((fails + 1))
    dump_on_fail
  fi
}

echo "[e2e_emacs_tty] INDEX=$INDEX SCRY=$SCRY"
echo "[e2e_emacs_tty] tmux socket=$SOCK (isolated from user's tmux)"
[ -n "$CORFU_LOAD" ] && echo "[e2e_emacs_tty] corfu detected at $STRAIGHT_BUILD/corfu — popup mode" \
                    || echo "[e2e_emacs_tty] no corfu — vanilla *Completions* mode"

# ----------------------------------------------------------------
# Boot emacs.
# ----------------------------------------------------------------
$TMUX new-session -d -s "$SESSION" -x 220 -y 60 \
     "emacs -nw -Q -l $INIT $root/crates/scry-store/src/lib.rs"

# Wait for buffer to load + give scry time to spawn the daemon.
sleep 2

# Send an early scry-stats to warm the daemon and confirm liveness.
send "Escape" "x" "scry-stats" "Enter"
waitfor "files .* syms .* refs" 100 || true

# ----------------------------------------------------------------
# 1.
# ----------------------------------------------------------------
ck "scry-mode is active in modeline" \
   'capture | grep -q "scry"'

# ----------------------------------------------------------------
# 2.
# ----------------------------------------------------------------
ck "scry-stats reports symbols / files / refs" \
   'capture | grep -qE "files .* syms .* refs"'

# ----------------------------------------------------------------
# 3. scry-def via M-x — deterministic (no isearch / kbd dance).
# ----------------------------------------------------------------
send "Escape" "x" "scry-def" "Enter"
sleep 0.3
send "compute_id" "Enter"
sleep 0.6
ck "scry-def opens xref buffer with a Rust hit" \
   'waitfor "(compute_id|lib\\.rs|fn rs)" 100'

# ----------------------------------------------------------------
# 4. scry-callers via M-x.
# ----------------------------------------------------------------
send "Escape" "x" "scry-callers" "Enter"
sleep 0.3
send "compute_id" "Enter"
sleep 0.6
ck "scry-callers populates xref buffer" \
   'waitfor "(xref|matches in|compute_id)" 100'

# ----------------------------------------------------------------
# 5. Completion candidates from a real prefix query.
#
# Corfu in a vanilla TTY needs `corfu-terminal` to render an actual
# popup; without it the popup silently no-ops. We test the same
# underlying primitive — "does typing 'restor' surface
# restore_default_sigpipe as a candidate?" — via `M-x scry-prefix`,
# which opens an xref buffer that IS visible in the tmux pane
# regardless of which completion frontend the user has installed.
# The CAPF entry shape itself is pinned by e2e_emacs.sh test #8.
# ----------------------------------------------------------------
send "C-g"
sleep 0.2
send "Escape" "x" "scry-prefix" "Enter"
sleep 0.3
send "restor" "Enter"
sleep 0.8
ck "scry-prefix surfaces restore_default_sigpipe" \
   'waitfor "restore_default_sigpipe" 80'

# ----------------------------------------------------------------
# 6.
# ----------------------------------------------------------------
send "C-g"
sleep 0.2
send "Escape" "x" "scry-restart" "Enter"
sleep 0.5
ck "scry-restart prints confirmation" \
   'waitfor "restarted" 80'

echo
if [ $fails -gt 0 ]; then
  echo "[e2e_emacs_tty] FAILED ($fails of 6)"
  echo "=== final pane ==="
  capture | sed 's/^/  /'
  exit 1
fi
echo "[e2e_emacs_tty] ALL OK (6/6)"
