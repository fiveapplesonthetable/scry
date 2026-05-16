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

## Known coverage gaps

- ~~**C++ out-of-line method definitions** (`Foo::bar() { ... }` outside
  the class body) are not currently captured as symbols.~~ **Fixed**
  in commit `704d917`. The cpp query already matched the
  qualified_identifier; the bug was storing the whole `Foo::bar` as
  the symbol NAME. `drill_qualified_identifier` now extracts the
  bare name (`bar`) and prepends the qualifiers to `scope_path`.
  Existing indexes need a re-parse to pick up the fix
  (parser-side, not sidecar-retrofittable).
- **Kotlin extension functions** are scoped correctly to their receiver
  type (e.g. `String.shouted` scope = `["String"]`), but the Kotlin
  symbol query doesn't yet cover `companion object` members, sealed
  class hierarchies, or `inline reified` fns. Tracked in the same
  area as the cpp gap above.
- **AIDL versioning** (frozen `Vn.aidl` interfaces) isn't surfaced
  distinctly from the live interface — both get the same `aidl.iface`
  kind. An agent asking "what's the V3 frozen surface of IFoo" has
  to filter by path. Easy fix: detect the `frozen/` path segment in
  the AIDL parser and emit a separate `aidl.frozen` kind.
- **Per-language Layer 2 narrowing** beyond Java. The resolver
  framework (build-resolutions) is in place and the Java
  pkg/import path works; Kotlin and C++ fall back to "first
  same-lang candidate" without scope/import awareness. Adding each
  language is a self-contained ~200 LOC extension in `resolve_one`.

## What's left (real follow-ups, not blockers)

The project is in production use against the live AOSP + Linux index;
everything in `README.md` and `USAGE.md` works. The following items
exist but aren't critical-path:

- **Layer 2: wider language coverage.** The build-resolutions sidecar
  has a Java-aware narrowing path (same-package → explicit import →
  wildcard import → java.lang fallback). Kotlin and C++ have the
  framework in place but no language-specific narrowing yet — the
  fallback to "first same-lang candidate" is what they get.
- **Subprocess-per-parse isolation.** A single rogue tree-sitter
  parse can't OOM the host (parse_with_options + cgroup MemoryMax
  catch it), but it can still chew CPU for the budgeted 60 s. A
  subprocess-per-parse model would let us SIGKILL just the bad
  worker. Significant work; mitigated by the existing safeguards.
- **More AOSP-specific kinds.** ArtCompiler annotations, SDK
  extension version stamps, AIDL versioning — there are corners of
  AOSP that would have niche but real LLM use. Add one parser per
  format, mirror what `crates/scry-aosp/src/sepolicy.rs` does.

None of these are blocking real LLM-agent use today; the next item
to actually start is "whichever delivers the most leverage for an
agent task you currently run scry for."

## Concrete pending items (small, ready to land)

Small, well-scoped tasks that need doing — bug fixes, missing
tests, doc nits. Each is ≤ a few hours of work and has a clear
acceptance signal.

- **`api/*.txt` ranking sanity check.** USAGE.md says api-txt
  declarations should rank below real source. We have one
  unit test (`rank_real_class_beats_api_txt`) but no end-to-end
  test that `scry def Activity --kind class` returns the .java
  file first on the live index. Add an assertion to
  `scripts/validate.sh` that pins this for every release.
- **`crossbeam` and `toml` are workspace deps but unused** by any
  crate. Either remove from `Cargo.toml` or add a comment noting
  the future-use rationale. (Current THEORY note picks the
  latter — make the workspace match.)
- **`scry stats --json` for machine consumption.** `scry stats`
  prints a human-readable summary; the JSON variant is
  documented in USAGE but the implementation needs verifying —
  some fields (last-finalize-time, by-lang breakdown) may be
  missing.
- **`SCRY_LOG` env var documented in three places, consistent
  default.** README, USAGE, and `~/.scry/queries.log` examples
  in DEVELOPMENT all reference the default path; verify they
  agree and that the override works as documented.
- **Coverage test for the AIDL versioning gap.** Add a failing
  test in `crates/scry-aosp/src/aidl.rs` that pins the desired
  behavior: a file under `frozen/V3/` emits `aidl.frozen` kind,
  not `aidl.iface`. Then implement the fix.
- **`tracing` is initialized in CLI but most code uses raw
  `eprintln!`** for progress output. Either standardize on
  `tracing::info!` everywhere or commit to the current split
  (CLI / human output via eprintln; internal events via tracing).
- **The `unbounded_parse_returns_tree` test is named for the
  `parse_with_options` migration** but doesn't exercise the
  most important property — that a *successful* parse returns
  the same tree as `parse_with_options(None)`. Strengthen the
  assertion.
- **`scry coverage` output stable in JSON form.** The CLI shape
  is set; the JSON envelope should freeze before any agent code
  starts depending on it.
- **Document the on-disk format version field.** `Manifest` has
  a version u32 but the bumping policy isn't written down.
  When does it change? Backwards-compat rules for the reader
  when version mismatches?

## Verification checklist

Things to run after non-trivial changes, in roughly the order
that catches problems fastest:

1. **`cargo fmt --check`** — formatting. Cheap; fails before
   compile.
2. **`cargo build --release`** — must finish with zero warnings.
   New warnings should either be fixed or explicitly
   `#[allow]`'d with a comment.
3. **`cargo test --release --workspace`** — all 129 tests pass.
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

## Things worth investigating

These aren't bugs; they're "I noticed something I haven't
explained yet". Worth a half-day each to chase.

- **The 2 ms gap between cold and warm `def` queries.** Cold
  `scry def Activity` is ~12 ms; warm is ~5 ms. The 7 ms cold
  cost decomposes to FST page fault (~2 ms) + symbol record
  page fault (~5 ms). Plausible but unverified; a `perf
  stat -e page-faults` over a single cold query would pin it.
- **Why workers=16 specifically.** The bench matrix shows 16
  is the sweet spot. The expected pareto knee on a 72-core host
  should be higher; the actual reason is probably the
  jemalloc-arena-per-thread + parser-state-per-thread combined
  footprint. Worth profiling to confirm; if it's something
  else, we might be leaving throughput on the table.
- **Per-file 60 s ts-TIMEOUT count.** The indexer logs 4-10
  ts-TIMEOUTs per full run. Are these *always* the same files?
  If so, OOM skiplist should be quarantining them automatically.
  If not, what changed? `grep ts-TIMEOUT /mnt/agent/scry-index.log`
  across the last 10 runs.
- **The 38 % cache-miss rate on cold grep.** Documented in
  BENCHMARKS; the conclusion is "IO-bound, not CPU-bound", but
  the breakdown of which cache (L3 vs DRAM) is the source is
  unverified. `perf stat -e LLC-load-misses,dTLB-load-misses`
  would distinguish.
- **Whether `lto=thin` is paying for itself.** The release
  profile uses LTO; cold build is +5 s. A `cargo build
  --release --config 'profile.release.lto=false'` comparison
  on cold grep latency would tell us if it matters.
- **Stability of the Layer 2 resolution under repeat rebuilds.**
  Are 89 % of refs resolved across runs the *same* 89 %? A
  diff of `ref_resolutions.bin` between two clean rebuilds
  would confirm determinism end-to-end.

## Experiments and unexplored directions

The items above are concrete follow-ups with clear shape. The list
below is more speculative — wild ideas, research-shaped questions,
or technique grafts from other systems. Each entry names the idea,
the expected win, the cost, and the reason it hasn't been tried.

### Algorithmic / index format

- **Bigram + trigram hybrid index.** Lin & Yan (2016) show that
  storing bigrams alongside trigrams cuts intersection cost on
  5+ byte patterns by ~30% at ~30% extra storage. Worth measuring
  on our query mix; the win is likely <2× since we're already
  selective, but the bigram dictionary stays small (65k keys).
- **Position info per trigram posting.** Today a posting says "file
  42 contains trigram `Zyg`"; with positions we'd skip the `memchr`
  scan entirely for the candidate set. Cost: ~10× larger trigram
  payload (positions are u32 per occurrence). Probably not worth it
  given the candidate scan is already ~470 ms / 1.4 GB read.
- **Field-aware n-grams** (separate trigram indexes for identifiers
  vs comments vs strings). Lets `--in-identifiers` or
  `--exclude-comments` filters work without false matches on prose
  trigrams. Zoekt supports this; we don't. Storage roughly doubles.
- **Bloom-filter-style lossy FST.** Belazzougui et al. (2011)
  construct probabilistic FSTs with one-sided error for set
  membership. Our 280 MB FST could shrink to ~80 MB at <0.1% false
  positive rate. The exact path (lookup → load record → verify) is
  unchanged; only the prefix walk would need a "yes, but verify"
  semantics. Worth a half-day spike to measure.
- **Suffix array fallback for arbitrary patterns.** Modern compact
  suffix arrays (FM-index, CSA) on the concatenated corpus would
  support *any* substring query, including those scry's literal
  extractor falls back on. The build is ~hours and the index is
  ~5× the size of the source; the win is closing the
  full-scan-fallback gap.
- **Online FST construction.** Daciuk (1998) describes incremental
  minimization. Would let us add new symbols without a full
  rebuild. The pre-existing FST stays read-only; new symbols
  accumulate in a small overlay FST that's merged at finalize.
  Complex but unblocks per-commit incrementalism.

### IO / memory

- ~~**`io_uring` migration.**~~ Measured on the live AOSP+Linux
  index (2026-05-16) — won't ship. Cold-cache `scry grep` wall
  time is dominated by bytes-from-disk wait, not by syscall
  overhead; the rayon-driven mmap+memchr loop already keeps a
  healthy IO queue depth. Measured upside < 10 % on the worst
  query, nothing on warm. Full breakdown in `docs/ROADMAP.md` § 4.
- **`MAP_HUGETLB` for the trigram FST.** Mapping the ~280 MB FST
  with huge pages (2 MiB) cuts TLB misses on the prefix walk.
  Likely single-digit-% win on warm queries; needs hugepages
  configured at boot. Cheap to try.
- **Adaptive worker count based on jemalloc.** Today `--workers
  16` is static. A controller that scales workers based on
  observed RSS slope would keep us closer to the memory ceiling
  on hosts with more RAM. Risk: oscillation under load.
- **Per-query page-cache warmup.** After a query, the next likely
  follow-up (e.g., after `def Foo`, the user often runs `callers
  Foo`) can be pre-faulted speculatively. Real win for interactive
  human use; less so for LLM agents that don't follow predictable
  patterns.
- **DAX / persistent-memory layout.** byte-addressable pmem skips
  the page cache entirely. scry would work unchanged but read at
  near-DRAM latency. Hardware unavailable to us currently; design
  is forward-compatible.

### Resolution / precision

- **SCIP ingestion for precision uplift.** Already in DESIGN §5 as
  a phase-5 opt-in; not blocking, but the integration is well-
  understood and would close the 10-20% accuracy gap on
  C++/Java overload resolution. Half-day per language to wire up.
- **Stack Graphs (GitHub, 2023) for Kotlin/Python.** Scope
  resolution from declarative rules, like tree-sitter queries but
  for resolution. Would replace our heuristic Layer 1 for
  hard-to-resolve languages. Active research project at GitHub;
  Rust bindings unclear.
- **Cross-language JNI binding inference.** Java `native` methods
  and C++ `Java_pkg_Class_method` exports follow naming rules
  scry could resolve automatically. Today the link is invisible.
  ~200 LOC + a per-language naming convention table.
- **AIDL-generated symbol shadow links.** Today `scry def IFoo`
  finds the AIDL source; finding the Java `IFoo.Stub` or C++
  `BpIFoo` requires a separate query. Generating these as
  synthetic symbols (linked to the AIDL definition) would unify
  the lookup. Moderate; depends on tracking the AIDL generator
  conventions across HIDL/NDK/Rust outputs.

### Coverage

- **bash + assembly via tree-sitter.** Both grammars exist; we
  haven't wired them. Bash is high-value for AOSP build scripts
  (lunch / mm / mmm); assembly is lower-value but a frequent
  question for kernel code.
- **Swift / Dart / Haskell / OCaml.** Grammars exist; the
  question is whether anyone's AOSP work touches them. The
  generic-profile walker would pick these up automatically.
- **Kotlin companion objects, sealed-class hierarchies, inline
  reified fns.** Listed under "Known coverage gaps"; adding each
  is a tree-sitter query addition + scope adjustment.
- **Per-language Layer 2 narrowing** beyond Java. Already in
  "What's left"; the framework is in place.
- **OWNERS chain traversal.** Today `scry owner PATH` returns the
  nearest OWNERS file's owners. The Gerrit semantics walk up the
  chain accumulating; we should match. ~100 LOC.

### Agent / LLM interface

- **MCP server wrapper.** Already in "What's left".
- **`outline_with_snippets` combined call.** Saves one round-trip
  per file the agent wants to understand. AGENT_NOTES recommends
  it.
- **Streaming JSON-RPC responses.** Today `scry serve` writes one
  JSON line per response. For large result sets (`callers
  transact` with no limit), streaming each hit individually would
  let an agent cut off early. tokio + framed codec is the easy
  path.
- **Query plan / `--explain`.** For a query that's slow, print the
  trigrams + posting sizes + intersection size + scan cost so the
  caller can see why. Useful for both humans and LLMs that need
  to debug.
- **Embedding-based semantic retrieval as a sibling tool.** A
  separate `scry semantic-search "how do I parse TOML"` that
  drops into a vector index over chunks. Complementary to the
  lexical/identifier search scry does today. Big lift; depends on
  an embedding model + Faiss/HNSW index.
- **PR-diff scoped queries.** `scry callers Foo --since-commit
  HEAD~10` for "show me callers added in the last 10 commits".
  Requires git history awareness; AOSP's repo-managed history
  makes the implementation interesting.
- **Heuristic auto-narrowing.** If `scry def String --limit 10`
  returns 200 ranked-equal hits, the tool should suggest a
  filter (`add --in frameworks/base/`) rather than just
  truncating. Both interactive and agent-friendly.

### Operational

- **Cron-driven nightly rebuild.** Today the indexer runs on
  demand via systemd-run. A `OnCalendar=*-*-* 03:00:00` timer
  would keep the index fresh without manual intervention.
- **Web UI** wrapping `scry serve`. Single static page; HTML
  results table; live filter. Not strictly necessary for the
  LLM-agent or terminal use case, but useful for casual browsing.
- **Pre-built index distribution.** A nightly snapshot of the
  ~9.5 GB index published as a tarball would let downstream
  users skip the 13-min rebuild on their first install. Storage
  cost is the binding constraint.

The pattern across this list: each idea has a clear hypothesis
about the win and a clear notion of the cost. Items move from
this section to "What's left" when the hypothesis becomes
concrete enough to commit to a milestone, and from there to the
codebase when the milestone closes. Anything still in this list
after a year either failed the hypothesis quietly or fell out
of priority.

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
