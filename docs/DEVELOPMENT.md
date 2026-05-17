# scry — development

How the workspace is laid out, how to build / test / benchmark, and
where the not-yet-finished work lives.

## Workspace layout

```
scry/
├── crates/
│   ├── scry-walker/   gitignore-aware parallel file walker + FileKind classification
│   ├── scry-lang/     tree-sitter integration + per-language symbol/ref queries
│   ├── scry-aosp/     AOSP-specific format parsers (Soong, AIDL, HIDL, OWNERS,
│   │                  aconfig, init.rc, sepolicy, AndroidManifest.xml, Bazel,
│   │                  CMake, GN, api/*.txt — 12 parsers, each in its own file)
│   ├── scry-store/    on-disk index format: bincode columns + FST + trigram
│   │                  postings + lazy/mmap reader. THE ONLY crate with unsafe.
│   └── scry-cli/      the `scry` binary: CLI + JSON-RPC + build-* utilities
├── docs/
│   ├── DESIGN.md      as-built design (cgroup envelope, format, ranking)
│   ├── THEORY.md      from-scratch course on the CS behind scry (Rust → FST → EM model)
│   ├── OPERATIONS.md  knobs + recipe for production indexing
│   ├── USAGE.md       exhaustive command examples with real output
│   ├── BENCHMARKS.md  matrix numbers + perf decomposition
│   ├── FAST_PATH.md   trigram + lazy-reader optimization design
│   ├── AGENT_NOTES.md LLM-agent perspective: token economy, accuracy, small-model setup
│   └── DEVELOPMENT.md (you are here)
├── scripts/
│   ├── run_index.sh          production indexer wrapper (cgroup + resume)
│   ├── await_finalize.sh     post-finalize watcher; fires post_finalize.sh
│   ├── post_finalize.sh      build-offsets + build-file-symbols + build-trigrams
│   │                         + build-resolutions + validate + bench + email
│   ├── validate.sh           sanity-check every command against a real index
│   ├── bench_grep.sh         scry-vs-rg-vs-grep latency matrix
│   ├── bench_index.sh        index-time vs --workers matrix
│   ├── auto_recover.sh       5-min cron that restarts the indexer on failure
│   └── status_email.sh       hourly status email cron
├── Cargo.toml         workspace manifest
└── README.md          one-paragraph project pitch + quickstart
```

## Prerequisites

Hard requirements:

- **Linux x86_64 host.** Build is portable Rust, but the operations
  recipe (cgroup envelope, `posix_fadvise` page-cache prefetch) is
  Linux-only. Other targets compile but lose the production-mode
  memory safety guards described in `docs/OPERATIONS.md`.
- **Stable Rust 1.79 or newer.** Pinned in the workspace manifest
  (`rust-version = "1.79"`); newer is fine. Edition 2021. The
  `[workspace.lints]` table relies on the 1.74 manifest-lints
  feature.
- **Git ≥ 2.30.** For `scry diff --since` (parses `git diff --name-only`).
- **clang 14+ in `$PATH`.** Required only if you exercise the
  `--precise` path on `scry callers` (clangd-driven C++ type
  resolution). Absent clang, every other command works.
- **Disk space.** ~3 GB of cargo deps + target; the live AOSP+Linux
  index itself needs ~12 GB. The benchmarked corpus on this host
  (1,009,166 files, 70 GB source) does not need to be present to
  build or test — the synthetic-tree e2e test stands alone.

Soft requirements (optional but recommended):

- **`perf` (linux-tools-generic)** for the `perf stat` decomposition
  in `docs/BENCHMARKS.md`. Not needed for tests.
- **`ripgrep`** for the comparison benches (`./scripts/bench_grep.sh`).
- **`msmtp` or `sendmail`** if you want the hourly status email cron
  documented in `docs/OPERATIONS.md`.

## First-time setup

### Toolchain install

scry needs stable Rust 1.79+ plus `clippy` and `rustfmt`. The
canonical installer is [rustup](https://rustup.rs); on every
supported OS the one-liner is identical:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
. "$HOME/.cargo/env"
rustup component add clippy rustfmt
```

**Linux (Ubuntu / Debian).** Beyond rustup you'll want the C
toolchain for the tree-sitter grammars' build.rs scripts plus
`pkg-config`:

```sh
sudo apt update
sudo apt install -y build-essential pkg-config git curl
# (optional) for the precise C++ path: clangd >= 14
sudo apt install -y clangd
# (optional) for the comparison benches:
sudo apt install -y ripgrep
```

Fedora / RHEL / Rocky use `dnf install gcc gcc-c++ make
pkgconfig clang-tools-extra ripgrep`; Arch uses `pacman -S
base-devel pkgconf clang ripgrep`.

**macOS.** Xcode Command Line Tools (or the full Xcode) supply
the C toolchain rustup needs:

```sh
xcode-select --install
# rustup as above. Homebrew clangd + ripgrep (optional):
brew install llvm ripgrep
```

The Apple system clangd is older than what scry's `--precise`
path expects; either install `brew install llvm` and prefix
`PATH="$(brew --prefix llvm)/bin:$PATH"`, or skip `--precise`.

**Windows** is not officially supported (no production users on
Windows today). It should compile under the MSVC toolchain but
Unix-socket transport and SIGPIPE handling no-op out, and
nothing in CI covers it. PRs welcome.

### Building scry

```sh
git clone https://github.com/fiveapplesonthetable/scry
cd scry

# env.sh pins CARGO_HOME / RUSTUP_HOME under /mnt/agent so the
# build artifacts don't compete with the host's `~/` (the host's
# rootfs is the small partition on this layout). On any other
# host you can skip sourcing it and use your default cargo
# location.
. ./env.sh

# Confirm the toolchain. rustup picks rust-toolchain (none
# committed; uses your default) — bump to ≥ 1.79 if older.
rustup show active-toolchain
rustup component add clippy rustfmt   # idempotent

# Build + test. ~20 s cold for build; tests finish in ~3 s.
cargo build --release
cargo test --release --workspace
cargo clippy --release --workspace --all-targets   # must be clean

# Smoke-test the binary against the synthetic tree the e2e
# test built (or your own corpus):
./target/release/scry --help
```

If `cargo build --release` reports a missing system header (rare —
only happens with the C-FFI tree-sitter grammars on minimal
distros), install `build-essential` (Debian/Ubuntu) or the
equivalent toolchain group.

## Build

```sh
cargo build --release                              # ~20 s cold, ~5 s incremental
cargo clippy --release --workspace --all-targets   # must be clean
```

The workspace builds and lints clean — zero warnings across all
targets. Don't merge a change that introduces one; either fix it
or explicitly `#[allow]` it with a comment explaining why. The
strict policy is in the top-level `Cargo.toml` `[workspace.lints]`
block; see "Code quality posture" below for the rationale.

## Test

```sh
cargo test --release --workspace    # 129 tests, ~3 s total
```

Breakdown (counts as of 2026-05-16):

| crate        | tests | what they cover                                                                                              |
|--------------|------:|--------------------------------------------------------------------------------------------------------------|
| scry-aosp    |    19 | one happy-path per format parser (Soong, AIDL, HIDL, OWNERS, aconfig, init.rc, sepolicy, manifest, Bazel, CMake, GN, api-txt) plus the `cmake_comments_with_unbalanced_paren` regression that took down indexing |
| scry-cli     |    47 | regex literal extractor, file_symbols + lazy + epoch_iso, Layer 2 resolve_one branches, Java pkg/import narrowing edge cases, MCP arg validation (7), MCP version negotiation (4), OWNERS parser, ranking & path penalties |
| scry-cli e2e |     1 | end-to-end: synthetic source tree → real `scry index` subprocess → every CLI + JSON-RPC + MCP path; round-trips `scry index --incremental` (modify + add a file, replay unchanged) |
| scry-lang    |     9 | per-language minimal extraction (Java / Cpp / Rust / Go / Python), Cpp out-of-line method bare-name + scope, Kotlin extension receiver scoping, progress-callback abort, unbounded-parse sanity. 2 ignored AST-dump helpers (`-- --ignored --nocapture` to see) |
| scry-store   |    51 | LazyVec round-trip (sequential / reverse / random / OOB / empty / refs-too), file_symbols entry decoder, trigram posting wire format (round-trip + truncation + malformed-varint), name posting wire format, rank_score tier ordering, epoch_to_iso8601 known values + leap year + pre-epoch, trigram extraction + query + intersection, file_digest absent-sidecar accessor |
| scry-walker  |     2 | FileKind classification |

The e2e test is the strongest single signal — it runs the just-built
binary against a synthetic source tree, exercises writer + reader +
CLI + JSON-RPC + MCP + incremental indexing + Layer 2 resolution +
trigram grep, finishes in ~2 s. Any cross-crate API drift surfaces
there.

### Adding a test

For a new file format parser: add a fixture inline as `&str`, parse
it, assert on the resulting `RawSymbol` list. See
`crates/scry-aosp/src/bp.rs` for the pattern.

For a new tree-sitter language pattern: add a minimal source snippet,
call `extract(FileKind::X, src)`, assert the names + scope_path. See
the `kotlin_extension_receiver_scoping` test for a working example
that pinned a real bug (compute_scope was pushing a function's own
name onto its own scope_path).

For a new end-to-end behavior: extend `crates/scry-cli/tests/e2e.rs`.
The fixture builder is `build_index(src, idx)`; add files there and
call the new query through `Command::new(scry_bin())`.

## Bench

All bench numbers in `docs/BENCHMARKS.md` come from one of the
scripts below. Every script is in-tree at `scripts/` and runnable
against your own index — change `SCRY_INDEX_DIR` to point at it.
The scripts are the source of truth; the markdown is a snapshot.

### Query latency: scry vs ripgrep vs POSIX grep

```sh
# Best-of-3 for scry + rg, single run for POSIX grep
# (grep takes 5+ min per pattern, so one run only).
SCRY_INDEX_DIR=/mnt/agent/scry-index ./scripts/bench_grep.sh

# Skip POSIX grep entirely (saves ~25 min on the 5-pattern run):
BENCH_INCLUDE_GREP=0 SCRY_INDEX_DIR=/mnt/agent/scry-index \
  ./scripts/bench_grep.sh
```

The script prints a markdown table with columns `pattern | scry(s)
| rg -j4 (s) | grep -rF (s)`. Five patterns spanning rare → common
literals, matching the table in `docs/BENCHMARKS.md`.

For cold-cache measurement (to validate `posix_fadvise(WILLNEED)`
wins on your hardware), drop the page cache between runs:

```sh
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches
/usr/bin/time -f "wall=%e sys=%S user=%U" \
  /mnt/agent/scry/target/release/scry grep PATTERN \
    --index /mnt/agent/scry-index --limit 100 > /dev/null
```

### Index throughput vs `--workers`

```sh
# Default sweep: 2, 8, 16, 32 workers on the Linux kernel only
# (~85k files; ~3 min total).
./scripts/bench_index.sh

# Custom sweep:
BENCH_ROOT=/path/to/repo \
  BENCH_WORKERS="4 8 16 32 64" \
  BENCH_MEM_CAP=16 \
  ./scripts/bench_index.sh
```

The script captures wall time, peak RSS (via `/usr/bin/time -v`),
index size, and files/sec; produces a markdown table matching the
"Indexing: throughput vs --workers" section of
`docs/BENCHMARKS.md`.

### Full-corpus production index

The production wrapper, with cgroup envelope + auto-resume +
post-finalize chain:

```sh
systemd-run --user --unit=scry-index --collect \
  -p MemoryMax=60G -p MemorySwapMax=0 \
  -p Restart=on-failure -p RestartSec=3 \
  -p StandardOutput=append:/mnt/agent/scry-index.log \
  -p StandardError=append:/mnt/agent/scry-index.log \
  /mnt/agent/scry/scripts/run_index.sh
```

This is what produced the "13.3 min for 1 M files" number in
`docs/BENCHMARKS.md`. `scripts/await_finalize.sh` runs as a
sibling job and fires `scripts/post_finalize.sh` automatically when
the writer exits, chaining build-offsets → build-file-symbols →
build-trigrams → build-resolutions → validate → bench → email.

### `perf stat` decomposition

```sh
sudo sysctl -w kernel.perf_event_paranoid=1     # allow user perf

/usr/bin/perf stat \
  -e task-clock,cycles,instructions,cache-references,cache-misses,page-faults,context-switches \
  /mnt/agent/scry/target/release/scry grep "ActivityManagerService" \
    --index /mnt/agent/scry-index --limit 100 > /dev/null
```

Reproduces the "CPU + memory profile" section of
`docs/BENCHMARKS.md` (the 38% cache-miss / IO-bound finding).

For a flamegraph-quality `perf record`, rebuild with frame
pointers so DWARF unwind succeeds:

```sh
RUSTFLAGS="-C strip=none -C force-frame-pointers=yes" \
  cargo build --release
/usr/bin/perf record --call-graph dwarf -- \
  target/release/scry grep ... > /dev/null
/usr/bin/perf report --stdio --no-children --percent-limit 1.0
```

### Sanity-check the index

`scripts/validate.sh` exercises every CLI command + JSON-RPC
shape against a real index; useful after an indexer change to
catch regressions before the bench scripts run:

```sh
SCRY_INDEX_DIR=/mnt/agent/scry-index /mnt/agent/scry/scripts/validate.sh
```

### Script reference

| script                       | purpose                                                                |
|------------------------------|------------------------------------------------------------------------|
| `scripts/run_index.sh`       | production indexer wrapper (cgroup + resume + post-finalize trigger)   |
| `scripts/await_finalize.sh`  | watches for indexer exit; fires `post_finalize.sh`                     |
| `scripts/post_finalize.sh`   | build-offsets + build-file-symbols + build-trigrams + build-resolutions + validate + bench + email |
| `scripts/validate.sh`        | sanity-checks every CLI / JSON-RPC command shape                       |
| `scripts/bench_grep.sh`      | scry vs rg vs POSIX grep query-latency matrix                          |
| `scripts/bench_index.sh`     | indexing throughput vs `--workers`                                     |
| `scripts/auto_recover.sh`    | 5-min cron; restarts the indexer if the unit fails or stalls           |
| `scripts/status_email.sh`    | hourly status email cron (last-finalize timestamp + index size + uptime) |

All scripts are POSIX shell, idempotent where it makes sense,
and tolerant of missing optional env vars (sensible defaults).
Read the script before running on shared infrastructure — most
write to `/mnt/agent/` paths that are specific to this host's
layout.

## CPU / memory profile

```sh
# perf stat summary (no symbols needed)
/usr/bin/perf stat -e task-clock,cycles,instructions,cache-references,\
cache-misses,page-faults,context-switches \
  target/release/scry grep "ActivityManagerService" --index /mnt/agent/scry-index

# perf record with full call graph (DWARF unwind; binary should not be stripped)
RUSTFLAGS="-C strip=none -C force-frame-pointers=yes" cargo build --release
/usr/bin/perf record --call-graph dwarf -o grep.perf.data -- \
  target/release/scry grep "..." --index /mnt/agent/scry-index
/usr/bin/perf report -i grep.perf.data --stdio --no-children --percent-limit 1.0
```

Default release build has stripped symbols; for hot-path investigation
add the RUSTFLAGS above and re-build.

## Operations log

Every CLI invocation appends one JSON line to `~/.scry/queries.log`
(override with `SCRY_LOG=…`). Useful for:

- "What did I search for in this session?"
- "Which queries were slow?" — sort by `elapsed_ms`.
- "Which queries returned zero hits?" — filter `hits == 0`.

```sh
# Slow queries:
jq -s 'sort_by(-.elapsed_ms)[:10]' ~/.scry/queries.log

# Empty results:
jq -c 'select(.hits == 0)' ~/.scry/queries.log

# Throughput per command kind:
jq -r '.cmd' ~/.scry/queries.log | sort | uniq -c | sort -rn
```

## Code quality posture

- `#![forbid(unsafe_code)]` on every crate EXCEPT scry-store. See the
  module-level "Unsafe policy" doc at the top of
  `crates/scry-store/src/lib.rs` for the contract. All 11 mmap call
  sites go through one `safe_mmap(path)` helper.
- Result-first error handling. The few `unwrap`s remaining are either
  on hardcoded tree-sitter queries (panic = malformed compile-time
  string, programmer bug) or on slice bounds that the immediately-
  preceding check guarantees (provably safe).
- Defensive decoders. Every on-disk reader (read_posting,
  read_trigram_posting, read_file_symbols_entry) returns an empty
  Vec / None on truncated or out-of-range input rather than panicking.
  The lone `read_u32_le` helper is the single source of truth for
  bounded LE-u32 reads.
- Bench-protected hot paths. The trigram pre-filter has tests pinning
  the intersection-by-smallest-first invariant; the lazy reader has
  random-access round-trip tests. A future refactor that subtly
  breaks either gets caught at `cargo test`.
- **Strict clippy lints workspace-wide** via `[workspace.lints]` in
  the top-level `Cargo.toml`: `clippy::correctness` denied,
  `suspicious` / `perf` / `style` / `complexity` warned, plus a
  curated pedantic subset (redundant_closure_for_method_calls,
  explicit_iter_loop, match_wildcard_for_single_variants,
  inefficient_to_string, implicit_clone). Noisy lints
  (needless_pass_by_value, unnecessary_wraps, doc_lazy_continuation,
  match_same_arms) are documented-and-omitted in the manifest so the
  choice can't drift silently. The workspace is clippy-clean (0
  warnings, `cargo clippy --release --workspace --all-targets`); a
  new warning fails CI before it gets a chance to merge.

## Language coverage

scry today wires tree-sitter for: C, C++ (incl. `.h`/`.hpp`), Java,
Kotlin, Rust, Go, Python, Bash, TypeScript (incl. `.tsx`), Proto
(proto2 + proto3), HTML, CSS, SCSS, Markdown, TOML, YAML. Plus
the hand-rolled AOSP / build-format parsers: Soong, AIDL, HIDL,
OWNERS, aconfig, init.rc, sepolicy, AndroidManifest.xml, Bazel,
CMake, GN, api/*.txt.

This covers the headline corpora end-to-end:

| Corpus                | Languages it actually uses             | Coverage      |
|-----------------------|-----------------------------------------|---------------|
| AOSP + Linux          | C/C++/Java/Kotlin/Python + Soong/AIDL/HIDL/init.rc/sepolicy/manifest | full |
| Perfetto              | TypeScript (UI) + Proto + C++/Python + SCSS/CSS/HTML + GN | full |
| scry itself           | Rust + Bash + Markdown + TOML + YAML    | full          |

## Known coverage gaps

The intentionally short list of things scry does not understand yet.
Each entry is something an agent or human would reasonably expect to
find and currently won't.

- **Swift / Dart / Haskell / OCaml** are not wired (tree-sitter
  grammars exist; no headline corpus exercises them).
- **Assembly** (kernel `.S` / `.s`) is not wired. The generic-profile
  walker classifies these files but no symbol extractor runs;
  agents asking after asm functions get no hits today.
- **Embedded C++ DSLs** — Soong-generated `aidl_const.cpp`, big
  generated NeuralNetworks `.cpp` test fixtures — sometimes
  trip the 60 s tree-sitter timeout and end up skipped (the
  ts-TIMEOUT skiplist quarantines them on subsequent runs).

## Roadmap

Live work items, sized roughly. Each one has a clear acceptance
signal; items move to the changelog once landed.

- **SCIP ingestion** (phase-5 opt-in per DESIGN §5). Closing the
  10-20 % overload-resolution gap for C++/Java would require
  consuming SCIP indexes the build system already emits for IDE
  use. Half-day per language to wire up. Pending an AOSP build
  step that actually produces SCIP for arbitrary modules.
- **Per-commit incremental on the build-resolutions sidecar.**
  Today `scry build-resolutions` re-resolves every ref on each
  run; a smaller-than-corpus per-file resolver delta would let
  the nightly timer maintain the sidecar in seconds instead of
  minutes.
- **Layer 2 determinism check in CI.** Build the resolutions
  sidecar twice; diff. Should be byte-identical (every map is
  `HashMap<u32, ...>` keyed by file_id; iter order is on-disk).
  Cheap enough to gate releases on.

## Verification checklist

Things to run after non-trivial changes, in roughly the order
that catches problems fastest:

1. **`cargo fmt --check`** — formatting. Cheap; fails before
   compile.
2. **`cargo build --release`** — must finish with zero warnings.
   New warnings should either be fixed or explicitly
   `#[allow]`'d with a comment.
3. **`cargo test --release --workspace`** — all 157 tests pass.
   The e2e test is the strongest single signal; if it fails,
   something cross-crate broke.
4. **`./scripts/validate.sh`** — exercises every CLI command +
   JSON-RPC shape against the live index. Catches CLI-shape
   regressions the e2e test misses (real index, real corpus).
5. **`./scripts/bench_grep.sh` quick mode** —
   `BENCH_INCLUDE_GREP=0 ./scripts/bench_grep.sh` runs scry vs
   rg in ~5 min. Sanity-check that scry's per-pattern numbers
   are within ~20% of the BENCHMARKS table; large regressions
   are the most useful single perf signal.
6. **`scry stats --index /mnt/agent/scry-index`** — checks the
   reader can open the production index after any
   reader-format change.
7. **Spot-check `~/.scry/queries.log`** — every command writes
   one JSON line; if your change touched the CLI loop, verify
   the log still works (`tail -1 ~/.scry/queries.log | jq .`).
8. **For format changes (writer-side)**, the cycle is:
   delete `/mnt/agent/scry-index/*`, re-run `scripts/run_index.sh`,
   wait for the finalize email, then run validate + bench.
   ~14 minutes; only needed when the on-disk schema changes.

The first three should run on every PR. Items 4-7 run before
landing format/CLI changes. Item 8 is the full reindex and is
only needed for writer-side changes.

## Measurement notes (live AOSP + Linux index)

These describe what the production index actually does, distilled
from `perf stat` and timed runs. Numbers reported in
`docs/BENCHMARKS.md`; the conclusions below are what shaped the
code.

- **Cold-vs-warm `def` gap is page-fault dominated.** Cold cost
  is `sys` time bringing `names.fst`, `name_postings.bin`,
  `file_symbols`, `ref_resolutions` into RAM. A single warm-up
  reuses the pages and the bulk disappears.
- **Cold grep is IO-bound.** Cache-miss rate sits around 17 %;
  `sys` >> `user` confirms the time is in page-faulting candidate
  files, not in the scan loop. The trigram pre-filter is doing
  its job — lowering the threshold further only helps if the
  per-file scan goes to zero, which means skipping content.
- **`lto=thin` is for binary size, not speed.** Warm-grep wall
  times under `lto=thin` vs `lto=false` fall inside run-to-run
  noise. The release profile keeps `lto=thin` for the ~2 %
  binary-size win.
- **`--workers 16` is the throughput knee on a 72-core host.**
  Beyond 16, jemalloc arena contention and per-thread parser
  state cost more than they save. Smaller hosts should set this
  to the physical core count.
- **`ts-TIMEOUT` recurrence is bounded.** The per-file 60 s
  parse cap trips deterministically on a small set of
  data-as-headers files in `external/libwebsockets/`; the OOM
  skiplist records them and the next run skips parsing without
  touching the rest of indexing.
- **Layer 2 resolution is deterministic by construction.** The
  resolver iterates refs in on-disk order against
  `HashMap<u32, …>` keyed by `file_id` and writes via tmp +
  atomic rename, so two `scry build-resolutions` runs against
  the same index produce a byte-identical `ref_resolutions.bin`.

## Decisions: ideas considered and not pursued

The roadmap stays short by saying no to clear-shape ideas that
don't earn their cost. Captured here so the rationale survives
("why didn't we just …"):

- **Subprocess-per-parse isolation.** Significant work for a
  problem the existing safeguards (`parse_with_options` progress
  callback + cgroup `MemoryMax` + 60 s parse budget) already
  bound. No measured failure mode pushed us past the budget.
- **Bigram + trigram hybrid index, position-in-posting, field-aware
  n-grams, bloom-style lossy FST, suffix-array fallback,
  online FST construction.** All explored as algorithmic
  improvements; each either lacked a clear win on our query mix
  (bigrams: < 2× when we're already trigram-selective; positions:
  candidate scan is already ~470 ms / 1.4 GB read) or had a cost
  profile (5× corpus size for suffix arrays, doubled storage for
  field-aware) that didn't earn it.
- **`io_uring` migration.** Measured on the live index — cold
  grep is bytes-from-disk-wait bound, not syscall bound; the
  rayon mmap+memchr loop already saturates the IO queue depth
  the page cache can absorb. Upside < 10 % on the worst query,
  nothing on warm. Full breakdown in `docs/ROADMAP.md` § 4.
- **`MAP_HUGETLB` for the trigram FST.** Single-digit-% win on
  warm queries; needs huge pages configured at boot, doesn't
  pay for the deployment friction.
- **Adaptive worker count.** Oscillation risk under load with
  unclear benefit over the static 16. The bench matrix knee is
  reproducible; the static value lands on it.
- **Per-query page-cache warmup.** Marginal for LLM agents
  (which don't follow predictable follow-up patterns) and adds
  state to an otherwise stateless query path.
- **DAX / persistent-memory layout.** Hardware-blocked. The
  on-disk format is already forward-compatible with mmap-then-read
  semantics; nothing to do until pmem hardware shows up.
- **Stack Graphs for Kotlin / Python.** Active research project
  at GitHub; Rust bindings unclear. Revisit if the project
  publishes a stable Rust API and our heuristic Layer 1 starts
  costing us meaningful precision.
- **Streaming JSON-RPC responses.** Agents can already cap with
  `--limit`; the tokio + framed-codec dep is significant for the
  marginal value of early cut-off.
- **Embedding-based semantic retrieval.** Out of scope for a
  lexical / identifier-search tool. Complement to scry, not part
  of it.
- **PR-diff scoped queries** (`scry callers Foo --since-commit
  HEAD~10`). Useful but the git/repo interaction with AOSP's
  repo-managed history adds complexity disproportionate to the
  agent value.
- **Web UI wrapping `scry serve`.** Out of scope; the LLM-agent
  and terminal use cases are the design center.
- **Pre-built index distribution** as a tarball. Storage and
  hosting costs disproportionate to the value of skipping a
  13-min one-time rebuild.

## Contributing

The standard Rust loop:

```sh
. ./env.sh
cargo fmt
cargo build --release
cargo test --release
```

A change is mergeable when:
- `cargo test --release` is green workspace-wide.
- New behavior has at least one assertion pinning it (a per-crate
  unit test, or an extension to `tests/e2e.rs`).
- The L7 review heuristics in this doc still hold: no new unsafe
  outside scry-store, no `.unwrap()` on user/FS input without a
  defensive guard, on-disk format additions are backwards-compatible
  via the optional-sidecar pattern (see `file_symbols_mmap`,
  `ref_resolutions_mmap`).
- If the change touches CLI shape, `docs/USAGE.md` is updated to
  reflect the new flag / command with at least one real-output
  example.

## Open work

State as of the last commit. Each entry should land its own PR + a
test pinning the new behaviour.

### Coverage vs full Kythe parity

What's currently shipping vs what's missing for 100% Kythe-class
symbol-identity coverage on the four reference corpora:

| Language | Corpus      | Current | Gap to 100% | Blocker                                    |
|----------|-------------|---------|-------------|--------------------------------------------|
| C/C++/ObjC | Perfetto (GN)    | 100% real-failures-free (1349 missing-source TUs correctly classified as skipped) | none | — |
| C        | Linux (Kbuild)    | ~97% (634 / 24,980 TUs `CXError_ASTReadError`) | 3% | classify the 634 by directory + dump representative `CXDiagnostic` per group; most are likely arch-specific include paths in the compdb |
| Java     | AOSP (Soong)      | 92% OK + partial (1119 OK / 81 partial / 13 no-output of 1213) | 8% | modules whose ALL variants are unbuilt — needs `m droid` to materialise Soong intermediates |
| Kotlin   | AOSP (Soong)      | ~89% (200 OK + 31 partial of 258, pre-variant-fallback) | 11% | same as Java + kotlinc 2.x codegen bugs (`Exception while generating code for`) we can't fix in our patch |
| Rust     | scry (Cargo)     | 100% | none | — |
| Rust     | AOSP             | not exercised | n/a | AOSP's Rust crates compile via Soong's `cargo` integration; bridge plumbing exists but no end-to-end run yet |
| Go       | AOSP             | not exercised | n/a | no Go in AOSP root; would apply to a separate Go corpus |
| TypeScript | scry-ui          | partial — `editors/vscode` works | unknown | full scry-ui workspace not yet indexed |
| Python   | scry             | partial — one .py file missing on disk gets cleanly skipped | unknown | full Python corpus not yet exercised |

The **only blocker for AOSP Java + Kotlin reaching 100%** is
running `m droid` (or targeted `m <module>` for the specific
failing modules) to materialise the Soong intermediates the
build-symbols pipeline reads. scry's own bridge can't generate
KAPT / AAPT2 / AIDL / aconfig stubs — that's Soong's job, and
re-implementing it inside scry-bridge is explicitly out of scope
(2-3 weeks of work, would conflict with the user's existing
`out/soong/` state).

What this means in practice: every CALLER of any AOSP symbol that
WAS reached during the build is precisely resolvable. The
remaining 8% / 11% are modules where we have neither source nor
classpath because the build never produced them.

### build-symbols: AOSP coverage ceiling without a full `m`

Current AOSP pipeline ships every JVM-side fix that doesn't require
re-running Soong's own codegen. Concretely (after commits
`77062bf … aa3dd4c`):

- ninja-variable expansion across all shards
- `javacFlags` / `kotlincFlags` forwarders with surgical filtering
- patched `semanticdb-kotlinc` (FirFileSymbol stderr noise)
- `--patch-module=java.base=…` for libcore + ART
- shell-aware quoted-blob handling for ErrorProne
- AIDL / KAPT srcjar extraction, sibling `R.jar`/`aconfig` classpath
- variant fallback (pick the variant whose `.rsp` files exist)

The remaining failures are modules whose entire variant set is
unbuilt on disk (no `.rsp`, no `classpath.rsp`, no `gen/aidl/*`).
For these, the only path to "0 failures" is materialising the
Soong intermediates. Two options for the operator:

1. **Targeted**:
   ```sh
   cd ~/dev/aosp && source build/envsetup.sh
   lunch aosp_arm64-trunk_staging-userdebug
   m PhotopickerLib SystemUI-core Launcher3QuickStepLib \
     HealthConnectLibrary PermissionController-lib \
     PlatformComposeSceneTransitionLayout
   ```
   ~30 min, ~5 GB additional `out/soong/` cost. Plugs ~20 specific
   modules currently in `no-output`.

2. **Full**:
   ```sh
   m droid
   ```
   ~2–4 h incremental, ~50–150 GB `out/soong/` cost depending on
   lunch combo. Materialises every reachable variant.

Out of scope: implementing AAPT2 / AIDL / KAPT / KSP / protoc /
aconfig codegen pipelines inside scry-bridge ("become Soong"). The
practical sequence is `m` first, scry build-symbols second.

### Per-language partial-success classifier

`tolerate_javac_errors=true` and `tolerate_kotlinc_errors=true`
currently dump every PartialOnError compilation's first stderr line
to the log. The classifier should split partials into:

- **classpath-gap partial**: stderr contains `unresolved reference`
  / `cannot find symbol` / `package X does not exist`. Plot of
  resolved jars suggests these are missing-jar issues, not
  scry-bridge bugs. Aggregate into one summary line per run.
- **codegen partial**: stderr contains `Exception while generating
  code for` or kotlinc 2.x FIR/IR error. Real plugin / compiler
  bugs.
- **source-error partial**: stderr contains a `.java:N:M: error:`
  with a real syntactic / type-system fault from the user's code.
  Real source defects — should never be tolerated, but currently
  are.

Splitting the partial bucket lets operators tell "20 module's worth
of missing-codegen Hilt stubs" apart from "one source file has a
genuine compile error".

### Kernel 634 parse failures

`scry build-symbols --build-kbuild …` on the current Linux corpus
produces 634 `CXError_ASTReadError` (code 4) failures, mostly in
`drivers/gpu/drm/amd/`, `virt/kvm/`, and arch-specific subsystems.
Probable causes (in order of likelihood):

1. The compile-commands.json was generated for one defconfig but
   the source files reference headers selected by a different
   defconfig (e.g. AMD GPU code expects `CONFIG_DRM_AMDGPU=y`).
2. Cross-arch include paths (the entry says
   `-D__KERNEL__ -Iarch/x86/include` but the file lives in
   `arch/arm64/`).
3. libclang's resolution of kernel-specific macros
   (`__builtin_va_list`, gcc-only intrinsics) — but the
   `is_gcc_only_flag` filter already handles the common ones.

Open: classify the 634 failures by directory + dump one
representative CXDiagnostic per group to identify the root cause.

### `usr_for_window` warm latency — fixed in v0.1.66

Cured by two changes that landed together:

- `ClangUsrIndex::precompute_by_file_ids` + `ByFileLookup` materialise
  a per-`file_id` slice of sorted `(byte_offset, usr_id)` tuples at
  query start. The per-ref loop in `apply_precision_filter` then
  binary-searches that slice (`partition_point` + ±window scan)
  instead of re-allocating a display_path and probing the
  `HashMap<String, ...>` per ref.
- The `display_path` cache (`StoreReader::display_path_cached`)
  removes the per-ref `String` alloc + `PathBuf::push` in the same
  hot loop.

Combined, the strict precision query on the live AOSP+kernel corpus
went from ~17 s warm on 2.5 k refs to single-digit milliseconds on
the same query through `scry serve` (with cached paths).

The same precompute lives in `scip_index::ByFileSymbolLookup` for
SCIP sidecar lookups.

### File split: scry-cli main.rs

`crates/scry-cli/src/main.rs` is ~10,800 lines, well past the
700-line cap we hold elsewhere. Split candidates (roughly
self-contained):

- `cmd_grep` + the literal/regex scan path
- `cmd_callgraph` + traversal helpers
- `cmd_impact` + `cmd_subclasses`
- the `build-*` sidecar commands (trigrams, offsets, file_refs,
  file_symbols, resolutions, digests, embeddings)
- the `Index` command's mega-function (currently inline)

Each split should land as its own commit with no behavioural
change (pure code motion + `pub(crate)` adjustments). Easier to
do incrementally than as one huge refactor.
