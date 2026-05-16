scry.vim — Vim binding for scry
===============================

Vim 8+ / Neovim plugin that brings scry's autocomplete +
jump-to-definition + find-references to Vim through `omnifunc`
and quickfix. Async via vim 8 channels; one long-lived
`scry serve` per Vim session.

Requires Vim 9+ (for `ch_open` on unix sockets) and a working
`scry` binary on PATH (or set `g:scry_binary`).

Install
-------

### vim-plug

```vim
Plug 'fiveapplesonthetable/scry', {'rtp': 'editors/vim'}
```

### packer.nvim

```lua
use {'fiveapplesonthetable/scry', rtp = 'editors/vim'}
```

### manual

```sh
cp -r /path/to/scry/editors/vim/* ~/.vim/
```

(or in Neovim, `~/.config/nvim/`).

### Configure

Add to `~/.vimrc` (or `~/.config/nvim/init.vim`):

```vim
let g:scry_binary    = 'scry'                          " or absolute path
let g:scry_index_dir = '/mnt/agent/scry-index'         " or your project's index

" Recommended keymaps (drop if you have something better).
nnoremap <silent> <C-]>  :ScryDef<CR>
nnoremap <silent> g]     :ScryCallers<CR>
nnoremap <silent> gr     :ScryRef<CR>

" Enable omnifunc-completion in any prog-buffer.
autocmd FileType c,cpp,rust,go,python,java,kotlin,typescript,proto
      \ setlocal omnifunc=scry#omnifunc
```

Trigger completion with `<C-x><C-o>` (vanilla omni) or wire it
into your popup-completion plugin of choice
([asyncomplete](https://github.com/prabirshrestha/asyncomplete.vim),
[ncm2](https://github.com/ncm2/ncm2), nvim-cmp, etc.).

Building (or pointing at) an index
----------------------------------

```sh
scry index /path/to/repo -o /mnt/agent/scry-index --workers 8
```

Subsequent incremental refreshes after edits:

```sh
scry index --incremental /path/to/repo -o /mnt/agent/scry-index
```

What you get
------------

| command           | scry call   | what it does                                                                |
|-------------------|-------------|-----------------------------------------------------------------------------|
| `:ScryDef [name]` | `def`       | definitions → quickfix → first hit. Default: word under cursor.            |
| `:ScryCallers`    | `callers`   | call sites → quickfix.                                                      |
| `:ScryRef`        | `ref`       | every reference → quickfix.                                                 |
| `:ScryPrefix`     | `prefix`    | name-prefix matches → quickfix. Useful for "I know it starts with X".      |
| `:ScryFuzzy`      | `fuzzy`     | substring/Levenshtein search → quickfix.                                    |
| `:ScryOutline`    | `outline`   | this file's symbols → quickfix.                                             |
| `:ScryStats`      | `stats`     | one-line daemon health in the command area.                                 |
| `:ScryRestart`    | (kill+spawn)| use after a re-index to drop the cached mmap.                              |
| `scry#omnifunc`   | `prefix`    | hook into `omnifunc` for in-line completion.                                |

Quickfix navigation: `:cnext` / `:cprev` / `:cfirst` / `:clast`
(or `<C-n>`/`<C-p>` if you've remapped them). `:copen` shows the
list; the first result is jumped to automatically.

Configuration
-------------

| variable                          | default                                  | meaning                                  |
|-----------------------------------|------------------------------------------|------------------------------------------|
| `g:scry_binary`                   | `'scry'`                                 | binary name or absolute path             |
| `g:scry_index_dir`                | `''`                                     | empty = scry's own default               |
| `g:scry_socket_path`              | `/tmp/scry-vim-${pid}.sock`              | unique per Vim instance                  |
| `g:scry_max_completions`          | `50`                                     | `prefix` `--limit`                       |
| `g:scry_completion_min_length`    | `2`                                      | don't fire for shorter prefixes          |
| `g:scry_request_timeout_ms`       | `5000`                                   | per-request ms                           |

Notes
-----

- **One daemon per Vim session**: spawned on the first scry call
  and reused. Survives until Vim exits or you call
  `:ScryRestart`.
- **Channel mode**: line-mode (`nl`) JSON-RPC. We do our own
  `json_decode` so 64-bit symbol IDs survive (Vim's `mode: json`
  would lose them to its int53 representation).
- **Quickfix**: every command populates the quickfix list with
  `[kind lang] name` text and the file path / line / col, so the
  built-in `:cnext` flow Just Works.

Troubleshooting
---------------

- `scry serve did not bind ... within 5 s`: the binary failed to
  start. Try `:ScryStats` to see the daemon's stderr in messages.
- `scry request timed out`: `:ScryRestart`.
- Nothing happens on `<C-x><C-o>`: confirm `omnifunc` is set —
  `:setlocal omnifunc?` should show `scry#omnifunc`.

Headless verification
---------------------

```sh
cd /path/to/scry
./editors/tests/e2e_vim.sh
```

Spawns Vim in non-interactive mode, runs every public call
against a real index, asserts on the shape. Exits 0 when all 8
assertions pass.
