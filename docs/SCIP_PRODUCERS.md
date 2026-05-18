# SCIP producer matrix

scry's precision sidecars (`scip_index.bin`, `clang_usrs.bin`) are
populated by `scry build-symbols`. The native path runs Kythe v0.0.75
indexers under the hood; the `--scip FILE` escape hatch imports a
pre-built SCIP protobuf from any external producer. The same query
path (`scry ref` / `callers` / `callgraph` / `impact`) consumes both
without per-flag opt-in — precision is automatic when a sidecar is
present.

**Quick install:** every producer below is installed in one shot by
`scripts/install_indexers.sh` (see [`BUILD_AWARE.md`](BUILD_AWARE.md)).

| Language          | Producer                      | Generate command                                                                  | Notes                                                                                                                                |
|-------------------|-------------------------------|-----------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| **TypeScript**    | `scip-typescript`             | `npm i -D @sourcegraph/scip-typescript && npx scip-typescript index`              | Standard producer. Outputs `index.scip`.                                                                                             |
| **JavaScript**    | `scip-typescript`             | same as above; pass `.js` files via your `tsconfig.json`                          | Same producer, allows JS via `allowJs: true`.                                                                                        |
| **Python**        | `scip-python`                 | `npx @sourcegraph/scip-python index .`                                            | Requires Python 3.8+ on PATH.                                                                                                        |
| **Java**          | Kythe kzip                    | `scry build-symbols --build-kzip PATH.kzip`                                       | scry's preferred path for AOSP / Bazel / anything Kythe-integrated. Use `scip-java` (then `--scip`) for standalone Gradle / Maven.   |
| **Kotlin**        | Kythe kzip                    | `scry build-symbols --build-kzip PATH.kzip`                                       | scry's only reliable Kotlin path. Use a Kythe-aware build.                                                                           |
| **Go**            | `gopls`                       | `gopls scip ./...`                                                                | Standard distribution; no extra install on most setups.                                                                              |
| **Rust**          | `rust-analyzer`               | `rust-analyzer --output-scip index.scip`                                          | Available in any `rustup component add rust-analyzer` install. For AOSP, Rust ships in the same kzip via Soong's `xref_rust`.        |
| **C / C++ / ObjC** | Kythe kzip                   | `scry build-symbols --build-kzip PATH.kzip`                                       | Preferred path. The kzip is produced by `cxx_extractor` (standalone, reads `compile_commands.json`) or by Soong's `xref_cxx` on AOSP. |
| **C / C++ / ObjC (alt)** | `lsif-clang`           | `lsif-clang --project-root=. compile_commands.json > index.scip`                  | Use when you already have lsif-clang wired into a non-Kythe build.                                                                   |
| **Ruby**          | `scip-ruby`                   | `scip-ruby --index-file index.scip`                                               | https://github.com/sourcegraph/scip-ruby                                                                                             |
| **C#**            | `scip-dotnet`                 | `dotnet scip --output index.scip`                                                 | Preview as of 2026-05.                                                                                                               |

## Plumbing into scry

```bash
# Build the scry base index for your source tree (one-time per source
# change). This produces the lexical layer: symbols.bin, refs.bin,
# names.fst, etc.
scry index /path/to/repo -o /path/to/scry-index

# Then layer in precision. Pick one route:

# Route A — kzip from a Kythe-integrated build (AOSP, Bazel, custom).
# Spawns the Kythe v0.0.75 indexers per CU and writes both
# clang_usrs.bin (cxx) and scip_index.bin (jvm / go / proto / textproto)
# directly. --source-root tells scry-kzip what the kzip's relative
# paths are anchored to.
scry build-symbols \
  --build-kzip /path/to/all.kzip \
  --source-root /path/to/repo \
  --index /path/to/scry-index

# Route B — pre-built SCIP file from a non-Kythe producer (scip-typescript,
# gopls scip, rust-analyzer, lsif-clang, etc.). Same destination sidecar.
scry build-symbols \
  --scip /path/to/index.scip \
  --index /path/to/scry-index

# Verify the sidecars loaded and the index is otherwise healthy.
scry health --index /path/to/scry-index

# Use it. No precision flag needed — `ref` / `callers` / `callgraph` /
# `impact` automatically narrow by Kythe-class symbol identity when
# clang_usrs.bin / scip_index.bin are present.
scry ref Animal --index /path/to/scry-index
scry callers Foo --index /path/to/scry-index
```

`scry health` prints `clang_usrs    v1, N USRs, M records` and
`scip_index    v1, N symbols, M records` lines whenever the sidecars
are present and round-trip cleanly.

## Filter composition

Three precision narrowings stack, applied in this order to the
underlying ref / callers lookup:

1. **Reachability** (`--reachable`) — drop refs in modules the build
   graph proves can't reach the callee's module. Requires
   `module_graph.json` (`scry build-modgraph`).
2. **Clang USR identity** — drop refs whose clang USR ≠ the def's
   USR. Active whenever `clang_usrs.bin` is present.
3. **SCIP symbol identity** — drop refs whose SCIP symbol ≠ the
   def's symbol. Active whenever `scip_index.bin` is present.

Sidecars are *narrowing* filters: a site without coverage in a given
sidecar passes through unchanged (an "uncovered TU" in the
diagnostic), so a partial sidecar removes false positives where it
can without blocking lookups where it can't. The default precision
behavior reports the funnel — `15 → 10 refs (clang: 0 id-mismatch,
0 uncovered TU; SCIP: 5 id-mismatch, 0 uncovered TU)` — so you can
distinguish "sidecar disagreed" from "sidecar didn't see this file".

Pass `--lexical` on any query to opt OUT of precision and see the
raw lexical-only candidate set; useful when you suspect the sidecar
is too narrow or you want to compare scopes.

## Picking a route

- **Kzip → Route A.** Single dependency (the Kythe release tree),
  one indexer-per-language stable wire format, both `clang_usrs.bin`
  and `scip_index.bin` populated from one ingest. Required for AOSP
  (Soong only emits kzip for cross-references, not SCIP).
- **External SCIP → Route B.** Use when an upstream tool already
  produces SCIP and you don't want to maintain a Kythe pipeline:
  scip-typescript for TS / JS, gopls for Go, rust-analyzer for
  standalone Rust workspaces. Route B writes the same
  `scip_index.bin`, so downstream query behavior is identical.

For C/C++ codebases without an existing SCIP pipeline, Route A's
kzip path is the lower-friction option since the cxx kzip extractor
ships in the Kythe release tree.
