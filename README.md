# scry

Semantic code search and cross-reference engine for AOSP and the Linux
kernel. A single static Rust binary that indexes ~1 million source
files in ~13 minutes, then answers symbol, reference, and content
queries at interactive latency — **30–45× faster than ripgrep**,
**>700× faster than POSIX grep** — with one warm `mmap`'d index.

Coverage:
- **Source languages**: C, C++, Java, Kotlin, Rust, Go, Python, shell,
  assembly — via tree-sitter.
- **Build systems**: Android.bp (Soong), Android.mk, Bazel BUILD,
  Kconfig, Makefile, CMake, GN, Gradle, *.bzl.
- **Android platform config**: aconfig flags, init.rc services,
  SELinux .te policy, AndroidManifest.xml components,
  `api/*.txt` SDK surface.
- **IPC**: AIDL interfaces + methods + parcelables, HIDL interfaces.
- **Ownership**: OWNERS files.

Driven by both humans at a terminal and LLM agents over JSON-RPC.
Every CLI query has a `--json` variant; `scry serve` reads
newline-delimited JSON on stdin and replies on stdout. Every CLI
invocation prints a stats footer and appends one line to
`~/.scry/queries.log` for after-the-fact introspection.

Docs:
- `docs/USAGE.md` — exhaustive examples with real output from the live
  AOSP master + Linux 7.0-rc7 index.
- `docs/DESIGN.md` — full design, including the cgroup envelope that
  keeps indexing inside its memory budget.
- `docs/OPERATIONS.md` — production knobs + the systemd recipe for
  long-running indexing.
- `docs/FAST_PATH.md` — the trigram + lazy-mmap optimization design.
- `docs/BENCHMARKS.md` — measured numbers vs ripgrep and POSIX grep,
  index-time vs `--workers` matrix, perf-stat decomposition.
- `docs/DEVELOPMENT.md` — workspace layout, how to test / bench /
  contribute, and remaining roadmap items.

## Quickstart

```sh
cd /mnt/agent/scry
. ./env.sh                         # CARGO_HOME / RUSTUP_HOME / PATH
cargo build --release              # ~20 s cold, ~10 s incremental

# One-shot indexing against the AOSP + Linux default roots.
./target/release/scry index

# Or pass explicit roots:
./target/release/scry index ~/dev/aosp /mnt/agent/dev/linux \
    -o /mnt/agent/scry-index

# Production-grade: systemd-managed loop with cgroup memory cap +
# auto-resume after OOM-kills. See docs/OPERATIONS.md for the recipe.
./scripts/run_index.sh

# Query.
./target/release/scry def ActivityManagerService --kind class
./target/release/scry callers transact --lang Java --limit 20
./target/release/scry ref liblog --lang Soong                            # who depends on liblog?
./target/release/scry def libbinder --kind soong                         # Soong module info
./target/release/scry def zygote --kind init.svc                         # init.rc service
./target/release/scry def IBinder --kind aidl.iface                      # AIDL interface
./target/release/scry def Activity --in frameworks/base/services/        # subdir-scoped
./target/release/scry callers transact --in art/                         # subdir-scoped
./target/release/scry prefix Activity --limit 20
./target/release/scry fuzzy ParcelFile --limit 10
./target/release/scry grep "TODO\(.*\): " --regex --lang Java
./target/release/scry grep "ZygoteInit"                                  # trigram-accelerated literal
./target/release/scry outline frameworks/base/cmds/app_process/app_main.cpp   # all symbols in one file
./target/release/scry stats
```

## Operator knobs (all CLI flags, nothing hardcoded)

```
--workers N             rayon pool size (default: all cores)
--flush-bytes N         MiB of records per batch; adaptive batch size (default 1024)
--flush-every N         hard file cap per batch (default 50000)
--mem-cap N             soft jemalloc backpressure ceiling, GiB
--big-file-bytes N      files > N route SERIAL (default 64 KiB)
--max-file-bytes N      hard refuse-to-open ceiling (default 100 MiB)
--no-refs               skip ref extraction (smaller index, no xrefs)
--resume                pick up from progress.json checkpoint
--build-trigrams        build trigram index (100× faster grep on literals)
--profile aosp/linux    select walker skiplist

# env var (read once at startup):
SCRY_PARSE_TIMEOUT_MS   per-file tree-sitter parse budget (default 0 = unlimited)
```

## Standalone post-index utilities (no re-parsing required)

```sh
scry build-trigrams --index /path  # add trigram index (100× grep)
scry build-offsets  --index /path  # add lazy-reader sidecars (30× cold open)
```

Useful for retrofitting old indexes or indexes built without
`--build-trigrams`. Both are atomic — safe to run on a live-serving index.

See `docs/OPERATIONS.md` for what each does and when to tune it; see
`docs/FAST_PATH.md` for the design behind trigram + lazy reader.

## LLM/agent integration

`scry serve` reads newline-delimited JSON-RPC requests from stdin and
writes JSON responses to stdout. Open it once per task and reuse the
warm mmap'd index.

```sh
$ printf '%s\n' \
    '{"id":1,"cmd":"def","args":{"name":"Binder","limit":3}}' \
    '{"id":2,"cmd":"callers","args":{"name":"transact","in":"frameworks/base/","limit":3}}' \
    '{"id":3,"cmd":"grep","args":{"pattern":"ZygoteInit","limit":5}}' \
    '{"id":4,"cmd":"outline","args":{"path":"app_main.cpp"}}' \
  | scry serve --index /mnt/agent/scry-index
{"id":1,"result":[{"name":"Binder","kind":"class","lang":"Java","path":"…","line":85,...}]}
{"id":2,"result":[{"name":"transact","ref_kind":"call","lang":"Java","path":"…","line":1234,...}]}
{"id":3,"result":[{"path":"…","line":42,"col":7,"snippet":"…ZygoteInit…","lang":"Cpp"}]}
{"id":4,"result":{"path":"…/app_main.cpp","lang":"Cpp","symbols_total":13,"symbols_shown":13,"symbols":[…]}}
```

Per-command argument schema (LLM-friendly, all field names lowercase
snake-case, all optional except where noted):

| `cmd`       | required arg | other args                          | result shape           |
|-------------|--------------|-------------------------------------|------------------------|
| `def`       | `name`       | `lang`, `kind`, `in`, `limit`       | `[symbol, …]`          |
| `ref`       | `name`       | `lang`, `kind`, `in`, `limit`       | `[ref, …]`             |
| `callers`   | `name`       | `lang`, `in`, `limit`               | `[ref, …]` (kind=call) |
| `prefix`    | `prefix`     | `in`, `limit`                       | `[symbol, …]`          |
| `fuzzy`     | `substr`     | `in`, `limit`                       | `[symbol, …]`          |
| `grep`      | `pattern`    | `lang`, `in`, `limit`               | `[hit, …]` (literal)   |
| `outline`   | `path`       | `limit`                             | `{path, lang, symbols_total, symbols_shown, symbols: […]}` |
| `stats`     | —            | —                                   | metadata object        |

All search commands accept `"in"` (root-relative subdir prefix); same
substring semantics as the CLI's `--in`. `grep` is literal-only (regex
queries belong on the CLI where rayon parallelism is available); it
shares the same trigram pre-filter for sub-ms matches on selective
patterns. `outline` takes a `path` (full path or suffix like
`app_main.cpp`) and returns every symbol defined in that file.

Back-compat: all commands also accept `"name"` as a fallback for
their primary arg, so older callers that hardcoded `{"name":…}` for
grep / prefix / fuzzy / outline still work.

## Architecture (one paragraph)

A parallel `ignore`-crate walker classifies files into 40 categories
(source langs, build files, AOSP configs, OWNERS), then rayon pumps
each file through either a tree-sitter parser (for source languages,
with a per-file 60 s parse budget enforced via the `parse_with_options`
progress callback) or a tiny custom parser (for `.bp`, `.aidl`,
`.aconfig`, `.te`, `.rc`, `AndroidManifest.xml`, `OWNERS`, …).
Definitions and references are collected with scope paths,
blake3-hashed for stable ids; a streaming finalize pass builds the
per-record byte-offset sidecars, the FSTs over symbol + ref names,
the file → symbol-ids reverse index, the trigram FST for grep, and
the Layer 2 ref-to-def resolution sidecar — all written to `.tmp/`
and atomically renamed into place. The `StoreReader` `mmap`'s every
sidecar; queries decode one record at a time without ever loading
the full 10 GB columnar payload.

See `docs/DESIGN.md` for the full design including the cgroup
envelope, `docs/FAST_PATH.md` for the trigram + lazy-reader
optimizations, and `docs/BENCHMARKS.md` for measured numbers.

## Why not just $existing_tool

- **ctags / gtags / cscope**: tag-only, no scope awareness, no AOSP-
  specific kinds (aconfig flags, init services, SELinux types), no
  build-graph awareness, single-threaded indexing.
- **ripgrep**: full-text grep only; no symbol model, no xrefs, no
  language- or module-scoped filtering.
- **clangd / IntelliJ / Android Studio**: precise but per-language,
  slow to warm, can't scale to the whole AOSP tree at once, not
  LLM-shaped.
- **Sourcegraph / Zoekt**: closest in spirit, but heavyweight services
  not designed to be driven from a CLI loop in an LLM agent.
- **Kythe / Glean**: industrial-grade semantic graphs; require deep
  per-language indexer integration we'd rather avoid.

## Constraints

- **Zero changes inside `~/dev/aosp/` or `/mnt/agent/dev/linux/`.**
- All scry source, dependencies, indexes, and reference repos live
  under `/mnt/agent/` (the host's `/` filesystem only has ~42 G free).
- Static Rust binary; no daemon required for one-shot CLI use, and
  `scry serve` is a stdin/stdout RPC, not a long-lived service.
- 7-day cron-driven hourly status emails to the maintainer; project
  is built autonomously and continuously.
