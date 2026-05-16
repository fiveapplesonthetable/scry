" scry.vim — autoload. Talks to a long-lived `scry serve` over a
" unix socket using vim 8+ channels (no Neovim-specific APIs).
"
" Public functions:
"   scry#def(name)        — :ScryDef wrapper. Loads quickfix.
"   scry#callers(name)
"   scry#ref(name)
"   scry#prefix(prefix)
"   scry#fuzzy(substr)
"   scry#outline()
"   scry#stats()
"   scry#omnifunc(findstart, base) — for :setlocal omnifunc=scry#omnifunc
"   scry#restart()
"   scry#request(cmd, args)        — low-level synchronous JSON-RPC

let s:server_job = v:null
let s:channel    = v:null
let s:next_id    = 1

" ----------------------------------------------------------------
" Process + channel management
" ----------------------------------------------------------------

function! scry#_ensure_server() abort
  if s:server_job isnot v:null && job_status(s:server_job) ==# 'run'
        \ && s:channel isnot v:null && ch_status(s:channel) ==# 'open'
    return
  endif

  " Tear down any half-up state.
  if s:channel isnot v:null && ch_status(s:channel) !=# 'closed'
    call ch_close(s:channel)
  endif
  if s:server_job isnot v:null && job_status(s:server_job) ==# 'run'
    call job_stop(s:server_job)
  endif
  let s:channel = v:null
  let s:server_job = v:null

  if filereadable(g:scry_socket_path) || getftype(g:scry_socket_path) ==# 'socket'
    call delete(g:scry_socket_path)
  endif

  let l:cmd = [g:scry_binary, 'serve',
        \ '--listen', 'unix:' . g:scry_socket_path,
        \ '--max-conns', '4']
  if !empty(g:scry_index_dir)
    let l:cmd += ['--index', g:scry_index_dir]
  endif

  let s:server_job = job_start(l:cmd, {
        \ 'in_io': 'null',
        \ 'out_io': 'null',
        \ 'err_io': 'null',
        \ 'stoponexit': 'term'
        \ })

  " Wait up to 5 s for the socket to appear.
  let l:t0 = reltime()
  while (getftype(g:scry_socket_path) !=# 'socket')
        \ && reltimefloat(reltime(l:t0)) < 5.0
    sleep 30m
  endwhile
  if getftype(g:scry_socket_path) !=# 'socket'
    throw 'scry serve did not bind ' . g:scry_socket_path . ' within 5 s'
  endif

  " Connect. Line-mode (one JSON per line). Vim's `json` mode would
  " parse for us but uses int53; scry's u64 symbol IDs would lose
  " precision, so we keep raw lines and parse with json_decode which
  " preserves bigints as numbers (vim's number type is at least 64-bit
  " on most builds).
  "
  " Vim 9.1 ch_open accepts unix paths via the 'unix:' prefix. The
  " connect attempt is synchronous; we already waited above for the
  " socket file to appear.
  let s:channel = ch_open('unix:' . g:scry_socket_path, {'mode': 'nl'})
  if ch_status(s:channel) !=# 'open'
    throw 'failed to connect to scry socket ' . g:scry_socket_path
  endif
endfunction

" Low-level synchronous request. Returns the 'result' value or
" throws on error / timeout. Multiplexing-safe: each call gets its
" own id, sends, then waits on ch_read until a line with the
" matching id arrives. Out-of-order lines are buffered and replayed
" — though in practice we're synchronous-blocking so out-of-order
" only happens when multiple async callers hit this in sequence.
let s:inbox = {}     " id -> parsed JSON object pulled off the wire ahead of time

function! scry#request(cmd, ...) abort
  call scry#_ensure_server()
  let l:args = a:0 > 0 ? a:1 : {}
  let l:id = s:next_id
  let s:next_id += 1
  let l:req = {'id': l:id, 'cmd': a:cmd}
  if !empty(l:args)
    let l:req['args'] = l:args
  endif
  call ch_sendraw(s:channel, json_encode(l:req) . "\n")

  if has_key(s:inbox, l:id)
    let l:obj = remove(s:inbox, l:id)
  else
    let l:timeout = g:scry_request_timeout_ms
    while v:true
      let l:line = ch_read(s:channel, {'timeout': l:timeout})
      if empty(l:line)
        throw printf('scry request timed out (cmd=%s)', a:cmd)
      endif
      let l:obj = json_decode(l:line)
      if has_key(l:obj, 'id') && l:obj.id == l:id
        break
      endif
      " Out-of-order — stash for whoever is waiting on this id.
      let s:inbox[l:obj.id] = l:obj
    endwhile
  endif

  if has_key(l:obj, 'error')
    throw 'scry: ' . string(l:obj.error)
  endif
  return get(l:obj, 'result', v:null)
endfunction

function! scry#restart() abort
  if s:server_job isnot v:null
    call job_stop(s:server_job)
  endif
  if s:channel isnot v:null && ch_status(s:channel) !=# 'closed'
    call ch_close(s:channel)
  endif
  let s:server_job = v:null
  let s:channel = v:null
  let s:inbox = {}
  call scry#_ensure_server()
  echo '[scry] restarted'
endfunction

" ----------------------------------------------------------------
" Symbol-at-point + language hint
" ----------------------------------------------------------------

function! scry#_word_under_cursor() abort
  let l:w = expand('<cword>')
  return (l:w =~# '^[A-Za-z_][A-Za-z0-9_]*$') ? l:w : ''
endfunction

function! scry#_lang_for_buffer() abort
  if empty(expand('%')) | return '' | endif
  let l:ext = expand('%:e')
  let l:map = {
        \ 'rs': 'Rust', 'go': 'Go', 'py': 'Python',
        \ 'c': 'C', 'cc': 'Cpp', 'cpp': 'Cpp', 'cxx': 'Cpp',
        \ 'h': 'Header', 'hh': 'Header', 'hpp': 'Header', 'hxx': 'Header',
        \ 'java': 'Java', 'kt': 'Kotlin', 'kts': 'Kotlin',
        \ 'ts': 'TypeScript', 'tsx': 'TypeScript',
        \ 'proto': 'Proto', 'sh': 'Bash', 'bash': 'Bash',
        \ 'html': 'Html', 'htm': 'Html',
        \ 'css': 'Css', 'scss': 'Scss',
        \ 'md': 'Markdown', 'toml': 'Toml', 'yaml': 'Yaml', 'yml': 'Yaml',
        \ }
  return get(l:map, l:ext, '')
endfunction

" ----------------------------------------------------------------
" Quickfix integration
" ----------------------------------------------------------------

function! scry#_rows_to_qflist(rows) abort
  let l:out = []
  for l:r in a:rows
    let l:name = get(l:r, 'name', '')
    let l:kind = get(l:r, 'kind', get(l:r, 'ref_kind', '?'))
    let l:lang = get(l:r, 'lang', '?')
    call add(l:out, {
          \ 'filename': get(l:r, 'path', ''),
          \ 'lnum':     get(l:r, 'line', 1),
          \ 'col':      get(l:r, 'col', 1),
          \ 'text':     printf('[%s %s] %s', l:kind, l:lang, l:name),
          \ })
  endfor
  return l:out
endfunction

function! scry#_load_quickfix(rows, label) abort
  let l:qf = scry#_rows_to_qflist(a:rows)
  call setqflist([], ' ', {'title': '[scry] ' . a:label, 'items': l:qf})
  if empty(l:qf)
    echo printf('[scry] no results for %s', a:label)
    return
  endif
  copen
  " Jump to first hit. cfirst would also reopen the qf window noisily.
  cfirst
endfunction

" ----------------------------------------------------------------
" Public commands
" ----------------------------------------------------------------

function! scry#_resolve_name(arg) abort
  if !empty(a:arg) | return a:arg | endif
  let l:w = scry#_word_under_cursor()
  if empty(l:w)
    throw '[scry] no symbol at cursor and no argument given'
  endif
  return l:w
endfunction

function! scry#def(...) abort
  let l:name = scry#_resolve_name(a:0 > 0 ? a:1 : '')
  let l:args = {'name': l:name, 'limit': 25}
  let l:lang = scry#_lang_for_buffer()
  if !empty(l:lang) | let l:args.lang = l:lang | endif
  call scry#_load_quickfix(scry#request('def', l:args), 'def ' . l:name)
endfunction

function! scry#callers(...) abort
  let l:name = scry#_resolve_name(a:0 > 0 ? a:1 : '')
  let l:args = {'name': l:name, 'limit': 200}
  let l:lang = scry#_lang_for_buffer()
  if !empty(l:lang) | let l:args.lang = l:lang | endif
  call scry#_load_quickfix(scry#request('callers', l:args), 'callers ' . l:name)
endfunction

function! scry#ref(...) abort
  let l:name = scry#_resolve_name(a:0 > 0 ? a:1 : '')
  let l:args = {'name': l:name, 'limit': 500}
  call scry#_load_quickfix(scry#request('ref', l:args), 'ref ' . l:name)
endfunction

function! scry#prefix(...) abort
  let l:prefix = scry#_resolve_name(a:0 > 0 ? a:1 : '')
  let l:args = {'prefix': l:prefix, 'limit': 100}
  call scry#_load_quickfix(scry#request('prefix', l:args), 'prefix ' . l:prefix)
endfunction

function! scry#fuzzy(...) abort
  let l:substr = scry#_resolve_name(a:0 > 0 ? a:1 : '')
  let l:args = {'substr': l:substr, 'limit': 50}
  call scry#_load_quickfix(scry#request('fuzzy', l:args), 'fuzzy ' . l:substr)
endfunction

function! scry#outline() abort
  let l:path = expand('%:p')
  if empty(l:path)
    throw '[scry] buffer has no file'
  endif
  let l:r = scry#request('outline', {'path': l:path, 'limit': 1000})
  call scry#_load_quickfix(get(l:r, 'symbols', []), 'outline ' . expand('%:t'))
endfunction

function! scry#stats() abort
  let l:s = scry#request('stats')
  echo printf('[scry] %s · %s files · %s syms · %s refs · indexed_at=%s',
        \ get(l:s, 'scry_version', '?'),
        \ get(l:s, 'files_total', 0),
        \ get(l:s, 'symbols', 0),
        \ get(l:s, 'refs', 0),
        \ get(l:s, 'indexed_at', '?'))
endfunction

" ----------------------------------------------------------------
" Autocompletion: omnifunc and completefunc
" ----------------------------------------------------------------

" Two-call protocol (see :help complete-functions):
"  findstart=1 → return byte offset where the word being completed starts
"  findstart=0 → return list of {word, menu, kind} suggestions
function! scry#omnifunc(findstart, base) abort
  if a:findstart
    let l:line = getline('.')
    let l:start = col('.') - 1
    while l:start > 0 && l:line[l:start - 1] =~# '[A-Za-z0-9_]'
      let l:start -= 1
    endwhile
    return l:start
  endif

  if len(a:base) < g:scry_completion_min_length
    return []
  endif

  let l:rows = []
  try
    let l:rows = scry#request('prefix',
          \ {'prefix': a:base, 'limit': g:scry_max_completions})
  catch
    return []
  endtry

  let l:items = []
  for l:r in l:rows
    call add(l:items, {
          \ 'word': get(l:r, 'name', ''),
          \ 'kind': get(l:r, 'kind', ''),
          \ 'menu': printf('[%s %s] %s',
          \                get(l:r, 'kind', '?'),
          \                get(l:r, 'lang', '?'),
          \                fnamemodify(get(l:r, 'path', ''), ':t')),
          \ 'info': printf('%s:%s', get(l:r, 'path', ''), get(l:r, 'line', '?')),
          \ })
  endfor
  return l:items
endfunction
