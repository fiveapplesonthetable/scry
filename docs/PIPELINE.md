# Pipeline

End-to-end view of how scry turns a source tree into a queryable
symbol graph at Kythe-grade precision. Each stage names the crate
that owns it.

## One-line summary

```
   source tree
        │
        ▼
   ┌────────────────────────┐
   │ scry index             │   tree-sitter walk + mmap'd sidecars
   └────────────────────────┘
        │  files_packed.bin, names.fst, file_symbols.bin,
        │  trigram postings, modgraph.bin, ref_resolutions.bin
        ▼
   ┌────────────────────────┐
   │ scry build-symbols     │   precision sidecar producer
   │   --build-kzip         │     (Kythe — AOSP via build_kzip.bash,
   │   --build-{gn,kbuild,  │      Bazel, anything Kythe-integrated)
   │    cmake}              │     (compile_commands.json → libclang)
   │   --build-cargo        │     (rust-analyzer scip)
   │   --with-polyglot      │     (Rust / Go / TS / Python alongside)
   │   --scip FILE          │     (escape hatch: pre-built .scip)
   └────────────────────────┘
        │  clang_usrs.bin   (packed mmap, USR per site)
        │  scip_index.bin   (packed mmap, SCIP symbol per site)
        ▼
   ┌────────────────────────┐
   │ scry def / callers /   │   query layer, strict precision by
   │   ref / impact /       │   default, sidecar-aware. --lexical
   │   callgraph / uses     │   opts out of the precision filter.
   └────────────────────────┘
```

## Stage 1 — `scry index`

Owner: `scry-cli::cmd_index` + `scry-walker` + `scry-lang`.

Walks the source tree, picks tree-sitter grammars by extension, emits
every sidecar the query layer needs:

- `files_packed.bin` — mmap'd file table (path interning, lazy access).
- `file_symbols.bin` + `file_symbols_offsets.bin` — per-file symbol
  list (name, byte_start, kind).
- `names.fst` + `name_postings.bin` — name → file_id postings, FST-
  compressed.
- `name_trigrams.fst` + `name_trigram_postings.bin` — trigram index
  for fuzzy / substring queries.
- `modgraph.bin` — module dependency graph built from per-file
  imports.
- `ref_resolutions.bin` — per-ref unresolved → candidate file_ids,
  using same-package + imports + inheritance (layer-2 attribution).
- `manifest.json` — corpus stats + binding hashes.

No build system involvement. Pure lexical extraction; tree-sitter gives
byte-accurate token positions but no type information. `scry index`
runs every phase end-to-end with per-phase progress on stderr — users
never need to invoke the individual sidecar builders.

## Stage 2 — `scry build-symbols`

Owner: `scry-cli::cmd_build_symbols` + `scry-clang` + `scry-scip` +
`scry-bridge`.

Routes to a producer based on one explicit `--build-*` flag. Outputs
go alongside the stage-1 sidecars in the index dir; readers mmap them
and the query layer applies the precision filter automatically.

### 2a. Kythe-integrated builds — `--build-kzip PATH`

The canonical path for AOSP (and any other build wrapping its
compilers with Kythe extractors: Bazel, Gradle via plugins, custom
pipelines).

```
build_kzip.bash (Soong)        any Kythe-aware build
   │                                   │
   ▼  every javac/kotlinc/clang/rustc invocation runs through
   │  a Kythe extractor that records the EXACT compiler input
   │  (post-rewrite sources, full classpath, every flag) into
   │  a per-compile .kzip (zip-of-protobuf-and-files)
   │
   ▼  Soong's merge_zips packs every per-compile .kzip into ONE all.kzip
   │
   ▼  scry build-symbols --build-kzip PATH/all.kzip
   │      decomposes the kzip per CU and invokes the matching Kythe
   │      v0.0.75 indexer (cxx_indexer for C/C++/ObjC, java_indexer
   │      for source-level Java, jvm_indexer for JVM bytecode,
   │      go_indexer, proto_indexer, textproto_indexer). Each
   │      indexer's delimited Entry-proto stream is decoded inline
   │      into scry's packed sidecar format — no SCIP intermediate.
   │
   ▼  packed sidecars
   clang_usrs.bin   (cxx_indexer output)
   scip_index.bin   (java + jvm + go + proto + textproto output)
```

The Kythe v0.0.75 public release does not ship a Rust indexer or a
source-level Kotlin indexer; CUs labeled `rust` are skipped and
logged, and kotlin CUs are routed to `jvm_indexer` only when the
CU ships real `.class` inputs (Soong's kotlinc emits `.java` srcjars
which trip jvm_indexer's missing-JarDetails check, so those skip
too). Per-language CU counts and skip reasons land in
`<index>/kythe-logs/summary.txt`.

#### Unit encodings in the kzip

AOSP-style kzips carry compilation units in **two** encodings:

* `root/pbunits/<sha>` — binary proto (`IndexedCompilation`). Newer
  extractors (go_extractor, kotlinc plugin, rust_extractor) write this.
* `root/units/<sha>`   — proto3-JSON of the same `IndexedCompilation`.
  cxx_extractor and the historic java_extractor write this; Soong's
  `merge_zips` preserves both encodings rather than transcoding.

scry's walker reads both sub-trees and dedups by SHA (proto wins on
collision). Both encoding paths feed the same `KzipUnit` shape; every
downstream stage (dispatch, indexer, entries, emit) is encoding-
agnostic. JSON parsing lives only in `walker.rs` (plus the per-CU
sub-kzip builder in `indexer.rs`, which re-serializes JSON units to
proto before handing them to a Kythe indexer — every v0.0.75 indexer
rejects mixed-encoding kzips with `multiple unit encodings but
different entries`).

The walker has two paths chosen by the driver based on
`SCRY_KZIP_MAX_UNITS`:

* unset → parallel walk (rayon, one `ZipArchive` per worker via
  `for_each_init`). With ~100 K JSON units in an AOSP kzip the JSON
  decode dominates; parallel cuts phase 1 from minutes to seconds.
* set   → serial streaming iterator with early break, so smoke runs
  capped at N units don't pay full-walk cost.

In both paths, when `SCRY_KZIP_LANGS` is set a cheap pre-peek over the
raw unit bytes (manual proto-varint walk for proto, serde with a
minimal shape for JSON) extracts `v_name.language` and short-circuits
CUs that don't match the filter — saving the full decode cost on the
~90 %+ of CUs the smoke / scoped runs ignore.

Two further env knobs scope an ingest to a subtree of the repo. They
operate on the CU's *primary source path* — the first
`required_input` entry whose extension is a known source-language
suffix (`.cc`, `.java`, `.kt`, `.go`, `.rs`, …). Skipping headers and
classpath jars matters: Java CUs put bootclasspath jars first in
`required_input`, so naive `required_input[0]` filtering would miss
every `.java` source.

* `SCRY_KZIP_PATH_PREFIX=frameworks/base/,frameworks/native/` — keep
  only CUs whose primary source starts with one of these prefixes.
* `SCRY_KZIP_PATH_EXCLUDE=external/,prebuilts/,out/` — drop CUs
  whose primary source starts with any listed prefix. Evaluated
  BEFORE the include filter, so excludes win.

**When to use which:**

| Intent | Pick |
|---|---|
| Index one specific subtree (e.g. just `frameworks/base/`) | `PATH_PREFIX` alone — short, explicit |
| Index "all platform code, skip third-party" | `PATH_EXCLUDE` alone — the third-party set is small + stable (`external/`, `prebuilts/`, `vendor/`, `kernel/`, `hardware/`, `device/`, `toolchain/`, `out/`), the platform set is large + grows over AOSP releases |
| Subtree minus its generated noise (e.g. `frameworks/` but not `out/`) | Both — include narrows scope, exclude trims noise inside the included subtree |

The two compose: a CU passes iff it's NOT under any exclude prefix
AND (no includes set OR matches at least one include).

Pure include alone forces enumeration of every wanted dir — fragile
because new platform dirs added in future AOSP releases would be
silently dropped. Pure exclude alone is the natural shape for "give
me everything except a handful of known-noise prefixes" and survives
codebase growth without maintenance.

Path filters apply only to runnable CUs; Skip-kind CUs (rust, kotlin
without bytecode) are always counted in the per-language skip tally
regardless of path scope so the summary stays accurate.

#### Resume semantics (`--resume`)

A full AOSP kzip ingest is 3-12 hours of indexer work. A SIGTERM
mid-phase-3 would otherwise discard every per-CU record committed so
far (the in-memory `PackedEmitter` buckets only get written to
`clang_usrs.bin` / `scip_index.bin` at phase 4). The driver checkpoints
each successful CU's records to disk under
`<out>/kythe-logs/checkpoint/`:

```
<out>/kythe-logs/checkpoint/
   manifest.json           kzip + env fingerprint + done-shas
   cxx.records.log         append-only framed bincode (cxx_indexer)
   scip.records.log        append-only framed bincode (everything else)
```

Each frame is `[u64 cu_sha_hash][u32 record_count][u32 byte_len][bincode]`.
On `--resume` the driver replays both logs into the in-memory buckets,
seeds the done-shas set from the manifest (the source of truth — the
log itself — covers any CUs that landed between the last manifest
flush and the crash), and the walker skips matching SHAs in phase 1.
Truncated tails (kill mid-write) are detected by the length prefix and
discarded with a warning — the per-CU dedup key
`(path, offset, symbol_id, role)` makes a re-processed CU idempotent.

**Three-state validator** (enforced before phase 1):

| `--out` has checkpoint | `--resume` | Behavior |
|---|---|---|
| no | absent | fresh run; create checkpoint as work progresses |
| yes | absent | hard error: pass `--resume` or `rm -rf` the checkpoint |
| no | present | hard error: no checkpoint at the given path |
| yes, fingerprint mismatch | present | hard error: kzip / env differ from checkpoint |
| yes, fingerprint matches | present | resume cleanly |

Fingerprint = `(kzip path + size + mtime + SCRY_KZIP_LANGS +
SCRY_KZIP_PATH_PREFIX + SCRY_KZIP_PATH_EXCLUDE + SCRY_KZIP_MAX_UNITS +
kythe_root)`. Different fingerprint means the checkpoint covers a
different slice of work; mixing them would silently produce a
mixed-scope sidecar.

**Why explicit-flag, no auto-resume:** production tools (kubelet,
terraform, bazel) require explicit opt-in to continue a partial run.
Auto-resume on fingerprint match would let a user re-run against the
same out-dir and silently inherit half-finished state, then be
confused when the result doesn't match a fresh run. The opt-in is one
flag (`--resume`); the cost of forgetting it is one human-readable
error.

The checkpoint dir is **NOT** auto-deleted after a successful sidecar
flush. Forensics (per-language CU progression, exact byte budgets,
recovery) beats magic. `rm -rf <out>/kythe-logs/checkpoint/` when
you're done with it.

**Manifest flush cadence:** `SCRY_KZIP_CHECKPOINT_EVERY=N` (default
100) controls how often `manifest.json` is re-flushed. The records
logs are appended every CU regardless — this knob only affects how
fresh the JSON manifest's `done_shas` list stays. A small value (e.g.
5 for the integration test) makes the smoke loop verifiable; the
default amortises the JSON write cost.

The Kythe extractors see what the compiler sees: post-rewrite sources
from protologsrc / jarjar / AAPT2 / hiddenAPI / AIDL / KAPT,
variant-selected source sets, every javac shard, every flag. Soong's
action graph lives inside the kzip; scry doesn't reason about it
separately.

For AOSP, kzip is produced via:

```
cd ~/dev/aosp
XREF_CORPUS=android.googlesource.com/platform/superproject \
DIST_DIR=/path/to/output \
TARGET_PRODUCT=aosp_cf_x86_64_phone \
build/soong/build_kzip.bash
# → /path/to/output/<KZIP_NAME>.kzip
```

The script invokes Soong's `xref_{cxx,java,kotlin,rust}` phony
targets, which run with Kythe extractors wrapping each compile, then
calls `merge_zips` to pack everything into one all.kzip.

### 2b. compile_commands-based builds — `--build-{gn,kbuild,cmake}`

For builds that already emit a `compile_commands.json`:

```
compile_commands.json
       │
       ▼  scry-clang (in-process libclang, rayon-parallel)
       │     - tolerance flags (-Wno-unknown-warning-option, -Wno-error)
       │     - gcc-only flag filter (-fno-allow-store-data-races, …)
       │     - relative-path rewrite (-I, -isystem, -include, …)
       │     - source-file de-duplication
       │
       ▼  CXCursor visitor → (path, byte_offset, USR, kind) tuples
       ▼  scry_store::clang_usrs::write
   clang_usrs.bin   (packed; same layout as scip_index.bin, different magic)
```

USR = libclang's Unified Symbol Resolution string
(`c:@F@ActivityManager#bindService#`). Stable across TUs and compile
flags; identifies the same declaration site uniquely.

- `--build-gn DIR` expects `args.gn` in the build dir; runs
  `gn gen --export-compile-commands` if `compile_commands.json` is
  missing.
- `--build-kbuild DIR` runs Linux's
  `scripts/clang-tools/gen_compile_commands.py` against the kernel's
  out dir.
- `--build-cmake DIR` expects `CMakeCache.txt` in the build dir; rerun
  `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` if compile_commands.json
  is missing.

### 2c. Polyglot / Cargo — `--build-cargo` / `--with-polyglot`

For Rust / Go / TypeScript / Python projects:

- `--build-cargo` drives rust-analyzer scip over a Cargo workspace.
- `--with-polyglot` (composes with any other `--build-*` flag) drives
  rust-analyzer, gopls, scip-typescript, scip-python over the
  matching subtrees of `--source-root`.

Per-language SCIP is imported into the same `scip_index.bin` via the
internal scip-import path.

### 2d. Escape hatch — `--scip FILE`

Already have a `.scip` file from somewhere? Import directly. Same
internal path the other producers fan into.

## Stage 3 — query

Every query type (`def`, `callers`, `ref`, `impact`, `callgraph`,
`uses`, `subclasses`, `grep`) consults a layered pipeline at query
time:

1. **Trigram / FST filter** — narrow candidate files (~100 µs).
2. **File symbol scan** — read per-file symbol slabs, match by kind +
   name (~100 µs–10 ms depending on result size).
3. **Resolution lookup** — for callers/refs, traverse
   `ref_resolutions.bin`.
4. **Precision filter** — if a sidecar covers the file, require the
   USR / SCIP symbol of each hit to match the definition's USR /
   symbol. `--lexical` opts out.

Sidecars are mmap'd once per `StoreReader` (cached via `OnceLock`);
per-query cost is the algorithmic cost only, no decode tax. On
AOSP-scale data, expect cold strict queries in 0.3–0.6 s wall, warm
queries through `scry serve` in single-digit ms.

## Performance ceiling

The whole pipeline is built around making query-time work cheap and
amortising indexing cost across periodic background runs:

- Stage 1 walks the corpus in O(N) per file; one-time cost on the
  initial run, incremental on subsequent runs via mtime comparisons.
- Stage 2's kzip path runs once per build cycle. The kzip itself is
  produced by Soong's own xref targets; scry's ingest is bounded by
  the per-language SCIP indexer's runtime.
- Stage 3 reads only the per-query slice of the sidecars
  (`precompute_by_file_ids` + bisect-on-records); sub-second wall on
  the 14M+ records of an AOSP-scale SCIP sidecar.

See `docs/BENCHMARKS.md` for live numbers on the AOSP + Linux +
Perfetto corpora.

## File-level pointers

| Stage | Crate / module |
|-------|----------------|
| 1     | `scry-cli::cmd_index` + `scry-walker`, `scry-lang` |
| 2a    | `scry-bridge::kzip` (planned) + `scry-scip` |
| 2b    | `scry-bridge::{gn,kbuild,cmake}` + `scry-clang` |
| 2c    | `scry-bridge::polyglot` + `scry-scip` |
| 2d    | `scry-scip::import_scip` |
| 3     | `scry-cli::apply_precision_filter` |
| Sidecar reader | `scry-store::{clang_usrs,scip_index,precision_packed}` |
| Sidecar writer | `scry-store::precision_packed::write` |
