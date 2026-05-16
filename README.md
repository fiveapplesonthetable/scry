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

## Quickstart

```sh
. ./env.sh                                       # CARGO_HOME pinned
cargo build --release                            # ~20 s cold, ~5 s incremental
./target/release/scry def ActivityManagerService --kind class      # ~8 ms
./target/release/scry callers transact --lang Java --limit 10      # ~80 ms
./target/release/scry grep ZygoteInit                              # ~580 ms (rg: 21.2 s — 36×)
./target/release/scry outline frameworks/base/cmds/app_process/app_main.cpp   # ~600 ms
./target/release/scry coverage frameworks/base/services            # ~250 ms
```

Times above are warm-cache P50 on the live AOSP + Linux index
(1,009,166 files, 70.4 GB source). The `rg` comparison is `rg -j4
ZygoteInit ~/dev/aosp` against the same tree.

Indexing the whole corpus from scratch is **13.3 minutes** wall on
a 72-core host at `--workers 16`; ripgrep doesn't have an
indexing phase, so it pays the full ~20 s walk cost on every
query forever. Full per-pattern bench table and reproducibility
recipe in [`docs/BENCHMARKS.md`].

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

[`docs/USAGE.md`]: docs/USAGE.md
[`docs/BENCHMARKS.md`]: docs/BENCHMARKS.md
[`docs/DESIGN.md`]: docs/DESIGN.md
[`docs/THEORY.md`]: docs/THEORY.md
[`docs/FAST_PATH.md`]: docs/FAST_PATH.md
[`docs/OPERATIONS.md`]: docs/OPERATIONS.md
[`docs/DEVELOPMENT.md`]: docs/DEVELOPMENT.md
[`docs/AGENT_NOTES.md`]: docs/AGENT_NOTES.md

## One-paragraph architecture

A parallel `ignore`-crate walker classifies files into 40 categories,
then rayon pumps each file through either a tree-sitter parser (for
source languages, with a per-file 60 s parse budget enforced via the
`parse_with_options` progress callback) or a small custom parser (for
the AOSP-specific formats). Definitions and references are collected
with scope paths and blake3-hashed for stable ids. A streaming finalize
pass builds the per-record byte-offset sidecars, the FSTs over symbol
and ref names, the file → symbol-ids reverse index, the trigram FST
for grep, and the Layer 2 ref-to-def resolution sidecar — all written
to `.tmp/` and atomically renamed into place. The `StoreReader`
`mmap`s every sidecar; queries decode one record at a time without
loading the full 10 GB columnar payload.

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
