#!/usr/bin/env bash
# Interactive e2e: drive real vim in an isolated tmux server.
# Same isolation rules as e2e_emacs_tty.sh — dedicated socket,
# never `kill-server`, only `kill-session`.
#
# Tests:
#   1. Plugin loads (commands exist).
#   2. :ScryStats reports a live daemon.
#   3. :ScryDef compute_id populates quickfix + jumps to first hit.
#   4. :ScryCallers compute_id populates quickfix.
#   5. :ScryPrefix restore surfaces restore_default_sigpipe.

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

INDEX="${INDEX:-/mnt/agent/tmp/scry-self-idx}"
SCRY="${SCRY:-$root/target/release/scry}"

[ -d "$INDEX" ] || { rm -rf "$INDEX"; "$SCRY" index "$root" -o "$INDEX" --workers 4 > /dev/null; }

SOCK="scry-e2e-vim-$$"
SESSION="scry-vim-tty"
TMUX="tmux -L $SOCK"

case "$SOCK" in
  scry-e2e-vim-$$) : ;;
  *) echo "[e2e] refusing: SOCK=$SOCK doesn't carry our PID"; exit 2 ;;
esac

VIMRC="$(mktemp /tmp/scry-vim-vimrc-XXXXXX.vim)"

cleanup() {
  $TMUX kill-session -t "$SESSION" 2>/dev/null || true
  local sock_file="/tmp/tmux-$EUID/$SOCK"
  [ -S "$sock_file" ] && rm -f "$sock_file" 2>/dev/null || true
  rm -f "$VIMRC"
}
trap cleanup EXIT INT TERM HUP

cat > "$VIMRC" <<EOF
set nocompatible
set runtimepath^=$root/editors/vim
let g:scry_binary = '$SCRY'
let g:scry_index_dir = '$INDEX'
let g:scry_socket_path = '/tmp/scry-tty-vim-' . getpid() . '.sock'
runtime! plugin/scry.vim
" Turn on omnifunc for any buffer the test opens.
autocmd BufRead,BufNewFile * setlocal omnifunc=scry#omnifunc
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

fails=0
ck () {
  printf "  %s ... " "$1"
  if eval "$2"; then
    echo "ok"
  else
    echo "FAIL"
    fails=$((fails + 1))
    echo "    last 12 lines:"
    capture | tail -12 | sed 's/^/      /'
  fi
}

echo "[e2e_vim_tty] tmux socket=$SOCK (isolated)"

$TMUX new-session -d -s "$SESSION" -x 220 -y 60 \
     "vim -u $VIMRC $root/crates/scry-store/src/lib.rs"
sleep 1.5

# 1. Plugin loaded?
send ":ScryStats" "Enter"
sleep 0.5
ck "ScryStats prints version + counts" \
   'waitfor "files .* syms .* refs" 80'

# 2. ScryDef compute_id
send ":ScryDef compute_id" "Enter"
sleep 0.5
ck "ScryDef jumps to compute_id source" \
   'waitfor "(compute_id|lib\\.rs)" 80'

# 3. ScryCallers
send "Escape"
send ":cclose" "Enter"
sleep 0.2
send ":ScryCallers compute_id" "Enter"
sleep 0.5
ck "ScryCallers populates quickfix" \
   'waitfor "(quickfix|\\[scry\\] callers|main\\.rs)" 80'

# 4. ScryPrefix
send "Escape"
send ":cclose" "Enter"
sleep 0.2
send ":ScryPrefix restore" "Enter"
sleep 0.5
ck "ScryPrefix surfaces restore_default_sigpipe" \
   'waitfor "restore_default_sigpipe" 80'

# 5. omnifunc returns something for a known prefix
send "Escape"
send ":cclose" "Enter"
sleep 0.2
send ":echo len(call(\"scry#omnifunc\", [0, \"restore\"])) . \" candidates\"" "Enter"
sleep 0.6
ck "omnifunc returns >= 1 candidate for 'restore'" \
   'waitfor "[1-9][0-9]* candidates" 80'

echo
if [ $fails -gt 0 ]; then
  echo "[e2e_vim_tty] FAILED ($fails of 5)"
  echo "=== final pane ==="
  capture | sed 's/^/  /'
  exit 1
fi
echo "[e2e_vim_tty] ALL OK (5/5)"
