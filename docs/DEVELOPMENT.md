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
│   ├── OPERATIONS.md  knobs + recipe for production indexing
│   ├── USAGE.md       exhaustive command examples with real output
│   ├── BENCHMARKS.md  matrix numbers + perf decomposition
│   ├── FAST_PATH.md   trigram + lazy-reader optimization design
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

## Build

The whole workspace builds with stable Rust 1.73+ (uses `div_ceil`,
otherwise vanilla 2021 edition):

```sh
. ./env.sh                  # CARGO_HOME / RUSTUP_HOME under /mnt/agent/cargo
cargo build --release       # ~20 s cold, ~5 s incremental
```

Two pre-existing warnings (a dead-code `write_postings_and_fst` in
scry-store and an `unused_assignments` in scry-aosp::aidl) are
tolerated; everything else compiles clean.

## Test

```sh
cargo test --release        # 80 tests, ~1 s total
```

Breakdown:

| crate       | tests | what they cover                                                                                              |
|-------------|------:|--------------------------------------------------------------------------------------------------------------|
| scry-aosp   |    15 | one happy-path per format parser (Soong, AIDL, HIDL, OWNERS, aconfig, init.rc, sepolicy, manifest, Bazel, CMake, GN, api-txt) plus the `cmake_comments_with_unbalanced_paren` regression that took down indexing |
| scry-cli    |    18 | regex literal extractor (7), file_symbols + lazy + epoch_iso (refactor-out tests), Layer 2 resolve_one (8 branches), Java pkg/import narrowing edge cases |
| scry-cli e2e |   1 | end-to-end: synthetic 5-file tree → real `scry index` subprocess → `def` / `outline` / `grep` / `callers` queries via CLI and JSON-RPC, assertions on every shape |
| scry-lang   |   7+2 | per-language minimal extraction (Java / Cpp / Rust / Go / Python), Cpp out-of-line method bare-name + scope, Kotlin extension receiver scoping, progress-callback abort, unbounded-parse sanity. 2 ignored AST-dump helpers (`-- --ignored --nocapture` to see) |
| scry-store  |    35 | LazyVec round-trip (sequential / reverse / random / OOB / empty / refs-too), file_symbols entry decoder (round-trip / OOB / empty / truncated), trigram posting wire format (round-trip + empty + truncated count + truncated varint + malformed varint), name posting wire format (round-trip + truncated + empty + OOB), rank_score tier ordering, epoch_to_iso8601 known values + leap year + pre-epoch, trigram extraction + query + intersection |
| scry-walker |     2 | FileKind classification |

The e2e test is the strongest single signal — it runs the just-built
binary against a synthetic source tree, exercises writer + reader +
CLI + JSON-RPC + Layer 2 resolution + trigram grep, finishes in 0.4 s.
Any cross-crate API drift surfaces there.

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

- **Incremental indexing.** Today every run is a full re-parse. A
  per-file mtime-based incremental pass would save the ~13 min cost
  when a single AOSP module changes. Requires per-file digest in the
  index format.
- **Layer 2: wider language coverage.** The build-resolutions sidecar
  has a Java-aware narrowing path (same-package → explicit import →
  wildcard import → java.lang fallback). Kotlin and C++ have the
  framework in place but no language-specific narrowing yet — the
  fallback to "first same-lang candidate" is what they get.
- **MCP server wrapper.** `scry serve` is stdin/stdout JSON-RPC; an
  MCP wrapper would expose the same surface to any MCP-aware client.
  Mechanical port; nothing in the core needs to change.
- **`posix_fadvise(WILLNEED)` on grep candidate lists.** The perf
  decomposition in `BENCHMARKS.md` shows the dominant cost is
  page-faulting cold mmap'd file contents (1.37 s sys vs 0.6 s user
  on the 680 ms test query). Pre-faulting could shave another
  30-50% off the cold-cache case. Single syscall per file.
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
