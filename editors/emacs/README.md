scry.el — Emacs binding for scry
================================

Brings scry's sub-10 ms autocomplete, jump-to-definition, and
find-references into Emacs through the standard `completion-at-point`
and `xref` interfaces. No new keybindings to learn — `M-.` jumps,
`M-?` lists references, and your normal completion UI (corfu,
company, vanilla `completion-at-point`) gets scry-backed candidates.

Requires Emacs 29.1+ and a working `scry` binary on PATH (or set
`scry-binary` to the absolute path).

Install
-------

### From this checkout

```elisp
(add-to-list 'load-path "/path/to/scry/editors/emacs")
(require 'scry)
(setq scry-index-dir "/mnt/agent/scry-index")   ;; or wherever your index lives
(add-hook 'prog-mode-hook #'scry-mode)
```

### One-liner via `use-package`

```elisp
(use-package scry
  :load-path "/path/to/scry/editors/emacs"
  :custom (scry-index-dir "/mnt/agent/scry-index")
  :hook   (prog-mode . scry-mode))
```

### Globally

```elisp
(require 'scry)
(setq scry-index-dir "/mnt/agent/scry-index")
(global-scry-mode 1)        ;; turns scry-mode on for every prog-mode buffer
```

Building (or pointing at) an index
----------------------------------

If you don't already have a scry index, build one over the tree
you want to search:

```sh
scry index /path/to/repo -o /mnt/agent/scry-index --workers 8
```

Subsequent incremental refreshes (after editing files):

```sh
scry index --incremental /path/to/repo -o /mnt/agent/scry-index
```

You can index AOSP-scale corpora (1M+ files) in ~13 min on a 72-core
host; small repos finish in well under a second. See
`docs/OPERATIONS.md` in the main repo for the production setup.

What you get
------------

| binding / call            | scry call                | what it does                                  |
|---------------------------|--------------------------|-----------------------------------------------|
| `completion-at-point`     | `prefix`                 | autocomplete on the identifier at point       |
| `M-.` (`xref-find-definitions`) | `def`              | jump to definition (or buffer of choices)     |
| `M-?` (`xref-find-references`) | `callers`           | list call sites                               |
| `C-M-.` (`xref-find-apropos`) | `fuzzy`               | fuzzy/substring symbol search                 |
| `M-x scry-def`            | `def`                    | same as `M-.` but prompts                     |
| `M-x scry-callers`        | `callers`                | same as `M-?` but prompts                     |
| `M-x scry-ref`            | `ref`                    | every reference (types, fields, calls)        |
| `M-x scry-outline`        | `outline`                | list all symbols in current file              |
| `M-x scry-prefix`         | `prefix`                 | prefix search (shows full hits, not just names)|
| `M-x scry-stats`          | `stats`                  | one-line daemon health                        |
| `M-x scry-restart`        | (kills daemon)           | use after a re-index to drop cached mmap     |

`completion-at-point` is added as a non-exclusive provider, so
language-specific completers (lsp-mode, eglot, dabbrev, ...) keep
working alongside scry; scry just adds candidates from across the
whole index.

### Popup frontends (recommended)

Vanilla CAPF pops `*Completions*` in a window split — usable but
plain. Install one of these for an inline popup with kind icons,
filename annotations, and snippet docs:

| frontend                                                | works in GUI Emacs | works in `emacs -nw` | scry hooks used |
|---------------------------------------------------------|:------------------:|:--------------------:|-----------------|
| [`corfu`](https://github.com/minad/corfu)               | yes                | no (needs `corfu-terminal`) | `:annotation-function`, `:company-kind`, `:company-doc-buffer` |
| `corfu` + [`corfu-terminal`](https://codeberg.org/akib/emacs-corfu-terminal) | yes                | **yes**              | same             |
| [`company`](https://github.com/company-mode/company-mode) | yes                | yes (native)         | `:company-kind`, `:company-location`, `:company-doc-buffer` |

Minimal corfu setup that gives you the popup everywhere (GUI + TTY):

```elisp
(use-package corfu
  :init (global-corfu-mode 1)
  :custom (corfu-auto t) (corfu-auto-prefix 2) (corfu-auto-delay 0.05))

;; Inline popup inside `emacs -nw`:
(use-package corfu-terminal
  :unless (display-graphic-p)
  :after corfu
  :config (corfu-terminal-mode 1))
```

scry.el's CAPF already exports every property corfu / company
recognize (kind icons, annotation chips with `[kind lang] file`,
a doc buffer with the symbol's FQN + scope + location). Both
frontends pick this up automatically — no per-package config.

For company users:

```elisp
(use-package company
  :init (global-company-mode 1)
  :custom (company-minimum-prefix-length 2)
          (company-idle-delay 0.05))
```

Configuration
-------------

| variable                       | default                                | meaning                                                  |
|--------------------------------|----------------------------------------|----------------------------------------------------------|
| `scry-binary`                  | `"scry"`                               | binary name or absolute path                             |
| `scry-index-dir`               | `nil`                                  | nil = scry's own default; set per project                |
| `scry-socket-path`             | `/tmp/scry-emacs-${UID}.sock`          | unique per user                                          |
| `scry-max-completions`         | `50`                                   | `prefix` `--limit`                                       |
| `scry-completion-min-length`   | `2`                                    | don't query for prefixes shorter than this               |
| `scry-request-timeout`         | `5.0`                                  | per-request seconds                                      |

How it works
------------

`scry-mode` does two things to your buffer: adds a `xref-backend`
of kind `scry` and a `completion-at-point` provider. The first
time either fires, scry.el spawns `scry serve` and connects to its
unix socket; that connection is reused for every later request in
this Emacs session.

When the daemon dies (OOM, manual kill), the next request will
respawn it transparently — the previous in-flight request will
return an error, anything after that succeeds.

Notes
-----

- **Bignum-safe JSON**: scry returns 64-bit symbol IDs that
  overflow `json-parse-string`. scry.el uses the pure-Lisp
  `json-read-from-string` instead, which hands those back as
  Emacs bignums.
- **Performance**: `prefix` and `def` are sub-5 ms warm on the
  AOSP+Linux corpus (~1 M files). The Emacs side adds maybe
  1 ms of `accept-process-output` polling. Total round-trip
  inside `completion-at-point` is well under one frame at 60 Hz.
- **`scry-mode` lighter**: " scry" in the modeline once enabled.
- **Multiple repos**: `scry-index-dir` is buffer-local-friendly;
  set it via `.dir-locals.el` to give per-project indexes.

Troubleshooting
---------------

- `[scry] scry serve: ...` in the message area: the daemon is
  telling you why it died. Open `*scry-serve*` for the full stderr.
- `scry request timed out`: the daemon hung. `M-x scry-restart`.
- `bad JSON: ...`: a protocol skew or a binary mismatch. Confirm
  `scry --version` matches your scry.el (any v0.1.6+ works).

Headless verification
---------------------

```sh
cd /path/to/scry
./editors/tests/e2e_emacs.sh
```

Spawns Emacs `--batch`, loads scry.el, runs the daemon, exercises
every public function (stats, prefix, def, callers, outline,
fuzzy, xref backend integration, CAPF tuple shape). Exits 0 when
all 8 assertions pass.
