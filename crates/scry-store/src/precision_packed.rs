//! Mmap-backed packed layout shared by the two precision sidecars
//! (`clang_usrs.bin` from libclang USRs, `scip_index.bin` from SCIP
//! imports). Both sidecars carry the same shape: per-record
//! `(abs_path, byte_offset, symbol_id, kind)` plus two interned
//! string tables (symbol strings, absolute paths).
//!
//! The earlier bincode shape stored an owned `String abs_path` per
//! record. On the AOSP-scale SCIP sidecar (14 M records, 1.9 GB on
//! disk) the open-time decode + per-record `String::clone` for the
//! `HashMap<String, …>` indices cost ~17 s wall-clock and dominated
//! every strict-precision query. This layout retires that:
//!
//! - mmap the whole file (`safe_mmap`, single syscall).
//! - Records sorted by `(path_id, byte_offset)` so the records for
//!   a given path are a contiguous slice. Path table stores the
//!   `(records_start, records_count)` range — windowed lookup
//!   becomes a `partition_point` over a slice.
//! - Symbol/path string tables use the offsets+blob pattern from
//!   [`crate::files_packed`]: `(n+1) * u32` offsets pointing into a
//!   single concatenated UTF-8 blob, sentinel last.
//!
//! Open is two `safe_mmap` calls + header parse + range checks
//! (sub-millisecond). All accessors return borrowed `&str` /
//! `&[..]` from the mmap, so the per-query cost is the algorithmic
//! cost only — no decode tax.
//!
//! ## On-disk layout (little-endian throughout)
//!
//! ```text
//! Header (64 bytes):
//!   [ 0.. 8] magic              = b"SCRYUP01" (USR variant)
//!                                 b"SCRYSP01" (SCIP variant)
//!   [ 8..12] version            = 1                u32
//!   [12..16] n_records                              u32
//!   [16..20] n_symbols                              u32
//!   [20..24] n_paths                                u32
//!   [24..32] records_off                            u64
//!   [32..40] sym_table_off                          u64
//!   [40..48] sym_blob_off                           u64
//!   [48..56] path_table_off                         u64
//!   [56..64] path_blob_off                          u64
//!
//! Records (n_records * RECORD_LEN = 12 bytes each), sorted by
//!   (path_id ASC, byte_offset ASC):
//!     [ 0.. 4] byte_offset                          u32
//!     [ 4.. 8] symbol_id                            u32
//!     [ 8.. 9] kind / role                          u8
//!     [ 9..12] pad                                  [u8; 3]
//!
//! Symbol table ((n_symbols + 1) * 4 bytes): sentinel last so
//!   sym_blob[off[i]..off[i+1]] is the i'th symbol's UTF-8 bytes.
//!
//! Symbol blob: concatenated UTF-8 bytes of every unique symbol.
//!
//! Path table ((n_paths + 1) * 12 bytes), one entry per unique path:
//!     [ 0.. 4] path_blob_off (offset into path_blob)            u32
//!     [ 4.. 8] records_start (first record_id for this path)    u32
//!     [ 8..12] records_count                                    u32
//!   (The sentinel n_paths'th entry only carries path_blob_off so
//!   path_i bytes = path_blob[off[i]..off[i+1]].)
//!
//! Path blob: concatenated UTF-8 bytes of every unique abs_path.
//! ```

use anyhow::{anyhow, Context, Result};
use memmap2::Mmap;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 64;
pub const RECORD_LEN: usize = 12;
pub const PATH_TABLE_ENTRY_LEN: usize = 12;

pub const MAGIC_CLANG_USR: &[u8; 8] = b"SCRYUP01";
pub const MAGIC_SCIP: &[u8; 8] = b"SCRYSP01";

/// Per-record `kind` byte: 0 = decl, 1 = ref, 2 = call.
pub const KIND_DECL: u8 = 0;
pub const KIND_REF:  u8 = 1;
pub const KIND_CALL: u8 = 2;

/// Bitmask of allowed kinds passed to the `_kind`-suffixed window
/// lookups. Bit i ⇔ kind i. Pick the mask that matches your query
/// site: looking up the def-anchor at a known def → `KIND_MASK_DECL_ONLY`;
/// looking up a ref-anchor at a call site → `KIND_MASK_REF_OR_CALL`
/// (excluding decl avoids resolving an in-method call to the
/// enclosing method's own def via the byte-offset window).
pub const KIND_MASK_DECL_ONLY:   u8 = 1u8 << KIND_DECL;
pub const KIND_MASK_REF_OR_CALL: u8 = (1u8 << KIND_REF) | (1u8 << KIND_CALL);
pub const KIND_MASK_ANY:         u8 = KIND_MASK_DECL_ONLY | KIND_MASK_REF_OR_CALL;

/// Mmap'd precision sidecar. Both variants (clang USR / SCIP) share
/// the same code path; only the magic bytes differ.
pub struct PrecisionPacked {
    mmap: Mmap,
    n_records: u32,
    n_symbols: u32,
    n_paths: u32,
    records_off: usize,
    sym_table_off: usize,
    sym_blob_off: usize,
    path_table_off: usize,
    path_blob_off: usize,
    /// `abs_path` → `path_id`. Built lazily on first lookup so
    /// callers that go through `precompute_by_file_ids` directly
    /// (file_id-indexed) skip building it.
    path_index_cell: OnceLock<HashMap<String, u32>>,
}

impl std::fmt::Debug for PrecisionPacked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrecisionPacked")
            .field("n_records", &self.n_records)
            .field("n_symbols", &self.n_symbols)
            .field("n_paths", &self.n_paths)
            .field("size_bytes", &self.mmap.len())
            .finish()
    }
}

impl PrecisionPacked {
    /// Open and validate the sidecar. `expected_magic` selects USR
    /// vs SCIP variant — wrong magic returns an error so a clang
    /// reader can't accidentally consume a SCIP sidecar and vice
    /// versa. Missing file → `Ok(None)`.
    pub fn open(path: &Path, expected_magic: &[u8; 8]) -> Result<Option<Self>> {
        if !path.exists() { return Ok(None); }
        let f = File::open(path)
            .with_context(|| format!("open {} for mmap", path.display()))?;
        // SAFETY: see crate::safe_mmap docs — the indexer writes via
        // tmp+rename so a finalized sidecar is never mutated in place.
        let mmap = unsafe { Mmap::map(&f) }
            .with_context(|| format!("mmap {}", path.display()))?;
        if mmap.len() < HEADER_LEN {
            return Err(anyhow!("{}: short header", path.display()));
        }
        if &mmap[0..8] != expected_magic {
            return Err(anyhow!(
                "{}: bad magic — got {:?}, expected {:?}",
                path.display(), &mmap[0..8], expected_magic,
            ));
        }
        let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(anyhow!(
                "{}: unsupported precision_packed version {} (expected {})",
                path.display(), version, VERSION,
            ));
        }
        let n_records = u32::from_le_bytes(mmap[12..16].try_into().unwrap());
        let n_symbols = u32::from_le_bytes(mmap[16..20].try_into().unwrap());
        let n_paths = u32::from_le_bytes(mmap[20..24].try_into().unwrap());
        let records_off = u64::from_le_bytes(mmap[24..32].try_into().unwrap()) as usize;
        let sym_table_off = u64::from_le_bytes(mmap[32..40].try_into().unwrap()) as usize;
        let sym_blob_off = u64::from_le_bytes(mmap[40..48].try_into().unwrap()) as usize;
        let path_table_off = u64::from_le_bytes(mmap[48..56].try_into().unwrap()) as usize;
        let path_blob_off = u64::from_le_bytes(mmap[56..64].try_into().unwrap()) as usize;
        // Bounds sanity.
        let need = records_off
            .checked_add((n_records as usize).checked_mul(RECORD_LEN)
                .ok_or_else(|| anyhow!("{}: n_records*RECORD_LEN overflow", path.display()))?)
            .ok_or_else(|| anyhow!("{}: records segment overflow", path.display()))?;
        if mmap.len() < need
            || mmap.len() < sym_table_off
            || mmap.len() < sym_blob_off
            || mmap.len() < path_table_off
            || mmap.len() < path_blob_off
        {
            return Err(anyhow!("{}: truncated body", path.display()));
        }
        Ok(Some(Self {
            mmap, n_records, n_symbols, n_paths,
            records_off, sym_table_off, sym_blob_off,
            path_table_off, path_blob_off,
            path_index_cell: OnceLock::new(),
        }))
    }

    pub fn record_count(&self) -> usize { self.n_records as usize }
    pub fn symbol_count(&self) -> usize { self.n_symbols as usize }
    pub fn path_count(&self) -> usize { self.n_paths as usize }
    pub fn is_empty(&self) -> bool { self.n_records == 0 }

    /// Symbol string by id. None if id out of range.
    pub fn symbol(&self, sym_id: u32) -> Option<&str> {
        if sym_id >= self.n_symbols { return None; }
        let table = self.sym_table_off + (sym_id as usize) * 4;
        let off = u32::from_le_bytes(self.mmap.get(table..table + 4)?.try_into().ok()?) as usize;
        let next = u32::from_le_bytes(self.mmap.get(table + 4..table + 8)?.try_into().ok()?) as usize;
        let start = self.sym_blob_off + off;
        let end = self.sym_blob_off + next;
        std::str::from_utf8(self.mmap.get(start..end)?).ok()
    }

    /// Absolute path by id. None if id out of range.
    pub fn path_of(&self, path_id: u32) -> Option<&str> {
        if path_id >= self.n_paths { return None; }
        let table = self.path_table_off + (path_id as usize) * PATH_TABLE_ENTRY_LEN;
        let off = u32::from_le_bytes(self.mmap.get(table..table + 4)?.try_into().ok()?) as usize;
        // The NEXT entry's path_blob_off is the upper bound. The
        // n_paths'th entry is a sentinel that only carries the
        // upper-bound offset.
        let next_table = table + PATH_TABLE_ENTRY_LEN;
        let next = u32::from_le_bytes(self.mmap.get(next_table..next_table + 4)?.try_into().ok()?) as usize;
        let start = self.path_blob_off + off;
        let end = self.path_blob_off + next;
        std::str::from_utf8(self.mmap.get(start..end)?).ok()
    }

    /// (records_start, records_count) range for a given path_id.
    pub fn path_records_range(&self, path_id: u32) -> Option<(u32, u32)> {
        if path_id >= self.n_paths { return None; }
        let table = self.path_table_off + (path_id as usize) * PATH_TABLE_ENTRY_LEN;
        let start = u32::from_le_bytes(self.mmap.get(table + 4..table + 8)?.try_into().ok()?);
        let count = u32::from_le_bytes(self.mmap.get(table + 8..table + 12)?.try_into().ok()?);
        Some((start, count))
    }

    /// Read one record's (byte_offset, symbol_id, kind).
    pub fn record(&self, record_id: u32) -> Option<(u32, u32, u8)> {
        if record_id >= self.n_records { return None; }
        let base = self.records_off + (record_id as usize) * RECORD_LEN;
        let bo = u32::from_le_bytes(self.mmap.get(base..base + 4)?.try_into().ok()?);
        let sid = u32::from_le_bytes(self.mmap.get(base + 4..base + 8)?.try_into().ok()?);
        let kind = *self.mmap.get(base + 8)?;
        Some((bo, sid, kind))
    }

    /// Build (or borrow) the `abs_path → path_id` HashMap. Cost on
    /// first call: n_paths string-clone+insert (~50 k on AOSP), a
    /// few ms. Cached for subsequent lookups.
    fn path_index(&self) -> &HashMap<String, u32> {
        self.path_index_cell.get_or_init(|| {
            let mut m: HashMap<String, u32> = HashMap::with_capacity(self.n_paths as usize);
            for i in 0..self.n_paths {
                if let Some(p) = self.path_of(i) {
                    m.insert(p.to_string(), i);
                }
            }
            m
        })
    }

    /// Look up the symbol id for an exact (abs_path, byte_offset).
    /// None if no record covers that site.
    pub fn symbol_at(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
        let &path_id = self.path_index().get(abs_path)?;
        let (start, count) = self.path_records_range(path_id)?;
        let slice_start = start as usize;
        let slice_end = slice_start + count as usize;
        for i in slice_start..slice_end {
            let (bo, sid, _) = self.record(i as u32)?;
            if bo == byte_offset { return self.symbol(sid); }
            if bo > byte_offset { return None; }  // sorted
        }
        None
    }

    /// Windowed lookup: returns the symbol id of the record whose
    /// `byte_offset` is closest to `byte_offset` within ±`window`,
    /// or None if no record falls in that range. Same semantics as
    /// the bincode-era `usr_for_window`/`symbol_for_window`.
    pub fn symbol_for_window(
        &self,
        abs_path: &str,
        byte_offset: u32,
        window: u32,
    ) -> Option<&str> {
        self.symbol_for_window_kind(abs_path, byte_offset, window, KIND_MASK_ANY)
    }

    /// Same as [`symbol_for_window`] but only considers records whose
    /// `kind` is allowed by `kind_mask` (bit 0 = decl, bit 1 = ref,
    /// bit 2 = call). Use [`KIND_MASK_DECL_ONLY`] when looking up the
    /// def-anchor at a known def site, and [`KIND_MASK_REF_OR_CALL`]
    /// when looking up the ref-anchor at a call site — the unfiltered
    /// "nearest in window" otherwise collapses an in-method call with
    /// the enclosing method's decl, mis-attributing the ref to the
    /// surrounding def (real case: `return Binder.clearCallingIdentity()`
    /// on the line right after a `public long clearCallingIdentity() {`
    /// def in AOSP's AMS::Injector wrapper resolved to the wrapper
    /// itself because the unfiltered window picked the closer decl
    /// anchor).
    pub fn symbol_for_window_kind(
        &self,
        abs_path: &str,
        byte_offset: u32,
        window: u32,
        kind_mask: u8,
    ) -> Option<&str> {
        let &path_id = self.path_index().get(abs_path)?;
        self.symbol_for_window_by_path_id_kind(path_id, byte_offset, window, kind_mask)
    }

    /// Same as [`symbol_for_window`] but when the caller already
    /// has the path_id (e.g. via `precompute_by_file_ids`). Skips
    /// the HashMap probe.
    pub fn symbol_for_window_by_path_id(
        &self,
        path_id: u32,
        byte_offset: u32,
        window: u32,
    ) -> Option<&str> {
        self.symbol_for_window_by_path_id_kind(path_id, byte_offset, window, KIND_MASK_ANY)
    }

    /// Same as [`symbol_for_window_by_path_id`] but filters by kind
    /// mask (see [`symbol_for_window_kind`]).
    pub fn symbol_for_window_by_path_id_kind(
        &self,
        path_id: u32,
        byte_offset: u32,
        window: u32,
        kind_mask: u8,
    ) -> Option<&str> {
        let (start, count) = self.path_records_range(path_id)?;
        if count == 0 { return None; }
        let lo = byte_offset.saturating_sub(window);
        let hi = byte_offset.saturating_add(window);
        // Bisect to find the first record whose byte_offset >= lo.
        let slice_start = start as usize;
        let slice_end = slice_start + count as usize;
        let mut left = slice_start;
        let mut right = slice_end;
        while left < right {
            let mid = (left + right) / 2;
            let (bo, _, _) = self.record(mid as u32)?;
            if bo < lo { left = mid + 1; } else { right = mid; }
        }
        let mut best: Option<(u32, u32)> = None;
        for i in left..slice_end {
            let (bo, sid, k) = self.record(i as u32)?;
            if bo > hi { break; }
            if kind_mask & (1u8 << k) == 0 { continue; }
            let d = bo.abs_diff(byte_offset);
            if best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, sid));
            }
        }
        best.and_then(|(_, sid)| self.symbol(sid))
    }

    /// Span-based lookup: returns the symbol id of the record whose
    /// `byte_offset` lies inside `[byte_start, byte_end]`, filtered
    /// by `kind_mask`. Returns the record nearest `byte_start` (the
    /// usual identifier-start anchor); ties prefer the earlier
    /// record.
    ///
    /// Use this at REF sites where the ref's
    /// `(byte_start, byte_end)` covers exactly the identifier the
    /// tree-sitter ref points at: Kythe emits separate anchors for
    /// EVERY identifier in a dotted access (e.g.
    /// `Binder.clearCallingIdentity()` gets one ref-anchor at the
    /// `Binder` receiver and another at the `clearCallingIdentity`
    /// method name), and a fat byte-window picks the closer
    /// receiver anchor instead of the intended method anchor. The
    /// identifier span eliminates that bleed.
    pub fn symbol_in_span_kind(
        &self,
        abs_path: &str,
        byte_start: u32,
        byte_end: u32,
        kind_mask: u8,
    ) -> Option<&str> {
        let &path_id = self.path_index().get(abs_path)?;
        self.symbol_in_span_by_path_id_kind(path_id, byte_start, byte_end, kind_mask)
    }

    /// `path_id`-keyed twin of [`symbol_in_span_kind`].
    pub fn symbol_in_span_by_path_id_kind(
        &self,
        path_id: u32,
        byte_start: u32,
        byte_end: u32,
        kind_mask: u8,
    ) -> Option<&str> {
        let (start, count) = self.path_records_range(path_id)?;
        if count == 0 || byte_end < byte_start { return None; }
        let slice_start = start as usize;
        let slice_end = slice_start + count as usize;
        let mut left = slice_start;
        let mut right = slice_end;
        while left < right {
            let mid = (left + right) / 2;
            let (bo, _, _) = self.record(mid as u32)?;
            if bo < byte_start { left = mid + 1; } else { right = mid; }
        }
        let mut best: Option<(u32, u32)> = None;
        for i in left..slice_end {
            let (bo, sid, k) = self.record(i as u32)?;
            if bo > byte_end { break; }
            if kind_mask & (1u8 << k) == 0 { continue; }
            let d = bo - byte_start;
            if best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, sid));
            }
        }
        best.and_then(|(_, sid)| self.symbol(sid))
    }

    /// Build a per-file_id slice index for the query hot path.
    /// Walks the provided `(file_id, abs_path)` iterator, probes
    /// the lazy path index, and records the path_id per file_id
    /// (or `u32::MAX` for files the sidecar doesn't cover).
    /// Per-ref lookup then becomes a `Vec::get(file_id)` +
    /// bisect-on-records, no HashMap probe.
    pub fn precompute_by_file_ids<'a>(
        &'a self,
        paths_by_file_id: impl Iterator<Item = (u32, &'a str)>,
        file_count: usize,
    ) -> ByFileLookup<'a> {
        let idx = self.path_index();
        let mut path_ids: Vec<u32> = vec![u32::MAX; file_count];
        for (fid, path) in paths_by_file_id {
            if (fid as usize) >= path_ids.len() { continue; }
            if let Some(&pid) = idx.get(path) {
                path_ids[fid as usize] = pid;
            }
        }
        ByFileLookup { packed: self, path_ids }
    }
}

/// Per-query precomputed lookup keyed by `file_id`. Yielded by
/// [`PrecisionPacked::precompute_by_file_ids`].
pub struct ByFileLookup<'a> {
    packed: &'a PrecisionPacked,
    path_ids: Vec<u32>,
}

impl<'a> ByFileLookup<'a> {
    pub fn symbol_for_window(
        &self,
        file_id: u32,
        byte_offset: u32,
        window: u32,
    ) -> Option<&'a str> {
        self.symbol_for_window_kind(file_id, byte_offset, window, KIND_MASK_ANY)
    }

    /// Same as [`symbol_for_window`] but only considers records whose
    /// `kind` is allowed by `kind_mask`. See the standalone
    /// `symbol_for_window_kind` on [`PrecisionPacked`] for the
    /// rationale (lookup at a def site vs at a ref site need
    /// different masks to avoid mutual misattribution).
    pub fn symbol_for_window_kind(
        &self,
        file_id: u32,
        byte_offset: u32,
        window: u32,
        kind_mask: u8,
    ) -> Option<&'a str> {
        let pid = *self.path_ids.get(file_id as usize)?;
        if pid == u32::MAX { return None; }
        self.packed.symbol_for_window_by_path_id_kind(pid, byte_offset, window, kind_mask)
    }

    /// Span-based ref-site lookup; see standalone
    /// [`PrecisionPacked::symbol_in_span_kind`] for rationale (the
    /// fat byte-window picks Kythe's adjacent receiver-anchor
    /// instead of the intended method anchor in dotted accesses
    /// like `Binder.clearCallingIdentity()`).
    pub fn symbol_in_span_kind(
        &self,
        file_id: u32,
        byte_start: u32,
        byte_end: u32,
        kind_mask: u8,
    ) -> Option<&'a str> {
        let pid = *self.path_ids.get(file_id as usize)?;
        if pid == u32::MAX { return None; }
        self.packed.symbol_in_span_by_path_id_kind(pid, byte_start, byte_end, kind_mask)
    }

    /// True when the sidecar has any records for this file_id.
    /// Lets callers distinguish "sidecar covers this file but says
    /// nothing about this byte offset" (a meaningful unresolved) from
    /// "sidecar doesn't cover this file at all" (legitimate fallback
    /// to a different resolver).
    pub fn covers_file_id(&self, file_id: u32) -> bool {
        self.path_ids.get(file_id as usize)
            .copied()
            .map(|pid| pid != u32::MAX)
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Input record handed to [`write`]. Owned strings on the writer
/// side; the writer interns them.
pub struct Record<'a> {
    pub abs_path: &'a str,
    pub byte_offset: u32,
    pub symbol: &'a str,
    pub kind: u8,
}

/// Write a packed sidecar from the given records. `magic` selects
/// USR vs SCIP variant. Records are de-duplicated within a path
/// (same offset + same symbol = one row); symbol and path strings
/// are interned. Output is sorted by `(path_id, byte_offset)` so
/// the reader can binary-search per-path.
pub fn write(
    path: &Path,
    magic: &[u8; 8],
    records: &[Record<'_>],
) -> Result<()> {
    // Intern symbols and paths in insertion order to keep the
    // writer deterministic (two runs against the same input emit
    // the same bytes).
    let mut sym_to_id: HashMap<&str, u32> = HashMap::new();
    let mut symbols: Vec<&str> = Vec::new();
    let mut path_to_id: HashMap<&str, u32> = HashMap::new();
    let mut paths: Vec<&str> = Vec::new();
    let mut decorated: Vec<(u32, u32, u32, u8)> = Vec::with_capacity(records.len());
    for r in records {
        let sid = *sym_to_id.entry(r.symbol).or_insert_with(|| {
            let id = symbols.len() as u32;
            symbols.push(r.symbol);
            id
        });
        let pid = *path_to_id.entry(r.abs_path).or_insert_with(|| {
            let id = paths.len() as u32;
            paths.push(r.abs_path);
            id
        });
        decorated.push((pid, r.byte_offset, sid, r.kind));
    }
    // Sort by (path_id, byte_offset). Stable sort keeps insertion
    // order for ties (rare: same path+offset with different symbol
    // id usually means decl-as-ref overlap from the indexer).
    decorated.sort_by_key(|r| (r.0, r.1));

    // Compute per-path (records_start, records_count). decorated
    // is sorted by path_id, so we can sweep.
    let n_paths = paths.len() as u32;
    let mut path_ranges: Vec<(u32, u32)> = vec![(0, 0); paths.len()];
    {
        let mut i = 0usize;
        while i < decorated.len() {
            let pid = decorated[i].0;
            let mut j = i;
            while j < decorated.len() && decorated[j].0 == pid { j += 1; }
            path_ranges[pid as usize] = (i as u32, (j - i) as u32);
            i = j;
        }
    }

    // Compute symbol/path blob offsets (and sentinels).
    let mut sym_offsets: Vec<u32> = Vec::with_capacity(symbols.len() + 1);
    let mut sym_blob: Vec<u8> = Vec::new();
    for s in &symbols {
        sym_offsets.push(sym_blob.len() as u32);
        sym_blob.extend_from_slice(s.as_bytes());
    }
    sym_offsets.push(sym_blob.len() as u32);

    let mut path_offsets_only: Vec<u32> = Vec::with_capacity(paths.len() + 1);
    let mut path_blob: Vec<u8> = Vec::new();
    for p in &paths {
        path_offsets_only.push(path_blob.len() as u32);
        path_blob.extend_from_slice(p.as_bytes());
    }
    path_offsets_only.push(path_blob.len() as u32);

    // Layout the file:
    //   header  (HEADER_LEN)
    //   records (n_records * RECORD_LEN)
    //   sym table  ((n_symbols + 1) * 4)
    //   sym blob
    //   path table ((n_paths + 1) * PATH_TABLE_ENTRY_LEN)
    //   path blob
    let n_records = decorated.len() as u32;
    let records_off = HEADER_LEN as u64;
    let sym_table_off = records_off + (n_records as u64) * (RECORD_LEN as u64);
    let sym_blob_off = sym_table_off + ((symbols.len() as u64) + 1) * 4;
    let path_table_off = sym_blob_off + (sym_blob.len() as u64);
    let path_blob_off = path_table_off + ((paths.len() as u64) + 1) * (PATH_TABLE_ENTRY_LEN as u64);

    let mut f = std::io::BufWriter::with_capacity(
        1 << 20,
        File::create(path)
            .with_context(|| format!("create {}", path.display()))?,
    );
    // Header
    f.write_all(magic)?;
    f.write_all(&VERSION.to_le_bytes())?;
    f.write_all(&n_records.to_le_bytes())?;
    f.write_all(&(symbols.len() as u32).to_le_bytes())?;
    f.write_all(&n_paths.to_le_bytes())?;
    f.write_all(&records_off.to_le_bytes())?;
    f.write_all(&sym_table_off.to_le_bytes())?;
    f.write_all(&sym_blob_off.to_le_bytes())?;
    f.write_all(&path_table_off.to_le_bytes())?;
    f.write_all(&path_blob_off.to_le_bytes())?;
    // Records
    let mut buf = [0u8; RECORD_LEN];
    for (_pid, bo, sid, kind) in &decorated {
        buf[0..4].copy_from_slice(&bo.to_le_bytes());
        buf[4..8].copy_from_slice(&sid.to_le_bytes());
        buf[8] = *kind;
        buf[9..12].fill(0);
        f.write_all(&buf)?;
    }
    // Symbol table
    for off in &sym_offsets {
        f.write_all(&off.to_le_bytes())?;
    }
    // Symbol blob
    f.write_all(&sym_blob)?;
    // Path table — for each path, (path_blob_off, records_start, records_count).
    // Sentinel n_paths'th entry only carries path_blob_off (the
    // other 8 bytes are zero, never read).
    let mut entry = [0u8; PATH_TABLE_ENTRY_LEN];
    for i in 0..paths.len() {
        let off = path_offsets_only[i];
        let (rs, rc) = path_ranges[i];
        entry[0..4].copy_from_slice(&off.to_le_bytes());
        entry[4..8].copy_from_slice(&rs.to_le_bytes());
        entry[8..12].copy_from_slice(&rc.to_le_bytes());
        f.write_all(&entry)?;
    }
    // Sentinel: only off matters; rs/rc zeroed.
    let sentinel_off = *path_offsets_only.last().unwrap_or(&0);
    entry[0..4].copy_from_slice(&sentinel_off.to_le_bytes());
    entry[4..12].fill(0);
    f.write_all(&entry)?;
    // Path blob
    f.write_all(&path_blob)?;
    f.flush()?;
    Ok(())
}

/// Generate a typed wrapper around [`PrecisionPacked`] with
/// language-flavoured method names. Each producer (`scry-clang`,
/// `scry-scip`) calls this once to get the type + write helper it
/// exposes; downstream callers keep their domain-specific naming
/// (`usr_for_window` vs `symbol_for_window`, `usr_count` vs
/// `symbol_count`) at no extra runtime cost.
///
/// Arguments:
///   `$index`      — wrapper type name (`ClangUsrIndex` / `ScipIndex`)
///   `$lookup`     — per-file lookup type name (`ByFileLookup` / `ByFileSymbolLookup`)
///   `$record`     — writer-side input struct name (`UsrRecord` / `ScipRecord`)
///   `$record_doc` — doc string for the input record struct
///   `$symid`      — field on `$record` carrying the symbol-table index
///                   (`usr_id` / `symbol_id`)
///   `$kind`       — field on `$record` carrying the per-record kind byte
///                   (`kind` / `role`)
///   `$kind_doc`   — doc string for that field
///   `$lookup_fn`  — public name of the per-file accessor on `$index` /
///                   `$lookup` (`usr_for_window` / `symbol_for_window`)
///   `$exact_fn`   — public name of the exact-offset accessor on `$index`
///                   (`usr_for` / `symbol_for`)
///   `$count_fn`   — public name of the symbol-table size accessor
///                   (`usr_count` / `symbol_count`)
///   `$iter_fn`    — public name of the symbol-table iterator
///                   (`iter_usrs` / `iter_symbols`)
///   `$table_arg`  — name of the symbol-table arg on `write`
///                   (`usr_table` / `symbol_table`)
///   `$magic`      — magic constant from [`crate`]
#[macro_export]
macro_rules! precision_sidecar_wrapper {
    (
        index = $index:ident,
        lookup = $lookup:ident,
        record = $record:ident,
        record_doc = $record_doc:expr,
        symid = $symid:ident,
        kind = $kind:ident,
        kind_doc = $kind_doc:expr,
        lookup_fn = $lookup_fn:ident,
        exact_fn = $exact_fn:ident,
        count_fn = $count_fn:ident,
        iter_fn = $iter_fn:ident,
        table_arg = $table_arg:ident,
        magic = $magic:path,
    ) => {
        use $crate::precision_packed::{self, PrecisionPacked, Record};
        use anyhow::Result;
        use std::path::Path;

        #[doc = $record_doc]
        #[derive(Debug, Clone)]
        pub struct $record {
            pub abs_path: String,
            pub byte_offset: u32,
            pub $symid: u32,
            #[doc = $kind_doc]
            pub $kind: u8,
        }

        /// Mmap'd precision sidecar. All accessors borrow from the
        /// mmap, so per-query cost is the algorithmic cost only.
        #[derive(Debug)]
        pub struct $index {
            packed: PrecisionPacked,
        }

        impl $index {
            /// Open the sidecar. Returns `Ok(None)` if the file is
            /// absent, an error if it's present but malformed / has
            /// the wrong magic.
            pub fn open(path: &Path) -> Result<Option<Self>> {
                Ok(PrecisionPacked::open(path, $magic)?
                    .map(|packed| Self { packed }))
            }

            /// Exact lookup at `(abs_path, byte_offset)`. None if no
            /// record covers that site.
            pub fn $exact_fn(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
                self.packed.symbol_at(abs_path, byte_offset)
            }

            /// Windowed lookup — returns the CLOSEST record within
            /// `±window` of `byte_offset`, or None. Used when the
            /// query and the producing indexer disagree on whether a
            /// cursor points at keyword vs identifier start.
            pub fn $lookup_fn(
                &self,
                abs_path: &str,
                byte_offset: u32,
                window: u32,
            ) -> Option<&str> {
                self.packed.symbol_for_window(abs_path, byte_offset, window)
            }

            pub fn len(&self) -> usize { self.packed.record_count() }
            pub fn is_empty(&self) -> bool { self.packed.is_empty() }
            pub fn $count_fn(&self) -> usize { self.packed.symbol_count() }

            /// Iterate the interned symbol table (string at id 0, 1,
            /// …). Used by `scry sidecar-inspect` for sample output.
            pub fn $iter_fn(&self) -> impl Iterator<Item = &str> {
                (0..self.packed.symbol_count() as u32)
                    .filter_map(|i| self.packed.symbol(i))
            }

            /// Stream every `(abs_path, byte_offset, symbol, kind)`
            /// row in the sidecar. Path order matches on-disk (sorted
            /// by path_id then byte_offset), so callers get per-path
            /// runs together.
            pub fn iter_records(&self) -> impl Iterator<Item = (&str, u32, &str, u8)> {
                (0..self.packed.path_count() as u32).flat_map(move |pid| {
                    let path = self.packed.path_of(pid).unwrap_or("");
                    let (start, count) = self.packed
                        .path_records_range(pid).unwrap_or((0, 0));
                    (start..start + count).filter_map(move |rid| {
                        let (bo, sid, k) = self.packed.record(rid)?;
                        let sym = self.packed.symbol(sid)?;
                        Some((path, bo, sym, k))
                    })
                })
            }

            /// Build a [`$lookup`] indexed by scry's `FileEntry::id`,
            /// so per-ref lookups in a query loop become a
            /// `Vec::get(file_id)` + binary search instead of a
            /// `HashMap<String, …>::get(&display_path)` per ref.
            /// `paths_by_file_id` must yield each `(file_id, abs_path)`
            /// pair at most once.
            pub fn precompute_by_file_ids<'a>(
                &'a self,
                paths_by_file_id: impl Iterator<Item = (u32, &'a str)>,
                file_count: usize,
            ) -> $lookup<'a> {
                $lookup {
                    inner: self.packed
                        .precompute_by_file_ids(paths_by_file_id, file_count),
                }
            }
        }

        /// Per-query precomputed lookup keyed by `file_id`. Built by
        /// [`$index::precompute_by_file_ids`] and consulted per ref
        /// inside the precision filter.
        pub struct $lookup<'a> {
            inner: precision_packed::ByFileLookup<'a>,
        }

        impl<'a> $lookup<'a> {
            /// Same window semantics as [`$index::$lookup_fn`].
            pub fn $lookup_fn(
                &self,
                file_id: u32,
                byte_offset: u32,
                window: u32,
            ) -> Option<&'a str> {
                self.inner.symbol_for_window(file_id, byte_offset, window)
            }

            /// Kind-filtered variant of [`Self::$lookup_fn`]. Use
            /// `precision_packed::KIND_MASK_DECL_ONLY` when looking
            /// up the def-anchor at a def site;
            /// `precision_packed::KIND_MASK_REF_OR_CALL` when looking
            /// up the ref-anchor at a call site (excluding decl
            /// avoids resolving an in-method call to the enclosing
            /// method's own def via the byte-offset window).
            pub fn symbol_for_window_kind(
                &self,
                file_id: u32,
                byte_offset: u32,
                window: u32,
                kind_mask: u8,
            ) -> Option<&'a str> {
                self.inner.symbol_for_window_kind(
                    file_id, byte_offset, window, kind_mask,
                )
            }

            /// Span-based variant. See
            /// [`crate::precision_packed::ByFileLookup::symbol_in_span_kind`].
            /// Use at ref sites where `[byte_start, byte_end]` is the
            /// exact tree-sitter identifier span — Kythe emits a
            /// separate ref-anchor for each identifier in dotted
            /// accesses (e.g. both the receiver `Binder` and the
            /// method `clearCallingIdentity` in
            /// `Binder.clearCallingIdentity()`), and a fat byte-window
            /// would pick the closer receiver anchor instead of the
            /// intended method anchor.
            pub fn symbol_in_span_kind(
                &self,
                file_id: u32,
                byte_start: u32,
                byte_end: u32,
                kind_mask: u8,
            ) -> Option<&'a str> {
                self.inner.symbol_in_span_kind(
                    file_id, byte_start, byte_end, kind_mask,
                )
            }

            /// True when the sidecar has any records for this file_id.
            /// Lets callers distinguish "sidecar covers this file but
            /// says nothing about this byte offset" from "sidecar
            /// doesn't cover this file at all".
            pub fn covers_file_id(&self, file_id: u32) -> bool {
                self.inner.covers_file_id(file_id)
            }
        }

        /// Write the sidecar in packed format. Writer side keeps
        /// owned `String`s in `$table_arg` and indexes them by id;
        /// we expand each record to a borrowed (path, symbol) pair
        /// for [`precision_packed::write`] to intern.
        pub fn write(
            path: &Path,
            $table_arg: &[String],
            records: &[$record],
        ) -> Result<()> {
            let pp_records: Vec<Record<'_>> = records
                .iter()
                .map(|r| Record {
                    abs_path: r.abs_path.as_str(),
                    byte_offset: r.byte_offset,
                    symbol: $table_arg
                        .get(r.$symid as usize)
                        .map(String::as_str)
                        .unwrap_or(""),
                    kind: r.$kind,
                })
                .collect();
            precision_packed::write(path, $magic, &pp_records)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_roundtrip() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-pp-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write(&tmp, MAGIC_CLANG_USR, &[]).unwrap();
        let p = PrecisionPacked::open(&tmp, MAGIC_CLANG_USR).unwrap().unwrap();
        assert!(p.is_empty());
        assert_eq!(p.record_count(), 0);
        assert_eq!(p.symbol_count(), 0);
        assert_eq!(p.path_count(), 0);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn roundtrip_two_files() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-pp-rt-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let recs = vec![
            Record { abs_path: "/x/A.cpp", byte_offset: 100, symbol: "c:@F@foo#",  kind: 0 },
            Record { abs_path: "/x/A.cpp", byte_offset: 200, symbol: "c:@F@bar#",  kind: 1 },
            Record { abs_path: "/x/B.cpp", byte_offset:  10, symbol: "c:@F@foo#",  kind: 0 },
            Record { abs_path: "/x/B.cpp", byte_offset: 500, symbol: "c:@F@bar#",  kind: 2 },
            // Reverse-order insertion: writer must still emit in sorted order.
            Record { abs_path: "/x/B.cpp", byte_offset:  50, symbol: "c:@F@baz#",  kind: 1 },
        ];
        write(&tmp, MAGIC_SCIP, &recs).unwrap();
        let p = PrecisionPacked::open(&tmp, MAGIC_SCIP).unwrap().unwrap();
        assert_eq!(p.record_count(), 5);
        assert_eq!(p.symbol_count(), 3);  // foo, bar, baz
        assert_eq!(p.path_count(), 2);    // A.cpp, B.cpp

        // Exact lookup
        assert_eq!(p.symbol_at("/x/A.cpp", 100), Some("c:@F@foo#"));
        assert_eq!(p.symbol_at("/x/A.cpp", 200), Some("c:@F@bar#"));
        assert_eq!(p.symbol_at("/x/B.cpp",  10), Some("c:@F@foo#"));
        assert_eq!(p.symbol_at("/x/B.cpp", 500), Some("c:@F@bar#"));
        assert_eq!(p.symbol_at("/x/B.cpp",  50), Some("c:@F@baz#"));
        // Miss
        assert_eq!(p.symbol_at("/x/A.cpp", 150), None);
        assert_eq!(p.symbol_at("/x/C.cpp", 100), None);

        // Window — prefers the closer record.
        assert_eq!(p.symbol_for_window("/x/A.cpp", 105, 32), Some("c:@F@foo#"));
        assert_eq!(p.symbol_for_window("/x/A.cpp", 195, 32), Some("c:@F@bar#"));
        // Distance 50 ties; first found (foo at 100) wins via strict-less tie-break.
        assert_eq!(p.symbol_for_window("/x/A.cpp", 150, 60), Some("c:@F@foo#"));
        // Out of window
        assert_eq!(p.symbol_for_window("/x/A.cpp", 400, 32), None);

        // Wrong magic rejected
        assert!(PrecisionPacked::open(&tmp, MAGIC_CLANG_USR).is_err());

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn precompute_by_file_ids_borrows() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-pp-fid-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let recs = vec![
            Record { abs_path: "/x/A.cpp", byte_offset: 100, symbol: "s1", kind: 0 },
            Record { abs_path: "/x/B.cpp", byte_offset: 200, symbol: "s2", kind: 1 },
        ];
        write(&tmp, MAGIC_CLANG_USR, &recs).unwrap();
        let p = PrecisionPacked::open(&tmp, MAGIC_CLANG_USR).unwrap().unwrap();
        // Pretend scry's file table has 3 files: 0=A.cpp, 1=B.cpp, 2=unknown.
        let pairs = [(0u32, "/x/A.cpp"), (1u32, "/x/B.cpp"), (2u32, "/x/Z.cpp")];
        let lookup = p.precompute_by_file_ids(pairs.iter().copied(), 3);
        assert_eq!(lookup.symbol_for_window(0, 100, 16), Some("s1"));
        assert_eq!(lookup.symbol_for_window(1, 200, 16), Some("s2"));
        assert_eq!(lookup.symbol_for_window(2, 0, 16), None);  // unknown file
        assert_eq!(lookup.symbol_for_window(7, 0, 16), None);  // OOB file_id
        std::fs::remove_file(&tmp).ok();
    }
}
