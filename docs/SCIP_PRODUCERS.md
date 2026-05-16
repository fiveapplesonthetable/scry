# SCIP producer matrix

`scry scip-import` (shipped in v0.1.16) reads any SCIP protobuf
index. The same importer + the same `scry ref --scip-precise`
filter work across every producer below; the table is here so you
can copy-paste the command to generate the SCIP file for your
language without hunting through eight tool repos.

If a producer is missing from this list, file an issue — the
format is canonical, so adding one is just a docs change.

| Language          | Producer                      | Generate command                                          | Notes                                                                 |
|-------------------|-------------------------------|-----------------------------------------------------------|-----------------------------------------------------------------------|
| **TypeScript**    | `scip-typescript`             | `npm i -D @sourcegraph/scip-typescript && npx scip-typescript index`              | Dogfooded against scry's CI fixture in v0.1.16. Outputs `index.scip`. |
| **JavaScript**    | `scip-typescript`             | same as above; pass `.js` files via your `tsconfig.json`  | Same producer, allows JS via `allowJs: true`.                         |
| **Python**        | `scip-python`                 | `npx @sourcegraph/scip-python index .`                    | Requires Python 3.8+ on PATH.                                         |
| **Java**          | `scip-java`                   | `coursier launch com.sourcegraph:scip-java_2.13:0.10.0 -- index --build-tool gradle` | Works with Gradle / Maven / Bazel.                                    |
| **Kotlin**        | `scip-kotlin`                 | `scip-kotlin --output index.scip src/`                    | https://github.com/sourcegraph/scip-kotlin                            |
| **Go**            | `gopls`                       | `gopls scip ./...`                                        | Standard distribution; no extra install on most setups.               |
| **Rust**          | `rust-analyzer`               | `rust-analyzer --output-scip index.scip`                  | Available in any `rustup component add rust-analyzer` install.        |
| **C / C++ / ObjC** | `lsif-clang`                 | `lsif-clang --project-root=. compile_commands.json > index.scip`                  | Alternative to in-tree `scry clang-index`; useful when you already have lsif-clang in your pipeline. |
| **Ruby**          | `scip-ruby`                   | `scip-ruby --index-file index.scip`                       | https://github.com/sourcegraph/scip-ruby                              |
| **C#**            | `scip-dotnet`                 | `dotnet scip --output index.scip`                         | Preview as of 2026-05.                                                |

## Plumbing into scry

After running any of the above, you get an `index.scip` file. Then:

```bash
# Build the scry index for your source tree (one-time per source change).
scry index /path/to/repo -o /path/to/scry-index

# Import the SCIP sidecar (one-time per SCIP regenerate).
scry scip-import \
  --scip /path/to/index.scip \
  --index /path/to/scry-index \
  --root /path/to/repo   # only needed if SCIP's project_root is wrong/missing

# Verify it loaded.
scry scip-stats --index /path/to/scry-index

# Use it. --scip-precise drops name-collision noise.
scry ref Animal --index /path/to/scry-index --scip-precise
scry callers Foo --index /path/to/scry-index --scip-precise
```

## Composing filters

The three precision filters compose; they're applied in this order
on the result of the underlying ref/callers lookup:

1. **`--reachable`** — drop refs in modules the build graph proves
   can't reach the callee's module. Requires `module_graph.json`
   (`scry build-modgraph`).
2. **`--clang-precise`** — drop refs whose clang USR ≠ the def's
   USR. Requires `clang_usrs.bin` (`scry clang-index`).
3. **`--scip-precise`** — drop refs whose SCIP symbol ≠ the def's
   symbol. Requires `scip_index.bin` (`scry scip-import`).

Any subset can be enabled. Filters that don't have their sidecar
print a one-line stderr note and pass through unchanged. Sites
without coverage in a given sidecar also pass through — these
filters only *remove* false positives, they never *block* lookups
for code the sidecar didn't see.

## Tradeoffs

- **Path B (`scry clang-index`)** lives in-tree, runs libclang
  per-TU, needs your `compile_commands.json`. C/C++/ObjC only.
- **Path C (`scry scip-import`)** ingests an external SCIP file
  that someone else's tool produced. Covers every language with
  a SCIP indexer, including C++ via `lsif-clang`. Cost: the
  external tool is its own dependency.

For C++ codebases that already produce SCIP via lsif-clang, prefer
Path C — one less moving part. For C/C++/ObjC codebases without an
existing SCIP pipeline, Path B is the lower-friction option since
`scry clang-index` ships in the scry binary.
