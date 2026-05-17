# Changelog

All notable changes to scry are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.27] — 2026-05-17

Honest Java method-call resolution in `scry build-resolutions`.
v0.1.26 shipped `--def-in PATH` to narrow xrefs by def-site
location, but it returned 0 hits on the live index because the
Layer 2 resolver was over-confidently pinning every ambiguous
method call (e.g. all 65 503 `close()` calls on AOSP+Linux) to
one arbitrary `pool[0]` candidate, hiding the real call graph.

**Resolver changes (`resolve_one` in `cmd_build_resolutions`):**

- **Same-file preference (all langs).** A call to `foo()` inside
  file F that has a unique `foo` def in F now resolves there
  first, before falling through to per-language narrowing.
  Catches self-calls and inner-class refs without needing
  receiver-type inference.
- **Truthful unresolved for ambiguous method calls.** When a
  `Call` / `Ctor` / `FieldAccess` ref remains ambiguous after
  all narrowing rules, the resolver now returns 0 (unresolved)
  instead of picking the misleading `pool[0]`. Without
  receiver-type inference we cannot honestly pick between
  many same-named methods, so the right behavior is to be
  silent — and let `--def-in PATH`'s permissive branch include
  the ref as "we don't know, keep it." Other ref kinds
  (`TypeUse`, `Import`, `InheritFrom`, `UsingNamespace`) keep
  the `pool[0]` fallback — types referenced unqualified are far
  less ambiguous than methods.

**Impact on `--def-in PerfettoTrace.java`:**

- Before: 65 503 `close()` refs → 0 kept (all mis-resolved to
  the same canonical close).
- After (post-rebuild): same-file calls land on the right
  PerfettoTrace.Session.close, cross-file ambiguous calls
  pass through unresolved-but-kept. Users see the actual
  call graph.

**To take effect on an existing index:**
- Run `scry build-resolutions --index DIR` to rewrite the
  resolutions sidecar. No symbols/refs rebuild needed.
- New tests: `resolve_one_java_call_ambiguous_returns_unresolved`,
  `resolve_one_java_typeuse_ambiguous_keeps_pool0_fallback`,
  `resolve_one_same_file_preference`.

**Parallel `scry build-resolutions` (pass 3):**

- Resolution is now parallelized via rayon: refs are batched
  into 64K-record chunks, dispatched to a worker pool, and
  written back in iteration order (the sidecar format requires
  `byte_offset = ref_idx * 8`).
- Live AOSP+Linux corpus (63.3 M refs) measurement:
  - pass 3 before: ~11+ min (single-threaded, killed)
  - pass 3 after: **61 s** (~11×+ on a 16-core box)
- Atomic tmp+rename is unchanged; failed runs leave the
  previous sidecar intact.

**Live AOSP+Linux numbers after the rebuild:**

- 25.6 M refs resolved (vs ~60 M previously — the drop is
  expected and correct; we no longer claim false certainty
  on ambiguous method calls).
- 7.5 M refs narrowed via Java context (same-pkg / imports /
  same-file preference / C++ namespace).
- `scry callers close --def-in PerfettoTrace.java` now returns
  57 890 over-included candidate callers (previously returned 0).

## [0.1.26] — 2026-05-17

Two agent-critique fixes bundled together.

**`--def-in PATH` for `ref` / `callers`:**

- New flag lets you narrow xrefs to a specific callee location
  when many unrelated classes share a method name. Example:
  `scry callers close --def-in PerfettoTrace.java` keeps only
  refs whose Layer 2 resolution (`resolved_to`) points at a
  def whose file path contains "PerfettoTrace.java".
- Refs that build-resolutions couldn't narrow (`resolved_to =
  None`) pass through unfiltered — we'd rather over-include
  than silently drop the ones cross-file resolution didn't
  reach. CLI prints a one-line stderr summary
  (`N → M refs (K resolved to a def in scope, M-K
  unresolved-but-kept)`).
- Wired through CLI, JSON-RPC, and MCP. Daemon path stays
  silent (no per-request diagnostics).
- Empty target def set ⇒ no-op narrowing + a diagnostic
  suggesting either a better path substring or rebuilding the
  resolutions sidecar.

**`grep` no-literal-extracted regex fallback:**

- When `grep_candidates_for_regex` couldn't extract any usable
  literal seed from a regex with too much alternation /
  character-class noise (e.g.
  `Trace\.traceBegin.*[Bb]roadcast`), the trigram pre-filter
  returned an empty candidate set and the search silently
  returned 0 hits. Fixed by collapsing `Some(empty) → None`
  so the search falls back to a full content scan. Regression
  test `grep_regex_with_lossy_literals_falls_back_to_full_scan`
  covers the original failing pattern.

## [0.1.25] — 2026-05-17

`scry uses NAME` — outgoing edges from NAME's body. Symmetric
counterpart to `scry callers NAME`: for each def of NAME, walks
refs whose `byte_start` falls in NAME's body byte range
(computed via the next-function heuristic from v0.1.20) and
returns them. Plus a quieter version warning.

**`scry uses`:**

- CLI: `scry uses NAME [--in PREFIX] [--kind call] [--limit N] [--json]`.
- JSON-RPC + MCP: `{"cmd":"uses","args":{"name":"...","kind":"call"}}`.
- New file_refs sidecar (`<index>/file_refs.bin` +
  `file_refs_offsets.bin`) makes per-file ref lookup O(1)
  instead of scanning 63M refs. ~245MB on AOSP+Linux. Built by
  `scry build-file-refs` or auto via `scry finalize`.
- Without the sidecar, `uses` falls back to a linear scan with
  an actionable stderr warning suggesting the build command.
- Live AOSP measurement (`uses bindServiceLocked`):
  - without file_refs: 12.6 s (linear scan)
  - with file_refs: **306 ms** (~41×)
- E2E test (`uses_e2e_outgoing_edges`) builds a 4-method Java
  fixture and asserts `uses run` returns ONLY the calls inside
  `run()`'s body, not `main()`'s call to the same method.

**Quieter version warning:**

- The "index built with scry X.Y.Z; running A.B.C" stderr line
  used to fire on any version mismatch, including patch-level
  drift (0.1.17 → 0.1.24). Patch releases are bugfix-only and
  don't invalidate indexes, so the warning was alarmist noise.
- Now only fires when major.minor differs (0.1.x → 0.2.x), so
  the user only sees it when an actual rebuild is worth doing.

**Plumbing:**

- `scry finalize` adds `build-file-refs` to its always-run
  stages so freshly-built indexes get `uses` precision for
  free.
- `StoreReader::refs_for_file(file_id)` mirrors the existing
  `symbols_for_file` shape, reusing the same packed decoder.

## [0.1.24] — 2026-05-17

`--reachable` cold latency 22s → 4.7s (~5×) via an on-disk
cache for the Warshall reachability bitmap. The bitmap itself
(~1GB on AOSP's 91k modules) is what made `--reachable`
unaffordable on every cold CLI invocation; computing it from
scratch was the only slow part of building `ModuleGraph` from
`module_graph.json`. Now we compute once, write to
`<index>/module_graph_reach.bin`, and read it back on every
later open.

**Cache layout (locked v1):**

```
bytes  0..  9  magic = b"scryREAC1"
bytes  9.. 13  format version (u32)
bytes 13.. 21  n_modules (u64)
bytes 21.. 29  stride (u64)
bytes 29.. 61  binding hash (32 bytes; blake3 of source JSON)
bytes 61..     raw u64 reachability bitmap
```

Any header mismatch (missing file, wrong magic / version /
hash / dimensions) silently falls back to a full Warshall +
fresh cache write — corruption is self-healing. Atomic
tmp + rename for the write path.

**End-to-end on AOSP `/mnt/agent/scry-index` (cold cache):**

| Query                                     | v0.1.23 | v0.1.24 |
|-------------------------------------------|---------|---------|
| `ref bindService` (default)               |   333ms |   333ms |
| `ref bindService --reachable` (first)     | 22 s    | 22 s (writes cache) |
| `ref bindService --reachable` (later)     | 22 s    |  **4.7 s** |

Two new unit tests cover round-trip and binding-hash invalidation.

## [0.1.23] — 2026-05-17

Three connected fixes, all from a cold-start eval agent's critique
of v0.1.21:

**1. Cold-open 45s → 342ms (130× faster).**
   `module_graph.json` (256MB on AOSP) was eager-parsed inside
   every `StoreReader::open`, so every `scry def` paid ~30s of
   JSON-parse + Warshall reachability build even when the query
   didn't need it. Made it lazy via `OnceLock`: first
   `module_graph()` call pays the cost, queries that never touch
   it (def, outline, coverage, grep, plain ref/callers) skip it
   entirely. All 9 call sites in scry-cli switched to the
   accessor.

**2. `--no-precise` flag; precision now default-on.**
   `--clang-precise` and `--scip-precise` were opt-in flags
   nobody knew to pass. Per user feedback ("no need for a
   billion flags. just have precise as a flag enabled by
   default"), precision now auto-engages: clang USR + SCIP
   identity filters apply automatically when their sidecars are
   present, no-op gracefully when absent. Pass `--no-precise`
   for raw name-match results. `--reachable` stays explicit
   opt-in because the module_graph parse is the expensive one.
   Individual flags (`--reachable`, `--clang-precise`,
   `--scip-precise`) still work, just hidden from `--help`.

**3. `--scope CLASS` filter on ref / callers.**
   Drops the 1417-unfilterable-hits problem on hub functions:
   `scry callers traceBegin --scope BroadcastQueueImpl` keeps
   only call sites whose enclosing scope_path contains
   `BroadcastQueueImpl` as an exact segment. Cheap exact match;
   pairs naturally with the auto-on clang/scip precision for
   really tight result sets on overloaded names.

Result for the agent's specific query path
(`scry callers bindService --index /mnt/agent/scry-index`):
   v0.1.21 cold: 45000ms
   v0.1.23 cold: 328ms
   v0.1.23 cold with explicit `--reachable`: 44000ms (graph parse)

## [0.1.22] — 2026-05-17

CLI inconsistency fix: `scry fuzzy` now accepts `--in PREFIX`,
matching the same flag on `def` / `ref` / `callers` / `grep` /
`subclasses` / `impact` / `callgraph`. Previously the daemon
(JSON-RPC + MCP) `fuzzy` tool already honored an `in` arg via
`serve_fuzzy_with_distance`; the CLI alone was the outlier.
The fuzzy ranker still sees the full candidate set; the prefix
test is applied to the ranked output so a tight subdir filter
doesn't bury closer matches.

```
scry fuzzy bindservice --in frameworks/base/services/ --distance 4
```

Caught by a cold-start eval agent who reasonably expected
`--in` to be uniform across symbol-name queries.

## [0.1.21] — 2026-05-16

\`scry finalize\` — one-shot post-index sidecar pipeline. Rebuilds
every sidecar scry's query path knows how to use, in one
command, with per-stage timings. Discoverable from \`--help\` so
new users don't have to learn the build-* command zoo.

\`\`\`bash
# After scry index finishes:
scry finalize --index /path/to/idx \\
  --build-soong /path/to/aosp \\
  --scip /path/to/index.scip \\
  --clang-compile-commands /path/to/compile_commands.json
\`\`\`

Stages run in order, fail-fast on any error:
1. \`build-offsets\`        — random-access readers.
2. \`build-file-symbols\`   — outline + enclosing_function fast path.
3. \`build-trigrams\`       — literal grep 100×.
4. \`build-resolutions\`    — Layer 2 ref → def overrides.
5. \`build-modgraph KIND\`  — module_graph.json (one of soong /
   kernel / gn / bazel / cargo; first non-None wins, since only
   one module_graph fits per index).
6. \`scip-import FILE\`     — Path C cross-language precision.
7. \`clang-index FILE\`     — Path B C/C++/ObjC precision.

Each stage is opt-in via its \`--build-<kind>\` / \`--scip\` /
\`--clang-compile-commands\` flag. The first four always run; the
later ones only when their input flag is provided.

This makes the AOSP rebuild ritual a single command instead of
chaining six. The post-finalize script (\`scripts/post_finalize.sh\`)
will start using it in a follow-up.

## [0.1.20] — 2026-05-16

`scry callgraph NAME` — recursive callers tree. LLM-shaped
"how does control flow reach this function?" query, walking
N levels up with cycle detection and a node-budget cap.

**CLI:**

- `scry callgraph NAME [--in PREFIX] [--depth N] [--max-nodes N]
  [--reachable] [--json]`. Default depth 3, max-nodes 200.
- Indented-tree output by default; `--json` returns a structured
  `{callee, depth, max_nodes, callers}` tree for programmatic use.

**JSON-RPC + MCP:**

- New `callgraph` tool. Same args as the CLI.

**Internals:**

- New `StoreReader::enclosing_function(file_id, byte_offset)` —
  finds the function/method-like symbol whose source body encloses
  a byte offset. Uses the `file_symbols` sidecar + sort-and-
  partition-point on `byte_start` because tree-sitter records the
  identifier-only range for symbols (not the full body), so the
  obvious `byte_start <= o < byte_end` check doesn't work directly.
- Caller attribution prefers `enclosing_function` over
  `RefRecord.scope_path.last()` (which on Java reports the
  enclosing class, not the method).
- E2E test (`callgraph_e2e_walks_caller_chain`) builds a 4-method
  Java chain `d() → c() → b() → a()` and validates depth=1 vs
  depth=3 expansion via both CLI and the implicit JSON shape.

## [0.1.19] — 2026-05-16

`scry health` now reports the three precision sidecars
(`module_graph.json`, `clang_usrs.bin`, `scip_index.bin`) at
both the raw-file-presence level and as structured per-sidecar
checks (version + record counts + actionable "run X to
generate" hints when absent).

Output excerpt:

\`\`\`
  [ OK] module_graph.json     OK (256477806 bytes)
  [ OK] clang_usrs.bin        OK (49560901 bytes)
  [ OK] scip_index.bin        absent (optional)
  [ OK] module_graph          v1, 91347 modules, 552496 dep edges, 1397276 file attributions
  [ OK] clang_usrs            v1, 17822 USRs, 541626 records
  [ OK] scip_index            absent (run \`scry scip-import\`)
\`\`\`

A corrupt sidecar (wrong version, bad protobuf, malformed JSON)
surfaces as a soft warning with the parse error inline so you
can diagnose without re-running every precision command.

## [0.1.18] — 2026-05-16

`scry impact NAME` — "what breaks if I change NAME?" — composes
callers + transitive subclasses into a single deduped impact set.
LLM-shaped pre-flight check before refactors: small counts → safe
rename; large counts → split the change.

**CLI:**

- `scry impact NAME [--in PREFIX] [--subclass-depth N]
  [--reachable] [--limit N] [--json]`. Default depth = 2 (covers
  most class-hierarchy refactors; raise for deep hierarchies).
- Default output shows the three totals up top
  (`callers / subclasses / files_touched`) then up to `--limit`
  rows of each, grouped. `--json` returns the same payload as
  the JSON-RPC handler.

**JSON-RPC + MCP:**

- New `impact` tool taking `name` (required), `in`, `limit`,
  `subclass_depth`, `reachable`. Returns
  `{ name, callers[], subclasses[], files_touched[], totals }`.
  Same shape as the CLI `--json` output so an LLM can consume
  either uniformly.

**Composes with `--reachable`** on the callers leg — same
build-graph semantics as `scry callers --reachable`. The subclass
leg is not reachability-filtered because inheritance edges don't
respect build deps (a child class can live anywhere that imports
the parent's header).

**E2E test** (`impact_e2e_via_cli_and_rpc`) builds a 4-class Java
fixture (Animal → Dog → Puppy plus a Caller invoking `Animal.speak`)
and validates that `impact Animal` returns the right subclasses,
the caller, and the union of touched files via both CLI and
JSON-RPC paths.

## [0.1.17] — 2026-05-16

Real-producer validation for v0.1.16's SCIP importer + producer
matrix doc.

- Validated the importer end-to-end against `scip-typescript`'s
  output on a tiny TS fixture (\`Animal\` / \`Dog\` classes): 8
  unique symbols, 12 occurrences, byte_offset alignment lined
  up exactly with scry's tree-sitter def site so
  \`--scip-precise\` correctly resolved a def symbol on real
  TypeScript SCIP records.
- New \`docs/SCIP_PRODUCERS.md\` lists every SCIP producer
  scry's importer supports (TypeScript, JavaScript, Python,
  Java, Kotlin, Go, Rust, C/C++/ObjC via lsif-clang, Ruby,
  C#) with the exact CLI to generate the index, plus filter-
  composition rules and Path B vs Path C tradeoffs.

## [0.1.16] — 2026-05-16

SCIP importer + `--scip-precise` filter — Path C. Brings symbol-
identity precision to every language with a SCIP indexer (Java,
Kotlin, Go, Rust, TypeScript, Python, …) without us writing per-
language parsers. Reuses the same alignment-window + filter
composition that already powers `--clang-precise`.

**Three new subcommands on the main `scry` binary:**

- `scry scip-import --scip FILE.scip --index DIR` — read a SCIP
  protobuf (emitted by `scip-java`, `scip-kotlin`,
  `rust-analyzer --output-scip`, `gopls scip`, `scip-typescript`,
  `scip-python`, `lsif-clang`, …), translate each occurrence's
  `(line, col)` to a `byte_offset` against the source on disk,
  intern symbols, write `scip_index.bin`. Use `--root PATH` to
  override the SCIP file's `project_root` (CI vs local checkout).
- `scry scip-stats --index DIR` — sidecar shape with sample
  symbols; helpful "run scip-import first" message if absent.
- `scry scip-lookup --index DIR --path P --offset N` — scriptable
  symbol-at-location lookup. Empty stdout when no record covers.

**`--scip-precise` filter (CLI + JSON-RPC + MCP):**

- `scry ref NAME --scip-precise`, `scry callers NAME --scip-precise`.
- Composes after `--clang-precise` and `--reachable`. All three
  stack: build-graph reachability → clang USR → SCIP symbol, then
  the result is what you see.
- MCP tools `ref` and `callers` advertise `scip_precise: boolean`.
- Sites without a SCIP record pass through unchanged — the filter
  only removes false-positive name collisions, never blocks
  lookups for code outside the SCIP coverage.

**Internals:**

- New crate `scry-scip` holds the protobuf-decoding logic + line→
  byte offset translation. Depends on Sourcegraph's official
  `scip` crate (0.7) + `protobuf` (3.x) for `Index::parse_from_bytes`.
- New `scry-store::scip_index::ScipIndex` reader — same shape as
  `ClangUsrIndex`: `(abs_path, byte_offset)` exact map + per-path
  sorted offset list for `symbol_for_window`. 4 unit tests cover
  exact + window + missing + bad-version.
- End-to-end test in `scry-scip` builds a synthetic 1-document
  `Index` in memory, writes it as protobuf, re-imports via
  `import_scip`, and validates the sidecar round-trip including
  line/col → byte_offset math.
- Sidecar schema v1 locked:
  `ScipSidecar { version, symbol_table: Vec<String>,
  records: [{ abs_path, byte_offset, symbol_id, role (u8) }] }`.
- `role` keeps the low 8 bits of SCIP's `symbol_roles` bitmap
  verbatim — `0x01` Definition / `0x02` Import / `0x04` WriteAccess /
  `0x08` ReadAccess / etc. — so future filters can read them.

## [0.1.15] — 2026-05-16

Type-hierarchy queries: `scry subclasses NAME` and `scry implementations
NAME` (alias). LSP analogues `typeHierarchy/subtypes` and
`implementationProvider`. No new sidecar — walks the tree-sitter
`InheritFrom` refs that the indexer already records, with a same-file
scope-anchored resolution so the child class lands as a real
SymbolRecord (kind/lang/scope intact).

**CLI:**

- `scry subclasses Activity --in frameworks/base/` → 597 direct
  subclasses on AOSP, ranked. `--depth N` walks transitively (BFS,
  bounded to keep pathological hierarchies tractable; `--depth 0`
  = direct only).
- `scry implementations IBinder` — same algorithm, Java-flavored naming.

**JSON-RPC + MCP:**

- New tools `subclasses` and `implementations`, both taking `name`
  (required), `in`, `limit`, `depth`. Behaviorally identical;
  exposed as separate tools so LLM clients pick the right verb
  for the domain (Java callers vs C++ inheritance).

**Internals:**

- New `StoreReader::subclasses(parent)` / `subclasses_transitive(parent, depth)`.
  For each `InheritFrom` ref to `parent`, the child is identified by
  `scope_path.last()`; the outer scope (`scope_path[..last]`) +
  `file_id` resolve back to a SymbolRecord via the existing name FST.
- E2E test (`subclasses_e2e_via_cli_and_rpc`) builds a 3-class Java
  hierarchy (Animal → Dog → Puppy) and validates direct + transitive
  + alias + JSON-RPC paths in one fixture, well under a second.
- Updated `mcp_required_args_for` so the schema/validator drift test
  keeps catching new tools at compile time.

## [0.1.14] — 2026-05-16

`--clang-precise` ref/callers filter — Path B's payoff. Uses the
clang USR sidecar to drop name-collision noise for C/C++/ObjC: a
ref is kept only if its `(file, byte_offset)` site maps to the same
USR as the def. Wired into both the CLI and the JSON-RPC + MCP
surfaces, with the same alignment-window the daemon uses internally.

**What this fixes:**

- Multiple decls of `Hash` / `Init` / similar common identifiers
  across vendored deps no longer pollute `scry ref Hash`. The filter
  keeps only the ref sites whose clang USR matches a def's USR.
- Sites without a clang record (non-C/C++ files, or TUs the user
  didn't pass to `scry clang-index`) pass through unchanged — the
  filter only *removes* incorrect hits, never blocks lookups.

**Wiring:**

- CLI: `scry ref NAME --clang-precise`, `scry callers NAME --clang-precise`.
- JSON-RPC: `{cmd: "ref", "args": {"clang_precise": true, ...}}`.
- MCP: tools `ref` and `callers` advertise `clang_precise: boolean`.
- All three compose with `--reachable` (build-graph reachability +
  USR identity, applied in sequence).

**Internals:**

- New `ClangUsrIndex::usr_for_window(path, offset, window)` —
  binary-searches the per-path sorted offset list and returns the
  USR of the closest record within `±window` bytes. Defaults to a
  64-byte window in both CLI and daemon paths because clang's
  cursor location for struct/class/typedef decls sits at the
  *keyword* while tree-sitter's `byte_start` sits at the
  *identifier*. 64 covers all real-world widths without colliding
  across distinct decls.
- 5th unit test in `scry-store::clang_usrs` covers
  `usr_for_window` (exact + window + miss + cross-path).

## [0.1.13] — 2026-05-16

Path B precision (clang USR sidecar). Three new subcommands on the
main `scry` binary — no separate tool to install:

- `scry clang-index --compile-commands FILE --index DIR` parses each
  TU through libclang and writes `clang_usrs.bin` next to the main
  scry index. Cross-TU symbol identity for C/C++/ObjC: two `foo`s
  with the same name but different declarations get distinct USRs.
  Dogfooded on trace_processor_d (93 TUs, 23k unique USRs, 1.4M
  records, 11 s including parse).
- `scry clang-stats --index DIR` reports sidecar shape (USR count,
  record count, sample USRs) with a helpful message if the sidecar
  is missing.
- `scry clang-lookup --index DIR --path P --offset N` returns the
  USR (or empty) for a source location. Scriptable.

**Internals:**

- `libclang` loaded dynamically at runtime via `clang-sys`'s
  runtime feature → no compile-time LLVM dep, no extra binary.
  Per-thread loading via `thread_local!` so rayon workers each get
  their own instance.
- System-header records dropped (`/usr/include`, `/usr/lib/gcc`,
  `/usr/lib/llvm-*`, `/usr/lib/x86_64-linux-gnu`) — cuts sidecar
  size by ~74%.
- Sidecar schema v1 (locked):
  `UsrSidecar { version, usr_table, records: [{ abs_path,
  byte_offset, usr_id, kind ∈ {0=decl, 1=ref, 2=call} }] }`.
- New crate layout: `scry-clang` holds the `unsafe` libclang FFI
  so `scry-cli` stays `#![forbid(unsafe_code)]`. Single binary
  externally; cleaner internals.
- `scry-store::clang_usrs::ClangUsrIndex` reader, indexed by
  `(abs_path, byte_offset)` for O(1) lookups; 4 unit tests.

The full ref/callers `--clang-precise` filter (use the USR to
match ref sites against the def's USR instead of name alone)
lands in v0.1.14 — needs careful tree-sitter ↔ clang location
alignment, which is its own slice of work.

## [0.1.12] — 2026-05-16

Build-graph-aware precision: scry now understands module boundaries
in five build systems and can prune callers/refs to only those
reachable across declared dependency edges.

**Highlights:**

- New `--reachable` flag on `scry callers` and `scry ref` (also
  exposed via JSON-RPC and MCP). When a `module_graph.json`
  sidecar is present in the index, results are filtered to
  refs whose owning module can reach the callee's module
  through the build graph's transitive closure. Validated on
  AOSP: `bindService` callers go from 1981 → 1567 hits (~21%
  unreachable noise pruned) with ~300 ms filter overhead.

- New `scry build-modgraph` subcommand emits a canonical v1
  `module_graph.json` from any of five build systems:
  - **`cargo`** — Rust workspaces (Cargo.toml + path-deps).
  - **`soong`** — AOSP, via cached `out/soong/module-info-<target>.json`.
    Dogfooded on real AOSP (~91k modules, 552k deps, 1.4M file
    attributions, 14 s).
  - **`kernel`** — Linux Kbuild (top-level subsystems; 22
    modules, 462 deps, 72k files on linux/master, 712 ms).
  - **`gn`** — Chromium/perfetto/V8/ANGLE (parses
    `gn gen --ide=json` output).
  - **`bazel`** — parses `bazel query --output=jsonproto`.

- New `ModuleGraph` reader (`scry-store::modgraph`): JSON v1
  schema, Warshall transitive-closure into a packed reachability
  bitmap for O(1) `is_reachable(from, to)` queries at filter time.

**Internals:**

- Soong adapter's single-walk algorithm: dir→module HashMap with
  longest-prefix-wins via sort-length-desc + or_insert, then walk
  non-overlapping roots once. Earlier per-module walks blew up at
  AOSP scale (9 GB RSS / 18 min hung, vs 14 s now).

## [0.1.11] — 2026-05-16

A wide-ranging quality-and-throughput drop. Three classes of fix:

1. **Search UX**: fuzzy is fast and typo-tolerant; grep has a real
   case-insensitive mode; case-folded substring matching everywhere.
2. **Indexer throughput**: ~10–15× faster end-to-end on AOSP+Linux
   via mutex-contention elimination, parallel-everything (no more
   serial big-file queue), auto-scaled batch / file thresholds keyed
   to `--mem-cap`, and live `[progress]` lines with throughput + ETA.
3. **Operability**: per-file parse timeout (no more hangs on
   adversarial inputs), binary-content sniff in the walker (catches
   MPEG-TS / proto.bin files mislabeled as source), `--resume`
   bails on file-count drift instead of silently corrupting,
   skiplist self-heals via probe on full index, per-failure parse
   log, `scry warm` + daemon auto-warm.

Headline numbers on the rebuilt AOSP+Linux corpus (1.03 M files,
31 M symbols, 63 M refs):

- `fuzzy bindservice` (case-insensitive substring): 326 ms warm.
- `fuzzy Bndservce` (5-edit typo, tier-3 subseq fallback): 334 ms.
- `grep -i bindservice`: 346 ms.
- Index rebuild end-to-end: **5968 files/s** (~173 s for 1 M files
  on 64 workers with 100 GiB `--mem-cap`); previously ~13 min.
- Workspace tests: 170/170. Clippy: clean.

### Added — name-trigram sidecar

Four new files in every index, written automatically on
`scry index` and `scry index --incremental`:

- `name_trigrams.fst` — FST mapping each 3-byte trigram present
  in any unique symbol name → offset into the postings file
- `name_trigram_postings.bin` — varint+delta-encoded lists of
  unique-name ordinals containing that trigram
- `unique_names.bin` + `unique_name_offsets.bin` — random-access
  store of the de-duplicated symbol-name table

The trigram-over-names structure is a 25 MB sidecar against a
1 M-file / 25 M-symbol index — small enough to mmap entirely,
large enough to skip 99 % of the unique-name scan that v0.1.10
did naively. Built once at index time, read-only after.

### Changed — `lookup_substring` is now three tiers

`StoreReader::lookup_substring` (powers `scry fuzzy`'s substring
fallback) was a single linear scan over every unique symbol
name. Replaced with three ordered tiers, each strictly more
forgiving than the last:

1. **Exact-case substring** via trigram intersection. The
   needle's trigrams index into `name_trigrams.fst`; we
   intersect their posting lists, verify each candidate name
   with `memchr::find`, and stop at the result cap. Sub-millisecond
   warm on AOSP-scale indexes.
2. **Case-insensitive substring** — same intersection, but
   trigrams are lowercased on both sides and verification uses
   ASCII-folded `memchr`. This is what makes `bindservice` find
   `bindService` and `BinderProxy` find `BINDER_PROXY` without
   needing user-visible regex flags.
3. **fzf-style subsequence** (case-insensitive). Final tier for
   queries that aren't a substring at all — e.g. `Bndservce` →
   `bindService` (5 edits, beyond the Levenshtein-2 bound). Only
   runs if tiers 1 + 2 returned fewer hits than the limit; walks
   the unique-name table directly since trigrams of a typo
   don't match the target's trigrams.

### Performance — measured

| query                | v0.1.10 wall | v0.1.11 wall |
|----------------------|--------------|--------------|
| `bindService` (hit)  | ~700 ms      |   30 ms      |
| `Bndservce` (typo)   | ~2.8 s, 0 hits | 30 ms, finds bindService |
| `looksub`  (subseq)  | ~2.8 s, 0 hits | 30 ms, finds lookup_substring |
| `FUZZY_QUERY` (miss) | ~2.8 s       |   30 ms      |

Numbers are wall-clock on the AOSP+Linux corpus (~1 M files, ~25 M symbols).
The flat ~30 ms floor is the fixed cost of reading the trigram FST + a
small posting list; tier-3 subseq adds a few ms when the first two tiers
miss the cap.

### Added — per-failure parse log

Before v0.1.11, files that failed to parse were silently counted
under `files_failed` in the final DONE line — operators had no
way to know *which* files failed. Now every parse failure emits
one line to stderr:

```
[fail] /path/to/Generated.java kind=Java size=4.2 MB reason=tree-sitter: timed out after 2500 ms
[fail-panic] /path/to/weird.proto kind=Proto size=1.1 MB panic=index out of bounds: …
```

- `[fail]` covers parser-level errors returned by `parse_one`:
  tree-sitter failures, I/O issues, registry-rejected kinds.
- `[fail-panic]` covers panics caught by `catch_unwind` inside
  the per-file parse — usually a tree-sitter bug or an extractor
  edge case worth filing.
- Both still bump the `files_failed` counter, so the DONE line
  total is unchanged. Operators can `grep '^\[fail' build.log`
  to triage; the path + reason is enough to reproduce.

### Added — live indexing progress

`scry index` was silent for tens of minutes on big corpora. Now
every 1000 parsed/failed files emits one line to stderr:

```
[progress] 423000/1009161 files (41.9%) · 6512 f/s · ETA 1m30s · batch 24 · 8.2M syms · 21.4M refs
```

- Whole-job denominator (sum across all walked roots), not just
  per-batch — users see "1009161" not "1000".
- Throughput is rolling over the full job, so it stays stable
  across batch boundaries instead of resetting each batch.
- ETA derives from the rolling rate. Renders as `45s`, `12m30s`,
  or `2h05m`; `—` for NaN / unknown.
- One milestone, one print: a monotonic high-water mark (atomic
  `fetch_max` over `p / step`) prevents the per-thread race that
  printed some milestones twice in the unmonitored draft.

Overhead: ~10 ns per file (one atomic load + one `fetch_max`).
On a 51k-file warm rebuild, indistinguishable from noise vs the
pre-progress binary.

### Added — case-insensitive grep (`-i` / `case_insensitive`)

Separate from the fuzzy fix above: `scry grep` now has a real
case-insensitive mode. Same complaint pattern — users typing
`bindservice` looking for `bindService` got zero hits because
grep was always literal exact-case.

- **CLI**: `scry grep -i PATTERN` (or `--ignore-case`). Works
  alongside `--regex` for case-folded regex (`-i --regex`).
- **JSON-RPC / MCP**: `{"cmd":"grep","args":{"pattern":"...","case_insensitive":true}}`.
  Tool description in `tools/list` advertises the new option;
  MCP schema lists it under grep.
- **Trigram pre-filter stays fast**: new
  `StoreReader::grep_candidates_ci` expands each 3-byte query
  trigram across its ASCII case variants (≤ 8 per trigram),
  unions their posting lists, then intersects across positions.
  No 8× cost in practice — postings for each case variant are
  small and the union runs once per trigram.
- **Inner matcher**: `regex::bytes::RegexBuilder` with
  `case_insensitive(true)`. Literal patterns are escaped first
  (via `regex::escape`) so meta-characters in the user pattern
  stay literal.

Validated by:
- `crates/scry-store/src/lib.rs::trigram_case_variants_expands_letters_only`
- `crates/scry-cli/tests/e2e.rs::synthetic_tree_roundtrip` — 3 new
  assertions: CI literal grep finds `Bravo` for `bravo`, control
  case-sensitive grep does NOT (proves the flag is the only
  thing flipping behavior), CI regex (`br.vo -i`) finds `Bravo`
- `crates/scry-cli/tests/e2e.rs::tcp_serve_roundtrip` — 2 new
  JSON-RPC assertions: `case_insensitive:true` finds `Hello` for
  `hello`, control without the flag returns empty array

### Removed — wall-clock cap hack

The earlier draft of this fix added a 250 ms wall-clock cap to
`lookup_substring`. That bounded the worst case but lied to the
user (results silently truncated to whatever the budget caught).
Removed entirely once the sidecar made it unnecessary — there is
now no time budget on substring lookups; they are simply fast.

### Index compatibility

The new sidecar is required for tiers 1 + 2. Indexes built by
v0.1.10 or earlier still open, but `lookup_substring` logs a
one-line "rebuild me with v0.1.11+ for fast fuzzy" notice and
falls back to the slow linear scan from v0.1.10. Rebuild with
`scry index ...` (full) or `scry index --incremental` (cheap
refresh) to get the speed.

### Added — per-file parse timeout

`scry index` could hang indefinitely on adversarial tree-sitter
inputs (the canonical example: a 6.7 MB generated `old.html`
under `NeuralNetworks/.../systrace_parser/test/`). Now every
parser invocation runs under a wall-clock budget enforced via
`Parser::set_timeout_micros` + a progress callback. Default
**60 000 ms per query** (a file gets up to 2 min total across
the symbols query + the refs query). Override with
`SCRY_PARSE_TIMEOUT_MS=N`. On timeout, the file logs
`[ts-TIMEOUT]` and counts as failed, then the indexer moves on.

### Added — binary-content sniff in walker

Files whose extension claims they're source but whose first
512 bytes are mostly non-printable bytes (NUL byte, > 10 %
control characters outside ASCII whitespace + UTF-8
continuations) are now refused at walk time with
`[skip-binary] <path>`. Catches `capture_stream.ts` (73 MB
MPEG-TS broadcast stream mislabeled as TypeScript), `.proto`
files that are actually `.proto.bin`, and 60+ ExoPlayer test
assets that were being silently mis-parsed before. ~256-byte
read per file; negligible walker overhead.

### Added — skiplist self-heals via probe

Every full `scry index` run now probes each entry in
`oom_skiplist.txt` with the new bounded parse timeout. If a
file now parses cleanly (or no longer classifies as source, or
no longer exists), it's dropped from the skiplist. Stale entries
from older binaries that didn't have the parse timeout get
purged automatically the first time you rebuild with v0.1.11.

### Changed — `--resume` is strict on file-count drift

The previous behavior was to warn on `[resume] file count drift`
and continue, which silently corrupted the index (file_ids past
the insertion point shifted, lookups returned wrong paths). Now
drift is a hard error: remove `${index}.tmp/` and re-index
without `--resume`.

### Added — auto-scaled batch / file thresholds

When `--mem-cap N` is set, three previously-fixed knobs now
scale with the cap so big-memory hosts actually use the
budget:

- `--flush-bytes` (target in-RAM bytes per batch): default
  was 1 GiB; now ~25 % of `mem_cap` (so `--mem-cap 100` →
  25 GiB target).
- `--flush-every` (batch file-count cap): default was 50 000;
  now `mem_cap × 50000`, capped at 5 M (so `--mem-cap 100` →
  5 M files, letting the bytes target actually be reached).
- `--big-file-bytes` (serial-bucket threshold; now sort hint
  only — see below): default was 64 KiB; now `mem_cap × 16 KiB`
  capped at 4 MiB (so `--mem-cap 100` → 1.6 MiB).

Explicit `--flush-bytes` / `--flush-every` / `--big-file-bytes`
on the CLI overrides the auto-scale.

### Changed — mutex contention eliminated (10× indexer speedup)

The hot path used to lock three `parking_lot::Mutex`'es per
parsed file (syms / refs / trigrams sinks). With 64 workers
× tens of thousands of files per batch, that's hundreds of
thousands of contended lock takes. Replaced with a per-worker
`LocalAccum` accumulator via rayon `fold` + `reduce`; the
global sinks are touched **3 times per batch** instead of
**3 × N files**. Throughput on the small-file parallel pass
jumped from ~1 070 f/s to ~10 000 f/s on AOSP.

### Changed — parallel-everything (no more serial big-file queue)

The previous design serialized any file larger than
`--big-file-bytes` to bound peak transient RAM. That cost
75 seconds on a single 27 MB MaskRCNN generated CPP file
while 63 workers sat idle. Now **every file goes through the
parallel pool**; backpressure is provided by the existing
`await_memory_headroom()` (parks workers when jemalloc-
reported allocation exceeds 85 % of `--mem-cap`) and the
ultimate safety net is `cgroup` + the hardened `--resume`.
Files are sort hinted smallest-first so workers stay
saturated through most of the batch and the slowest tail
only blocks the final seconds. Combined with the contention
fix, indexer end-to-end goes from ~13 min to ~3 min on
AOSP+Linux.

### Added — live indexing progress + per-failure log

`scry index` now emits a `[progress]` line every 1 000 files
with whole-job throughput (rolling f/s), percent done, ETA
(`45s` / `12m30s` / `2h05m` form), batch number, and running
symbol/ref counts. A monotonic high-water mark ensures each
milestone prints exactly once. Per-failure log: `[fail]` lines
for parser-level errors and `[fail-panic]` for caught panics,
so operators can `grep '^\[fail' build.log` and triage which
files didn't parse instead of just seeing the aggregate
`files_failed` count.

### Added — `scry warm` + daemon auto-warm

New `scry warm --index DIR` subcommand prefaults every sidecar
into the OS page cache via parallel sequential reads. `scry serve`
and `scry mcp` now auto-warm on startup so the first agent /
Claude query lands warm instead of paying cold-mmap latency.
Uses the available RAM as a working set (≈ 9.5 GB for the
AOSP+Linux index); subsequent queries land sub-10 ms warm.

### Validated

- Workspace: `cargo test --release --workspace` → 170/170 pass
- Clippy: `cargo clippy --release --workspace --all-targets -- -D warnings` clean
- Editor e2e: `./editors/tests/run_all.sh` → 5/5 suites green
- Fuzzy: every query in the perf table above re-measured on the
  rebuilt AOSP+Linux index
- Indexer: full AOSP + Linux rebuild from scratch in 173 s on a
  72-core host with `--workers 64 --mem-cap 100` — 1.03 M files,
  31 M symbols, 63 M refs, 0 failures, 9.5 GB on disk

## [0.1.10] — 2026-05-16

The editor-UX polish drop. Validates every editor plugin from
v0.1.9 inside a real, interactive editor session — not just at
the API level — and adds the popup-frontend integration
(`corfu`, `company`, `corfu-terminal`) that the user-facing
autocomplete UX wants.

### Added — interactive TTY e2e suites

- **`editors/tests/e2e_emacs_tty.sh`** — drives real `emacs -nw`
  in an isolated tmux server. Six assertions covering modeline
  lighter, scry-stats, scry-def (jumps to a Rust source line),
  scry-callers (xref buffer fills), scry-prefix (autocomplete
  candidate visible), scry-restart confirmation. Auto-loads
  `corfu` from the user's `~/.emacs.d/straight/build/corfu` if
  present, falls back to vanilla `*Completions*` otherwise.
- **`editors/tests/e2e_vim_tty.sh`** — same shape for `vim`:
  ScryStats, ScryDef, ScryCallers, ScryPrefix, omnifunc
  candidate count.
- **Isolation**: both TTY suites use `tmux -L scry-e2e-$$` to
  create a per-script tmux SERVER (separate socket from
  `tmux/default`), so they CANNOT touch the user's existing
  tmux sessions. Cleanup uses `kill-session` only, never
  `kill-server`. A defensive check refuses to run if the socket
  name doesn't carry the PID.
- **`run_all.sh`** auto-picks up both TTY suites when `tmux`
  is installed; falls back to batch-only otherwise.

### Improved — Emacs popup integration

- `scry-completion-at-point` now also exports
  `:company-doc-buffer`, which both `corfu` and `company` use
  to render a small doc panel beside the popup. Contents are
  the symbol's FQN, kind, lang, scope chain, and path:line —
  enough that the user can branch between two same-named
  candidates without leaving the popup.
- Emacs README gets a "Popup frontends (recommended)" section
  with the `corfu` + `corfu-terminal` recipe for in-buffer
  popups under `emacs -nw`, plus the `company` alternative.
  Every CAPF property scry exports is enumerated against the
  three frontends so users can see exactly what they'll get.

### Validated

`./editors/tests/run_all.sh` on this host:

```
editor e2e: emacs       8 ok
editor e2e: vim         8 ok
editor e2e: vscode      7 ok
editor e2e: emacs_tty   6 ok   (real `emacs -nw` in tmux)
editor e2e: vim_tty     5 ok   (real `vim` in tmux)
                       --
                       36 assertions across 5 suites — all green
```

Zero binary changes (scry-cli, scry-store, scry-lang, scry-aosp,
scry-walker bytes are identical to v0.1.9). Pure editor + test
work.

## [0.1.9] — 2026-05-16

The editor-bindings drop. scry now ships first-class plugins for
the three editors that matter: Emacs (gold-standard), Vim,
VS Code. All three speak the same JSON-RPC to a long-lived
`scry serve` over a unix socket — autocomplete + jump-to-def +
find-refs land at the standard editor APIs (`completion-at-point` /
`xref` for Emacs, `omnifunc` / quickfix for Vim, the four LSP-style
provider APIs for VS Code). Headless e2e suite for each (`emacs
--batch` / `vim -nu` / `node + ScryClient`) covers 23 assertions
total; all 3 suites green this release.

### Added — `editors/`

- **`editors/emacs/scry.el`** — single-file Emacs 29+ plugin.
  Registers a `scry` xref backend so `M-.` / `M-?` work out of
  the box, plus a `completion-at-point` provider with rich
  annotations (kind + lang + filename per candidate). `scry-mode`
  per-buffer; `global-scry-mode` for prog-mode-wide. Bignum-safe
  JSON parser (uses `json-read-from-string`, since `json-parse-string`
  rejects scry's u64 symbol IDs). 9 customization variables for
  binary path / index dir / socket path / completion length / etc.
  9 interactive commands (`scry-def`, `scry-callers`, `scry-ref`,
  `scry-outline`, `scry-prefix`, `scry-fuzzy`, `scry-stats`,
  `scry-restart`, plus `scry-mode` / `global-scry-mode`).
- **`editors/vim/`** — vim 8+ plugin (`plugin/scry.vim` +
  `autoload/scry.vim`). Async via vim 8 channels (`ch_open` on
  unix sockets). 7 commands (`:ScryDef`, `:ScryCallers`,
  `:ScryRef`, `:ScryPrefix`, `:ScryFuzzy`, `:ScryOutline`,
  `:ScryStats`, `:ScryRestart`) all populating the quickfix list,
  plus `scry#omnifunc` for `:setlocal omnifunc=scry#omnifunc`-style
  completion wiring.
- **`editors/vscode/`** — TypeScript extension (`package.json` +
  `tsconfig.json` + `src/extension.ts`). Registers
  `CompletionItemProvider`, `DefinitionProvider`,
  `ReferenceProvider`, `DocumentSymbolProvider` against every
  language scry knows. 5 commands (`scry.def`, `scry.callers`,
  `scry.outline`, `scry.stats`, `scry.restart`). 5 configuration
  settings. Bignum-safe JSON via a pre-parse rewrite of the `id`
  field so `JSON.parse` doesn't blow Number.MAX_SAFE_INTEGER.

### Added — protocol + tests

- **`editors/common/PROTOCOL.md`** — the wire-shape contract
  every plugin targets: request/response shape, the ~7 commands
  plugins actually use, latency budgets, the persistent-socket
  pattern, expected error modes. Stable; doesn't change without
  a minor-version bump.
- **`editors/tests/e2e_emacs.sh`** — emacs `--batch` driver that
  exercises every public function plus the xref-backend / CAPF
  integration points. 8 assertions.
- **`editors/tests/e2e_vim.sh`** — vim `-nu` driver, same coverage
  via `scry#request` + `scry#omnifunc`. 8 assertions.
- **`editors/tests/e2e_vscode.sh`** — node driver that imports the
  compiled `extension.js`'s `ScryClient` directly (no VS Code
  binary needed in CI). 7 assertions, including a u64-ID
  precision check that catches JSON parser regressions.
- **`editors/tests/run_all.sh`** — master harness. Builds the
  scry binary + a scry-of-scry index if missing, runs all three
  suites, reports pass/fail. Exits 0 when all 3 green.

### Per-editor READMEs

Each editor directory carries an install + index + use guide:
- `editors/README.md` — overview matrix.
- `editors/emacs/README.md` — load-path / use-package / global recipes,
  full keybinding table, troubleshooting, headless-verify command.
- `editors/vim/README.md` — vim-plug / packer.nvim / manual install,
  recommended keymaps, omnifunc wiring.
- `editors/vscode/README.md` — developer install + future `.vsix`
  install, settings table, troubleshooting.

Linux is the supported platform (unix-socket transport).

### Validated

All three plugins pass their headless e2e on the scry-of-scry
index (1357 symbols, 53 files, 591 ms cold build). The full
matrix:

| primitive  | Emacs | Vim | VS Code |
|------------|:-----:|:---:|:-------:|
| `stats`    | ✓     | ✓   | ✓       |
| `prefix`   | ✓     | ✓   | ✓       |
| `def`      | ✓     | ✓   | ✓       |
| `callers`  | ✓     | ✓   | ✓       |
| `outline`  | ✓     | ✓   | ✓       |
| `fuzzy`    | ✓     | ✓   | ✓       |
| `xref-backend-definitions` integration | ✓ | — | — |
| `completion-at-point` CAPF shape | ✓ | — | — |
| `omnifunc` findstart + candidates | — | ✓ | — |
| u64 ID JSON precision | (via json.el) | (via json_decode) | ✓ explicit assertion |

### Notes

- The `serve` daemon was already capable of every primitive
  editors need; this release adds zero new CLI / JSON-RPC
  surface. The work is in the plugins themselves + the
  protocol documentation + the e2e harness.
- u64 symbol IDs are the only protocol-level edge case plugin
  authors hit. Each binding handles it differently because each
  language's JSON parser handles bignums differently. All three
  approaches are documented in their respective plugin source.

## [0.1.8] — 2026-05-16

The "walked-but-not-symbolized" cleanup. Two more tree-sitter parsers
wired so the small leftover gap on scry's own repo (and any Rust
project + any CI-driven repo) goes away.

### Added

- **TOML** (`tree-sitter-toml-ng 0.7`) — captures table headers
  (`[package]`, `[dependencies]`) as Module-kind plus every key
  (`name`, `serde`, `anyhow`, ...) as Field-kind. `scry def serde
  --lang Toml` lands on every Cargo.toml that declares it.
- **YAML** (`tree-sitter-yaml 0.7`) — captures every mapping key
  as Field-kind. `scry def jobs --lang Yaml` lands on
  `.github/workflows/ci.yml`; `scry def lint --lang Yaml` lands
  on the job that defines it. Covers GitHub Actions workflows,
  k8s manifests, ansible playbooks with the same generic flow.
- `short_lang()` chips for `toml` and `yaml` so result rows
  display `(field toml)` instead of `(field ?)`.

### Improved — coverage of scry's own repo

| file kind  | v0.1.7 syms | v0.1.8 syms |
|------------|------------:|------------:|
| Toml       |           0 |         213 |
| Yaml       |           0 |          40 |
| License    |           0 |           0 |

License files (LICENSE, NOTICE, METADATA) stay unsymbolized
intentionally — they're legalese / attribution text, not
structured. The 0-symbol entry in coverage output isn't a gap;
it's "scry knows what this file is and has nothing useful to
extract from it." That distinction is the whole point of running
the classifier even when the parser stays a no-op.

### Migration notes

None. Existing AOSP indexes can be reused; rebuild only if you
want symbol coverage on TOML / YAML files that were
walked-but-not-symbolized before.

## [0.1.7] — 2026-05-16

The "works on non-Android repos too" drop. Wires six new tree-sitter
parsers — TypeScript, Proto (proto2 + proto3), HTML, CSS, SCSS,
Markdown — so a single scry binary covers the perfetto trace_viewer,
its own Rust source, scry-ui, and any web-shaped repo with the same
zero-config flow as AOSP. Pure addition: every existing AOSP
extractor stays where it was; the new parsers slot in via the
existing FormatRegistry trait. Zero Android regressions (164 tests
pass, was 158).

### Added — language support

- **TypeScript** (`tree-sitter-typescript 0.23`) wired for `.ts` /
  `.tsx`. Captures class / interface / function / method / enum /
  type-alias / top-level variable. Built for the perfetto
  trace_viewer's ~5500 .ts files; works for any TS project.
- **Proto** (`tree-sitter-proto 0.4`) — proto2 + proto3 in one
  grammar. Captures message / enum / service / rpc. Uses the
  existing `ProtoMessage` / `ProtoEnum` / `ProtoService`
  SymbolKinds that were defined but unused before this release.
- **HTML** (`tree-sitter-html 0.23`) — captures `id=` / `name=` /
  any `attribute_value` text as XmlId symbols so JS-side
  `getElementById("x")` calls have a definition to resolve to.
- **CSS** (`tree-sitter-css 0.25`) — class selectors → Class,
  id selectors → XmlId, `@keyframes` names → Type.
- **SCSS** (`tree-sitter-scss 1.0`) — superset of CSS captures
  plus `@mixin` and `@function` definitions as Function-kind.
  perfetto's UI uses SCSS as the primary stylesheet format.
- **Markdown** (`tree-sitter-md 0.5`) — atx + setext headings as
  Module-kind symbols. `scry def "Verification checklist"` jumps
  to that section of DEVELOPMENT.md without grepping.

### Added — corpora validated this release

| Corpus     | Files | Symbols | Notes                          |
|------------|------:|--------:|--------------------------------|
| perfetto   | 40478 | 1.26 M  | TS UI + proto + C++/Python + SCSS/CSS/HTML + GN — full coverage, 8 min on 12 workers |
| scry repo  |    53 |    1357 | Rust + Bash + Markdown headings; 591 ms cold |
| scry-ui    |    43 |     746 | TypeScript + SCSS + HTML; 100 ms cold |

All three exercised end-to-end this release. `scry def Track --lang
TypeScript` lands on the interface in `perfetto/ui/src/public/track.ts`;
`scry def TracePacket --kind proto.msg` lands on the proto definition;
`scry def "Quickstart"` finds every Markdown Quickstart heading in
the perfetto docs tree.

### Improved

- `short_lang()` (the display-side lang chip on result rows) now
  knows ts/html/css/scss/md so result lines say `(iface ts)` not
  `(iface ?)`.
- README "Also works on" section names the three non-AOSP corpora
  scry was exercised against this release, with the indexed
  file/symbol/time numbers.
- DEVELOPMENT.md's "Known coverage gaps" tightened to reflect
  reality — assembly stays a gap; bash / TS / proto / HTML / CSS
  / SCSS / Markdown have moved out of "gap" and into the
  positive-coverage table above it.

### Migration notes

None. The added grammars are pure additions to scry-lang; the
walker just learned five new file extensions (`.ts` / `.tsx` /
`.html` / `.htm` / `.css` / `.scss`) to classify (Markdown
classification was already in place). Existing AOSP indexes can
be reused; rebuild only if you want symbol coverage on web /
proto / docs files that were walked-but-not-symbolized before.

## [0.1.6] — 2026-05-16

The DEVELOPMENT.md sweep. Worked the "What's left" / "Concrete
pending" / "Experiments" backlog end-to-end: kept the ideas that
earned it, deleted the ones that didn't, measured the ones that
needed measuring. Every item now lives somewhere — implemented,
discarded with a rationale in DEVELOPMENT § "Decisions", or
sitting in BENCHMARKS § "Investigation findings" as a
result-of-record.

### Added

- `scry stats --json` for machine consumption. Stable shape:
  scry_version + manifest_version + indexed_at + roots +
  files_total/parsed/failed + bytes_total + symbols + refs +
  elapsed_ms + by_lang + by_kind histograms. Pinned by e2e.
- `scry grep --explain` query plan dumper. Lists every extracted
  trigram (smallest-first, with posting size), the final
  candidate count, and a rough scan-cost estimate. Short-circuits
  the scan; use to diagnose why a grep feels slow.
- `scry owner PATH --accumulate` emits the union of emails across
  every visited OWNERS layer (the Gerrit "potential approvers"
  set). OWNERS chain walk now respects `set noparent` (and the
  bare `noparent` form) per Gerrit semantics.
- AIDL frozen-version kind (`aidl.frozen`). Files under
  `aidl_api/<pkg>/<N>/` are promoted from `aidl.iface` so agents
  can filter `--kind aidl.frozen` to scope to a specific frozen
  surface version.
- JNI binding shadows (`SymbolKind::JniBinding`, "jni"). Every
  Java `native` method emits a synthetic symbol named after the
  standard JNI mangling (`Java_<pkg>_<class>_<method>` with
  `_`→`_1`, `$`→`_00024`, `;`→`_2`, `[`→`_3`). `scry def
  Java_android_os_Parcel_nativeWriteString` now lands on the
  Java declaration even when the C++ side is missing.
- Kotlin companion-object coverage. Anonymous `companion object
  { ... }` injects a synthetic Class symbol named "Companion"
  scoped to the enclosing class; members get `[Outer, Companion]`
  scope. Named companions captured directly.
- Layer 2 narrowing for Kotlin and C++. Kotlin mirrors Java
  (same-package → explicit import → wildcard → implicit-import
  fallback over kotlin / kotlin.collections / kotlin.io /
  kotlin.text / ...). C++ does same-namespace > using-namespace
  > fallback via a new `RefKind::UsingNamespace`.
- Bash tree-sitter wiring (`tree-sitter-bash 0.25`) — captures
  function definitions + top-level variable assignments. Surfaces
  AOSP envsetup.sh's `lunch / mm / mmm / croot` family.
- Auto-narrow hint: when a result set saturates `--limit` and
  shown paths share a 2+ segment common directory prefix, a
  stderr line suggests `--in <prefix>/` to narrow. Suppressed
  by `SCRY_QUIET=1`.
- Nightly rebuild systemd .timer recipe in OPERATIONS.md.

### Improved

- `Manifest::version` hoisted to `MANIFEST_VERSION` const with a
  documented bump policy.
- Coverage `--json` shape pinned by an explicit e2e shape assertion.
- `unbounded_parse_returns_tree` test strengthened: compares the
  `timeout=0` result to a reference `parse_with_options(_, None)`
  call (root kind, byte range, node count, no parse error).
- `validate.sh` hard-fails if `def Activity --kind class` returns
  an `api/*.txt` first hit instead of a `.java`/`.kt` source.
- Resolver tests grew from 7 (Java only) to 13 (Java + Kotlin +
  C++); resolve_one has line-by-line coverage of every narrowing
  path it can take.

### Removed

- `tracing` + `tracing-subscriber` deps. The subscriber was
  initialized in `main()` but no code emitted through it;
  eprintln! is the convention. -127 Cargo.lock entries.
- `crossbeam` + `toml` from workspace deps (declared but never
  imported by any crate).

### Investigated

Findings in BENCHMARKS § "Investigation findings":
- Cold-vs-warm `def` gap: ~300 ms (not 7 ms); page-fault dominated.
- Cold-grep cache-miss rate: 17.7 % (not 38 %); IO-bound.
- `lto=thin` payoff: sub-1 % perf delta on warm grep.
- ts-TIMEOUT recurrence: same two libwebsockets files every run.

### Documentation

- DEVELOPMENT.md collapsed 747 → 578 lines. "Known coverage
  gaps" / "What's left" / "Concrete pending" / "Experiments"
  merged into "Roadmap" + "Things worth investigating" +
  "Decisions: ideas considered and not pursued".
- BENCHMARKS.md gets an "Investigation findings" appendix.
- OPERATIONS.md documents the nightly-rebuild .timer recipe.

## [0.1.5] — 2026-05-16

The capacity-caps + agent-affordances drop. Address the
explicit ask "do the CPU/mem caps work for query, not just
indexing?" plus the long-deferred `scry tldr` and a
counter-intuitive finding from the small-model retest.

### Added
- **`scry serve --max-conns N`** — bound concurrent connections
  to the daemon. Each accepted connection runs grep with its
  own rayon pool, so unbounded fan-in × per-query fan-out can
  OOM a host. `0` (default) preserves prior unlimited
  behavior. Over-cap accepts receive a JSON-RPC error
  (`code: -32004`, `data.retryable: true`) before the server
  closes the connection — clients see an actionable hint, not
  silent EOF. An RAII `ConnSlot` guard releases the slot even
  on panic. USAGE.md "Index admin" gets a new subsection
  documenting the cap reply + the standard Unix tools for
  inspecting / killing the daemon (`ss`, `lsof`, `pkill`). New
  e2e regression `unix_serve_max_conns_drops_over_cap` asserts
  the error code, the `retryable: true` flag, and the stderr
  log line.
- **`scry tldr PATH`** — one-call file summary: language,
  total symbol count, per-kind histogram, top 3 ranked symbols
  (by `rank_score`), and the first non-blank line of the file
  (typically the package decl or leading docstring). Cuts ~70%
  of the tokens vs `outline + 3×def` for "what does this file
  do?" agent queries. Exposed as the `tldr` MCP tool. New e2e
  block exercises both JSON and plain output shapes.
- **Strengthened MCP tool descriptions.** Every tool's
  description now leads with the most common failure mode an
  agent will hit (e.g. `def` opens with "If a name is common
  (Activity, Binder), ALWAYS pass `kind` and/or `lang`";
  `limit` reminds "Do NOT pass placeholders like 'N'"). Helps
  ≥3B-class models meaningfully; see AGENT_NOTES §6.5 for the
  honest counter-finding on ≤1B models.
- **DESIGN §6.5 — Ranking and narrowing heuristics.** Full
  documentation of `rank_score` (kind tiers, lang penalty,
  scope penalty), grep candidate path-quality penalty, Layer 2
  resolver narrowing rules per language (Java's pkg → import →
  wildcard → fallback chain), trigram intersection ordering,
  and fuzzy ranking composition. Tied to the source files that
  implement each.
- **DEVELOPMENT.md toolchain install commands** for Ubuntu /
  Debian / Fedora / Arch / macOS. rustup one-liner + the
  optional clang / ripgrep packages.

### Tests
- 144 → **146 tests** across the workspace. New: serve
  `--max-conns` over-cap drop regression, `scry tldr` JSON +
  plain output assertions.

## [0.1.4] — 2026-05-16

The small-model-comparison drop. Ran Qwen 2.5 0.5B (Ollama, CPU)
against the same task I'd give myself (Claude) and captured the
interaction patterns. The comparison surfaced one real
consistency gap that small models hit hardest.

### Added
- **`--format count`** on `scry callers` and `scry ref`. Emits
  one short line (`N callers` / `N ref`) for the "how many
  references does X have?" agent query. Was only on `grep`
  before; small models reach for verb-only commands and
  shouldn't need to count lines themselves. Mutually exclusive
  with `--json`. New e2e regression block.
- **AGENT_NOTES §6.5** — a real Qwen 2.5 0.5B vs Claude
  side-by-side on the BatteryStats / noteAlarmStart task.
  Verbatim prompts, verbatim outputs, actual scry invocations,
  what Qwen got wrong and why (missed `--kind class`, missed
  `--lang Java`, used literal `N` instead of a number),
  measured timing (823 s for 200 tokens at 0.4 t/s on CPU).
- **AGENT_NOTES §6.6** — updated 8B-model recommendation,
  reflecting what the comparison taught: default `--format
  count` on first-invocation, expose `with_snippets` via
  outline, hint at `--kind` for ambiguous `def`.

### Tests
- 143 → 144 tests across the workspace (new `callers --format
  count` + `ref --format count` + `--format + --json` mutual-
  exclusion checks).

## [0.1.3] — 2026-05-16

The user-shouldn't-have-to-know drop. The stale-index version
skew check that v0.1.2 added to `scry health` was the right
diagnostic but the wrong UX — most users won't think to run
`scry health` before believing query results. Fixed: scry now
warns automatically.

### Added
- **Auto stale-index warning.** Every command that opens an
  index now emits a one-line stderr warning if the manifest's
  `scry_version` doesn't match the running binary. The warning
  is informational — queries still run — and includes the exact
  rebuild command. Catches the silent-bad-data class of bug
  (e.g. the pre-0.1.2 Java/C++ scope_path double-encoding) the
  moment it could mislead a result.
- **`SCRY_QUIET=1`** env var to suppress the warning. For CI,
  scripted use, or operators who've consciously decided to keep
  using a known-older index.
- New e2e regression test `stale_index_emits_warning_on_every_open`
  pinning: warning fires by default, `SCRY_QUIET=1` suppresses,
  matching versions stay silent.

## [0.1.2] — 2026-05-16

The LLM-self-test drop: drove `scry mcp` end-to-end as an agent
would, fixed every paper-cut it surfaced, hardened the queries.log
for long-running MCP sessions, and pruned one sugar command that
didn't earn its keep.

### Added
- **`scry grep --format=lines`** — `path:line:col\tsnippet` rg-shape,
  one hit per line. 5–10× cheaper in tokens vs `--json` for
  "list call sites of X" agent queries.
- **`scry grep --format=count`** — just `N hits across M files`,
  no per-hit rows. Cheapest possible "is X referenced AT ALL?"
  reply.
- **`scry outline --with-snippets N`** — inline the first N source
  lines of each symbol so the agent doesn't need a per-symbol
  `def` round-trip. JSON gets a `snippet` field; plain output
  shows snippet blocks with `│` separators. Lines clip at 200
  chars to bound the worst case.
- **`SCRY_LOG_MAX_BYTES`** env var (default `100 MiB`) — rotates
  `~/.scry/queries.log` to `<path>.1` when it crosses the cap,
  bounding total disk to 2 × cap. `0` disables rotation.
  **`SCRY_LOG=`** (empty) disables logging entirely for ephemeral
  MCP sessions. Matters at MCP scale where a tight loop can
  write ~6 M rows / week.
- **`queries.log` schema** gains `scry_version` and `pid` fields
  so usage analysis can disambiguate parallel callers and
  correlate latency with code versions. Documented schema +
  `jq` / DuckDB analysis recipes in USAGE.md "Ops log".
- **`scry health`** now surfaces the `scry_version` that built
  the index alongside the running binary's version. A mismatch
  is a soft warning (rebuild recommended), not a failure.
- **THEORY.md Chapter 14** — the LLM-agent surface (JSON-RPC,
  MCP, token economy, persistence).
- **THEORY.md Chapter 15** — scaling beyond the canonical
  corpus, with concrete knobs for 3 M-file / 200 GB+
  internal-master setups.

### Fixed
- **MCP tool-error envelope was double-encoded.** Found by
  LLM-self-test: an `ask` against an index without embeddings
  returned `content[0].text = "{\"error\":\"no embedding
  sidecar…\"}"` — an LLM had to `json.parse` twice to find the
  hint. Now unwraps to the bare message. Regression test
  pinned.
- **Java/C++ scope_path doubled the class's own name.** Pre-
  704d917, every top-level class had `scope: [ClassName]` and
  `fqn: "ClassName::ClassName"`. The parser fix shipped; this
  release adds three `scope_regression_tests` (Java top-level,
  Java nested, C++ top-level) pinning the contract so a
  tree-sitter upgrade can't re-introduce the bug silently. Plus
  the version-skew warning in `scry health` to surface stale-
  index data built with the buggy older scry.
- **Friendlier first-run error.** A user who runs `scry def Foo`
  before ever building an index got `No such file or directory
  (os error 2)`. Now they get a clear "no scry index at <path>"
  + an actionable command to build one.
- **TCP listener** now logs the actually-bound address
  (`listener.local_addr()`) rather than the user-supplied
  string. Matters when binding to port 0 — without this you
  have no way to discover the resolved port.

### Removed
- **`scry mod NAME`** — pure sugar for `def NAME --kind soong`,
  duplicated the API surface for marginal convenience. Use the
  uniform `--kind` spelling instead.

### Tests
- 134 → **143 tests** across the workspace. New: scope-
  regression suite (3 tests), MCP tool-error unwrap (1),
  log rotation pure-helper (4), grep `--format` + outline
  `--with-snippets` e2e blocks (4 assertions).

## [0.1.1] — 2026-05-16

The release-polish drop: everything that should have been in v0.1.0
but wasn't. No new query features; no on-disk format changes.

### Added
- `LICENSE` file at repo root (Apache-2.0 full text).
- `CONTRIBUTING.md` with the contribution workflow.
- `SECURITY.md` with responsible-disclosure instructions.
- `CHANGELOG.md` (this file).
- `.github/workflows/ci.yml` — runs `cargo build --release`,
  `cargo test --release --workspace`, and `cargo clippy --release
  --workspace --all-targets -- -D warnings` on every push and PR
  against `master`. PRs that introduce a warning or a test
  failure are rejected at the CI gate.
- `scry completions <shell>` — emit shell completions to stdout
  for bash / zsh / fish / powershell / elvish via `clap_complete`.
- `scry man` — emit a roff-formatted man page to stdout via
  `clap_mangen`.
- README "Install" section pointing at the GitHub release asset
  and documenting the `cargo install --git` fallback.
- Prebuilt release binary attached to the v0.1.1 release:
  `scry-x86_64-unknown-linux-gnu.tar.gz`.

### Tests
- TCP listener path (`scry serve --listen tcp:127.0.0.1:0`) —
  round-trip a `def` query over a real TCP connection.
- Concurrent serve under load — 32 client threads, each sending
  10 queries against the same Unix-socket server, all must
  receive consistent results without panic or hang.
- `scry callers --precise` against a malformed
  `compile_commands.json` — must fail gracefully (clean error,
  non-zero exit), not panic or hang.
- Parse-budget timeout — pathological tree-sitter input with a
  1 ms budget must abort cleanly and continue with the rest of
  the corpus.

### Fixed
- DEVELOPMENT.md line 399 said "all 80 tests pass" — was stale;
  now reads 129 (the actual number).
- MCP `initialize` now negotiates protocol version per spec
  (echoes client's version when supported, otherwise replies
  with our latest). Previously hard-coded `2024-11-05`.

## [0.1.0] — 2026-05-16

First tagged release. Full feature surface implemented and tested.

### Added
- **Indexing**: 1M-file AOSP+Linux corpus in 13.3 min on a
  72-core host. cgroup-enveloped, OOM-resumable, jemalloc-
  backpressured. 40 file categories. Tree-sitter for source
  languages; custom parsers for Soong, AIDL, HIDL, init.rc,
  SELinux, AndroidManifest.xml, Bazel, CMake, GN, Kconfig,
  Makefile, Gradle, OWNERS, aconfig, api/*.txt.
- **Querying**: `def`, `ref`, `callers`, `prefix`, `fuzzy`,
  `grep` (literal + regex), `outline`, `coverage`, `stats`,
  `ask`, `diff --since`, `recall`, `owner`, `module-of`.
- **Incremental**: `scry index --incremental` re-parses only
  changed + added files, replays unchanged records, atomically
  swaps the new index into place. Sub-second on small change
  sets.
- **Transports**: stdio CLI, JSON-RPC via `scry serve` (stdio /
  Unix-socket / TCP), MCP via `scry mcp` (Claude Desktop, Cursor,
  Continue, custom). MCP supports protocol versions 2024-11-05
  through 2025-11-25.
- **Precision uplift**: `scry callers NAME --precise` via
  clangd for type-aware C++ references.
- **Sidecars**: file_digests, file_symbols, ref_resolutions,
  trigrams, chunks/embeddings — all built by separate `build-*`
  subcommands or inline with `--build-*` flags on `scry index`.

### Engineering posture
- 129 tests across the workspace; ~3 s end-to-end.
- Zero clippy warnings under strict `[workspace.lints]` policy.
- Pre-release discipline: no backward-compat shims carried.

[Unreleased]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/fiveapplesonthetable/scry/releases/tag/v0.1.0
