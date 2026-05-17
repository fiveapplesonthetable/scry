# Build-aware indexing

This file has two halves. The first is the **user-facing quick start**:
how to make scry consume compiler-backed indexer artifacts
(`compile_commands.json`, `*.scip`) for each major language + build
system. The second is the original v0.1.12 **design narrative** that
shaped the precision sidecars — kept for historical context.

For language-by-language SCIP producer commands (one-liners per tool),
see [`SCIP_PRODUCERS.md`](SCIP_PRODUCERS.md). This file focuses on
**how to point scry at the artifacts those tools produce.**

---

## Quick start

### Install every indexer in one shot

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/fiveapplesonthetable/scry/master/scripts/install_indexers.sh)
```

What it does (idempotent):
- Installs `libclang`, `JDK`, `Go`, `node`, `npm` via your distro
  package manager (apt-get / dnf / brew).
- Installs `scip-typescript`, `scip-python` via npm into
  `$PREFIX/lib/node_modules/.bin` (default `$PREFIX=~/.local`).
- Installs `rust-analyzer` via `rustup component add`.
- Installs `scip-go` via `go install`.
- Downloads `scip-java` launcher from the GitHub release.
- Downloads `gradle 8.10.2` (needed by scip-java + scip-kotlin).
- Downloads `sbt 1.10.5` and uses it to build `semanticdb-kotlinc`
  from source, publishing to `~/.m2/repository` (scip-kotlin isn't
  on Maven Central — sbt publishM2 is the only install path).

After it finishes, every `scry finalize --build-out PATH` call
auto-discovers `compile_commands.json` and `*.scip` produced by
these tools without per-language flag plumbing.

### Auto-discovery

`scry finalize` auto-discovers two kinds of artifacts inside each
indexed source root:

| Artifact                  | Consumed by              | Used for                                  |
|---------------------------|--------------------------|-------------------------------------------|
| `compile_commands.json`   | `scry clang-index`       | C / C++ / ObjC (libclang USRs)            |
| `*.scip`                  | `scry scip-import`       | Java, Kotlin, Rust, TypeScript, Go, Python (SCIP-producer outputs) |

Each artifact type is matched at most once per index. If multiple
candidates are found, `scry finalize` warns and skips — pass an
explicit flag (`--clang-compile-commands` / `--scip`) to disambiguate.

The source-root walker honors `.gitignore`. Most build systems write
their outputs to gitignored directories (`out/`, `build/`, `target/`,
`.gradle/`), so a normal walk can't see them. Use `--build-out PATH`
(repeatable) to point at one or more build-output dirs that should be
walked without the gitignore filter.

### AOSP / Soong (C / C++)

```bash
# 1. Generate the compdb. SOONG_GEN_COMPDB=1 makes Soong emit
#    out/soong/development/ide/compdb/compile_commands.json during
#    the build.
SOONG_GEN_COMPDB=1 m nothing      # or any other build target

# 2. Index and finalize, pointing --build-out at the compdb dir.
scry index /path/to/aosp -o /path/to/scry-index
scry finalize \
  --index /path/to/scry-index \
  --build-out /path/to/aosp/out/soong/development/ide/compdb \
  --build-soong /path/to/aosp        # also builds module_graph.json
```

Auto-discovery picks up `compile_commands.json` from the build-out
path and runs `scry clang-index` on it. `--build-soong` separately
builds the Soong module graph so `--reachable` queries work.

### CMake (C / C++)

Out-of-tree build with the compdb in `build/`:

```bash
cmake -B build -S . -DCMAKE_EXPORT_COMPILE_COMMANDS=ON
cmake --build build

scry index . -o ../scry-index
scry finalize --index ../scry-index --build-out build
```

Or in-tree build (compdb lives in source root, source-root walker
finds it without `--build-out`):

```bash
cmake . -DCMAKE_EXPORT_COMPILE_COMMANDS=ON && cmake --build .
scry index . -o ../scry-index
scry finalize --index ../scry-index    # no flag needed
```

### Make / autotools (C / C++)

Use [`bear`](https://github.com/rizsotto/Bear) to wrap the build and
emit a compdb:

```bash
bear -- make all
scry index . -o ../scry-index
scry finalize --index ../scry-index     # bear writes ./compile_commands.json
```

### Java (Gradle / Maven / Bazel) via scip-java

```bash
coursier launch com.sourcegraph:scip-java_2.13:0.10.0 -- \
  index --build-tool gradle              # writes index.scip in cwd

scry index . -o ../scry-index
scry finalize --index ../scry-index      # picks up ./index.scip
```

### Kotlin via scip-kotlin

```bash
scip-kotlin --output index.scip src/
scry index . -o ../scry-index
scry finalize --index ../scry-index
```

### Rust via rust-analyzer

```bash
rust-analyzer scip .                     # writes index.scip
scry index . -o ../scry-index
scry finalize --index ../scry-index --build-out target
# --build-out target catches the .scip if rust-analyzer wrote it under target/
```

### TypeScript / JavaScript via scip-typescript

```bash
npm i -D @sourcegraph/scip-typescript
npx scip-typescript index                # writes index.scip
scry index . -o ../scry-index
scry finalize --index ../scry-index
```

### Go via scip-go / gopls

```bash
gopls scip ./...                         # writes index.scip
scry index . -o ../scry-index
scry finalize --index ../scry-index
```

### Python via scip-python

```bash
npx @sourcegraph/scip-python index .
scry index . -o ../scry-index
scry finalize --index ../scry-index
```

### Verifying the sidecar landed

After `scry finalize`, run:

```bash
scry health --index /path/to/scry-index
```

Look for the `clang_usrs` and `scip_index` rows: `v1, N USRs, M
records` means the sidecar built and parsed; `absent (run ...)` means
the artifact wasn't found.

Then queries auto-engage the precision filter:

```bash
scry callers Foo --index /path/to/scry-index   # auto-uses both sidecars
scry ref Bar    --index /path/to/scry-index    # same
```

`--clang-precise` and `--scip-precise` flags explicitly opt in
to one filter; absent flags use both when available.

---

## Design notes (archived)

Below is the original v0.1.12 design narrative that shaped this work.
Kept for historical context — the **Quick start** above reflects what
actually shipped.

# scry v0.1.12 design: `--build soong` (build-boundary-aware)

Status: **DESIGN** (v0.1.11 ships first; this work begins after).

The goal is to give scry **Kythe-class precision** for "real
callers of X on the AOSP build I ship", at scry's existing
**13-min-index / sub-100 ms-query** speed envelope. The trade
is one new on-disk sidecar and a heavier indexer pass that runs
once per Soong configuration — not a per-TU compiler
instrumentation cycle on every query.

## The problem this fixes

Today, `scry callers IActivityManager.startActivity` returns
every name-matched call site in the indexed tree — including:

- references in vendor modules that don't link against the
  framework (false positive: never executed in this build),
- references inside `#ifdef` branches the compiler skipped,
- references in modules without visibility into the definition's
  APEX (false positive: would fail to link),
- the same `.cpp` compiled into 4 module variants seen as one
  set, not four.

The "right" tool today is Kythe (cs.android.com is Kythe-powered).
Kythe is in maintenance mode but functional; the upstream code
lives at `github.com/kythe/kythe` and we have a local clone at
`/mnt/agent/kythe-source` for study.

scry will not reimplement Kythe. scry will adopt the **two
ideas** Kythe gets right and skip the rest:

1. Per-TU semantic extraction with **stable, mangling-aware
   symbol keys** (Kythe USRs / clang USRs).
2. A separate **build-graph join** that filters references to
   the ones that are real on a given configuration.

The rest of Kythe — the schema, the indexers across N languages,
the verifier framework, the protobuf RPC layer — is out of scope.
scry stays a single static binary.

## The user-facing surface

```
$ scry index --build soong \
    /home/zim/dev/aosp /mnt/agent/dev/linux \
    -o /mnt/agent/scry-index \
    --compile-commands out/soong/development/ide/compdb/compile_commands.json \
    --module-graph    out/soong/module-graph.json
```

`--build soong` opts into the precise-index pipeline. The two
new flags point at Soong outputs the user produces themselves:

- `compile_commands.json` from `SOONG_GEN_COMPDB=1 m` — one TU
  per entry, with exact compile flags.
- `module-graph.json` from `m json-module-graph` — every Soong
  module's deps, visibility, partition, APEX, variants.

Without `--build soong`, scry behaves exactly as v0.1.11 today.
The build-aware index is **additive** (new sidecar files), so
v0.1.11 readers continue to work against a v0.1.12 index — they
just see the name-matched view, not the precise one.

Query path stays identical:

```
$ scry callers IActivityManager.startActivity --precise
... only real callers (per the indexed build config), with
    file:line:col and the owning Soong module name attached
```

`--precise` flips the resolution mode. Without it, the existing
name-match path runs (same speed, same false positives as
v0.1.11). With it, the reachability filter runs on top of the
name-match candidate set.

## On-disk format (additive sidecars)

Three new files in the index dir:

- **`usrs.fst`** — FST mapping `clang::USR` (canonical symbol
  key, mangling-aware) → ordinal into the USR table.
- **`usr_refs.bin`** — for each USR ordinal, the list of
  `(file_id, byte_off, kind, variant_hash)` references. Same
  varint+delta encoding as `refs.bin`.
- **`module_graph.bin`** — packed Soong module graph. Per
  module: name, deps (link-time + header-lib), partition,
  visibility, APEX. Reachability bitmap precomputed at finalize
  so query path is O(1) "is module A reachable from module B".

Plus one column on `FileEntry`:

- **`file_module: Option<u32>`** — index into the module table
  for "the Soong module this file belongs to". `None` for files
  not owned by any Soong module (e.g. Linux kernel sources,
  generated outputs).

Size budget on AOSP: USR table ~5 M entries × 32 B avg = ~160 MB
sidecar; reachability bitmap ~50 k modules × 50 k bits / 8 = ~300
MB. Total +500 MB on a 9.5 GB index — acceptable.

## Indexer pipeline

`scry index --build soong` runs the existing tree-sitter passes
**plus** three new ones:

### 1. Parse `module-graph.json`

Straight JSON → packed binary. Build the per-module reachability
closure (BFS over deps) once at index time, store as bitmap.
Estimated cost: 30 s on AOSP scale.

### 2. Per-TU clang index extraction

Read `compile_commands.json`. For each TU entry, invoke a clang
indexer with the exact flags from the compdb entry. Two paths:

- **Path 1 (lower risk, ship first)**: shell out to a small
  helper binary `scry-clang-index` that uses libclang (via the
  `clang-rs` crate) to walk the TU's AST and emit one line per
  reference: `USR\tfile\tbyte_off\tkind\tvariant_hash`. scry
  reads its stdout. Helper binary keeps libclang FFI isolated.
- **Path 2 (later)**: clang `IndexAction` plugin loaded once
  per worker, kept resident — skips fork/exec per TU. Win if the
  per-TU clang startup proves dominant.

`variant_hash` = blake3 of the sorted compile-flag list. Two TUs
with the same source file but different `-D` defines get
distinct `variant_hash`es and stay separate in `usr_refs.bin`.

Throughput target: 1000 TU/min × 64 workers = 64 k TU/min. AOSP
generic_arm64-userdebug is ~120 k TUs; ~2 min on the 72-core
host. The walker phase doesn't change; this runs alongside it.

### 3. Per-file owning-module attribution

For each indexed file, walk up the Soong module graph to find
the module that owns it (Soong knows: every `srcs` glob is
attributed). Populate `file_module`. Sub-second.

## Query path: `--precise` mode

```
fn precise_callers(usr: USR, from_module: Module) -> Vec<Hit>:
  candidates = usr_refs.get(usr)              # all USR-matched refs
  for c in candidates:
    c_module = files[c.file_id].module
    if reachability[c_module][from_module]:    # bitmap intersect
      emit c
```

Three lookups: USR → posting list, file_id → module, bitmap
intersect. Sub-millisecond per hit. Same memory model as v0.1.11
(mmap + decode). No regression on warm-query latency.

## Languages covered

Phase 1 (v0.1.12): **C / C++ only**. That's where the build
boundaries actually bite hardest, and clang USRs are mature.
Java/Kotlin/Rust callers still go through the name-match path.

Phase 2 (v0.1.13+): Java via `scip-java`, Kotlin via
`scip-kotlin`, Rust via `rust-analyzer`'s index. All output the
SCIP format; scry adds one more importer per language. (SCIP is
the Sourcegraph successor to Kythe-the-format; it's actively
maintained.)

This phasing matters: getting C++ right is 80% of the value on
AOSP because the false-positive rate is worst there. Java
heuristics already cover the same-package + import cases
correctly most of the time.

## What we copy from Kythe

After `/mnt/agent/kythe-source` finishes downloading, the
specific pieces to study:

- `kythe/cxx/extractor/` — how it wraps `clang::FrontendAction`
  to capture compile invocations + all transitively-included
  files. Especially `cxx_extractor.cc`. We need this pattern
  for the per-TU helper.
- `kythe/cxx/indexer/cxx/IndexerASTHooks.{h,cc}` — the visitor
  that emits references. We only need the subset for "this is a
  call / use of USR X at location Y"; we ignore Kythe's full
  graph schema.
- `kythe/proto/storage.proto` and `kythe/proto/xref.proto` —
  reference for the on-the-wire shape. We don't adopt protobuf;
  our binary format is simpler.

## What we explicitly do NOT copy

- Kythe's graph schema (anchor → node → edge with N edge kinds).
  We have one kind: "ref(USR, file, off, variant)". Anything
  fancier costs query-time joins we don't need.
- Kythe's verifier framework. We rely on round-trip tests over a
  small fixture (a 5-file AOSP-shaped synthetic tree with a
  known-correct set of `--precise` hits).
- Kythe's xref service / protobuf RPC. scry's existing
  JSON-RPC + MCP surface gets one new arg (`precise: true`)
  per query, nothing else.
- Kythe's indexer-as-builder design (run extraction during the
  actual Soong build). scry runs the extraction once per Soong
  config from the already-built compdb, post-hoc. Loses the
  "every CI build also produces an index" property; gains the
  "you don't need to modify the build" property. Worth it for an
  external tool.

## Why "100× faster than Kythe" is realistic

Kythe's full pipeline on a large monorepo is *as expensive as a
full build* (Kythe docs). For scry, we skip:

- Per-TU re-extraction of header content (Kythe captures every
  transitively-included file; we just record refs into them).
- The graph schema's edge tables (one ref kind, not 40).
- The protobuf serialization layer (binary varint+delta
  postings, mmapped).
- Re-invocation of the build (we read the compdb after the build
  already ran once).
- N-language indexers (C++ only in phase 1).

Estimated v0.1.12 indexer cost: existing 13 min + 2 min for the
clang USR pass + 30 s for module-graph reachability = ~16 min
total. Vs. Kythe-on-Chromium-scale runs measured at hours.

## Risks / open questions

1. **`clang-rs` FFI surface**: pulling in libclang as a build
   dep adds weight and pins us to a specific clang version.
   Mitigation: ship `scry-clang-index` as a separate binary;
   `scry` core stays pure-Rust. Users opt in to `--build soong`.
2. **Soong compdb stability**: SOONG_GEN_COMPDB has changed
   shape across AOSP releases. Need to test against the live
   tree at `/home/zim/dev/aosp`.
3. **Visibility rules**: Soong visibility is a non-trivial
   language (regex globs, package qualifiers, `:any`, APEX-
   scoped). v0.1.12 phase 1 implements link-time deps only;
   visibility/APEX boundaries come in a follow-up if the link
   filter proves insufficient.
4. **Variant explosion**: AOSP `generic_arm64-userdebug` has
   ~120 k TUs; `--all-variants` (`cf_x86_64_phone`,
   `aosp_cf_arm64_phone`, …) could 5× that. v0.1.12 indexes one
   config at a time; cross-config queries return per-config
   hits separately.

## Sequencing

1. v0.1.11 ships (in progress) — fuzzy + CI grep + progress + fail log.
2. v0.1.12 milestone opens. First slice:
   - Soong `module-graph.json` parser + reachability bitmap.
   - `file_module` column on `FileEntry`.
   - `--precise` flag on `callers` / `ref` filters EXISTING
     name-match candidates by module reachability. **No clang
     USRs yet — this alone removes most cross-module false
     positives on AOSP.**
3. v0.1.12 main slice:
   - `scry-clang-index` helper binary (libclang FFI).
   - `usrs.fst` + `usr_refs.bin` writers.
   - `--precise` route through USR lookups when available.
4. v0.1.13+: scip-java, scip-kotlin, rust-analyzer integration.
