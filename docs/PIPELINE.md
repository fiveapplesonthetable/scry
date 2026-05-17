# Pipeline

End-to-end view of how scry turns a source tree into a queryable
symbol graph at Kythe-grade precision. Each stage names the crate
that owns it; cross-references for ground truth.

## One-line summary

```
   source tree + build system
            │
            ▼
   ┌────────────────────────┐
   │ scry index             │  ← walk + tree-sitter, fast path
   └────────────────────────┘
            │  files.bin, file_symbols.bin, names.fst,
            │  trigram postings, modgraph.bin
            ▼
   ┌────────────────────────┐
   │ scry build-symbols     │  ← invoke build system per TU,
   │   --build-{soong,gn,   │    collect structured symbol IDs
   │    kbuild,cmake,cargo} │
   └────────────────────────┘
            │  clang_usrs.bin (libclang USRs)
            │  scip_index.bin (merged SCIP)
            ▼
   ┌────────────────────────┐
   │ scry build-resolutions │  ← attribute lexical refs to
   └────────────────────────┘    file_ids using sidecars + layer-2
            │  ref_resolutions.bin
            ▼
   ┌────────────────────────┐
   │ scry def / callers /   │  ← query layer, strict precision by
   │   ref / impact /       │    default, sidecar-aware
   │   callgraph            │
   └────────────────────────┘
```

## Stage 1 — `scry index` (fast path)

Owner: `crates/scry-cli/src/main.rs::cmd_index` + `scry-walker` +
`scry-lang`.

Walks the source tree, picks tree-sitter grammars by extension, emits:

- `files_packed.bin` — mmap'd file table (path interning, lazy access).
- `file_symbols.bin` + `file_symbols_offsets.bin` — per-file symbol
  list (name, byte_start, kind).
- `names.fst` + `name_postings.bin` — name → file_id postings, FST-
  compressed.
- `name_trigrams.fst` + `name_trigram_postings.bin` — trigram index
  for fuzzy/substring queries.
- `modgraph.bin` — module dependency graph built from per-file
  imports (currently Java/Kotlin; pluggable per language).
- `manifest.json` — corpus stats + binding hashes.

No build system involvement. Pure lexical extraction; tree-sitter
gives byte-accurate token positions but no type information.

## Stage 2 — `scry build-symbols` (build-aware sidecars)

Owner: `crates/scry-cli/src/main.rs::cmd_build_symbols` +
`scry-clang` + `scry-scip` + `scry-bridge`.

Single command, one explicit `--build-{soong,gn,kbuild,cmake,cargo}`
flag, optional `--with-polyglot`. The flag tells scry which build
system's intermediate representation to consume. Outputs go alongside
the stage-1 sidecars in the index dir.

### 2a. C / C++ / ObjC (Path B — libclang USRs)

Trigger: `--build-{gn,kbuild,cmake}`, or any `compile_commands.json`
discovered under the source tree.

```
compile_commands.json
       │
       ▼ libclang (in-process, per-TU, rayon-parallel)
       │   - tolerance flags (-Wno-unknown-warning-option, -Wno-error)
       │   - gcc-only flag filter (-fno-allow-store-data-races, …)
       │   - relative-path rewrite (-I, -isystem, -include, …)
       │   - source-file de-duplication (kernel's relative-path quirk)
       │
       ▼ CXCursor visitor → (path, byte_offset, USR, kind) tuples
       │
       ▼ scry_store::clang_usrs::write
   clang_usrs.bin   (packed: header + records + sym/path tables)
```

USR = libclang's Unified Symbol Resolution string
(`c:@F@ActivityManager#bindService#`). Stable across TUs and compile
flags; identifies the same declaration site uniquely.

### 2b. Java / Kotlin (Path C — SCIP via Soong bridge)

Trigger: `--build-soong <out>`.

```
out/soong/build.<target>.NN.ninja  (sharded ninja files)
       │
       ▼ scry-bridge: extract g.java.javac / g.java.kotlinc rules
       │   - rule-name classifier (ignores split-srcJars, jarjar, …)
       │   - variant selector (pick the one whose .rsp files exist)
       │   - ninja variable expansion (cross-shard)
       │   - javacFlags / kotlincFlags forwarder
       │   - sibling-output classpath augmentation (R.jar, aconfig)
       │   - srcjar extraction (AIDL stubs, KAPT factories)
       │
       ▼ Compilation { sources, classpath, bootclasspath, flags }
       │
       ▼ semanticdb-javac / semanticdb-kotlinc plugin (per TU)
       │   - emits .semanticdb files alongside .class files
       │
       ▼ scip-java index-semanticdb (corpus-wide merge)
       │   - reads every .semanticdb under <root>
       │   - emits merged SCIP protobuf (merged_jvm.scip)
       │
       ▼ scry scip-import
       │   - decode protobuf, walk documents/occurrences
       │   - translate (line, col) → byte_offset by reading source
       │   - intern symbols into a single table
       │
       ▼ scry_store::scip_index::write
   scip_index.bin   (packed: same shape as clang_usrs.bin,
                     different magic: SCRYSP01 vs SCRYUP01)
```

The 4-stage chain (plugin → .semanticdb → scip-java merge → packed
import) is how Java/Kotlin's type-resolved symbol identity reaches
scry's query path. Each step is independently restartable and the
intermediates are kept on disk so partial runs are usable.

### 2c. Polyglot (Path C, other languages)

Trigger: `--with-polyglot` (implied for `--build-cargo`).

Per-language indexer → SCIP → `scry scip-import --append` into the
same `scip_index.bin`:

- Rust → `rust-analyzer scip` (in-tree workspaces)
- Go → `gopls scip` (per Go module)
- TypeScript → `scip-typescript` (per `tsconfig.json`)
- Python → `scip-python` (per project root)

Each language is independent. The `--append` mode of scip-import
seeds the rebuild from the existing sidecar via `iter_symbols` /
`iter_records` so per-language runs compose without redundant decode.

## Stage 3 — `scry build-resolutions` (layer-2 attribution)

Owner: `crates/scry-cli/src/main.rs::cmd_build_resolutions`.

Bridges the gap between tree-sitter's per-file lexical refs and the
file-level resolution scry's query layer needs. For each ref, the
resolver uses:

1. The file's own scope (declarations in the same package).
2. Imports / use clauses (parsed once per file).
3. Inheritance edges from the modgraph (a method ref on subclass
   may resolve to the parent class's file).
4. Same-name fallback (a last-resort same-name match in the same
   compilation unit's classpath).

No compiler-grade type inference. The structured-symbol filter from
stage 2 (clang USRs / SCIP symbols) is what closes the precision gap
where the heuristic would otherwise overreach.

Output: `ref_resolutions.bin` — a per-file_id map of unresolved
references → candidate file_ids.

## Stage 4 — query

Every query type (`def`, `callers`, `ref`, `impact`, `callgraph`,
`grep`) consults a layered pipeline at query time:

1. **Trigram / FST filter** — narrow to candidate files (~100µs).
2. **File symbol scan** — read per-file symbol slabs, match by kind +
   name (~100µs–10ms depending on result set).
3. **Resolution lookup** — for callers/refs, traverse the
   `ref_resolutions.bin` map.
4. **Precision filter** — if a sidecar covers the file, require the
   USR / SCIP symbol of each hit to match the definition's USR /
   symbol. `--lexical` (off by default) skips this step.

Stage 4.4 is what makes scry's strict precision Kythe-grade. The
sidecars are mmap'd once per `StoreReader` (cached via `OnceLock`);
per-query cost is the algorithmic cost only, no decode tax.

## Performance ceiling

The whole pipeline is built around making query-time work cheap and
amortising indexing cost across periodic background runs:

- Stage 1 walks the corpus in O(N) per file; one-time cost on the
  initial run, incremental on subsequent runs via `files.bin` mtime
  comparisons.
- Stage 2 paths run in parallel; libclang scales linearly with TU
  count; scip-java/-kotlin are bounded by the build system's own
  intermediates.
- Stage 3 is bounded by the size of the cross-file ref set;
  re-resolution today is global per run (incremental delta is on
  the roadmap).
- Stage 4 reads only the per-query slice of the sidecars
  (`precompute_by_file_ids` + bisect-on-records); 0.5 s wall for an
  AOSP-wide `callers` query against the 14 M-record SCIP sidecar.

See `docs/BENCHMARKS.md` for live numbers on the AOSP + Linux +
Perfetto corpora.

## Restartability + caching

- Stage 2 sidecars are atomically renamed; the previous `.bin`
  survives a crashed run.
- `carry_over_sidecars` preserves `clang_usrs.bin` / `scip_index.bin`
  across a `scry index` rerun, so the fast-path rebuild doesn't
  invalidate Kythe-grade precision.
- `scry build-symbols --build-soong --append` (the default for
  per-language polyglot SCIP imports) merges into the existing
  sidecar via `ScipIndex::iter_records` rather than re-decoding the
  whole file.

## File-level pointers

| Stage | Crate / module |
|-------|----------------|
| 1     | `scry-cli/src/main.rs::cmd_index` + `scry-walker`, `scry-lang` |
| 2a    | `scry-clang/src/lib.rs::build_clang_usrs` |
| 2b    | `scry-bridge/src/soong.rs`, `scry-scip/src/lib.rs::import_scip` |
| 2c    | `scry-bridge/src/{rust,go,typescript,python}.rs`, `scry-scip` |
| 3     | `scry-cli/src/main.rs::cmd_build_resolutions` |
| 4     | `scry-cli/src/main.rs::apply_precision_filter` |
| Sidecar reader | `scry-store/src/{clang_usrs,scip_index,precision_packed}.rs` |
| Sidecar writer | `scry-store/src/precision_packed.rs::write` |
