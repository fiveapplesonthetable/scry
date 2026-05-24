# scry

A single static Rust binary that indexes ~1 M source files of AOSP +
Linux in ~13 minutes, then answers symbol, reference, and content
queries at interactive latency. On the live AOSP+Linux index it runs
**30–45× faster than `rg`** for the same patterns and **>700× faster
than POSIX `grep -rF`** — bench numbers in [`docs/BENCHMARKS.md`].

Coverage:

- **Source**: C, C++, Java, Kotlin, Rust, Go, Python, shell, assembly
  (tree-sitter).
- **Build**: Soong (`Android.bp`), `Android.mk`, Bazel `BUILD`, `*.bzl`,
  `CMake`, `GN`, `Kconfig`, `Makefile`, `Gradle`.
- **Android platform**: aconfig flags, `init.rc` services, SELinux
  `.te` policy, `AndroidManifest.xml` components, `api/*.txt` SDK
  surface.
- **IPC**: AIDL (interface + method + parcelable), HIDL.
- **Ownership**: OWNERS.

Driven by humans at a terminal **and** LLM agents over JSON-RPC.
Every CLI query has a `--json` variant; `scry serve` reads
newline-delimited JSON-RPC on stdin and replies on stdout. Every
invocation prints a stats footer and appends one JSON line to
`~/.scry/queries.log` so past sessions are auditable.

## Install

### Prebuilt binary (Linux x86_64)

```sh
curl -L https://github.com/fiveapplesonthetable/scry/releases/latest/download/scry-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz
sudo install scry /usr/local/bin/scry
scry --version
```

### From source

Requires stable Rust 1.79+; `cargo build --release` produces a static
binary at `target/release/scry`. See [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)
for the full prerequisites + first-time-setup walkthrough.

```sh
cargo install --git https://github.com/fiveapplesonthetable/scry --bin scry
# or, for active development:
git clone https://github.com/fiveapplesonthetable/scry && cd scry
cargo build --release
./target/release/scry --help
```

### Shell completions + man page

```sh
scry completions bash > /etc/bash_completion.d/scry         # or zsh / fish / powershell / elvish
scry man | gzip > /usr/local/share/man/man1/scry.1.gz
```

## Quickstart

scry is **tree-sitter only**: one `index` command builds the index, then you
query it. (For Kythe-grade *semantic* precision on the AOSP C++/Java slice —
compiler-resolved def/ref/callers — use the companion tool `scry2`; scry is
the broad, fast, lexical layer over every language + build/platform format.)

```sh
# 1. Index the source tree.
scry index ~/dev/myproject -o ./idx

# 2. Query.
scry def ActivityManagerService --kind class --index ./idx
scry callers transact --lang Java --limit 10 --index ./idx
scry ref Foo --index ./idx
scry grep ZygoteInit --index ./idx                   # 580 ms; rg: 21.2 s (36×)
```

### Cross-cutting filters (work on `ref`, `callers`, `callgraph`, `impact`)

```sh
scry subclasses Activity --in frameworks/base/                  # type hierarchy
scry impact bindService                                          # callers + subclasses + files
scry callgraph bindService --depth 3                             # recursive caller tree
scry callers bindService --reachable                             # build-graph pruned (module_graph.json)
scry callers close --format by-def --limit 10                    # histogram by callee def
```

Times above are warm-cache P50 on the live AOSP + Linux index
(1,009,166 files, 70.4 GB source). The `rg` comparison is `rg -j4
ZygoteInit ~/dev/aosp` against the same tree.

Indexing the whole corpus from scratch is **13.3 minutes** wall on
a 72-core host at `--workers 16`; ripgrep doesn't have an
indexing phase, so it pays the full ~20 s walk cost on every
query forever. Full per-pattern bench table and reproducibility
recipe in [`docs/BENCHMARKS.md`].

After the first full index, edits are picked up by
`scry index --incremental` — it diffs the source tree against the
stored content digests, reparses only changed + added files,
replays unchanged records, and atomically swaps the new index into
place. The old index stays queryable for the entire rebuild. Sub-
second on small change sets; the editor-loop refresh path.

Full command reference, JSON-RPC schema table, and exhaustive output
examples from the live AOSP index: [`docs/USAGE.md`].

Production indexing recipe (systemd unit + cgroup memory cap +
auto-resume after OOM): [`docs/OPERATIONS.md`].

## Docs

| doc                          | what                                                                       |
|------------------------------|----------------------------------------------------------------------------|
| [`docs/USAGE.md`]            | every command, every flag, real output snippets, the LLM-agent comparison  |
| [`docs/BENCHMARKS.md`]       | scry vs `rg` vs `grep` numbers, index-time scaling, perf-stat decomposition, reproducibility recipe |
| [`docs/DESIGN.md`]           | system design including the 8-layer cgroup envelope keeping the indexer inside 60 GiB |
| [`docs/THEORY.md`]           | from-scratch course on the CS behind scry — Rust, EM model, page cache, FST, trigram, work-stealing |
| [`docs/FAST_PATH.md`]        | Russ Cox-style trigram pre-filter + lazy/mmap reader design                |
| [`docs/OPERATIONS.md`]       | production knobs, the systemd recipe, troubleshooting                      |
| [`docs/DEVELOPMENT.md`]      | workspace layout, how to test/bench/profile, known coverage gaps, contributing |
| [`docs/AGENT_NOTES.md`]      | LLM-agent perspective — token economy, accuracy, setup for small models       |
| [`docs/ROADMAP.md`]          | concrete design sketches for the multi-day items ahead (in-place incremental writer) plus a measured-and-rejected io_uring write-up |

[`docs/USAGE.md`]: docs/USAGE.md
[`docs/BENCHMARKS.md`]: docs/BENCHMARKS.md
[`docs/DESIGN.md`]: docs/DESIGN.md
[`docs/THEORY.md`]: docs/THEORY.md
[`docs/FAST_PATH.md`]: docs/FAST_PATH.md
[`docs/OPERATIONS.md`]: docs/OPERATIONS.md
[`docs/DEVELOPMENT.md`]: docs/DEVELOPMENT.md
[`docs/AGENT_NOTES.md`]: docs/AGENT_NOTES.md
[`docs/ROADMAP.md`]: docs/ROADMAP.md

## One-paragraph architecture

A parallel `ignore`-crate walker classifies files into 40 categories,
then rayon pumps each file through either a tree-sitter parser (for
source languages, with a per-file 60 s parse budget enforced via the
`parse_with_options` progress callback) or a small custom parser (for
the AOSP-specific formats). Definitions and references are collected
with scope paths and blake3-hashed for stable ids. A streaming finalize
pass builds the per-record byte-offset sidecars, the FSTs over symbol
and ref names, the file → symbol-ids reverse index, and the trigram FST
for grep — all written to `.tmp/` and atomically renamed into place.
The `StoreReader`
`mmap`s every sidecar; queries decode one record at a time without
loading the full 10 GB columnar payload.

## Editor bindings

First-class plugins for Emacs, Vim, and VS Code live in
[`editors/`](editors/). Each spawns one long-lived `scry serve`
per editor session and hooks into the editor's standard
autocomplete / jump-to-def / find-references APIs:

| editor   | install                                               | autocomplete  | jump-to-def | find-refs | outline |
|----------|-------------------------------------------------------|---------------|-------------|-----------|---------|
| Emacs    | [editors/emacs/README.md](editors/emacs/README.md)    | CAPF          | M-./xref    | M-?/xref  | yes     |
| Vim      | [editors/vim/README.md](editors/vim/README.md)        | omnifunc      | :ScryDef    | yes       | yes     |
| VS Code  | [editors/vscode/README.md](editors/vscode/README.md)  | provider API  | F12         | Shift+F12 | yes     |

All three pass an end-to-end suite (`editors/tests/run_all.sh`)
that exercises every primitive against a real index. Sub-10 ms
warm-cache round-trips inside the editor are the design point —
fast enough for keystroke-driven completion.

## Also works on

The language set above is broad enough to cover other
non-Android tree-of-source-files projects out of the box. Indexed
in this release with zero per-corpus configuration:

| Corpus                | Files indexed | What stands out                            |
|-----------------------|--------------:|--------------------------------------------|
| **perfetto**          | ~40 k         | TypeScript trace_viewer UI (~29 k TS syms), proto schemas (~6 k messages + enums), C++ trace_processor, SCSS, GN/GNI builds. 1.26 M symbols in 8 min on a 12-worker run. |
| **scry itself**       | 53            | Rust + Bash scripts + Markdown headings. 591 ms cold; `scry def "Verification checklist"` jumps to that section of `DEVELOPMENT.md`. |
| **scry-ui** (sibling) | 43            | TypeScript + SCSS + HTML; 100 ms cold.    |

No project-specific code paths run for these — they exercise the
generic walker + the tree-sitter parsers wired in `scry-lang`.
The AOSP-specific format parsers (Soong, AIDL, init.rc, sepolicy,
manifest, …) stay in `scry-aosp`, behind an extension trait, so
they don't activate when there are no matching files.

## Why not just $existing_tool

- **ctags / gtags / cscope** — tag-only, no scope, no AOSP-specific
  kinds (aconfig flags, init services, SELinux types), no build-graph
  awareness, single-threaded indexing.
- **ripgrep** — full-text grep only; no symbol model, no xrefs, no
  language- or module-scoped filtering.
- **clangd / IntelliJ / Android Studio** — precise but per-language,
  slow to warm, don't scale to the whole AOSP tree at once, not
  LLM-shaped.
- **Sourcegraph / Zoekt** — closest in spirit; heavyweight services
  not designed to be driven from a CLI loop in an agent process.
- **Kythe / Glean** — industrial-grade semantic graphs; require deep
  per-language indexer integration we'd rather avoid.

## Constraints

- **Zero changes inside `~/dev/aosp/` or `/mnt/agent/dev/linux/`** —
  the indexer is strictly read-only against the source roots.
- All scry source, dependencies, indexes, and reference repos live
  under `/mnt/agent/` (the host's `/` filesystem only has ~42 GiB
  free).
- Static Rust binary; no daemon required for one-shot CLI use, and
  `scry serve` is a stdin/stdout RPC, not a long-lived service.
