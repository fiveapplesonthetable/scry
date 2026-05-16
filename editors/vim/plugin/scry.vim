" scry.vim — bootstrap. The actual logic lives in autoload/scry.vim
" so we don't pay for loading it until a scry command fires.
"
" Set g:scry_binary, g:scry_index_dir, g:scry_socket_path before this
" loads if you need non-default values; see :help scry (or the README).

if exists('g:loaded_scry')
  finish
endif
let g:loaded_scry = 1

if !exists('g:scry_binary')      | let g:scry_binary = 'scry'                 | endif
if !exists('g:scry_index_dir')   | let g:scry_index_dir = ''                  | endif
if !exists('g:scry_socket_path')
  let g:scry_socket_path = printf('/tmp/scry-vim-%d.sock', getpid())
endif
if !exists('g:scry_max_completions')        | let g:scry_max_completions = 50  | endif
if !exists('g:scry_completion_min_length')  | let g:scry_completion_min_length = 2 | endif
if !exists('g:scry_request_timeout_ms')     | let g:scry_request_timeout_ms = 5000 | endif

command! -nargs=? ScryDef     call scry#def(<q-args>)
command! -nargs=? ScryCallers call scry#callers(<q-args>)
command! -nargs=? ScryRef     call scry#ref(<q-args>)
command! -nargs=? ScryPrefix  call scry#prefix(<q-args>)
command! -nargs=? ScryFuzzy   call scry#fuzzy(<q-args>)
command!          ScryOutline call scry#outline()
command!          ScryStats   call scry#stats()
command!          ScryRestart call scry#restart()

" Default DWIM mappings. Users who want gtags-style key real estate
" can drop these in their vimrc:
"
"   nnoremap <buffer> <C-]>  :ScryDef<CR>
"   nnoremap <buffer> <C-^>  :ScryCallers<CR>
"
" We don't override anything by default to avoid stomping muscle memory.

" Enable omnifunc completion in any buffer the user opts in. To make
" it global: `set omnifunc=scry#omnifunc` (or in a filetype hook).
" Otherwise users call `:setlocal omnifunc=scry#omnifunc` per buffer.
