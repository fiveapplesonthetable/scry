# scry — fast-path design (the "100x faster than rg" plan)

**Status (2026-05-16): both pieces SHIPPED.** Lazy reader: 1041f2b.
Trigram index: e96a4ee + d1e507a. Measured wins documented below.

Two pieces of work that, together, let scry beat rg by 1-2 orders
of magnitude for the common query types. Both are well-understood
techniques (Google Code Search, livegrep, Hound) — not research.
Independent.


## 1. Lazy / mmap StoreReader  (saves 10 GB open cost)

**Today:** `StoreReader::open()` reads symbols.bin and refs.bin into
RAM as `Vec<SymbolRecord>` / `Vec<RefRecord>`. For a finalized AOSP+
Linux index that's ~10 GB resident before the first query, plus
several seconds of bincode deserialization.

**Tomorrow:**
1. Writer change: while concatenating per-chunk records into
   symbols.bin during finalize_streaming, also write
   `symbols_offsets.bin` — a packed `Vec<u64>` of byte offsets, one
   per record. ~150 MB for 19M symbols.
2. Reader change: `mmap` symbols.bin AND symbols_offsets.bin. Don't
   load the records `Vec`. Add `lookup_symbol(idx)` that reads
   `offsets[idx] -> byte offset` and decodes one record from
   `&symbols_mmap[offset..]` via `bincode::deserialize::<_>(slice)`.
3. Same for refs.bin / refs_offsets.bin.

**Impact:**
- `StoreReader::open()` drops from ~10 GB RSS to ~200 MB.
- Cold query latency drops from ~400 ms (bincode deserialize) to
  ~10 ms (mmap page fault + single record decode).
- Warm query latency drops from ~1 ms to <100 µs.
- Works on machines with <16 GB RAM.

**Cost:** New index format (offsets.bin alongside data.bin). Existing
indexes need re-indexing. ~4 hours of code; ~200 LOC delta touching
the writer's finalize_streaming, the reader's open + all lookup paths,
the FormatPath struct.


## 2. Trigram index for grep  (1000× speedup for literal-grep queries)

**Today:** `scry grep` walks every file matching the lang/in filters,
mmaps it, runs memchr or regex over the bytes. Same total IO as rg.
We win on filter pre-selection only (~3-5× over rg in practice).

**Tomorrow:** add a trigram index — for every 3-byte sequence that
appears in any file, a posting list of file_ids that contain it.
Query: trigrammify the literal pattern, intersect posting lists,
only open the candidate files.

### Index build
1. During the parse pipeline, additionally extract the set of unique
   trigrams in each file (`HashSet<[u8; 3]>`). For a typical 5 KB
   source file: ~3000 unique trigrams. Cheap to compute (one pass).
2. Per chunk, write a sorted side-file of (trigram, file_id) tuples.
   Reuse the same k-way merge machinery as names.fst.
3. Finalize: merge into two files:
   - `trigrams.fst`: maps 3-byte key → u64 offset into trigram_postings.bin
   - `trigram_postings.bin`: per-trigram delta-encoded varint file_id list
4. Estimated size: 1M files × ~3000 trigrams/file = 3 G pairs ≈ 7 GB
   raw, ~3 GB after delta+varint.

### Query
1. For pattern `foo bar`:
   - Trigrammify: `{"foo", "oo ", "o b", " ba", "bar"}`. (For < 3
     bytes or regex queries, fall back to full scan.)
   - Read each trigram's posting list. Skip trigrams not in FST
     (= zero matches anywhere, query returns empty).
   - Intersect the lists. Result: candidate file_ids.
2. Scan only candidates with current memchr/regex code.

### Impact
- Literal queries that match few files (`grep TODO\(\): ` → ~thousands
  of hits across the corpus, but only X files contain the EXACT
  trigram sequence): ~100-1000× speedup over rg.
- Queries that match many files (`grep void`): no speedup vs. rg
  because intersection still has most files. Fall back to current
  pipeline.
- Regex queries: extract literal substrings (lookup, longest common
  substring) → trigrammify those. Same approach as livegrep.

**Cost:** ~6 hours of code; ~500 LOC, new module. Doubles the on-disk
index size. Build time goes up ~20% (trigram extraction is cheap
relative to tree-sitter parse).


## Combined effect

Cold query times against a finalized 1 M-file AOSP+Linux index:

| Query                          | Today    | + lazy reader | + lazy + trigram |
|---|---|---|---|
| `scry def Activity`            | ~400 ms  | ~10 ms        | (same)           |
| `scry callers transact`        | ~400 ms  | ~10 ms        | (same)           |
| `scry grep '_ZN3art'`          | ~3 s     | (same)        | ~30 ms           |
| `scry grep 'TODO\(.*\):'`      | ~5 s     | (same)        | ~50 ms           |
| `scry serve` 100 queries       | ~1 s     | ~50 ms        | (same)           |

Both items belong on the roadmap. Lazy reader first (simpler, immediate
RAM relief). Trigram index second (bigger win but more code + needs
re-index).
