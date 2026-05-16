# ROADMAP — concrete design sketches for the deep items

The contained next-steps that landed in May 2026 — persistent
socket, streaming/budget, MCP, `scry recall`, `scry diff`, AIDL/HIDL
shadows — are all in `git log`. This document is the *other* list:
the five multi-day items that still sit ahead. Each one is sketched
in enough detail that a fresh contributor could read this doc, the
referenced source files, and start writing the change.

The items, in roughly the order I'd ship them:

1. [Semantic retrieval as a sibling tool](#1-semantic-retrieval--foundation-shipped-with-hashing-trick-embedding) — ✅ foundation shipped
2. [Incremental indexing](#2-incremental-indexing--shipped-2026-05-16) — ✅ shipped
3. [clangd-as-a-service for C++ precision](#3-clangd-as-a-service-for-c-precision) — ✅ shipped
4. [`io_uring` for the candidate scan](#4-candidate-scan-io-path--mmapmemchr-shipped--io_uring-measured-not-shipping) — ❌ measured, not shipping
5. [Fuzzy ranking by edit distance](#5-fuzzy-ranking-by-edit-distance--shipped) — ✅ shipped

Each section follows the same shape: **goal**, **why now**,
**design**, **new dependencies**, **acceptance criteria**,
**tradeoffs**, **what could go wrong**, **estimate**.

---

## 1. Semantic retrieval — ✅ foundation shipped with hashing-trick embedding

**Shipped** in `scry build-embeddings` + `scry ask`. Defaults:
chunks of 100 lines with 20-line overlap; 64-dim FNV-1a hashing-
trick embeddings; brute-force cosine search over the mmap'd
sidecar. Exposed via `serve` and `mcp` as the `ask` tool.

The hashing-trick embedding (Weinberger et al. 2009) catches
vocabulary overlap — the dominant signal for "how do I X in this
codebase" queries — without requiring a model download or any
new heavyweight dependencies. The wire format (chunks.bin +
embeddings.bin with dim/count header) is designed so a future
commit can swap in a transformer-based embedding (candle +
all-MiniLM or nomic-embed-code) behind a feature flag without
changing the sidecar layout or query API.

What's still future work for true transformer quality:
  - Add `--features transformer` that pulls in `candle-core` +
    `candle-transformers` + `tokenizers`.
  - Download model weights once at first run (or via separate
    `scry fetch-embedding-model` subcommand).
  - Per-chunk inference: ~3 ms × 3M chunks = 2.5 hours for the
    full corpus full build — within the "one cup of coffee" envelope.

### Goal (original)

Answer "how do I X in this codebase" questions where X isn't a
known identifier. Today scry can find `parse_toml` if it exists by
name; it can't surface the right *area* of code when the agent
doesn't know what to grep for. The fix is an embedding-based
retrieval path that complements (not replaces) the lexical one.

### Why now

Every modern RAG system over code uses both lexical and semantic
retrieval. Lexical catches identifiers and exact strings; semantic
catches conceptual matches. scry has the lexical side production-
ready; the semantic complement is the single biggest functional
gap from an LLM-agent perspective. Smaller models (Gemma 3 8B
class) benefit disproportionately because their reasoning about
"oh, maybe this token-soup belongs to a TOML parser" is weaker.

### Design

A new subcommand parallel to `scry grep`:

```sh
scry ask "how do I parse TOML in this codebase" [--limit N] [--in PREFIX] [--json]
```

The pipeline:

1. **At index time** (add a new finalize pass or a sidecar
   `build-embeddings` post-finalize utility): chunk every indexed
   source file into ~50–100 line windows, embed each chunk with a
   local code-capable model, write the embeddings to a sidecar.
   Expected size: ~2 GB for the full AOSP+Linux corpus at 768-dim
   half-precision.
2. **At query time**: embed the user's query, do an approximate
   nearest-neighbor search over the chunk index, return top-K
   chunks with file/line/snippet shaped like a grep result.

Concrete components and what they need:

| component | candidate | why |
|---|---|---|
| embedding model | `nomic-embed-code` or `all-minilm-l6-v2` via `candle` | Pure-Rust inference; no Python; works on CPU. |
| ANN index | `usearch` (Rust bindings) or hand-rolled HNSW | usearch is a single C++ dependency; hnsw_rs is pure-Rust. |
| chunker | 50–100 line windows with 20-line overlap | Standard RAG sizing for code; pin in tests. |
| sidecar files | `embeddings.bin` (packed f16 vectors), `embeddings.idx` (HNSW graph), `chunks.bin` (chunk metadata) | Mirrors the trigram + offset sidecar pattern. |

Output shape (parallels grep):

```json
{
  "path": "frameworks/base/.../TomlReader.java",
  "start_line": 102, "end_line": 158,
  "score": 0.84,
  "snippet": "…",
  "lang": "java"
}
```

Wiring to existing surfaces: `scry serve` gets an `ask` command,
`scry mcp` gets an `ask` tool, `--budget` and `--limit` honored
the same way as elsewhere.

### New dependencies

- `candle-core` + `candle-transformers` (embedding inference)
- Either `usearch` (C++ FFI, smaller code) or `hnsw_rs` (pure Rust)
- Model artifacts (~50–500 MB depending on choice)

### Acceptance criteria

- `scry ask "how to read TOML"` on the full index returns ≥1
  relevant chunk in the top 10 with no manual prompt tuning.
- Cold open of the embeddings index ≤ 200 ms (mmap'd HNSW).
- Per-query latency ≤ 500 ms warm on a 1 M-chunk index.
- Build pass (`scry build-embeddings`) completes in ≤ 4× the
  parse pass time (so a full reindex stays under an hour).
- The embeddings sidecar is *optional*: indexes without it answer
  every other query type unchanged; `scry ask` errors with a clear
  "run scry build-embeddings first" message.

### Tradeoffs

- **Model choice is binding.** Switching embedding models requires
  full reindex of the embeddings. Pin the model name + commit hash
  in `manifest.json`; refuse cross-model queries.
- **Storage cost.** ~2 GB extra index. Tolerable on the current
  envelope (~9.5 GB → ~12 GB).
- **Quality is workload-dependent.** AOSP-specific identifier soup
  will retrieve worse than well-commented OSS Rust. Worth
  benchmarking on a handful of real agent questions before claiming
  parity with grep.
- **Determinism.** ANN is approximate; two runs of the same query
  can return different orderings. Pin the random seed in the HNSW
  build for reproducibility.

### What could go wrong

- **The embedding model isn't quite good enough.** Code embeddings
  in 2026 are mediocre at long-form questions; we may need a
  hybrid scoring pass that re-ranks ANN candidates with a small
  cross-encoder.
- **CPU inference is too slow.** Per-chunk inference at 768d on a
  10 KB file is ~3 ms on this host's CPU. Full corpus build is
  ~3 M chunks × 3 ms = 2.5 hours — within budget but slow. GPU
  offload would help if available.
- **Index size grows past the page-cache budget.** 2 GB extra
  bytes; if the working set inflates beyond ~120 GB resident the
  page-cache LRU model degrades. Profile before committing.

### Estimate

- Skeleton (chunker + embedding pipeline + sidecar writer): 3 days
- Query path (load + ANN + serve integration): 2 days
- Bench + quality tuning on real questions: 2 days
- Total: **~1 week** of focused work.

---

## 2. Incremental indexing — ✅ shipped (2026-05-16)

**`scry index --incremental` is live.** Opens the existing index,
diffs the source tree against `file_digests.bin`, re-parses only
the changed + added files, replays unchanged files' records into a
fresh staging dir, then atomically swaps it into place. The old
index stays queryable for the duration; if the process dies
mid-build, the old index survives untouched.

Today's usable flow:
  1. `scry build-digests` (one-time after the initial full index).
  2. `scry index --incremental <roots>` after editing files. On a
     1-file change in a small tree this is ~77 ms; on the full
     corpus with sub-1% change rate, well inside the editor-loop
     budget.
  3. `scry index-diff` to preview without writing.
  4. Periodic full `scry index <roots>` rebuild when churn
     justifies the 13-min cost or you want a fresh trigram FST.

Foundation pieces that landed first and feed the incremental path:
  - `file_digests.bin` sidecar (per-file blake3) — `scry build-digests`
  - `tombstones.bin` bitmap — `scry tombstone PATH`
  - Tombstone filter on every read path (get_symbol, get_ref,
    grep_candidates)
  - `scry index-diff` — preview which files would be re-parsed,
    tombstoned, or added without modifying the index
  - `scry compact` — placeholder reporting tombstone counts; in-place
    rewrite still TODO

**Still future work**: a true append-only writer that mutates the
existing index in place (preserving file_ids; rebuilding only the
affected portions of the FSTs and trigram postings) instead of
replaying through a fresh writer. The replay-and-swap approach
shipped today is correct and fast enough for the editor-loop use
case; the in-place writer is a perf optimization for very large
corpora with very small change rates.

### Goal

Re-index only the files that changed, not the whole 1 M-file
corpus. Current cost is 13 min for "I edited one Soong file";
target is ≤ 30 s for any single-module change.

### Why now

The full rebuild is fine for once-an-hour cadence. For
"edited a file, want updated query results" — the killer feature
for editor integration and active development — 13 min is
infeasible. Incremental is the *only* path to making scry usable
in the editor loop.

### Design

Three pieces:

1. **Per-file content digest in the file table.** Today each
   `FileEntry` carries `(id, root_id, relpath, kind, size)`. Add a
   `content_digest: [u8; 32]` (blake3 of file bytes). Bumps
   `files.bin` by ~32 MB on the full corpus — negligible.

2. **Incremental walker that diffs digest sets.** New subcommand
   `scry index --incremental`. The walker rebuilds the candidate
   file list, computes digests in parallel, compares against the
   previous index's `files.bin`. Three sets:
   - **added**: in new walk, not in old. Parse + emit.
   - **removed**: in old, not in new. Tombstone.
   - **changed**: in both but digest differs. Tombstone old, parse new.

3. **Tombstones + compaction.** Symbols / refs aren't deleted from
   `symbols.bin`; instead the file_id is marked tombstoned in a
   sidecar bitmap (`tombstones.bin`, one bit per file_id). The
   reader filters out tombstoned file_ids in every query path.
   A separate `scry compact` (run nightly or when the tombstone
   ratio exceeds 20%) rebuilds clean by streaming records through,
   skipping tombstoned ones.

The trigram index needs similar treatment: per-trigram postings
get an "added since digest" sidecar that the merge intersects in.

### New dependencies

None. blake3 is already in.

### Acceptance criteria

- `scry index --incremental` after editing one file completes in
  ≤ 30 s on the full AOSP+Linux corpus.
- Tombstones don't leak into query results (filter is enforced in
  every `serve_*` and CLI path).
- A reindex from scratch produces a byte-identical
  `files.bin` / `symbols.bin` pair compared to running an
  incremental that touched every file (modulo the tombstone
  bitmap).
- `scry compact` reclaims tombstoned records and resets the
  bitmap.

### Tradeoffs

- **Tombstone-aware queries cost a hash-set lookup per result.**
  Microbenchmarked at < 5% overhead on warm queries; we'll re-measure.
- **Incremental output is slightly larger than a fresh build**
  until compaction. ~10% inflation per typical-week edit pattern.
- **The walker digest pass is the bottleneck.** Blake3 at ~3 GB/s
  on this CPU; full corpus digest is ~25 s. Cache digests of
  unchanged files (skip re-hash) by mtime + size first.

### What could go wrong

- **Concurrent reader sees a partial state.** Atomic rename of
  `files.bin` + the rest is the standard answer. We already do
  this in finalize; extend to the incremental commit too.
- **Tombstone bitmap drift.** If a tombstone is set but the
  compaction never runs, query results drop a record forever.
  Acceptance test must force a tombstone roundtrip.
- **The lazy reader caches don't see invalidations.** The lazy
  reader is mmap'd — by the time we re-open, the kernel will
  evict stale pages naturally. Test: write → read same connection
  doesn't show stale results (the connection must re-`StoreReader::open`
  after an incremental).

### Estimate

- File-table digest field + walker diff: 2 days
- Tombstone bitmap + reader filtering: 2 days
- Incremental commit path + atomicity: 2 days
- Compaction utility: 1 day
- Tests + bench validation: 2 days
- Total: **~9 days**.

---

## 3. clangd-as-a-service — ✅ per-query session shipped; persistent daemon pending

**Shipped**: `scry callers NAME --precise` spawns clangd, completes
the LSP `initialize` handshake, `didOpen`s the definition file, and
issues `textDocument/references`. Results are mapped back to scry's
file_id space and emitted with the same shape as the heuristic
path (plus a `precise: true` flag for JSON output).

Implementation lives in `crates/scry-cli/src/clangd.rs` — a small,
hand-rolled LSP client (~280 LOC) covering exactly the methods
we need (initialize, initialized, didOpen, references, shutdown,
exit). No async runtime; sync stdin/stdout framing per the LSP spec.

When clangd is missing from PATH or compile_commands.json is
not findable above the definition file, the command exits non-zero
with an actionable error: "install clangd" / "generate
compile_commands.json". The heuristic path (without --precise)
keeps working regardless.

**Still pending**: Persistent clangd daemon alongside `scry serve`,
so multi-precise-query sessions don't pay the ~1-min clangd warmup
each time. Mechanical wiring: hold a `ClangdSession` inside the
serve loop, lazy-init on first --precise request, keep alive for
the rest of the process lifetime. ~1 day of focused work; deferred
because the per-query cost is already acceptable for one-shot use.

### Goal (original)

Close the 10–20% precision gap on C++ overload resolution without
requiring the user to maintain a SCIP build. When the user asks
"who calls `Foo::bar()`", we want the *exact* overload set, not
the trigram-narrowed approximation tree-sitter gives us.

### Why now

C++ is half of AOSP. The current `scry callers` on a C++ method
name is correct ~85% of the time; the 15% are overload mistakes
(two `transact` methods with different signatures). For
exploratory code reading this is fine; for code review or
refactoring it's not.

### Design

Run a persistent `clangd` subprocess and route the precision-
critical C++ queries through it via LSP. scry stays the
"answer-fast, mostly-right" path; clangd is the "answer-slow,
precise" fallback for `--precise` queries.

Pieces:

1. **LSP client crate**. There isn't a great one in pure Rust yet;
   either pull in `lsp-server` + write the client side, or shell
   out to `clangd` and speak the protocol over stdin/stdout.
   ~500 LOC either way.
2. **Subprocess lifecycle**. clangd is heavyweight — needs a
   `compile_commands.json` and ~1 min to warm. We start it on
   demand at the first `--precise` query and keep it alive for the
   process lifetime; if `scry serve` is running, clangd lives as
   long as the server.
3. **Query routing**. `scry callers Foo --precise` invokes a new
   `precise_callers(name, file_hint)` that asks clangd for the
   symbol's USR (universal symbol resolution), then asks for
   `references`. Map back to the file table; emit.
4. **compile_commands.json discovery**. Walk up from one of the
   indexed roots; if not found, surface a clear "run
   `bear -- m` or equivalent" error.

### New dependencies

- `lsp-types` (well-maintained Rust types for LSP)
- Either `lsp-server` or hand-rolled LSP client (~500 LOC)
- An installed `clangd` binary (runtime dep, not link-time)

### Acceptance criteria

- `scry callers Foo --precise` on a known-ambiguous overload
  returns the correct subset (only callers of the matching
  signature).
- When clangd or `compile_commands.json` is missing, the precise
  path errors with an actionable message; the non-precise path
  still works.
- clangd subprocess warm time amortized across a session: ≤ 1
  min to first --precise query, then ≤ 200 ms per follow-up.
- `scry serve` keeps the clangd subprocess alive across many
  client connections.

### Tradeoffs

- **clangd's memory footprint is ~ 1–4 GB** for AOSP-sized
  projects. The cgroup envelope must account.
- **clangd needs the build to succeed**. AOSP partial builds
  produce partial compile_commands.json. The user-experience
  question is whether "no compile commands → no precise queries"
  is acceptable. (Yes for v1.)
- **Two indexes of truth**. clangd's symbol index can drift from
  scry's. Document that scry's `callers` and `--precise callers`
  may disagree; the user picks which they want.

### What could go wrong

- **clangd crashes or hangs.** Subprocess supervision (restart on
  exit, timeout per query). Don't let a clangd hang block other
  scry queries.
- **The LSP request shape changes.** lsp-types is versioned; pin.
- **AOSP doesn't produce a usable compile_commands.json out of
  the box.** Document the `b create_compile_db` Soong invocation
  somewhere visible.

### Estimate

- LSP client: 3 days
- Subprocess lifecycle + serve integration: 2 days
- Query routing + USR mapping: 2 days
- Tests with a real clangd + a small fixture: 2 days
- Total: **~9 days**.

---

## 4. Candidate-scan IO path — ✅ mmap+memchr shipped; ❌ io_uring measured, not shipping

**Shipped**: `scan_file_literal` in scry-store — mmap + memchr
helper that replaces `std::fs::read` for literal-pattern `scry grep`
queries. Avoids the per-file `Vec<u8>` allocation + copy; lets the
kernel manage memory via the page cache; overlaps cold-cache page
faults with the memmem scan loop. Tested with 7 round-trip cases
(multi-match, cap, empty needle, oversize-file refuse, missing
file, etc.).

**io_uring decision (2026-05-16): not shipping.** Measured against
the live AOSP+Linux index on this host, an io_uring rewrite would
deliver well under 10% improvement on the worst-case query and
nothing measurable on warm. The dominant cold-cache cost is bytes-
moving-from-disk, which io_uring does not change.

### Measurements that drove the decision

Host: virtio-backed disk, `/mnt/agent` reports `rotational=1`,
sequential read measured at **99.6 MB/s cold / 376 MB/s warm** via
`dd if=symbols.bin of=/dev/null bs=1M count=1024`.

Cold-cache `scry grep` after `echo 3 > /proc/sys/vm/drop_caches`:

| query              | candidates | wall | user CPU | sys CPU | disk read |
|--------------------|-----------:|-----:|---------:|--------:|----------:|
| `ZygoteInit`       |        104 | 1.09 s | 0.51 s | 0.85 s |   ~141 MB |
| `kAndroidLogTagLength` |    1   | 2.19 s | 0.49 s | 0.45 s |   ~115 MB |
| `Binder`           |     33,230 | 3.31 s | 0.43 s | 0.73 s |   ~127 MB |

Warm-cache for the same queries:

| query        | wall  | user | sys  |
|--------------|------:|-----:|-----:|
| `ZygoteInit` | 500 ms | 0.35 s | 0.56 s |
| `Binder`     | 610 ms | 0.43 s | 0.73 s |

What this tells us:

1. **Cold wall ≈ user + sys + IO_wait.** For the Binder query:
   3.31 s wall − 1.16 s on-CPU = **2.15 s spent waiting on disk**.
   io_uring cannot make the disk deliver bytes faster.
2. **Sys time is the only addressable surface.** On the worst-case
   warm query, sys is 0.73 s out of 0.61 s wall (multi-threaded;
   kernel time is summed across cores). Even a generous 30 %
   reduction in sys cost from io_uring's batched submission =
   ~220 ms × 1/cores recovered ≈ < 30 ms wall-time saving per
   query. Below the noise floor.
3. **The trigram pre-filter already does the heavy lifting.**
   33,230 candidates out of 1,009,166 indexed files = 3.3 %
   candidate ratio on a common substring. We aren't fighting IO at
   the file-count level we'd need for a uring batch to pay off.
4. **rayon already keeps a healthy IO queue depth.** 16 parallel
   workers each doing concurrent `mmap` + memchr; the kernel sees
   16 outstanding requests at any time. io_uring's "submit N reads
   in one syscall" win shrinks proportionally to how concurrent
   the baseline already is.

### Indexing path

Cold full index: 13.3 min wall to scan 70 GB. That's 90 MB/s —
within ~10 % of the measured disk ceiling (99.6 MB/s cold seq).
We're already disk-bound during index. io_uring would not buy us a
faster disk.

### Tradeoffs we'd inherit by shipping

- New dependency on `tokio-uring` (with the tokio runtime) or
  `glommio` (thread-per-core, harder to mix with rayon).
- Linux-only kernel ≥ 5.6 — would force a `cfg(target_os = "linux")`
  split, breaking the macOS dev path that contributors use.
- ~500 LOC of async-flavored, unsafe-adjacent code to maintain.
- New feature-flag matrix in CI.

For a measured upside below the noise floor, none of these costs
are warranted.

### When to revisit

- The disk gets meaningfully faster than the CPU can scan
  (e.g. on a host with > 5 GB/s NVMe paired with a slower CPU).
  Re-measure with `dd` first; if the cold sequential floor
  doubles, the math changes.
- A new use-case appears that's read-heavy and tiny-syscall-bound
  in a way grep isn't (e.g. a "millions of tiny config reads"
  feature). At that point the cost/benefit analysis is fresh.
- io_uring lands a "kernel does memchr too" extension. (Not on
  any near-term roadmap upstream.)

Until one of those holds, the mmap + memchr + rayon path is the
right call.

---

## 5. ~~Fuzzy ranking by edit distance~~ ✅ shipped

**Shipped in `scry fuzzy` — see USAGE.md "Fuzzy symbol search".**
Two candidate sources (substring + Levenshtein automaton) merged,
re-ranked by a smart score that prefers substring matches over
unrelated typos, then tie-broken by exact Wagner-Fischer distance.
Each result carries a `distance` field. Pin tests in
`crates/scry-store/src/lib.rs::tests` cover the canonical pairs and
the prefix-beats-middle-substring invariant.

### Goal (original)

`scry fuzzy ParcelFile --limit 10` currently returns matches in
FST traversal order, not sorted by edit distance to the query.
A user typing `ParcelFile` should get `ParcelFileDescriptor`
before `ParcellableFooBar`.

### Why now

Smallest, most contained win on this list — 1–2 days of work,
no new dependencies, no format changes. Pure UX polish.

### Design

After the Levenshtein-automaton walk produces candidate matches,
do a second pass that computes the actual edit distance
(Wagner–Fischer) for each candidate and sorts the result by
`(distance ASC, rank_score DESC, name ASC)`.

The candidate set is bounded (`limit` is typically ≤ 100); the
second pass costs O(K × |query| × max_name_length), which for
K=100 / |q|=12 / max_name=64 is ~75 µs total. Negligible.

### Implementation

1. Add `fuzzy_with_distance(query, k_distance, limit)` to
   StoreReader that returns `Vec<(SymbolRecord, u8)>` ordered by
   distance.
2. Update `cmd_fuzzy` and `serve_fuzzy` to use it and emit a
   `distance` field on each result.
3. Update the JSON schema in USAGE.md.

### Acceptance criteria

- `scry fuzzy <typo>` returns the closest matches first,
  measured against a small test corpus.
- A unit test pinning the ordering against a known-corpus +
  known-query.
- JSON output includes `distance` field; CLI prints it.

### Tradeoffs

- **One more pass over the candidate set.** Cost is bounded by
  `limit`, so worst case is ~hundreds of microseconds — not
  user-visible.
- **`distance: u8` in the output shape is a new field**;
  backwards-compatible (clients that don't expect it ignore
  it).

### What could go wrong

- Nothing concerning. The edit-distance pass is pure CPU on
  small inputs.

### Estimate

- **1–2 days** including tests and docs.

---

## Order of operations

If I were picking the order strictly by leverage:

1. **#5 fuzzy ranking** first — 1–2 days, no risk, pure UX win.
2. **#2 incremental indexing** — unblocks the editor-integration
   use case scry has been missing.
3. **#1 semantic retrieval** — the single biggest functional
   gap for LLM agents; biggest investment too.
4. **#3 clangd-as-a-service** — niche but valuable for C++
   reviewers; can be deferred indefinitely without blocking
   anyone.
5. **#4 io_uring** — measurable but small win on the current
   workload; only worth it if scry deploys to rotational /
   networked storage where the gain compounds.

Anything not in this document either lives in DEVELOPMENT.md's
"Experiments and unexplored directions" section (more speculative
ideas), or hasn't been thought through yet.
