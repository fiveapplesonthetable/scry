# scry

Semantic code search and cross-reference engine for AOSP and the Linux kernel.

**Status:** Phases 0–4 implemented + resumable production indexing. Live
on `/mnt/agent/scry-index` against `~/dev/aosp` + `/mnt/agent/dev/linux`.
Full design in `docs/DESIGN.md`; production knob + systemd recipe in
`docs/OPERATIONS.md`.

## What it is

A single static Rust binary, ripgrep-fast, build-aware code intelligence
tool that indexes a full AOSP checkout (~350 GB / ~925k files) plus a
Linux kernel tree (~37 GB / ~85k files) — combined **~1.01M files / ~70 GB
of indexed source** (the rest is binaries / build outputs the walker
filters out) — and answers semantic and substring queries at interactive
latency on a warm mmap index.

Coverage spans source languages (C, C++, Java, Kotlin, Rust, Go, Python,
shell), build systems (Android.bp, Android.mk, Bazel BUILD, Kconfig,
Makefile), Android-specific configs (aconfig flags, init.rc services,
SELinux .te policy, AndroidManifest.xml components), and AIDL interfaces.

Built to be driven by **both humans at a terminal and LLM agents over
JSON-RPC** — every query has a `--json` variant and `scry serve` reads
newline-delimited JSON on stdin.

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
    '{"id":2,"cmd":"callers","args":{"name":"transact","limit":3}}' \
  | scry serve --index /mnt/agent/scry-index
{"id":1,"result":[{"name":"Binder","kind":"class","lang":"Java","path":"…","line":85,...}]}
{"id":2,"result":[{"name":"transact","ref_kind":"call","lang":"Java","path":"…","line":1234,...}]}
```

Supported commands: `def`, `ref`, `callers`, `prefix`, `fuzzy`, `stats`.

## Architecture (one paragraph)

A parallel `ignore`-crate walker classifies files into 38 categories
(source langs / build files / AOSP configs / OWNERS), then rayon pumps
each file through either a tree-sitter parser (for source languages) or
a tiny custom parser (for `.bp`, `.aidl`, `.aconfig`, `.te`, `.rc`,
`AndroidManifest.xml`, `OWNERS`). Definitions and references are
collected with scope paths, blake3-hashed for stable ids, and resolved
by name to candidate definitions (best-effort Layer 1). The index ships
as bincode columns + an FST over symbol names + posting lists, all
atomically swapped into place. The `StoreReader` mmaps everything for
zero-copy reads.

See `docs/DESIGN.md` for the full design and `notes/AOSP_SCALE.md` for
the source-tree scale numbers.

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
