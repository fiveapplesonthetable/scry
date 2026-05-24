# ROADMAP — concrete design sketches for the deep items

The contained next-steps that landed in May 2026 — persistent
socket, streaming/budget, `scry diff`, AIDL/HIDL shadows — are all
in `git log`. This document is the *other* list:
the multi-day items that still sit ahead. Each one is sketched
in enough detail that a fresh contributor could read this doc, the
referenced source files, and start writing the change.

The items, in roughly the order I'd ship them:

1. [Incremental indexing](#1-incremental-indexing--shipped) — shipped
2. [`io_uring` for the candidate scan](#2-candidate-scan-io-path--mmapmemchr-shipped--io_uring-measured-not-shipping) — measured, not shipping
3. [Fuzzy ranking by edit distance](#3-fuzzy-ranking-by-edit-distance--shipped) — shipped

Each section follows the same shape: **goal**, **why now**,
**design**, **new dependencies**, **acceptance criteria**,
**tradeoffs**, **what could go wrong**, **estimate**.

---

## 1. Incremental indexing — shipped

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
   `files_packed.bin` by ~32 MB on the full corpus — negligible.

2. **Incremental walker that diffs digest sets.** New subcommand
   `scry index --incremental`. The walker rebuilds the candidate
   file list, computes digests in parallel, compares against the
   previous index's `files_packed.bin`. Three sets:
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
  `files_packed.bin` / `symbols.bin` pair compared to running an
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
  `files_packed.bin` + the rest is the standard answer. We already do
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

## 2. Candidate-scan IO path — mmap+memchr shipped; io_uring measured, not shipping

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

## 3. Fuzzy ranking by edit distance — shipped

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

1. **#3 fuzzy ranking** first — 1–2 days, no risk, pure UX win.
2. **#1 incremental indexing** — unblocks the editor-integration
   use case scry has been missing.
3. **#2 io_uring** — measurable but small win on the current
   workload; only worth it if scry deploys to rotational /
   networked storage where the gain compounds.

Anything not in this document either lives in DEVELOPMENT.md's
"Experiments and unexplored directions" section (more speculative
ideas), or hasn't been thought through yet.
