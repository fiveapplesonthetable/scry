#!/usr/bin/env bash
# Headless e2e for editors/vim/. Runs vim in non-interactive mode,
# drives the plugin against a real index, asserts results land in
# quickfix or come back from scry#request().

set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

INDEX="${INDEX:-/mnt/agent/tmp/scry-self-idx}"
SCRY="${SCRY:-$root/target/release/scry}"

if [ ! -d "$INDEX" ]; then
    echo "[e2e_vim] building index of scry's own repo at $INDEX"
    rm -rf "$INDEX"
    "$SCRY" index "$root" -o "$INDEX" --workers 4 > /dev/null
fi

echo "[e2e_vim] using INDEX=$INDEX SCRY=$SCRY"

# vim's :echo to stdout requires -es (silent-ex) and explicit
# `:redir`. Build a vimscript that runs every assertion, captures
# results into a file, then exits with 0 or 1.
RESULT_FILE="$(mktemp /tmp/scry-vim-e2e-XXXXXX.txt)"
trap "rm -f $RESULT_FILE" EXIT

cat > /tmp/scry-vim-driver-$$.vim <<EOF
set nocompatible
set runtimepath^=$root/editors/vim
let g:scry_binary = '$SCRY'
let g:scry_index_dir = '$INDEX'
let g:scry_socket_path = '/tmp/scry-e2e-vim-' . getpid() . '.sock'
" -nu means plugins won't autoload; source manually.
runtime! plugin/scry.vim

let s:fails = []

function! Ck(label, expr_fn, pred_fn) abort
  try
    let l:r = call(a:expr_fn, [])
    if !call(a:pred_fn, [l:r])
      call add(s:fails, printf('%s: got %s', a:label, string(l:r)[:200]))
      call writefile([a:label . ' ... FAIL'], '$RESULT_FILE', 'a')
    else
      call writefile([a:label . ' ... ok'], '$RESULT_FILE', 'a')
    endif
  catch
    call add(s:fails, printf('%s: %s', a:label, v:exception))
    call writefile([a:label . ' ... ERR: ' . v:exception], '$RESULT_FILE', 'a')
  endtry
endfunction

call Ck('stats',
      \ {-> scry#request('stats')},
      \ {r -> type(r) == v:t_dict && get(r, 'symbols', 0) > 0})

call Ck('prefix returns rows',
      \ {-> scry#request('prefix', {'prefix': 'restore', 'limit': 5})},
      \ {r -> type(r) == v:t_list && len(r) > 0
      \       && get(r[0], 'name', '') =~? '^restore'})

call Ck('def lands on path:line',
      \ {-> scry#request('def', {'name': 'compute_id', 'limit': 3})},
      \ {r -> type(r) == v:t_list && len(r) > 0
      \       && !empty(get(r[0], 'path', ''))
      \       && get(r[0], 'line', 0) > 0})

call Ck('callers returns RefRecords',
      \ {-> scry#request('callers', {'name': 'compute_id', 'limit': 5})},
      \ {r -> type(r) == v:t_list && len(r) > 0
      \       && !empty(get(r[0], 'ref_kind', ''))})

call Ck('outline lib.rs > 10 syms',
      \ {-> scry#request('outline', {'path': '$root/crates/scry-store/src/lib.rs', 'limit': 200})},
      \ {r -> type(r) == v:t_dict && len(get(r, 'symbols', [])) > 10})

call Ck('fuzzy finds sigpipe',
      \ {-> scry#request('fuzzy', {'substr': 'sigpipe', 'limit': 3})},
      \ {r -> type(r) == v:t_list && len(r) > 0})

call Ck('omnifunc findstart finds word boundary',
      \ {-> call('scry#omnifunc', [1, ''])},
      \ {r -> r >= 0})

call Ck('omnifunc returns candidates',
      \ {-> call('scry#omnifunc', [0, 'restore'])},
      \ {r -> type(r) == v:t_list && len(r) > 0
      \       && !empty(get(r[0], 'word', ''))})

if !empty(s:fails)
  call writefile([''], '$RESULT_FILE', 'a')
  call writefile(['=== FAILURES ==='], '$RESULT_FILE', 'a')
  for f in s:fails
    call writefile(['  ' . f], '$RESULT_FILE', 'a')
  endfor
  cquit
endif
qall
EOF

vim --not-a-term -nu /tmp/scry-vim-driver-$$.vim 2>/dev/null
EXIT=$?
rm -f /tmp/scry-vim-driver-$$.vim

cat "$RESULT_FILE"

exit $EXIT
