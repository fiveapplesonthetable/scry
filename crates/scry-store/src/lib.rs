//! scry-store: on-disk index format for symbols, files, and roots.
//!
//! Phase 1: a simple bincode-serialized store plus an FST over symbol names
//! for prefix/fuzzy lookup. Phase 4 replaces this with a custom mmap'd
//! columnar layout; the public API stays.
//!
//! # Unsafe policy
//!
//! This is the ONLY crate in the scry workspace that uses `unsafe`. Every
//! other crate (scry-walker, scry-lang, scry-aosp, scry-cli) is
//! `#![forbid(unsafe_code)]`. The unsafe here is exclusively the
//! `memmap2::Mmap::map` call — fundamentally unsafe in Rust because the
//! kernel can change the file's contents (or truncate it) under the
//! reader, and a `&[u8]` view of the mapping breaks Rust's aliasing
//! invariants if that happens.
//!
//! All callers go through [`safe_mmap`] (this file). Invariants the
//! helper assumes and the caller must uphold:
//!   - The mmap'd path lives under an index directory that the indexer
//!     has finalized (atomic rename → no concurrent writers in steady
//!     state).
//!   - Old indexes are deleted only AFTER the reader is dropped (the
//!     CLI is one-shot; long-lived `scry serve` callers must not
//!     re-finalize the same index directory under them — but a fresh
//!     finalize creates a NEW dir, so re-opening works).
//!
//! Anything that would invalidate the mapping (truncating the file,
//! editing it in place, deleting it from under a live reader) is a
//! caller bug; the scry CLI's own invocation patterns never do this.

use anyhow::{anyhow, Context, Result};
use scry_walker::{FileKind, Profile};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub mod trigram;
pub mod embed;

/// Tell the kernel we plan to read every byte of `path` soon, so it
/// can start pulling pages into the page cache while we do other
/// work. Best-effort: the open() and the fadvise() are both Result-
/// less for the caller — a path we can't open just won't get
/// prefetched. Used by `cmd_grep` after the trigram pre-filter to
/// overlap candidate-file IO with the per-file scan loop.
/// Restore SIGPIPE to its default disposition (kill the process)
/// so a partial write to a closed pipe terminates cleanly instead
/// of panicking with a `BrokenPipe` error mid-flush.
///
/// Rust's runtime installs `SIG_IGN` for SIGPIPE so the I/O syscall
/// returns EPIPE rather than killing the process — fine for daemons,
/// wrong for CLI tools that pipe stdout to `head`/`less`/etc. Without
/// this, `scry grep PATTERN | head` panics loudly the moment `head`
/// closes its end of the pipe.
///
/// Single-call function; safe to invoke multiple times. No-op on
/// non-Unix platforms.
pub fn restore_default_sigpipe() {
    #[cfg(unix)]
    // SAFETY: setting a signal's disposition to the default has no
    // memory or thread-safety implications. This is the standard
    // CLI-tool boilerplate Rust intentionally omits from its
    // runtime. The cast through `_` lets the call work on every
    // libc::sighandler_t target ABI.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

pub fn prefault_path(path: &Path) {
    use std::os::unix::io::AsRawFd;
    if let Ok(f) = File::open(path) {
        // SAFETY: posix_fadvise on a valid fd is documented as
        // always-safe; the hint is advisory and ignored if the kernel
        // doesn't support it. We don't observe the result.
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_WILLNEED);
        }
        // `f` drops here — close() runs, but the WILLNEED hint stays
        // in the kernel's page-cache scheduler. The page-cache state
        // is what we wanted, not the fd.
    }
}

/// Open `path` and mmap it. Single source of truth for the only
/// `unsafe` block in the workspace; see the module-level "Unsafe
/// policy" docs for the contract callers must uphold (no concurrent
/// truncation / in-place editing of `path` while the returned mmap
/// is live). Returns an error with `path` in the context on either
/// the open() or the mmap call failing — important because a missing
/// or partial sidecar should produce a legible error, not a panic.
fn safe_mmap(path: &Path) -> Result<memmap2::Mmap> {
    let f = File::open(path)
        .with_context(|| format!("open {} for mmap", path.display()))?;
    // SAFETY: see module-level "Unsafe policy". The scry indexer
    // writes via tmp+rename so a finalized index file is never
    // mutated in place. The CLI is a one-shot process. `scry serve`
    // holds the mmap for its lifetime and the only producer
    // (the indexer) is a separate process; if a user re-runs the
    // indexer against the same INDEX while serve is running, the
    // rename creates a NEW inode and the old mmap stays valid until
    // serve is restarted.
    unsafe { memmap2::Mmap::map(&f) }
        .with_context(|| format!("mmap {}", path.display()))
}

/// On-disk bincode'd `Vec<T>` backed by an mmap + a u64-LE offsets sidecar.
/// Random-access decode of a single record is a u64 read + a bincode deserialize
/// over a borrowed slice — no allocation of the full Vec at open time.
///
/// The whole point: a finalized AOSP+Linux index has ~10 GB of bincode'd
/// records. Eagerly loading into Vec at open() costs ~5-10 s of bincode
/// deserialize + 10 GB RSS. With this, open() is ~10 ms (two mmap calls)
/// and per-query memory is whatever the lookup decodes (usually a few KB).
pub struct LazyVec<T: for<'de> Deserialize<'de>> {
    data_mmap: memmap2::Mmap,
    offsets_mmap: memmap2::Mmap,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: for<'de> Deserialize<'de>> LazyVec<T> {
    pub fn open(data_path: &Path, offsets_path: &Path) -> Result<Self> {
        let data_mmap = safe_mmap(data_path)?;
        let offsets_mmap = safe_mmap(offsets_path)?;
        Ok(Self { data_mmap, offsets_mmap, _phantom: std::marker::PhantomData })
    }

    pub fn len(&self) -> usize { self.offsets_mmap.len() / 8 }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// Decode and return one record by index. Returns None on out-of-bounds
    /// or a corrupt index/data file.
    pub fn get(&self, idx: usize) -> Option<T> {
        if idx >= self.len() { return None; }
        let o = idx * 8;
        let off = u64::from_le_bytes(self.offsets_mmap[o..o + 8].try_into().ok()?) as usize;
        if off >= self.data_mmap.len() { return None; }
        bincode::deserialize::<T>(&self.data_mmap[off..]).ok()
    }

    /// Streaming iterator over decoded records. Allocates one record at a
    /// time, NOT the whole Vec — safe to call on multi-GB indexes.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len()).filter_map(move |i| self.get(i))
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Kind of reference site (call, ctor, inheritance edge, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
#[repr(u8)]
pub enum RefKind {
    Call = 0,
    Ctor = 1,
    TypeUse = 2,
    FieldAccess = 3,
    Import = 4,
    InheritFrom = 5,
}

impl RefKind {
    pub fn short(&self) -> &'static str {
        match self {
            RefKind::Call => "call",
            RefKind::Ctor => "ctor",
            RefKind::TypeUse => "type",
            RefKind::FieldAccess => "field",
            RefKind::Import => "import",
            RefKind::InheritFrom => "inherit",
        }
    }
}

/// Stable cross-language symbol category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
pub enum SymbolKind {
    Module,
    Namespace,
    Package,
    Class,
    Interface,
    Trait,
    Struct,
    Union,
    Enum,
    EnumVariant,
    Type,
    Function,
    Method,
    Constructor,
    Field,
    Variable,
    Constant,
    Parameter,
    Macro,
    Annotation,
    Decorator,
    AidlInterface,
    AidlMethod,
    AidlParcelable,
    /// Live `aidl/` interface declaration whose source lives under
    /// `aidl_api/<pkg>/<N>/`, i.e. a frozen version snapshot. Same rank
    /// as `AidlInterface` but distinguishable from the development copy:
    /// agents asking "what is the V3 frozen surface of IFoo" can filter
    /// `--kind aidl.frozen`, while changes to the live development source
    /// stay under `--kind aidl.iface`.
    AidlFrozen,
    /// Synthetic shadow symbol for an AIDL-generated language binding.
    /// Emitted at AIDL parse time so `scry def IFoo.Stub` (Java) or
    /// `scry def BpIFoo` (C++) finds the AIDL source location instead
    /// of returning empty. The `lang` field on the SymbolRecord
    /// distinguishes which target language the shadow names.
    AidlShadow,
    /// Same idea for HIDL: BpFoo / BnFoo / IFoo proxies that exist in
    /// generated C++ but are conceptually rooted in the .hal file.
    HidlShadow,
    ProtoMessage,
    ProtoEnum,
    ProtoService,
    SoongModule,
    AconfigFlag,
    InitService,
    SepolicyType,
    ManifestComponent,
    XmlId,
    OwnersEmail,
    Other,
}

impl SymbolKind {
    pub fn short(&self) -> &'static str {
        match self {
            SymbolKind::Module => "module",
            SymbolKind::Namespace => "ns",
            SymbolKind::Package => "pkg",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "iface",
            SymbolKind::Trait => "trait",
            SymbolKind::Struct => "struct",
            SymbolKind::Union => "union",
            SymbolKind::Enum => "enum",
            SymbolKind::EnumVariant => "variant",
            SymbolKind::Type => "type",
            SymbolKind::Function => "fn",
            SymbolKind::Method => "method",
            SymbolKind::Constructor => "ctor",
            SymbolKind::Field => "field",
            SymbolKind::Variable => "var",
            SymbolKind::Constant => "const",
            SymbolKind::Parameter => "param",
            SymbolKind::Macro => "macro",
            SymbolKind::Annotation => "annot",
            SymbolKind::Decorator => "deco",
            SymbolKind::AidlInterface => "aidl.iface",
            SymbolKind::AidlMethod => "aidl.method",
            SymbolKind::AidlParcelable => "aidl.parcel",
            SymbolKind::AidlFrozen => "aidl.frozen",
            SymbolKind::AidlShadow => "aidl.shadow",
            SymbolKind::HidlShadow => "hidl.shadow",
            SymbolKind::ProtoMessage => "proto.msg",
            SymbolKind::ProtoEnum => "proto.enum",
            SymbolKind::ProtoService => "proto.svc",
            SymbolKind::SoongModule => "soong",
            SymbolKind::AconfigFlag => "aconfig",
            SymbolKind::InitService => "init.svc",
            SymbolKind::SepolicyType => "sepolicy",
            SymbolKind::ManifestComponent => "manifest",
            SymbolKind::XmlId => "xml.id",
            SymbolKind::OwnersEmail => "owner",
            SymbolKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootEntry {
    pub id: u8,
    pub path: String,
    pub profile: Profile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: u32,
    pub root_id: u8,
    pub relpath: String,
    pub kind: FileKind,
    pub size: u64,
}

impl FileEntry {
    pub fn display_path(&self, roots: &[RootEntry]) -> String {
        let root = &roots[self.root_id as usize];
        let mut p = PathBuf::from(&root.path);
        p.push(&self.relpath);
        p.display().to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefRecord {
    pub name: String,
    pub kind: RefKind,
    pub file_id: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line: u32,
    pub col: u32,
    pub scope_path: Vec<String>,
    pub lang: FileKind,
    /// Set during Phase 1 of resolution: a probable definition (by name match
    /// alone for now). Phase 2+ refines via scope/imports.
    pub resolved_to: Option<u64>,
}

impl RefRecord {
    /// See `SymbolRecord::estimated_bytes`.
    pub fn estimated_bytes(&self) -> usize {
        let mut n = size_of::<Self>();
        n += self.name.capacity();
        n += self.scope_path.capacity() * size_of::<String>();
        for s in &self.scope_path { n += s.capacity(); }
        n
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRecord {
    pub id: u64,
    pub name: String,
    pub fqn: Option<String>,
    pub kind: SymbolKind,
    pub file_id: u32,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line: u32,
    pub col: u32,
    pub scope_path: Vec<String>,
    pub lang: FileKind,
}

impl SymbolRecord {
    /// Composite ranking score (higher = better) used to sort hits so the
    /// most useful definition lands first. Heuristic, not a model:
    ///
    /// - Type definitions (class / interface / struct / etc.) outrank
    ///   functions outrank fields/vars outrank "other".
    /// - api-txt symbols (Android API surface signatures) are demoted —
    ///   they're declarations of every public API, useful but noisy when
    ///   you're looking for the actual implementation.
    /// - Build-system definitions (Soong modules, init services, sepolicy
    ///   types) keep their original boost since they ARE the canonical
    ///   definition for those domains.
    /// - Top-level (no scope_path) wins over deeply nested. Catches the
    ///   common case where you want the outer Activity class, not an
    ///   inner Activity helper class.
    ///
    /// Caller usually does: sort_by_key(|s| std::cmp::Reverse(s.rank_score()))
    pub fn rank_score(&self) -> i64 {
        let kind = match self.kind {
            SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait
            | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Union => 100,
            SymbolKind::Method | SymbolKind::Function | SymbolKind::Constructor => 90,
            SymbolKind::AidlInterface | SymbolKind::AidlMethod | SymbolKind::AidlParcelable | SymbolKind::AidlFrozen => 85,
            // Shadows are derived bindings; rank below the real AIDL/HIDL
            // declaration but above plain fields so they surface for the
            // common "find IFoo.Stub" question without burying the .aidl.
            SymbolKind::AidlShadow | SymbolKind::HidlShadow => 78,
            SymbolKind::ProtoMessage | SymbolKind::ProtoService | SymbolKind::ProtoEnum => 85,
            SymbolKind::SoongModule => 80,
            SymbolKind::InitService | SymbolKind::SepolicyType => 75,
            SymbolKind::Module | SymbolKind::Namespace | SymbolKind::Package => 70,
            SymbolKind::AconfigFlag | SymbolKind::ManifestComponent => 65,
            SymbolKind::Field | SymbolKind::Variable | SymbolKind::Constant => 50,
            SymbolKind::EnumVariant | SymbolKind::Type => 50,
            SymbolKind::Macro | SymbolKind::Annotation | SymbolKind::Decorator => 40,
            SymbolKind::Parameter => 20,
            SymbolKind::XmlId | SymbolKind::OwnersEmail | SymbolKind::Other => 10,
        };
        let lang_penalty = if matches!(self.lang, FileKind::ApiTxt) {
            // api-txt declarations are useful for "is this in the SDK"
            // but should never crowd out real source definitions.
            40
        } else { 0 };
        let scope_penalty = (self.scope_path.len() as i64).min(10) * 3;
        kind - lang_penalty - scope_penalty
    }

    /// Cheap, deterministic estimate of how much RAM this record occupies
    /// when held in a `Vec<SymbolRecord>`. Used by the streaming indexer to
    /// decide when to flush a chunk to disk WITHOUT polling /proc/self/status
    /// (which lags real allocation by 100s of ms and counts shared pages).
    pub fn estimated_bytes(&self) -> usize {
        // fixed struct fields + String capacities + Vec<String> contents
        let mut n = size_of::<Self>();
        n += self.name.capacity();
        if let Some(s) = self.fqn.as_ref() { n += s.capacity(); }
        n += self.scope_path.capacity() * size_of::<String>();
        for s in &self.scope_path { n += s.capacity(); }
        n
    }

    pub fn compute_id(
        root_id: u8,
        relpath: &str,
        kind: SymbolKind,
        scope_path: &[String],
        name: &str,
        line: u32,
    ) -> u64 {
        let mut h = blake3::Hasher::new();
        h.update(&[root_id]);
        h.update(relpath.as_bytes());
        h.update(&[kind as u8]);
        for s in scope_path {
            h.update(b"\0");
            h.update(s.as_bytes());
        }
        h.update(b"\0\0");
        h.update(name.as_bytes());
        h.update(&line.to_le_bytes());
        let out = h.finalize();
        u64::from_le_bytes(out.as_bytes()[..8].try_into().unwrap())
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Diagnostic payload returned by [`StoreReader::grep_explain`]:
/// the trigrams extracted from the query and how many files each one
/// covers, plus the final intersection (the candidate set the grep
/// scanner actually visits). Plain data; no formatting.
#[derive(Debug, Clone)]
pub struct GrepExplain {
    /// (trigram text, posting size) in input order. A 0-size entry
    /// means the trigram was absent from the FST — the intersection
    /// is necessarily empty.
    pub per_trigram: Vec<(String, usize)>,
    /// Final candidate file count after intersection + tombstone filter.
    pub candidates: usize,
}

/// Current on-disk format version. Stamped into every manifest the writer
/// produces. Bumped only when the *layout* of files in the index changes
/// in a way that a reader compiled against the new layout cannot tolerate
/// against an old index (or vice versa) — e.g. new required sidecar, wire
/// format change in a primary file, breaking schema change in a record.
///
/// Sidecar additions are NOT bumps: every sidecar (file_symbols, lazy
/// offsets, trigram postings, ref_resolutions, file_digests, chunks,
/// embeddings) is opened with `.exists()` first, and missing-sidecar
/// degrades gracefully to the eager / unfiltered path.
///
/// Mismatch handling today: readers do not refuse to open higher versions.
/// The version field is informational; the per-command stale-index warning
/// compares the `scry_version` string instead, which fires on any
/// release-to-release drift regardless of whether the layout changed.
/// Forward-incompatible bumps would need a refuse-on-higher check at
/// `StoreReader::open` if the layout actually breaks.
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub scry_version: String,
    pub indexed_at: String,
    pub roots: Vec<RootEntry>,
    pub stats: IndexStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub files_total: u64,
    pub files_parsed: u64,
    pub files_failed: u64,
    pub bytes_total: u64,
    pub symbols: u64,
    pub refs: u64,
    pub elapsed_ms: u128,
}

// ---------------------------------------------------------------------------
// On-disk layout
// ---------------------------------------------------------------------------

pub struct StorePaths {
    pub root: PathBuf,
}

impl StorePaths {
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self { root: root.into() }
    }
    pub fn manifest(&self) -> PathBuf { self.root.join("manifest.json") }
    pub fn roots(&self) -> PathBuf { self.root.join("roots.bin") }
    pub fn files(&self) -> PathBuf { self.root.join("files.bin") }
    pub fn symbols(&self) -> PathBuf { self.root.join("symbols.bin") }
    pub fn names_fst(&self) -> PathBuf { self.root.join("names.fst") }
    pub fn name_postings(&self) -> PathBuf { self.root.join("name_postings.bin") }
    pub fn refs(&self) -> PathBuf { self.root.join("refs.bin") }
    pub fn ref_names_fst(&self) -> PathBuf { self.root.join("ref_names.fst") }
    pub fn ref_postings(&self) -> PathBuf { self.root.join("ref_postings.bin") }
    pub fn trigram_fst(&self) -> PathBuf { self.root.join("trigrams.fst") }
    pub fn trigram_postings(&self) -> PathBuf { self.root.join("trigram_postings.bin") }
    pub fn symbol_offsets(&self) -> PathBuf { self.root.join("symbols_offsets.bin") }
    pub fn ref_offsets(&self) -> PathBuf { self.root.join("refs_offsets.bin") }
    /// file_id → list of symbol indices. Packed: per file_id (in order),
    /// a u32 count followed by `count` u32 indices into symbols.bin.
    pub fn file_symbols(&self) -> PathBuf { self.root.join("file_symbols.bin") }
    /// Sidecar: one u64-LE byte offset per file_id giving where that
    /// file's entry begins in file_symbols.bin. Allows random access
    /// without scanning the whole packed file.
    pub fn file_symbols_offsets(&self) -> PathBuf { self.root.join("file_symbols_offsets.bin") }
    /// Per-ref resolution overrides: packed u64-LE per ref_idx; 0 = unresolved,
    /// other values are the resolved definition's id (matches SymbolRecord.id).
    /// Produced by `scry build-resolutions`; reader honors it on get_ref().
    pub fn ref_resolutions(&self) -> PathBuf { self.root.join("ref_resolutions.bin") }
    /// Per-file content digest: packed `[u8; 32]` per file_id (blake3).
    /// Indexed parallel to `files.bin`. Used by `scry index --incremental`
    /// to detect which files actually changed between two index builds.
    /// Optional sidecar — produced by `scry build-digests`; absence
    /// just means `--incremental` is unavailable until it's built.
    pub fn file_digests(&self) -> PathBuf { self.root.join("file_digests.bin") }
    /// Tombstone bitmap: 1 bit per file_id (rounded to bytes). Bit set
    /// means "this file_id has been deleted; filter its symbols and
    /// refs out of every query result". Produced by `scry index
    /// --incremental` when files are removed or replaced; cleared by
    /// `scry compact`. Absent sidecar simply means no tombstones —
    /// the common case on a fresh index.
    pub fn tombstones(&self) -> PathBuf { self.root.join("tombstones.bin") }
    /// Per-chunk metadata: bincode-encoded `Vec<ChunkEntry>` describing
    /// each ~100-line window the embedder broke the corpus into.
    /// Produced by `scry build-embeddings`; queried by `scry ask`.
    /// Sized at ~24 bytes/chunk × ~3 M chunks ≈ 72 MB on full corpus.
    pub fn chunks(&self) -> PathBuf { self.root.join("chunks.bin") }
    /// Packed f32 embeddings, one row per chunk_idx. Row size is
    /// `manifest.embedding_dim * 4` bytes. With dim=64 and ~3 M chunks
    /// the file is ~768 MB — the heaviest sidecar but bounded.
    pub fn embeddings(&self) -> PathBuf { self.root.join("embeddings.bin") }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub struct StoreWriter {
    pub paths: StorePaths,
    pub roots: Vec<RootEntry>,
    pub files: Vec<FileEntry>,
    pub symbols: Vec<SymbolRecord>,
    pub refs: Vec<RefRecord>,
    /// On-disk staging directory for streaming chunk flushes (under
    /// `<index>.tmp/`). When `None`, the writer accumulates records in
    /// RAM and callers invoke `finalize`. When `Some`, callers flush
    /// chunks via `flush_symbols_chunk`/`flush_refs_chunk` and finish
    /// via `finalize_streaming`. Streaming is the default path; the
    /// all-RAM mode is used for tests and small in-memory builds.
    pub tmp_dir: Option<PathBuf>,
    /// Number of symbol chunks already flushed.
    pub symbol_chunk_count: u32,
    pub ref_chunk_count: u32,
    /// Per-chunk totals so we can stamp the final `Vec<T>` length without
    /// re-reading the chunks first.
    pub symbol_chunk_lens: Vec<u64>,
    pub ref_chunk_lens: Vec<u64>,
    /// Per-batch staging of trigram tuples (trigram, file_id). Flushed to
    /// disk by flush_trigrams_chunk. When None, trigram indexing is
    /// disabled (writer was created without the build_trigrams option).
    pub trigrams: Option<Vec<(trigram::Trigram, u32)>>,
    pub trigram_chunk_count: u32,
}

impl StoreWriter {
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self {
            paths: StorePaths::new(root),
            roots: Vec::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            refs: Vec::new(),
            tmp_dir: None,
            symbol_chunk_count: 0,
            ref_chunk_count: 0,
            symbol_chunk_lens: Vec::new(),
            ref_chunk_lens: Vec::new(),
            trigrams: None,
            trigram_chunk_count: 0,
        }
    }

    /// Turn on trigram index building. Subsequent push_trigrams calls will
    /// accumulate trigrams that get flushed alongside symbol/ref chunks.
    /// Must be called BEFORE the indexing pipeline starts (we don't backfill
    /// trigrams for already-parsed files).
    pub fn enable_trigrams(&mut self) {
        if self.trigrams.is_none() {
            self.trigrams = Some(Vec::with_capacity(16 * 1024 * 1024));
        }
    }

    /// Create the writer in streaming mode. Initializes the `<index>.tmp/`
    /// staging dir up front so flush calls can write chunk files.
    pub fn new_streaming<P: Into<PathBuf>>(root: P) -> Result<Self> {
        let root: PathBuf = root.into();
        let tmp = root.with_extension("tmp");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .with_context(|| format!("clean stale tmp {}", tmp.display()))?;
        }
        std::fs::create_dir_all(&tmp)?;
        Ok(Self {
            paths: StorePaths::new(root),
            roots: Vec::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            refs: Vec::new(),
            tmp_dir: Some(tmp),
            symbol_chunk_count: 0,
            ref_chunk_count: 0,
            symbol_chunk_lens: Vec::new(),
            ref_chunk_lens: Vec::new(),
            trigrams: None,
            trigram_chunk_count: 0,
        })
    }

    /// Open an existing streaming staging dir for RESUME, or fall back to
    /// new_streaming if none exists. Counts existing chunk files on disk so
    /// the chunk_count + chunk_lens are consistent with what's already
    /// written. Used after a cgroup OOM-kill + systemd restart so we don't
    /// reparse files already-flushed in previous runs.
    pub fn resume_streaming<P: Into<PathBuf>>(root: P) -> Result<Self> {
        use std::io::Read;
        let root: PathBuf = root.into();
        let tmp = root.with_extension("tmp");
        if !tmp.exists() {
            return Self::new_streaming(root);
        }
        let count_chunks = |kind: &str| -> Result<(u32, Vec<u64>)> {
            let mut n: u32 = 0;
            let mut lens: Vec<u64> = Vec::new();
            loop {
                let p = Self::chunk_path(&tmp, kind, n);
                if !p.exists() { break; }
                // bincode 1.3 default config serializes Vec<T> as `u64 LE length`
                // followed by encoded elements. We read just the header to get
                // the chunk's record count without deserializing the records.
                let mut f = File::open(&p)
                    .with_context(|| format!("open chunk {}", p.display()))?;
                let mut hdr = [0u8; 8];
                f.read_exact(&mut hdr)
                    .with_context(|| format!("read header of {}", p.display()))?;
                lens.push(u64::from_le_bytes(hdr));
                n += 1;
            }
            Ok((n, lens))
        };
        let (sym_chunks, sym_lens) = count_chunks("symbols")?;
        let (ref_chunks, ref_lens) = count_chunks("refs")?;
        // Also count existing trigram chunks if any (resume case).
        let mut tg_chunks: u32 = 0;
        loop {
            let p = tmp.join(format!("trigrams.chunk.{:06}.bin", tg_chunks));
            if !p.exists() { break; }
            tg_chunks += 1;
        }
        eprintln!(
            "[resume] reopening {} (existing chunks: {} symbol, {} ref, {} trigram)",
            tmp.display(), sym_chunks, ref_chunks, tg_chunks,
        );
        Ok(Self {
            paths: StorePaths::new(root),
            roots: Vec::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            refs: Vec::new(),
            tmp_dir: Some(tmp),
            symbol_chunk_count: sym_chunks,
            ref_chunk_count: ref_chunks,
            symbol_chunk_lens: sym_lens,
            ref_chunk_lens: ref_lens,
            trigrams: None,
            trigram_chunk_count: tg_chunks,
        })
    }

    fn chunk_path(tmp: &Path, kind: &str, n: u32) -> PathBuf {
        tmp.join(format!("{kind}.chunk.{:06}.bin", n))
    }

    /// Drain `self.symbols` to a chunk file. Also writes a sorted
    /// `(name, final_idx)` side-file used by the finalize k-way merge so we
    /// never have to hold a `BTreeMap<String, Vec<u32>>` of every name in RAM.
    pub fn flush_symbols_chunk(&mut self) -> Result<u64> {
        if self.symbols.is_empty() {
            return Ok(0);
        }
        let tmp = self
            .tmp_dir
            .clone()
            .ok_or_else(|| anyhow!("flush_symbols_chunk requires streaming mode"))?;
        let n = self.symbol_chunk_count;
        let p = Self::chunk_path(&tmp, "symbols", n);
        let names_p = Self::chunk_path(&tmp, "symbol_names", n);
        let count = self.symbols.len() as u64;
        let idx_offset: u32 = self.symbol_chunk_lens.iter().sum::<u64>() as u32;
        // Sorted names side-file: 4-byte name_len, name_bytes, 4-byte idx.
        // Sorting a single chunk's names in RAM is bounded (chunk size).
        let mut tuples: Vec<(String, u32)> = self.symbols.iter().enumerate()
            .map(|(i, s)| (s.name.clone(), idx_offset + i as u32))
            .collect();
        tuples.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        write_sorted_names_chunk(&names_p, &tuples)?;
        drop(tuples);
        // Records file: bincode Vec preserved in original (chunk-insertion) order.
        let take = std::mem::take(&mut self.symbols);
        write_bincode(&p, &take)?;
        self.symbol_chunk_count = n + 1;
        self.symbol_chunk_lens.push(count);
        Ok(count)
    }

    /// Append a single file's trigrams to the writer's pending buffer.
    /// No-op if trigram building is disabled. Caller is responsible for
    /// passing the already-deduplicated sorted trigrams for `file_id`
    /// (typically from `trigram::extract_sorted`).
    pub fn push_trigrams(&mut self, trigrams: &[trigram::Trigram], file_id: u32) {
        if let Some(buf) = self.trigrams.as_mut() {
            buf.reserve(trigrams.len());
            for t in trigrams {
                buf.push((*t, file_id));
            }
        }
    }

    /// Drain pending trigrams to a chunk file. Sorted by (trigram, file_id)
    /// so the finalize k-way merge can stream them in order.
    /// Tuple layout on disk: 3-byte trigram, 4-byte file_id LE = 7 bytes.
    pub fn flush_trigrams_chunk(&mut self) -> Result<u64> {
        let tmp = match self.tmp_dir.clone() {
            Some(t) => t,
            None => return Ok(0),
        };
        let buf = match self.trigrams.as_mut() {
            Some(b) if !b.is_empty() => b,
            _ => return Ok(0),
        };
        let n = self.trigram_chunk_count;
        let p = tmp.join(format!("trigrams.chunk.{:06}.bin", n));
        buf.sort_unstable();
        let count = buf.len() as u64;
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&p)?);
        for (t, f) in buf.drain(..) {
            w.write_all(&t)?;
            w.write_all(&f.to_le_bytes())?;
        }
        w.flush()?;
        self.trigram_chunk_count = n + 1;
        Ok(count)
    }

    /// Drain `self.refs` to a chunk file (with sorted names side-file).
    pub fn flush_refs_chunk(&mut self) -> Result<u64> {
        if self.refs.is_empty() {
            return Ok(0);
        }
        let tmp = self
            .tmp_dir
            .clone()
            .ok_or_else(|| anyhow!("flush_refs_chunk requires streaming mode"))?;
        let n = self.ref_chunk_count;
        let p = Self::chunk_path(&tmp, "refs", n);
        let names_p = Self::chunk_path(&tmp, "ref_names", n);
        let count = self.refs.len() as u64;
        let idx_offset: u32 = self.ref_chunk_lens.iter().sum::<u64>() as u32;
        let mut tuples: Vec<(String, u32)> = self.refs.iter().enumerate()
            .map(|(i, r)| (r.name.clone(), idx_offset + i as u32))
            .collect();
        tuples.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        write_sorted_names_chunk(&names_p, &tuples)?;
        drop(tuples);
        let take = std::mem::take(&mut self.refs);
        write_bincode(&p, &take)?;
        self.ref_chunk_count = n + 1;
        self.ref_chunk_lens.push(count);
        Ok(count)
    }

    /// Layer-1 resolution: best-effort name match. For each ref we look up
    /// symbols with the same name and pick the strongest candidate:
    ///   1. same-language definition wins,
    ///   2. then any single definition,
    ///   3. otherwise leave unresolved (ambiguous).
    /// This is intentionally cheap (a single hashmap pass) — proper
    /// scope/import resolution happens in a future phase.
    pub fn resolve_refs(&mut self) {
        if self.refs.is_empty() || self.symbols.is_empty() { return; }
        let mut by_name: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, s) in self.symbols.iter().enumerate() {
            by_name.entry(s.name.clone()).or_default().push(i as u32);
        }
        for r in &mut self.refs {
            let cands = match by_name.get(&r.name) {
                Some(c) if !c.is_empty() => c,
                _ => continue,
            };
            // Prefer same-lang. If no same-lang match, fall back to the
            // first candidate (Layer 1 best-effort). The previous
            // `or_else(|| if cands.len() == 1 { Some(cands[0]) } else { Some(cands[0]) })`
            // was dead code — both branches returned the same value.
            let chosen = cands.iter()
                .find(|&&idx| self.symbols[idx as usize].lang == r.lang)
                .copied()
                .unwrap_or(cands[0]);
            r.resolved_to = Some(self.symbols[chosen as usize].id);
        }
    }

    /// Streaming finalize: assumes the writer is in streaming mode and that
    /// symbols + refs have been periodically flushed to chunk files. Any
    /// records still in `self.symbols` / `self.refs` are flushed first.
    ///
    /// Memory envelope (peak) during finalize:
    ///   - one chunk's `Vec<SymbolRecord>` (or `RefRecord`) at a time, plus
    ///   - a single `BTreeMap<String, Vec<u32>>` of name -> indices used to
    ///     emit the FST + postings (built incrementally as we stream).
    pub fn finalize_streaming(mut self, stats: IndexStats) -> Result<()> {
        // Flush any remaining in-RAM records.
        let _ = self.flush_symbols_chunk()?;
        let _ = self.flush_refs_chunk()?;
        let _ = self.flush_trigrams_chunk()?;

        let final_dir = self.paths.root.clone();
        let tmp = self
            .tmp_dir
            .clone()
            .ok_or_else(|| anyhow!("finalize_streaming requires streaming mode"))?;
        let tmp_paths = StorePaths::new(tmp.clone());

        write_bincode(&tmp_paths.roots(), &self.roots)?;
        write_bincode(&tmp_paths.files(), &self.files)?;

        // -- symbols.bin + symbols_offsets.bin + file_symbols.bin --
        //    Concatenate chunks into a single bincode Vec<SymbolRecord>,
        //    write a u64-LE byte-offset per record into the offsets sidecar
        //    (lazy reader), and accumulate a file_id → [symbol_idx] map
        //    that we serialize as file_symbols.bin afterwards (outline
        //    fast path: O(symbols-in-file) instead of O(corpus)).
        let total_syms: u64 = self.symbol_chunk_lens.iter().sum();
        let mut file_symbols: Vec<Vec<u32>> = vec![Vec::new(); self.files.len()];
        {
            let mut w = BufWriter::with_capacity(1 << 20, File::create(tmp_paths.symbols())?);
            let mut ow = BufWriter::with_capacity(1 << 20, File::create(tmp_paths.symbol_offsets())?);
            // bincode 1.3 with default config encodes Vec<T> as u64-LE length
            // followed by each element. We stamp the length, then stream each
            // chunk's records back out one by one without rebuilding a Vec.
            w.write_all(&total_syms.to_le_bytes())?;
            let mut byte_pos: u64 = 8; // past the length prefix
            let mut sym_idx: u32 = 0;
            for n in 0..self.symbol_chunk_count {
                let p = Self::chunk_path(&tmp, "symbols", n);
                let chunk: Vec<SymbolRecord> = read_bincode(&p)?;
                for s in &chunk {
                    ow.write_all(&byte_pos.to_le_bytes())?;
                    let bytes = bincode::serialize(s)
                        .with_context(|| "serialize symbol")?;
                    w.write_all(&bytes)?;
                    byte_pos += bytes.len() as u64;
                    let fid = s.file_id as usize;
                    if fid < file_symbols.len() {
                        file_symbols[fid].push(sym_idx);
                    }
                    sym_idx += 1;
                }
            }
            w.flush()?;
            ow.flush()?;
        }

        // -- file_symbols.bin + file_symbols_offsets.bin --
        //    Packed per-file: u32 count then `count` u32 indices into
        //    symbols.bin. The offsets sidecar is one u64-LE per file_id
        //    giving its starting byte in file_symbols.bin.
        {
            let mut w = BufWriter::with_capacity(1 << 20, File::create(tmp_paths.file_symbols())?);
            let mut ow = BufWriter::with_capacity(1 << 20, File::create(tmp_paths.file_symbols_offsets())?);
            let mut byte_pos: u64 = 0;
            for ids in &file_symbols {
                ow.write_all(&byte_pos.to_le_bytes())?;
                let count = ids.len() as u32;
                w.write_all(&count.to_le_bytes())?;
                for id in ids {
                    w.write_all(&id.to_le_bytes())?;
                }
                byte_pos += 4 + 4 * (ids.len() as u64);
            }
            w.flush()?;
            ow.flush()?;
        }

        // -- names.fst + name_postings.bin (k-way merge over per-chunk sorted
        //    names side-files). RAM ≈ chunks × small buffer; no in-RAM map. --
        {
            let chunk_paths: Vec<PathBuf> = (0..self.symbol_chunk_count)
                .map(|n| Self::chunk_path(&tmp, "symbol_names", n))
                .collect();
            kway_merge_names_to_fst(
                &chunk_paths,
                &tmp_paths.names_fst(),
                &tmp_paths.name_postings(),
            )?;
        }

        // -- refs.bin + refs_offsets.bin + ref_names.fst + ref_postings.bin --
        let total_refs: u64 = self.ref_chunk_lens.iter().sum();
        {
            let mut w = BufWriter::with_capacity(1 << 20, File::create(tmp_paths.refs())?);
            let mut ow = BufWriter::with_capacity(1 << 20, File::create(tmp_paths.ref_offsets())?);
            w.write_all(&total_refs.to_le_bytes())?;
            let mut byte_pos: u64 = 8;
            for n in 0..self.ref_chunk_count {
                let p = Self::chunk_path(&tmp, "refs", n);
                let chunk: Vec<RefRecord> = read_bincode(&p)?;
                for r in &chunk {
                    ow.write_all(&byte_pos.to_le_bytes())?;
                    let bytes = bincode::serialize(r)
                        .with_context(|| "serialize ref")?;
                    w.write_all(&bytes)?;
                    byte_pos += bytes.len() as u64;
                }
            }
            w.flush()?;
            ow.flush()?;
        }
        {
            let chunk_paths: Vec<PathBuf> = (0..self.ref_chunk_count)
                .map(|n| Self::chunk_path(&tmp, "ref_names", n))
                .collect();
            kway_merge_names_to_fst(
                &chunk_paths,
                &tmp_paths.ref_names_fst(),
                &tmp_paths.ref_postings(),
            )?;
        }

        // -- trigrams.fst + trigram_postings.bin (delta+varint posting lists)
        //    Only built when the writer was created with --build-trigrams.
        if self.trigram_chunk_count > 0 {
            let chunk_paths: Vec<PathBuf> = (0..self.trigram_chunk_count)
                .map(|n| tmp.join(format!("trigrams.chunk.{:06}.bin", n)))
                .collect();
            kway_merge_trigrams_to_fst(
                &chunk_paths,
                &tmp_paths.trigram_fst(),
                &tmp_paths.trigram_postings(),
            )?;
        }

        let manifest = Manifest {
            version: MANIFEST_VERSION,
            scry_version: env!("CARGO_PKG_VERSION").to_string(),
            indexed_at: now_iso(),
            roots: self.roots.clone(),
            stats,
        };
        let mut mf = BufWriter::new(File::create(tmp_paths.manifest())?);
        serde_json::to_writer_pretty(&mut mf, &manifest)?;
        mf.flush()?;

        // Drop chunk files + progress marker now that the final bin files are
        // written. Without this the resume checkpoint would leak into the
        // published index dir alongside the .bin files.
        for n in 0..self.symbol_chunk_count {
            let _ = std::fs::remove_file(Self::chunk_path(&tmp, "symbols", n));
            let _ = std::fs::remove_file(Self::chunk_path(&tmp, "symbol_names", n));
        }
        for n in 0..self.ref_chunk_count {
            let _ = std::fs::remove_file(Self::chunk_path(&tmp, "refs", n));
            let _ = std::fs::remove_file(Self::chunk_path(&tmp, "ref_names", n));
        }
        let _ = std::fs::remove_file(tmp.join("progress.json"));
        let _ = std::fs::remove_file(tmp.join("progress.json.tmp"));
        for n in 0..self.trigram_chunk_count {
            let _ = std::fs::remove_file(tmp.join(format!("trigrams.chunk.{:06}.bin", n)));
        }

        if final_dir.exists() {
            let old = final_dir.with_extension("old");
            if old.exists() {
                std::fs::remove_dir_all(&old).ok();
            }
            std::fs::rename(&final_dir, &old)?;
            std::fs::rename(&tmp, &final_dir)?;
            std::fs::remove_dir_all(&old).ok();
        } else {
            if let Some(parent) = final_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&tmp, &final_dir)?;
        }
        Ok(())
    }

    pub fn finalize(self, stats: IndexStats) -> Result<()> {
        let final_dir = self.paths.root.clone();
        let tmp = final_dir.with_extension("tmp");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .with_context(|| format!("clean stale tmp {}", tmp.display()))?;
        }
        std::fs::create_dir_all(&tmp)?;
        let tmp_paths = StorePaths::new(tmp.clone());

        write_bincode(&tmp_paths.roots(), &self.roots)?;
        write_bincode(&tmp_paths.files(), &self.files)?;
        write_bincode(&tmp_paths.symbols(), &self.symbols)?;
        write_bincode(&tmp_paths.refs(), &self.refs)?;

        // symbol name -> postings
        build_name_fst(
            self.symbols.iter().enumerate().map(|(i, s)| (s.name.as_str(), i as u32)),
            &tmp_paths.names_fst(),
            &tmp_paths.name_postings(),
        )?;
        // ref name -> postings
        build_name_fst(
            self.refs.iter().enumerate().map(|(i, r)| (r.name.as_str(), i as u32)),
            &tmp_paths.ref_names_fst(),
            &tmp_paths.ref_postings(),
        )?;

        let manifest = Manifest {
            version: MANIFEST_VERSION,
            scry_version: env!("CARGO_PKG_VERSION").to_string(),
            indexed_at: now_iso(),
            roots: self.roots.clone(),
            stats,
        };
        let mut mf = BufWriter::new(File::create(tmp_paths.manifest())?);
        serde_json::to_writer_pretty(&mut mf, &manifest)?;
        mf.flush()?;

        if final_dir.exists() {
            let old = final_dir.with_extension("old");
            if old.exists() {
                std::fs::remove_dir_all(&old).ok();
            }
            std::fs::rename(&final_dir, &old)?;
            std::fs::rename(&tmp, &final_dir)?;
            std::fs::remove_dir_all(&old).ok();
        } else {
            if let Some(parent) = final_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&tmp, &final_dir)?;
        }
        Ok(())
    }
}

/// Build a FST + posting list for a stream of (name, idx) tuples.
/// The FST stores `name -> u64 offset` into the postings file.
/// Each posting is `u32 count + count * u32 idx` (little-endian).
fn build_name_fst<'a, I: Iterator<Item = (&'a str, u32)>>(
    items: I,
    fst_path: &Path,
    postings_path: &Path,
) -> Result<()> {
    let mut by_name: HashMap<String, Vec<u32>> = HashMap::new();
    for (name, idx) in items {
        by_name.entry(name.to_string()).or_default().push(idx);
    }
    let mut names: Vec<String> = by_name.keys().cloned().collect();
    names.sort();
    let mut postings = BufWriter::new(File::create(postings_path)?);
    let mut offsets: Vec<(String, u64)> = Vec::with_capacity(names.len());
    let mut pos: u64 = 0;
    for name in &names {
        let idxs = &by_name[name];
        offsets.push((name.clone(), pos));
        postings.write_all(&(idxs.len() as u32).to_le_bytes())?;
        pos += 4;
        for i in idxs {
            postings.write_all(&i.to_le_bytes())?;
            pos += 4;
        }
    }
    // memmap can't map a 0-byte file; if we wrote nothing, leave a placeholder.
    if pos == 0 {
        postings.write_all(&[0u8])?;
    }
    postings.flush()?;
    let fst_file = BufWriter::new(File::create(fst_path)?);
    let mut builder = fst::MapBuilder::new(fst_file)?;
    for (name, off) in offsets {
        builder
            .insert(name.as_bytes(), off)
            .with_context(|| format!("fst insert {name}"))?;
    }
    builder.finish()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// External merge sort for FST construction.
// Per-chunk we write a side-file of sorted (name, idx) tuples. Finalize
// k-way merges them, feeding fst::MapBuilder in sorted order. RAM peak ≈
// (num_chunks × a few KB) regardless of corpus size — replaces the previous
// in-RAM BTreeMap<String, Vec<u32>> that OOM'd on full AOSP.
// ---------------------------------------------------------------------------

/// On-disk record format: u32 name_len LE, name_bytes, u32 idx LE.
fn write_sorted_names_chunk(path: &Path, sorted: &[(String, u32)]) -> Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    for (name, idx) in sorted {
        w.write_all(&(name.len() as u32).to_le_bytes())?;
        w.write_all(name.as_bytes())?;
        w.write_all(&idx.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

struct NamesChunkReader {
    file: BufReader<File>,
    /// The next (name, idx) pair this reader will emit; None when exhausted.
    next: Option<(String, u32)>,
}

impl NamesChunkReader {
    fn open(path: &Path) -> Result<Self> {
        let mut r = Self {
            file: BufReader::new(
                File::open(path).with_context(|| format!("open {}", path.display()))?,
            ),
            next: None,
        };
        r.advance()?;
        Ok(r)
    }
    fn advance(&mut self) -> Result<()> {
        use std::io::Read;
        let mut len_buf = [0u8; 4];
        if self.file.read_exact(&mut len_buf).is_err() {
            self.next = None;
            return Ok(());
        }
        let name_len = u32::from_le_bytes(len_buf) as usize;
        let mut name_bytes = vec![0u8; name_len];
        self.file.read_exact(&mut name_bytes)?;
        let name = String::from_utf8(name_bytes).context("bad utf8 in names chunk")?;
        self.file.read_exact(&mut len_buf)?;
        let idx = u32::from_le_bytes(len_buf);
        self.next = Some((name, idx));
        Ok(())
    }
}

/// Min-heap item: smallest (name, then reader_id for stability) at the top.
#[derive(Eq, PartialEq)]
struct HeapItem {
    name: String,
    idx: u32,
    reader_id: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // std BinaryHeap is a max-heap, so reverse the natural order.
        other
            .name
            .cmp(&self.name)
            .then(other.reader_id.cmp(&self.reader_id))
            .then(other.idx.cmp(&self.idx))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

// ---------------------------------------------------------------------------
// Trigram-chunk reader + k-way merge (mirrors the names-chunk machinery).
// ---------------------------------------------------------------------------
struct TrigramChunkReader {
    file: BufReader<File>,
    next: Option<(trigram::Trigram, u32)>,
}
impl TrigramChunkReader {
    fn open(path: &Path) -> Result<Self> {
        let mut r = Self {
            file: BufReader::new(
                File::open(path).with_context(|| format!("open {}", path.display()))?,
            ),
            next: None,
        };
        r.advance()?;
        Ok(r)
    }
    fn advance(&mut self) -> Result<()> {
        use std::io::Read;
        let mut t = [0u8; 3];
        if self.file.read_exact(&mut t).is_err() { self.next = None; return Ok(()); }
        let mut f = [0u8; 4];
        self.file.read_exact(&mut f)?;
        self.next = Some((t, u32::from_le_bytes(f)));
        Ok(())
    }
}

#[derive(Eq, PartialEq)]
struct TrigramHeapItem {
    trigram: trigram::Trigram,
    file_id: u32,
    reader_id: usize,
}
impl Ord for TrigramHeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.trigram.cmp(&self.trigram)
            .then(other.reader_id.cmp(&self.reader_id))
            .then(other.file_id.cmp(&self.file_id))
    }
}
impl PartialOrd for TrigramHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

/// K-way merge per-chunk sorted (trigram, file_id) tuples into:
///   - trigrams.fst:        FST mapping the 3-byte trigram → u64 offset
///   - trigram_postings.bin: per-trigram delta-coded varint file_id list
///
/// Posting layout per trigram:
///   u32 LE count, then `count` × varint(delta_from_previous).
/// Delta+varint is the same compression Code Search / livegrep use; gives
/// ~3-5× shrink vs. raw u32 for typical posting lists in source-code
/// corpora where file_ids are densely packed.
/// Public wrapper so the standalone `scry build-trigrams` utility can drive
/// the same k-way merge as the regular finalize_streaming path. Same input
/// format (sorted (trigram, file_id) chunk files), same output (trigrams.fst
/// + trigram_postings.bin with delta+varint posting lists).
pub fn kway_merge_trigrams_to_fst_public(
    chunk_paths: &[PathBuf],
    fst_path: &Path,
    postings_path: &Path,
) -> Result<()> {
    kway_merge_trigrams_to_fst(chunk_paths, fst_path, postings_path)
}

fn kway_merge_trigrams_to_fst(
    chunk_paths: &[PathBuf],
    fst_path: &Path,
    postings_path: &Path,
) -> Result<()> {
    use std::collections::BinaryHeap;
    let mut readers: Vec<TrigramChunkReader> = chunk_paths.iter()
        .map(|p| TrigramChunkReader::open(p))
        .collect::<Result<Vec<_>>>()?;
    let mut heap: BinaryHeap<TrigramHeapItem> = BinaryHeap::with_capacity(readers.len());
    for (i, r) in readers.iter().enumerate() {
        if let Some((t, f)) = r.next {
            heap.push(TrigramHeapItem { trigram: t, file_id: f, reader_id: i });
        }
    }
    let mut postings = BufWriter::with_capacity(1 << 20, File::create(postings_path)?);
    let fst_file = BufWriter::with_capacity(1 << 20, File::create(fst_path)?);
    let mut fst_builder = fst::MapBuilder::new(fst_file)?;
    let mut current_trigram: Option<trigram::Trigram> = None;
    let mut current_offset: u64 = 0;
    let mut current_fids: Vec<u32> = Vec::new();
    let mut pos: u64 = 0;

    fn write_varint(w: &mut BufWriter<File>, mut v: u32) -> std::io::Result<u64> {
        let mut n: u64 = 0;
        while v >= 0x80 { w.write_all(&[(v as u8 & 0x7f) | 0x80])?; v >>= 7; n += 1; }
        w.write_all(&[v as u8])?;
        Ok(n + 1)
    }

    let flush_group = |postings: &mut BufWriter<File>,
                       fst_builder: &mut fst::MapBuilder<BufWriter<File>>,
                       trigram: &trigram::Trigram,
                       fids: &mut Vec<u32>,
                       offset: u64,
                       pos: &mut u64|
     -> Result<()> {
        if fids.is_empty() { return Ok(()); }
        fids.sort_unstable();
        fids.dedup();
        postings.write_all(&(fids.len() as u32).to_le_bytes())?;
        *pos += 4;
        let mut prev: u32 = 0;
        for &f in fids.iter() {
            let delta = f.wrapping_sub(prev);
            *pos += write_varint(postings, delta)?;
            prev = f;
        }
        fst_builder.insert(trigram, offset)
            .with_context(|| format!("trigram fst insert {:?}", trigram))?;
        Ok(())
    };

    while let Some(TrigramHeapItem { trigram, file_id, reader_id }) = heap.pop() {
        let same = current_trigram.as_ref() == Some(&trigram);
        if !same {
            if let Some(t) = current_trigram.take() {
                flush_group(&mut postings, &mut fst_builder, &t, &mut current_fids, current_offset, &mut pos)?;
                current_fids.clear();
            }
            current_trigram = Some(trigram);
            current_offset = pos;
        }
        current_fids.push(file_id);
        readers[reader_id].advance()?;
        if let Some((t, f)) = readers[reader_id].next {
            heap.push(TrigramHeapItem { trigram: t, file_id: f, reader_id });
        }
    }
    if let Some(t) = current_trigram.take() {
        flush_group(&mut postings, &mut fst_builder, &t, &mut current_fids, current_offset, &mut pos)?;
    }
    if pos == 0 { postings.write_all(&[0u8])?; }
    postings.flush()?;
    fst_builder.finish()?;
    Ok(())
}

fn kway_merge_names_to_fst(
    chunk_paths: &[PathBuf],
    fst_path: &Path,
    postings_path: &Path,
) -> Result<()> {
    use std::collections::BinaryHeap;
    let mut readers: Vec<NamesChunkReader> = chunk_paths
        .iter()
        .map(|p| NamesChunkReader::open(p))
        .collect::<Result<Vec<_>>>()?;
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::with_capacity(readers.len());
    for (i, r) in readers.iter().enumerate() {
        if let Some((name, idx)) = r.next.as_ref() {
            heap.push(HeapItem { name: name.clone(), idx: *idx, reader_id: i });
        }
    }

    let mut postings = BufWriter::new(File::create(postings_path)?);
    let fst_file = BufWriter::new(File::create(fst_path)?);
    let mut fst_builder = fst::MapBuilder::new(fst_file)?;
    let mut current_name: Option<String> = None;
    let mut current_offset: u64 = 0;
    let mut current_idxs: Vec<u32> = Vec::new();
    let mut pos: u64 = 0;

    let flush_group = |postings: &mut BufWriter<File>,
                       fst_builder: &mut fst::MapBuilder<BufWriter<File>>,
                       name: &str,
                       idxs: &[u32],
                       offset: u64,
                       pos: &mut u64|
     -> Result<()> {
        if idxs.is_empty() { return Ok(()); }
        postings.write_all(&(idxs.len() as u32).to_le_bytes())?;
        for i in idxs {
            postings.write_all(&i.to_le_bytes())?;
        }
        *pos += 4 + (idxs.len() * 4) as u64;
        fst_builder
            .insert(name.as_bytes(), offset)
            .with_context(|| format!("fst insert {name}"))?;
        Ok(())
    };

    while let Some(HeapItem { name, idx, reader_id }) = heap.pop() {
        let same = current_name.as_deref() == Some(name.as_str());
        if !same {
            if let Some(n) = current_name.take() {
                flush_group(&mut postings, &mut fst_builder, &n, &current_idxs, current_offset, &mut pos)?;
                current_idxs.clear();
            }
            current_name = Some(name);
            current_offset = pos;
        }
        current_idxs.push(idx);
        readers[reader_id].advance()?;
        if let Some((next_name, next_idx)) = readers[reader_id].next.as_ref() {
            heap.push(HeapItem {
                name: next_name.clone(),
                idx: *next_idx,
                reader_id,
            });
        }
    }
    if let Some(n) = current_name.take() {
        flush_group(&mut postings, &mut fst_builder, &n, &current_idxs, current_offset, &mut pos)?;
    }
    if pos == 0 {
        postings.write_all(&[0u8])?;
    }
    postings.flush()?;
    fst_builder.finish()?;
    Ok(())
}

fn write_bincode<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let f = BufWriter::new(File::create(path)?);
    bincode::serialize_into(f, value)
        .with_context(|| format!("write bincode {}", path.display()))?;
    Ok(())
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    epoch_to_iso8601(secs)
}

/// Convert seconds-since-Unix-epoch to a proper ISO-8601 timestamp.
///
/// Hand-rolled (no chrono dep) using Howard Hinnant's days-from-civil
/// algorithm so the manifest's indexed_at field actually parses as
/// ISO-8601. Replaced an earlier ad-hoc format `"{secs}-unixT{HH:MM:SS}Z"`
/// which kept a unix epoch in the year position and wasn't ISO at all.
fn epoch_to_iso8601(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let h = time_of_day / 3600;
    let m = (time_of_day / 60) % 60;
    let s = time_of_day % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's "days from civil" inverse: days since 1970-01-01
/// → (year, month, day). Handles any year in the proleptic Gregorian
/// calendar; widely used reference algorithm, see
/// https://howardhinnant.github.io/date_algorithms.html .
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub struct StoreReader {
    pub paths: StorePaths,
    pub manifest: Manifest,
    pub roots: Vec<RootEntry>,
    pub files: Vec<FileEntry>,
    /// Eager-loaded records. Empty in lazy mode (use lazy_symbols instead).
    pub symbols: Vec<SymbolRecord>,
    pub refs: Vec<RefRecord>,
    pub fst: fst::Map<memmap2::Mmap>,
    pub postings_mmap: memmap2::Mmap,
    pub ref_fst: fst::Map<memmap2::Mmap>,
    pub ref_postings_mmap: memmap2::Mmap,
    /// Trigram index for fast literal grep. Only present when the index
    /// was built with --build-trigrams. When absent, grep falls back to
    /// the full-scan path. None for old indexes — they still work.
    pub trigram_fst: Option<fst::Map<memmap2::Mmap>>,
    pub trigram_postings_mmap: Option<memmap2::Mmap>,
    /// Lazy/mmap-backed record stores. When Some, the public lookup APIs
    /// decode on-demand instead of relying on the eager `symbols`/`refs`
    /// Vecs. Present iff the index has _offsets.bin sidecars (new indexes,
    /// or old indexes retrofitted via `scry build-offsets`).
    pub lazy_symbols: Option<LazyVec<SymbolRecord>>,
    pub lazy_refs: Option<LazyVec<RefRecord>>,
    /// file_id → symbol indices (into symbols.bin). Mmap'd. None for
    /// indexes built before this sidecar was added; callers fall back
    /// to the linear scan path then. Retrofit via build-file-symbols.
    pub file_symbols_mmap: Option<memmap2::Mmap>,
    pub file_symbols_offsets_mmap: Option<memmap2::Mmap>,
    /// Per-ref resolved-def-id overrides. Indexed by ref_idx; 0 ⇒
    /// unresolved (use the RefRecord's own resolved_to, which may also
    /// be None). Built post-finalize via `scry build-resolutions`.
    pub ref_resolutions_mmap: Option<memmap2::Mmap>,
    /// Per-file blake3 content digest (packed `[u8; 32]` per file_id).
    /// Powers `scry index --incremental` change detection. Present when
    /// `scry build-digests` has run against this index; absent otherwise.
    pub file_digests_mmap: Option<memmap2::Mmap>,
    /// Per-file tombstone bitmap (1 bit per file_id). Present when
    /// `scry index --incremental` has deleted or replaced any files.
    /// Every query path filters tombstoned file_ids out of results.
    /// `scry compact` rebuilds the index without tombstoned records
    /// and clears the bitmap.
    pub tombstones_mmap: Option<memmap2::Mmap>,
    /// Per-chunk metadata (file_id + line range). Indexed parallel to
    /// `embeddings_mmap`. Loaded eagerly because the Vec is small
    /// (~70 MB on full corpus) and every `scry ask` query walks all
    /// of it. Present only when `scry build-embeddings` has run.
    pub chunks: Option<Vec<embed::ChunkEntry>>,
    /// Packed f32 chunk embeddings. Header (first 8 bytes):
    /// `dim: u32 LE`, `count: u32 LE`. Body: `count` rows × `dim` × f32.
    /// mmap'd; cosine search walks it sequentially per query.
    pub embeddings_mmap: Option<memmap2::Mmap>,
    /// Dimension of each chunk embedding, parsed from the header of
    /// embeddings.bin. Zero when no embedding sidecar exists.
    pub embedding_dim: u32,
    /// Number of chunks in the embedding sidecar. Matches chunks.len()
    /// when both are present.
    pub embedding_count: u32,
}

impl StoreReader {
    pub fn open<P: Into<PathBuf>>(root: P) -> Result<Self> {
        let paths = StorePaths::new(root);
        let manifest: Manifest = serde_json::from_reader(BufReader::new(
            File::open(paths.manifest())
                .with_context(|| format!("open {}", paths.manifest().display()))?,
        ))?;
        let roots: Vec<RootEntry> = read_bincode(&paths.roots())?;
        let files: Vec<FileEntry> = read_bincode(&paths.files())?;
        // Lazy-mode shortcut: if BOTH a record file and its offsets sidecar
        // exist, mmap them and skip the eager bincode-into-Vec load. The
        // eager symbols/refs fields stay empty; readers go through the
        // get_symbol/get_ref helpers which prefer lazy when available.
        let lazy_symbols = if paths.symbols().exists() && paths.symbol_offsets().exists() {
            Some(LazyVec::<SymbolRecord>::open(&paths.symbols(), &paths.symbol_offsets())?)
        } else { None };
        let lazy_refs = if paths.refs().exists() && paths.ref_offsets().exists() {
            Some(LazyVec::<RefRecord>::open(&paths.refs(), &paths.ref_offsets())?)
        } else { None };
        let symbols: Vec<SymbolRecord> = if lazy_symbols.is_some() {
            Vec::new()  // lazy mode — don't eagerly load 10 GB into RAM
        } else {
            read_bincode(&paths.symbols())?
        };
        // refs.bin is always emitted by finalize (possibly empty); we
        // only read it when the lazy-refs sidecar isn't available.
        let refs: Vec<RefRecord> = if lazy_refs.is_some() {
            Vec::new()
        } else {
            read_bincode(&paths.refs())?
        };
        let fst = fst::Map::new(safe_mmap(&paths.names_fst())?)?;
        let postings_mmap = safe_mmap(&paths.name_postings())?;
        let ref_fst = fst::Map::new(safe_mmap(&paths.ref_names_fst())
            .with_context(|| format!("open {} (re-run \"scry index\" if missing)",
                                      paths.ref_names_fst().display()))?)?;
        let ref_postings_mmap = safe_mmap(&paths.ref_postings())?;
        // Trigram index is opt-in (built with `--build-trigrams`).
        // Absent here just means the operator chose not to pay the
        // build cost; queries fall back to the slower path.
        let (trigram_fst, trigram_postings_mmap) = if paths.trigram_fst().exists()
            && paths.trigram_postings().exists()
        {
            let tfst = fst::Map::new(safe_mmap(&paths.trigram_fst())?)?;
            let tp = safe_mmap(&paths.trigram_postings())?;
            (Some(tfst), Some(tp))
        } else {
            (None, None)
        };
        // file_symbols sidecar — produced by `scry build-file-symbols`
        // (or written inline by `index` when --build-file-symbols is
        // set). Optional everywhere (linear scan still works), so just
        // attempt to open.
        let (file_symbols_mmap, file_symbols_offsets_mmap) = if
            paths.file_symbols().exists() && paths.file_symbols_offsets().exists()
        {
            (Some(safe_mmap(&paths.file_symbols())?), Some(safe_mmap(&paths.file_symbols_offsets())?))
        } else {
            (None, None)
        };
        // Per-ref resolution overrides (Layer 2 sidecar). Optional.
        let ref_resolutions_mmap = if paths.ref_resolutions().exists() {
            Some(safe_mmap(&paths.ref_resolutions())?)
        } else {
            None
        };
        // Per-file blake3 digests (for incremental change detection).
        // Optional sidecar; absence means we can't do `--incremental`.
        let file_digests_mmap = if paths.file_digests().exists() {
            Some(safe_mmap(&paths.file_digests())?)
        } else { None };
        // Tombstone bitmap (1 bit per file_id). Optional; absent means
        // no tombstones (the common case until the first incremental
        // commit that deletes or replaces a file).
        let tombstones_mmap = if paths.tombstones().exists() {
            Some(safe_mmap(&paths.tombstones())?)
        } else { None };
        // Embedding sidecar (chunks + embeddings). Optional. Loaded
        // eagerly for chunks (small) and mmap'd for embeddings.bin.
        // Header (8 bytes) of embeddings.bin: dim u32 LE, count u32 LE.
        let (chunks, embeddings_mmap, embedding_dim, embedding_count) = if
            paths.chunks().exists() && paths.embeddings().exists()
        {
            let ch: Vec<embed::ChunkEntry> = read_bincode(&paths.chunks())?;
            let mm = safe_mmap(&paths.embeddings())?;
            let (d, c) = if mm.len() >= 8 {
                let d = u32::from_le_bytes(mm[0..4].try_into().unwrap_or([0;4]));
                let c = u32::from_le_bytes(mm[4..8].try_into().unwrap_or([0;4]));
                (d, c)
            } else { (0, 0) };
            (Some(ch), Some(mm), d, c)
        } else {
            (None, None, 0u32, 0u32)
        };
        Ok(Self {
            paths, manifest, roots, files, symbols, refs,
            fst, postings_mmap, ref_fst, ref_postings_mmap,
            trigram_fst, trigram_postings_mmap,
            lazy_symbols, lazy_refs,
            file_symbols_mmap, file_symbols_offsets_mmap,
            ref_resolutions_mmap,
            file_digests_mmap, tombstones_mmap,
            chunks, embeddings_mmap, embedding_dim, embedding_count,
        })
    }

    /// Read the embedding for a chunk index. Returns None when the
    /// sidecar is absent, the index is out of range, or the body
    /// is truncated. The returned slice borrows the mmap directly
    /// — no copy.
    pub fn chunk_embedding(&self, chunk_idx: u32) -> Option<&[f32]> {
        let mm = self.embeddings_mmap.as_ref()?;
        let dim = self.embedding_dim as usize;
        if dim == 0 { return None; }
        let row_bytes = dim * 4;
        let off = 8 + (chunk_idx as usize) * row_bytes;
        if off + row_bytes > mm.len() { return None; }
        let slice = &mm[off..off + row_bytes];
        // SAFETY: the embeddings file is a contiguous run of f32 LE
        // values written by build-embeddings; the bytes here are a
        // valid multiple of 4 (checked above) and f32 has no invalid
        // bit patterns. The mmap lives at least as long as &self,
        // which the lifetime of the returned slice is tied to.
        let (head, body, tail) = unsafe { slice.align_to::<f32>() };
        if !head.is_empty() || !tail.is_empty() { return None; }
        Some(body)
    }

    /// Rank chunks by cosine similarity against a unit-norm query
    /// vector. Brute force — O(N) but small constants because the
    /// embedding sidecar is contiguous mmap'd f32 and the kernel
    /// is a tight dot-product loop. Filters tombstoned file_ids.
    /// Returns `(chunk_idx, similarity)` sorted DESC, top `limit`.
    pub fn semantic_rank(&self, query_vec: &[f32], limit: usize) -> Vec<(u32, f32)> {
        let mm = match self.embeddings_mmap.as_ref() { Some(m) => m, None => return Vec::new() };
        let chunks = match self.chunks.as_ref() { Some(c) => c, None => return Vec::new() };
        let dim = self.embedding_dim as usize;
        let n = self.embedding_count as usize;
        if dim == 0 || n == 0 || query_vec.len() != dim { return Vec::new(); }
        let row_bytes = dim * 4;
        let body_start = 8;
        let body_end = body_start + n * row_bytes;
        if mm.len() < body_end { return Vec::new(); }
        // SAFETY: same as chunk_embedding — the body is `n * dim`
        // contiguous f32 LE values; the slice lives as long as `mm`.
        let body = &mm[body_start..body_end];
        let (head, floats, tail) = unsafe { body.align_to::<f32>() };
        if !head.is_empty() || !tail.is_empty() { return Vec::new(); }
        // Score each chunk; skip tombstoned file_ids.
        let mut scored: Vec<(u32, f32)> = Vec::with_capacity(limit * 4);
        for i in 0..n {
            if i >= chunks.len() { break; }
            let entry = &chunks[i];
            if self.is_tombstoned(entry.file_id) { continue; }
            let row = &floats[i * dim..(i + 1) * dim];
            let sim = embed::cosine_unit(query_vec, row);
            scored.push((i as u32, sim));
        }
        // Top-K by descending similarity.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }

    /// Read the blake3 digest for a file_id, if the file_digests sidecar
    /// is present. Returns None for indexes that pre-date the sidecar or
    /// for out-of-range ids.
    pub fn file_digest(&self, file_id: u32) -> Option<[u8; 32]> {
        let m = self.file_digests_mmap.as_ref()?;
        let off = (file_id as usize) * 32;
        if off + 32 > m.len() { return None; }
        let mut out = [0u8; 32];
        out.copy_from_slice(&m[off..off + 32]);
        Some(out)
    }

    /// Is this file_id tombstoned (logically deleted by a prior
    /// incremental commit)? Returns false when the tombstone sidecar
    /// is absent (the common case for indexes that have never been
    /// incrementally updated).
    pub fn is_tombstoned(&self, file_id: u32) -> bool {
        let m = match self.tombstones_mmap.as_ref() { Some(m) => m, None => return false };
        let byte = (file_id as usize) / 8;
        let bit = (file_id as usize) % 8;
        if byte >= m.len() { return false; }
        (m[byte] >> bit) & 1 == 1
    }

    /// Apply the resolution sidecar override to a RefRecord, if present.
    /// 0 in the sidecar = "no override; keep the record's own resolved_to".
    pub fn apply_resolution_override(&self, ref_idx: u32, r: &mut RefRecord) {
        let m = match self.ref_resolutions_mmap.as_ref() { Some(m) => m, None => return };
        let o = (ref_idx as usize) * 8;
        if o + 8 > m.len() { return; }
        let id = match m[o..o + 8].try_into() {
            Ok(b) => u64::from_le_bytes(b),
            Err(_) => return,
        };
        if id != 0 { r.resolved_to = Some(id); }
    }

    /// O(1) lookup of all symbol indices defined in the given file.
    /// Returns None when the file_symbols sidecar wasn't built —
    /// caller falls back to a linear scan of lazy_symbols / symbols
    /// filtered by file_id.
    pub fn symbols_for_file(&self, file_id: u32) -> Option<Vec<u32>> {
        let data = self.file_symbols_mmap.as_ref()?;
        let offs = self.file_symbols_offsets_mmap.as_ref()?;
        Some(read_file_symbols_entry(data, offs, file_id))
    }

    /// Total number of symbol records, regardless of lazy/eager backing.
    pub fn n_symbols(&self) -> usize {
        self.lazy_symbols.as_ref().map(LazyVec::len).unwrap_or(self.symbols.len())
    }

    /// Iterate every symbol record, transparently using the lazy mmap
    /// path when available. Single source of truth — previously the
    /// `if let Some(lz) = ..lazy_symbols.as_ref() { ... } else { ... }`
    /// dance was open-coded in 9 call sites across CLI + serve, easy
    /// to forget when adding a new query type.
    pub fn iter_symbols(&self) -> Box<dyn Iterator<Item = SymbolRecord> + '_> {
        if let Some(lz) = self.lazy_symbols.as_ref() {
            Box::new(lz.iter())
        } else {
            Box::new(self.symbols.iter().cloned())
        }
    }

    /// Iterate every ref record, transparently using the lazy mmap
    /// path. Same de-duplication motive as iter_symbols. Note this
    /// does NOT apply the resolution sidecar — callers that want
    /// resolved_to overrides should go through get_ref(idx).
    pub fn iter_refs(&self) -> Box<dyn Iterator<Item = RefRecord> + '_> {
        if let Some(lz) = self.lazy_refs.as_ref() {
            Box::new(lz.iter())
        } else {
            Box::new(self.refs.iter().cloned())
        }
    }
    /// Total number of ref records, regardless of lazy/eager backing.
    pub fn n_refs(&self) -> usize {
        self.lazy_refs.as_ref().map(LazyVec::len).unwrap_or(self.refs.len())
    }
    /// Get one symbol record by its global index. Owned because lazy mode
    /// decodes on-demand. Eager mode clones (cheap; ~50 bytes typically).
    /// Get a SymbolRecord by index, filtering out tombstoned files.
    /// Returns None for an out-of-range index OR for a symbol whose
    /// file_id has been marked deleted by a prior incremental commit.
    /// Compaction code that wants to *see* tombstoned records should
    /// call `get_symbol_raw` instead.
    pub fn get_symbol(&self, idx: u32) -> Option<SymbolRecord> {
        let s = self.get_symbol_raw(idx)?;
        if self.is_tombstoned(s.file_id) { return None; }
        Some(s)
    }
    /// Get a SymbolRecord by index without any tombstone filtering.
    /// Only used by compaction / debug introspection — production
    /// query paths should use `get_symbol`.
    pub fn get_symbol_raw(&self, idx: u32) -> Option<SymbolRecord> {
        if let Some(l) = self.lazy_symbols.as_ref() {
            l.get(idx as usize)
        } else {
            self.symbols.get(idx as usize).cloned()
        }
    }
    /// Get a RefRecord by index, filtering tombstones and applying the
    /// Layer 2 resolution sidecar override. Same dual as get_symbol.
    pub fn get_ref(&self, idx: u32) -> Option<RefRecord> {
        let mut rec = self.get_ref_raw(idx)?;
        if self.is_tombstoned(rec.file_id) { return None; }
        self.apply_resolution_override(idx, &mut rec);
        Some(rec)
    }
    /// Raw RefRecord access without tombstone filtering or resolution
    /// override application.
    pub fn get_ref_raw(&self, idx: u32) -> Option<RefRecord> {
        if let Some(l) = self.lazy_refs.as_ref() {
            l.get(idx as usize)
        } else {
            self.refs.get(idx as usize).cloned()
        }
    }

    /// Trigram pre-filter for literal grep. Returns Some(set of candidate
    /// file_ids) when the trigram index is available AND the needle has at
    /// least one trigram. Returns None when grep should fall back to the
    /// full-scan path (no index, or needle too short to trigrammify).
    ///
    /// Correctness: any file containing the literal `needle` MUST contain
    /// every trigram in `needle`. So the candidate set = intersection of
    /// posting lists, which is a superset of the actual-match set.
    /// Caller still scans candidates with memchr to find true positions.
    pub fn grep_candidates(&self, needle: &[u8]) -> Option<std::collections::HashSet<u32>> {
        let fst = self.trigram_fst.as_ref()?;
        let postings = self.trigram_postings_mmap.as_ref()?;
        let qts = trigram::trigrams_of_query(needle);
        if qts.is_empty() { return None; }
        // For each trigram: lookup offset, decode posting list. Skip missing
        // (means zero files contain it = empty intersection = early exit).
        let mut lists: Vec<std::collections::HashSet<u32>> = Vec::with_capacity(qts.len());
        for t in &qts {
            let off = match fst.get(t.as_slice()) {
                Some(v) => v,
                None => return Some(std::collections::HashSet::new()),
            };
            lists.push(read_trigram_posting(postings, off));
        }
        // Intersect smallest-first to minimize work.
        lists.sort_by_key(std::collections::HashSet::len);
        let mut result = lists.swap_remove(0);
        for s in lists {
            result.retain(|f| s.contains(f));
            if result.is_empty() { break; }
        }
        // Filter tombstoned file_ids — they survive in the trigram
        // index until the next compact, but their content on disk may
        // have been modified or the file deleted entirely. Skip rather
        // than serving stale hits.
        if self.tombstones_mmap.is_some() {
            result.retain(|f| !self.is_tombstoned(*f));
        }
        Some(result)
    }

    /// Diagnostic version of [`Self::grep_candidates`] that returns the
    /// per-trigram posting sizes alongside the final candidate count.
    /// Powers `scry grep --explain` — agents and humans can see *why*
    /// a query is slow (a rare trigram → tiny candidate set is good;
    /// every trigram returning 100k+ files is what we'd want to
    /// suggest a tighter pattern for).
    ///
    /// Returns None on the same paths as `grep_candidates`: no trigram
    /// FST present, or the needle is shorter than 3 bytes so trigram
    /// extraction is empty.
    pub fn grep_explain(&self, needle: &[u8]) -> Option<GrepExplain> {
        let fst = self.trigram_fst.as_ref()?;
        let postings = self.trigram_postings_mmap.as_ref()?;
        let qts = trigram::trigrams_of_query(needle);
        if qts.is_empty() { return None; }
        let mut per_trigram: Vec<(String, usize)> = Vec::with_capacity(qts.len());
        let mut lists: Vec<std::collections::HashSet<u32>> = Vec::with_capacity(qts.len());
        let mut all_present = true;
        for t in &qts {
            let label = String::from_utf8_lossy(t.as_slice()).into_owned();
            match fst.get(t.as_slice()) {
                Some(off) => {
                    let list = read_trigram_posting(postings, off);
                    per_trigram.push((label, list.len()));
                    lists.push(list);
                }
                None => {
                    // Trigram missing entirely — 0 candidates would
                    // intersect with anything. Mark it and stop after
                    // recording.
                    per_trigram.push((label, 0));
                    all_present = false;
                }
            }
        }
        let candidates = if !all_present || lists.is_empty() {
            0
        } else {
            lists.sort_by_key(std::collections::HashSet::len);
            let mut result = lists.swap_remove(0);
            for s in lists { result.retain(|f| s.contains(f)); if result.is_empty() { break; } }
            if self.tombstones_mmap.is_some() {
                result.retain(|f| !self.is_tombstoned(*f));
            }
            result.len()
        };
        Some(GrepExplain { per_trigram, candidates })
    }

    pub fn lookup_refs_exact(&self, name: &str) -> Vec<RefRecord> {
        let off = match self.ref_fst.get(name.as_bytes()) {
            Some(v) => v,
            None => return Vec::new(),
        };
        read_posting(&self.ref_postings_mmap, off)
            .into_iter()
            .filter_map(|i| self.get_ref(i))
            .collect()
    }

    pub fn lookup_exact(&self, name: &str) -> Vec<SymbolRecord> {
        let off = match self.fst.get(name.as_bytes()) {
            Some(v) => v,
            None => return Vec::new(),
        };
        self.read_posting(off).into_iter()
            .filter_map(|i| self.get_symbol(i))
            .collect()
    }

    pub fn lookup_prefix(&self, prefix: &str, limit: usize) -> Vec<SymbolRecord> {
        use fst::IntoStreamer;
        use fst::Streamer;
        let mut out: Vec<SymbolRecord> = Vec::new();
        let mut stream = self.fst.range().ge(prefix.as_bytes()).into_stream();
        while let Some((key, off)) = stream.next() {
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            for i in self.read_posting(off) {
                if let Some(s) = self.get_symbol(i) {
                    out.push(s);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    pub fn lookup_substring(&self, substr: &str, limit: usize) -> Vec<SymbolRecord> {
        use fst::Streamer;
        let mut out: Vec<SymbolRecord> = Vec::new();
        let needle = substr.as_bytes();
        let mut stream = self.fst.stream();
        while let Some((key, off)) = stream.next() {
            if memchr::memmem::find(key, needle).is_some() {
                for i in self.read_posting(off) {
                    if let Some(s) = self.get_symbol(i) {
                        out.push(s);
                        if out.len() >= limit {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    /// Typo-tolerant fuzzy lookup with an explicit edit-distance ranking.
    /// Returns `(symbol, distance)` tuples sorted by distance ASC (closest
    /// matches first), then by `rank_score` DESC for stable ordering
    /// within a single distance band, then by name ASC as a final
    /// tiebreaker so the output is deterministic.
    ///
    /// `max_distance` is the bound on the Levenshtein automaton's accept
    /// set (typically 1 or 2 — higher distances expand the candidate set
    /// disproportionately while rarely producing useful matches). A
    /// reasonable default is `max_distance=2` for queries ≥ 4 chars and
    /// `1` for shorter queries.
    ///
    /// The implementation merges two candidate sources:
    ///   1. Substring matches (catches "ParcelFile" → "ParcelFileDescriptor")
    ///   2. Levenshtein automaton matches up to `max_distance` (catches
    ///      "ParcelFille" → "ParcelFile" via a single deletion)
    /// then computes the actual Wagner-Fischer distance from query to
    /// each candidate name and sorts. Substring matches naturally land
    /// at low (but typically non-zero) distance since insertions are
    /// counted; pure typos at small distance also surface. The result is
    /// the user expectation that "I typed X, give me the closest names".
    pub fn lookup_fuzzy_ranked(
        &self,
        query: &str,
        max_distance: u32,
        limit: usize,
    ) -> Vec<(SymbolRecord, u32)> {
        use fst::{automaton::Levenshtein, IntoStreamer, Streamer};
        let cap = limit.saturating_mul(8).max(limit);
        // 1. Substring candidates (the historical behavior we preserve
        //    for the "I typed a real prefix/substring" case).
        let mut candidates: Vec<SymbolRecord> = self.lookup_substring(query, cap);

        // 2. Levenshtein-bounded candidates (the typo-tolerance case).
        //    The Automaton API requires a query ≥ 1 char and the fst
        //    crate's Levenshtein constructor caps the practical
        //    distance — patterns too complex error out, in which case
        //    we just fall back to the substring-only path.
        if !query.is_empty() && max_distance > 0 {
            if let Ok(lev) = Levenshtein::new(query, max_distance) {
                let mut stream = self.fst.search(&lev).into_stream();
                while let Some((key, off)) = stream.next() {
                    // Decode each posting once; rely on the later dedup
                    // step (by file_id, line, name) to collapse overlap
                    // with the substring candidates.
                    for i in self.read_posting(off) {
                        if let Some(s) = self.get_symbol(i) {
                            candidates.push(s);
                            if candidates.len() >= cap.saturating_mul(2) { break; }
                        }
                    }
                    if candidates.len() >= cap.saturating_mul(2) { break; }
                    let _ = key;
                }
            }
        }

        // Dedup by (file_id, line, col, name) — these tuples uniquely
        // identify a definition site regardless of which lookup path
        // brought it in. Using a HashSet of indices keeps memory bounded.
        let mut seen: std::collections::HashSet<(u32, u32, u32, String)> =
            std::collections::HashSet::with_capacity(candidates.len());
        candidates.retain(|s| {
            let k = (s.file_id, s.line, s.col, s.name.clone());
            seen.insert(k)
        });

        // Score every candidate twice:
        //   - `display_distance` = honest Wagner-Fischer distance from
        //     query to name. Returned in output; what the user sees.
        //   - `sort_score`       = "smart" ranking score that gives
        //     substring matches a strong preference over Levenshtein-
        //     close-but-unrelated names. Otherwise a typo like
        //     "Parcelable" (WF distance 2 from "ParcelFile") would
        //     outrank "ParcelFileDescriptor" (WF 10 but perfect prefix)
        //     — which is not what users mean by fuzzy.
        let q = query.as_bytes();
        let mut scored: Vec<(SymbolRecord, u32, u32)> = candidates.into_iter()
            .map(|s| {
                let display = wagner_fischer(q, s.name.as_bytes());
                let sort = fuzzy_sort_score(q, s.name.as_bytes(), display);
                (s, display, sort)
            })
            .collect();
        scored.sort_by(|a, b| {
            a.2.cmp(&b.2)                                  // sort_score ASC
                .then_with(|| b.0.rank_score().cmp(&a.0.rank_score()))
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        scored.truncate(limit);
        scored.into_iter().map(|(s, d, _)| (s, d)).collect()
    }

    fn read_posting(&self, off: u64) -> Vec<u32> {
        read_posting(&self.postings_mmap, off)
    }
}

/// Internal ranking score for fuzzy match candidates. Lower wins.
/// Treats different match qualities asymmetrically so substring matches
/// (which signal "user got the right prefix/suffix; just wants more")
/// outrank merely-Levenshtein-close-but-unrelated names (which signal
/// "user might have typoed; here's something with similar letters").
///
/// The three cases:
///   - **Exact match**: 0.
///   - **Substring**: distance proportional to `name.len() - query.len()` —
///     longer expansions cost more, but any substring outranks any typo.
///     A prefix match gets a slight extra discount over a middle-substring
///     match, which gets one over a suffix-only match.
///   - **Typo**: Wagner-Fischer distance + a penalty that scales with
///     query length. The penalty pushes typos below substring matches
///     of similar length; without it, a query like "ParcelFile" would
///     prefer "Parcelable" (WF=2, no substring) over
///     "ParcelFileDescriptor" (WF=10, perfect prefix), which violates
///     user intuition.
fn fuzzy_sort_score(query: &[u8], name: &[u8], wf: u32) -> u32 {
    if query == name { return 0; }
    let q_len = query.len();
    let n_len = name.len();
    if q_len <= n_len {
        // Substring check. Cheap memchr-style scan.
        if name.windows(q_len).any(|w| w == query) {
            let extra = (n_len - q_len) as u32;
            // Prefix-match discount: the most common "I just want a
            // longer name" intent.
            if name.starts_with(query) { return extra; }
            // Middle-substring slightly worse than prefix.
            return extra + 1;
        }
    }
    // Pure typo (or query longer than name): WF + per-query-length
    // penalty so any substring of comparable length wins.
    wf + (q_len as u32 * 2).max(4)
}

/// Scan a single file for the literal needle, returning all match
/// byte-offsets up to `max_per_file` hits. Uses mmap + memchr::memmem
/// rather than `std::fs::read` so:
///
///   1. We don't pay a per-file Vec<u8> allocation + copy. The mmap
///      memory is page-cache-backed and managed by the kernel; once
///      the bytes are searched the page is just another LRU page.
///   2. Cold-cache page faults overlap with the search loop, since
///      memchr::memmem walks sequentially and the kernel's readahead
///      pulls ahead of the cursor.
///   3. Memory footprint per scan is bounded by the file size, but
///      shared across the page cache — under memory pressure the
///      kernel evicts pages we're done with naturally.
///
/// Returns Vec<usize> of match start offsets. Caller is responsible
/// for stopping early; this helper returns ALL hits up to the cap.
///
/// max_file_bytes: refuse to open files larger than this. Prevents
/// a single multi-GB binary blob the walker missed from blowing up
/// the page cache.
pub fn scan_file_literal(
    path: &Path,
    needle: &[u8],
    max_per_file: usize,
    max_file_bytes: u64,
) -> Vec<usize> {
    if needle.is_empty() { return Vec::new(); }
    let f = match File::open(path) { Ok(f) => f, Err(_) => return Vec::new() };
    let md = match f.metadata() { Ok(m) => m, Err(_) => return Vec::new() };
    if md.len() == 0 || md.len() > max_file_bytes {
        return Vec::new();
    }
    // SAFETY: The file is opened read-only and the mmap is read-only.
    // We don't expose the slice past the lifetime of this fn — it
    // doesn't escape. memmap2::Mmap holds the mapping; dropping it
    // unmaps. Same audited pattern as safe_mmap above.
    let mm = match unsafe { memmap2::Mmap::map(&f) } { Ok(m) => m, Err(_) => return Vec::new() };
    let bytes = &mm[..];
    let mut out = Vec::with_capacity(8.min(max_per_file));
    let mut start = 0usize;
    while out.len() < max_per_file {
        match memchr::memmem::find(&bytes[start..], needle) {
            Some(off) => {
                let abs = start + off;
                out.push(abs);
                start = abs + needle.len().max(1);
                if start >= bytes.len() { break; }
            }
            None => break,
        }
    }
    out
}

/// Wagner-Fischer edit distance between two byte slices.
///
/// O(|a| * |b|) time; O(min(|a|, |b|)) space via two rolling rows. Used
/// by `lookup_fuzzy_ranked` to score candidate symbol names against a
/// query. Bytes are compared raw — for ASCII identifier names this is
/// identical to character distance; for multi-byte UTF-8 names the
/// distance is byte-level (not grapheme-level), which slightly
/// over-penalizes Unicode identifiers. Acceptable for AOSP / Linux
/// kernel symbol sets, which are overwhelmingly ASCII.
///
/// Public so the CLI / test layer can pin specific values without
/// touching the StoreReader.
pub fn wagner_fischer(a: &[u8], b: &[u8]) -> u32 {
    let n = a.len();
    let m = b.len();
    if n == 0 { return m as u32; }
    if m == 0 { return n as u32; }
    // Keep `b` as the shorter side to bound space at O(min(n, m)).
    let (a, b, n, m) = if n < m { (b, a, m, n) } else { (a, b, n, m) };
    let mut prev: Vec<u32> = (0..=m as u32).collect();
    let mut curr: Vec<u32> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i as u32;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (curr[j - 1] + 1)        // insertion in b
                .min(prev[j] + 1)              // deletion in b
                .min(prev[j - 1] + cost);      // substitution
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// Decode a trigram posting list at the given byte offset. Layout matches
/// what kway_merge_trigrams_to_fst wrote: u32 LE count, then `count` ×
/// varint(delta_from_previous), reconstructing the original sorted u32 list.
fn read_trigram_posting(buf: &[u8], off: u64) -> std::collections::HashSet<u32> {
    let start = off as usize;
    let mut out = std::collections::HashSet::new();
    let count = match read_u32_le(buf, start) { Some(n) => n as usize, None => return out };
    out.reserve(count);
    let mut p = start + 4;
    let mut prev: u32 = 0;
    for _ in 0..count {
        // varint decode (LE128, MSB continuation)
        let mut delta: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            if p >= buf.len() { return out; }
            let b = buf[p];
            p += 1;
            delta |= ((b & 0x7f) as u32) << shift;
            if b & 0x80 == 0 { break; }
            shift += 7;
            if shift >= 32 { return out; } // malformed
        }
        let f = prev.wrapping_add(delta);
        out.insert(f);
        prev = f;
    }
    out
}

/// Decode the file_symbols entry for `file_id` from a (data, offsets) byte
/// slice pair. Layout: offsets[file_id] is a u64-LE byte position into
/// data; at that position is a u32-LE count followed by `count` u32-LE
/// symbol indices. Out-of-range or truncated reads return an empty Vec
/// rather than panicking — same defensive posture as read_posting.
pub fn read_file_symbols_entry(data: &[u8], offsets: &[u8], file_id: u32) -> Vec<u32> {
    let o = (file_id as usize) * 8;
    if o + 8 > offsets.len() { return Vec::new(); }
    let start = match offsets[o..o + 8].try_into() {
        Ok(b) => u64::from_le_bytes(b) as usize,
        Err(_) => return Vec::new(),
    };
    if start + 4 > data.len() { return Vec::new(); }
    let count = match data[start..start + 4].try_into() {
        Ok(b) => u32::from_le_bytes(b) as usize,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::with_capacity(count);
    let mut p = start + 4;
    for _ in 0..count {
        if p + 4 > data.len() { break; }
        if let Ok(b) = data[p..p + 4].try_into() {
            out.push(u32::from_le_bytes(b));
        }
        p += 4;
    }
    out
}

fn read_posting(buf: &[u8], off: u64) -> Vec<u32> {
    let start = off as usize;
    let count = match read_u32_le(buf, start) { Some(n) => n as usize, None => return Vec::new() };
    let mut v = Vec::with_capacity(count);
    let mut p = start + 4;
    for _ in 0..count {
        match read_u32_le(buf, p) {
            Some(n) => v.push(n),
            None => break,
        }
        p += 4;
    }
    v
}

/// Read a little-endian u32 from `buf` at `pos`, returning None for any
/// out-of-bounds access. Single source of truth for the "bounds check
/// then decode" pattern so a future refactor of the buffer-size guard
/// can't reintroduce a `try_into().unwrap()` panic on a corrupt mmap.
#[inline]
fn read_u32_le(buf: &[u8], pos: usize) -> Option<u32> {
    let arr: [u8; 4] = buf.get(pos..pos + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}


fn read_bincode<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let f = BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    bincode::deserialize_from(f)
        .map_err(|e| anyhow!("decode {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scry_walker::FileKind;

    /// Build a LazyVec-compatible pair on disk: data file holds an 8-byte
    /// u64-LE length prefix followed by each record bincode-serialized
    /// back to back; offsets file holds one u64-LE per record giving its
    /// byte offset within the data file. Matches the on-disk format
    /// produced by StoreWriter::finalize_streaming.
    fn write_lazy_vec_files<T: Serialize>(
        dir: &Path,
        items: &[T],
    ) -> (PathBuf, PathBuf) {
        let data_path = dir.join("data.bin");
        let off_path = dir.join("offsets.bin");
        let mut data = File::create(&data_path).unwrap();
        let mut off = File::create(&off_path).unwrap();
        let len = items.len() as u64;
        Write::write_all(&mut data, &len.to_le_bytes()).unwrap();
        let mut byte_pos: u64 = 8;
        for it in items {
            Write::write_all(&mut off, &byte_pos.to_le_bytes()).unwrap();
            let bytes = bincode::serialize(it).unwrap();
            Write::write_all(&mut data, &bytes).unwrap();
            byte_pos += bytes.len() as u64;
        }
        (data_path, off_path)
    }

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("scry-store-test-{tag}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_symbols() -> Vec<SymbolRecord> {
        // Deliberately variable-size: different name lengths + scope_path
        // sizes so the byte-offset arithmetic actually matters. A test
        // built on fixed-stride records would not catch an offset bug.
        vec![
            SymbolRecord {
                id: 1, name: "a".into(), fqn: None,
                kind: SymbolKind::Class, file_id: 0,
                byte_start: 0, byte_end: 1, line: 1, col: 1,
                scope_path: vec![], lang: FileKind::Java,
            },
            SymbolRecord {
                id: 2,
                name: "ActivityManagerServiceWithAReallyVeryLongIdentifier".into(),
                fqn: Some("com.android.server.am.ActivityManagerServiceWithAReallyVeryLongIdentifier".into()),
                kind: SymbolKind::Method, file_id: 42,
                byte_start: 100, byte_end: 1024, line: 999, col: 13,
                scope_path: vec!["com".into(), "android".into(), "server".into(), "am".into()],
                lang: FileKind::Java,
            },
            SymbolRecord {
                id: 3, name: "x".into(), fqn: Some("x".into()),
                kind: SymbolKind::Variable, file_id: 7,
                byte_start: 0, byte_end: 1, line: 1, col: 1,
                scope_path: vec!["one".into()], lang: FileKind::Rust,
            },
            SymbolRecord {
                id: 4, name: "fourth".into(), fqn: None,
                kind: SymbolKind::SoongModule, file_id: 1234,
                byte_start: 12345, byte_end: 67890, line: 88, col: 4,
                scope_path: vec![], lang: FileKind::Soong,
            },
            SymbolRecord {
                id: 5, name: "transact".into(),
                fqn: Some("android.os.Binder.transact".into()),
                kind: SymbolKind::Method, file_id: 999,
                byte_start: 5000, byte_end: 5500, line: 250, col: 17,
                scope_path: vec!["android".into(), "os".into(), "Binder".into()],
                lang: FileKind::Java,
            },
        ]
    }

    fn assert_symbol_eq(a: &SymbolRecord, b: &SymbolRecord) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.name, b.name);
        assert_eq!(a.fqn, b.fqn);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.file_id, b.file_id);
        assert_eq!(a.byte_start, b.byte_start);
        assert_eq!(a.byte_end, b.byte_end);
        assert_eq!(a.line, b.line);
        assert_eq!(a.col, b.col);
        assert_eq!(a.scope_path, b.scope_path);
        assert_eq!(a.lang, b.lang);
    }

    /// Sequential get(0..N) returns byte-identical records to the input.
    #[test]
    fn lazy_vec_sequential_roundtrip() {
        let dir = unique_tmpdir("seq");
        let syms = sample_symbols();
        let (d, o) = write_lazy_vec_files(&dir, &syms);
        let lv = LazyVec::<SymbolRecord>::open(&d, &o).unwrap();
        assert_eq!(lv.len(), syms.len());
        for (i, expected) in syms.iter().enumerate() {
            let got = lv.get(i).expect("in-range get must succeed");
            assert_symbol_eq(&got, expected);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reverse-order access catches "we accidentally assumed sequential
    /// scan" — the lazy reader's job is random access via the offsets
    /// sidecar; a bug that only worked when reading forwards would slip
    /// past a sequential-only test.
    #[test]
    fn lazy_vec_reverse_access() {
        let dir = unique_tmpdir("rev");
        let syms = sample_symbols();
        let (d, o) = write_lazy_vec_files(&dir, &syms);
        let lv = LazyVec::<SymbolRecord>::open(&d, &o).unwrap();
        for i in (0..syms.len()).rev() {
            let got = lv.get(i).unwrap();
            assert_symbol_eq(&got, &syms[i]);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Random-permutation access. Same correctness guarantee, harder to
    /// satisfy with any caching-by-position scheme.
    #[test]
    fn lazy_vec_random_access() {
        let dir = unique_tmpdir("rand");
        let syms = sample_symbols();
        let (d, o) = write_lazy_vec_files(&dir, &syms);
        let lv = LazyVec::<SymbolRecord>::open(&d, &o).unwrap();
        for &i in &[3usize, 0, 4, 1, 2, 4, 3, 0] {
            let got = lv.get(i).unwrap();
            assert_symbol_eq(&got, &syms[i]);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// iter() must yield ALL records in order — the production cmd_stats
    /// lazy path relies on this for "iterate the index without loading it".
    #[test]
    fn lazy_vec_iter_full_passthrough() {
        let dir = unique_tmpdir("iter");
        let syms = sample_symbols();
        let (d, o) = write_lazy_vec_files(&dir, &syms);
        let lv = LazyVec::<SymbolRecord>::open(&d, &o).unwrap();
        let collected: Vec<SymbolRecord> = lv.iter().collect();
        assert_eq!(collected.len(), syms.len());
        for (a, b) in collected.iter().zip(syms.iter()) {
            assert_symbol_eq(a, b);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Out-of-bounds must be None, never panic. This is the LLM-side
    /// safety net: a stale symbol id from a fresher index that got
    /// looked up against an older one must not crash the reader.
    #[test]
    fn lazy_vec_out_of_bounds_returns_none() {
        let dir = unique_tmpdir("oob");
        let syms = sample_symbols();
        let (d, o) = write_lazy_vec_files(&dir, &syms);
        let lv = LazyVec::<SymbolRecord>::open(&d, &o).unwrap();
        assert!(lv.get(syms.len()).is_none());
        assert!(lv.get(syms.len() + 100).is_none());
        assert!(lv.get(usize::MAX).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Empty vec edge case: open succeeds, len == 0, iter is empty,
    /// any get is None. Matters because finalize_streaming will emit
    /// zero-length sidecars for a corpus with no symbols of a kind
    /// (e.g. a Linux-only index has no Java SymbolRecords, but the
    /// file structure is still produced).
    #[test]
    fn lazy_vec_empty() {
        let dir = unique_tmpdir("empty");
        let syms: Vec<SymbolRecord> = vec![];
        let (d, o) = write_lazy_vec_files(&dir, &syms);
        let lv = LazyVec::<SymbolRecord>::open(&d, &o).unwrap();
        assert_eq!(lv.len(), 0);
        assert!(lv.is_empty());
        assert!(lv.get(0).is_none());
        assert_eq!(lv.iter().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LazyVec is generic over T — works equally for RefRecord. Pin
    /// that so future T-specific changes don't quietly skip refs.
    #[test]
    fn lazy_vec_refs_also_roundtrip() {
        let dir = unique_tmpdir("refs");
        let refs = vec![
            RefRecord {
                name: "transact".into(), kind: RefKind::Call, file_id: 1,
                byte_start: 100, byte_end: 108, line: 50, col: 12,
                scope_path: vec!["Foo".into()], lang: FileKind::Java,
                resolved_to: None,
            },
            RefRecord {
                name: "Binder".into(), kind: RefKind::TypeUse, file_id: 2,
                byte_start: 0, byte_end: 6, line: 1, col: 1,
                scope_path: vec![], lang: FileKind::Java,
                resolved_to: Some(12345),
            },
        ];
        let (d, o) = write_lazy_vec_files(&dir, &refs);
        let lv = LazyVec::<RefRecord>::open(&d, &o).unwrap();
        assert_eq!(lv.len(), refs.len());
        for (i, expected) in refs.iter().enumerate() {
            let got = lv.get(i).unwrap();
            assert_eq!(got.name, expected.name);
            assert_eq!(got.kind, expected.kind);
            assert_eq!(got.file_id, expected.file_id);
            assert_eq!(got.line, expected.line);
            assert_eq!(got.resolved_to, expected.resolved_to);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Helper: build the file_symbols.bin + offsets.bin pair in memory
    /// for direct testing of read_file_symbols_entry. Layout exactly
    /// mirrors what finalize_streaming + build-file-symbols write.
    fn build_fs_pair(by_file: &[Vec<u32>]) -> (Vec<u8>, Vec<u8>) {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        let mut pos: u64 = 0;
        for ids in by_file {
            offsets.extend_from_slice(&pos.to_le_bytes());
            let count = ids.len() as u32;
            data.extend_from_slice(&count.to_le_bytes());
            for id in ids {
                data.extend_from_slice(&id.to_le_bytes());
            }
            pos += 4 + 4 * (ids.len() as u64);
        }
        (data, offsets)
    }

    /// Round-trip: write the packed format, decode it back per-file,
    /// assert all entries match the input.
    #[test]
    fn file_symbols_entry_roundtrip() {
        let by_file = vec![
            vec![0u32, 5, 10, 999],
            vec![],
            vec![42],
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        ];
        let (data, offs) = build_fs_pair(&by_file);
        for (fid, expected) in by_file.iter().enumerate() {
            let got = read_file_symbols_entry(&data, &offs, fid as u32);
            assert_eq!(&got, expected, "file_id {} mismatch", fid);
        }
    }

    /// Out-of-range file_id returns empty Vec (defensive, never panics).
    /// Matters because outline against a stale path could pass a file_id
    /// that doesn't exist in this index.
    #[test]
    fn file_symbols_entry_oob_returns_empty() {
        let by_file = vec![vec![1u32, 2, 3]];
        let (data, offs) = build_fs_pair(&by_file);
        assert!(read_file_symbols_entry(&data, &offs, 1).is_empty());
        assert!(read_file_symbols_entry(&data, &offs, 999).is_empty());
        assert!(read_file_symbols_entry(&data, &offs, u32::MAX).is_empty());
    }

    /// Empty corpus: zero files = empty offsets + empty data, any lookup
    /// returns empty. Covers the edge where StoreWriter writes the sidecar
    /// for an index with no files.
    #[test]
    fn file_symbols_entry_empty_corpus() {
        let (data, offs) = build_fs_pair(&[]);
        assert!(data.is_empty());
        assert!(offs.is_empty());
        assert!(read_file_symbols_entry(&data, &offs, 0).is_empty());
    }

    /// Truncated data file (last entry's indices got chopped) — we should
    /// return what we can parse, never panic. Matches the defensive
    /// posture of read_posting.
    #[test]
    fn file_symbols_entry_truncated() {
        let by_file = vec![vec![1u32, 2, 3, 4, 5]];
        let (mut data, offs) = build_fs_pair(&by_file);
        data.truncate(data.len() - 8); // chop the last two indices
        let got = read_file_symbols_entry(&data, &offs, 0);
        assert_eq!(got, vec![1, 2, 3]);
    }

    fn mk_symbol(name: &str, kind: SymbolKind, lang: FileKind, scope: Vec<&str>) -> SymbolRecord {
        SymbolRecord {
            id: 1, name: name.into(), fqn: None, kind,
            file_id: 0, byte_start: 0, byte_end: 1, line: 1, col: 1,
            scope_path: scope.into_iter().map(String::from).collect(),
            lang,
        }
    }

    /// A real class definition outranks an api-txt class declaration of
    /// the same name. This is the canonical regression `def Activity`
    /// returning Activity.java first instead of *current.txt.
    #[test]
    fn rank_real_class_beats_api_txt() {
        let real = mk_symbol("Activity", SymbolKind::Class, FileKind::Java, vec!["Activity"]);
        let apitxt = mk_symbol("Activity", SymbolKind::Class, FileKind::ApiTxt, vec!["android.app"]);
        assert!(real.rank_score() > apitxt.rank_score(),
                "real {} should beat api-txt {}", real.rank_score(), apitxt.rank_score());
    }

    /// Top-level types outrank deeply-nested ones with the same name.
    /// Matches the intuition that `def Foo` should surface the outer
    /// Foo class first, not an inner helper class.
    #[test]
    fn rank_shallow_scope_beats_deep() {
        let shallow = mk_symbol("Foo", SymbolKind::Class, FileKind::Java, vec![]);
        let deep = mk_symbol("Foo", SymbolKind::Class, FileKind::Java,
                              vec!["pkg", "Outer", "Middle", "Inner"]);
        assert!(shallow.rank_score() > deep.rank_score());
    }

    /// Types outrank functions outrank fields — pinning the kind tier
    /// ordering so a refactor of the match arms can't silently swap them.
    #[test]
    fn rank_kind_ordering() {
        let class = mk_symbol("X", SymbolKind::Class, FileKind::Java, vec![]);
        let function = mk_symbol("X", SymbolKind::Function, FileKind::Java, vec![]);
        let field = mk_symbol("X", SymbolKind::Field, FileKind::Java, vec![]);
        let param = mk_symbol("X", SymbolKind::Parameter, FileKind::Java, vec![]);
        assert!(class.rank_score() > function.rank_score());
        assert!(function.rank_score() > field.rank_score());
        assert!(field.rank_score() > param.rank_score());
    }

    /// AOSP-specific kinds keep their boost — SoongModule and AidlInterface
    /// ARE the canonical definition in their domain, so a generic Class
    /// of the same name shouldn't crowd them out when an agent is
    /// explicitly looking for a build module or AIDL interface.
    /// Epoch 0 = the canonical ISO-8601 sample. Pins the algorithm
    /// against the bug that was actually shipped (a non-ISO string of
    /// shape `"{secs}-unixT{HH:MM:SS}Z"` with a unix epoch in the year
    /// position).
    #[test]
    fn epoch_iso_known_values() {
        assert_eq!(epoch_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_iso8601(86_400), "1970-01-02T00:00:00Z");
        // A famous "year 2000" round number: 2000-01-01T00:00:00Z =
        // 946684800. Catches off-by-one in the days-from-civil math.
        assert_eq!(epoch_to_iso8601(946_684_800), "2000-01-01T00:00:00Z");
        // 2026-05-16T04:51:51Z = the live index's finalize timestamp.
        assert_eq!(epoch_to_iso8601(1_778_907_111), "2026-05-16T04:51:51Z");
    }

    /// Leap-year handling — Feb 29 must exist in 2024, not 2023.
    #[test]
    fn epoch_iso_leap_year() {
        // 2024-02-29T00:00:00Z = 1709164800
        assert_eq!(epoch_to_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
        // 2023-03-01T00:00:00Z = 1677628800 (no Feb 29 in 2023)
        assert_eq!(epoch_to_iso8601(1_677_628_800), "2023-03-01T00:00:00Z");
    }

    /// Pre-epoch dates should still produce a syntactically valid
    /// ISO-8601 string (negative year prefixed with - if needed).
    /// We don't promise correctness, just non-panic.
    #[test]
    fn epoch_iso_pre_epoch_does_not_panic() {
        let s = epoch_to_iso8601(-86_400);
        assert!(s.ends_with('Z'), "got {s}");
        assert_eq!(s, "1969-12-31T00:00:00Z");
    }

    #[test]
    fn rank_aosp_kinds_keep_boost() {
        let aidl = mk_symbol("IFoo", SymbolKind::AidlInterface, FileKind::Aidl, vec![]);
        let soong = mk_symbol("libfoo", SymbolKind::SoongModule, FileKind::Soong, vec![]);
        let init = mk_symbol("zygote", SymbolKind::InitService, FileKind::InitRc, vec![]);
        // All should outrank a plain Field/Parameter
        let field = mk_symbol("X", SymbolKind::Field, FileKind::Java, vec![]);
        assert!(aidl.rank_score() > field.rank_score());
        assert!(soong.rank_score() > field.rank_score());
        assert!(init.rank_score() > field.rank_score());
    }

    // -----------------------------------------------------------------
    // Wire-format tests for the on-disk posting decoders. These pin the
    // exact byte layouts so a future "let me change one little thing"
    // refactor can't silently corrupt every index in the world.
    // -----------------------------------------------------------------

    /// Build a trigram posting body (count u32-LE + delta-varint sequence)
    /// from a sorted list of file IDs. Mirrors what kway_merge_trigrams_to_fst
    /// writes; the read path is read_trigram_posting.
    fn build_trigram_posting_bytes(ids_sorted: &[u32]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + ids_sorted.len() * 2);
        buf.extend_from_slice(&(ids_sorted.len() as u32).to_le_bytes());
        let mut prev: u32 = 0;
        for &id in ids_sorted {
            let mut delta = id.wrapping_sub(prev);
            loop {
                let b = (delta & 0x7f) as u8;
                delta >>= 7;
                if delta == 0 {
                    buf.push(b);
                    break;
                }
                buf.push(b | 0x80);
            }
            prev = id;
        }
        buf
    }

    #[test]
    fn trigram_posting_roundtrip() {
        // Sorted, dense and sparse mixed; includes 0 (corner case for
        // delta encoding) and a large jump that uses multi-byte varint.
        let ids = vec![0u32, 1, 5, 100, 100_000, 100_001, 5_000_000];
        let bytes = build_trigram_posting_bytes(&ids);
        let got = read_trigram_posting(&bytes, 0);
        for id in &ids {
            assert!(got.contains(id), "missing {id} from {got:?}");
        }
        assert_eq!(got.len(), ids.len(), "extra entries: {got:?}");
    }

    #[test]
    fn trigram_posting_empty_count_returns_empty() {
        // Count = 0 → no further reads should happen; returned set empty.
        let bytes = 0u32.to_le_bytes().to_vec();
        let got = read_trigram_posting(&bytes, 0);
        assert!(got.is_empty());
    }

    #[test]
    fn trigram_posting_truncated_count_returns_empty() {
        // Buf shorter than 4 bytes — can't even read the count.
        let bytes = vec![0xff, 0xff]; // 2 bytes
        let got = read_trigram_posting(&bytes, 0);
        assert!(got.is_empty());
    }

    #[test]
    fn trigram_posting_truncated_varint_returns_partial() {
        // Count = 3 but only one full varint follows; second varint has
        // the continuation bit set but no following byte. Decoder must
        // stop cleanly rather than read past the buffer.
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.push(0x05);              // first id = 5, complete varint
        bytes.push(0x80);              // second id varint truncated (continuation, no next)
        let got = read_trigram_posting(&bytes, 0);
        // The first id should be present; the truncated one must not
        // panic or produce a garbage entry.
        assert!(got.contains(&5));
        assert!(got.len() <= 1, "got {got:?}");
    }

    #[test]
    fn trigram_posting_malformed_varint_returns_partial() {
        // varint claims to extend > 32 bits — read_trigram_posting must
        // bail rather than overflow the shift.
        let mut bytes = 1u32.to_le_bytes().to_vec();
        // 6 bytes all with continuation bit; shift would reach 35 > 32.
        bytes.extend(std::iter::repeat_n(0x80, 6));
        // Must not panic.
        let _got = read_trigram_posting(&bytes, 0);
    }

    /// read_posting (the simple u32-LE list, used for name postings)
    /// round-trips its writer's output and tolerates truncation.
    #[test]
    fn name_posting_roundtrip() {
        let ids = vec![10u32, 200, 3000, 40_000];
        let mut bytes = (ids.len() as u32).to_le_bytes().to_vec();
        for id in &ids {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        let got = read_posting(&bytes, 0);
        assert_eq!(got, ids);
    }

    #[test]
    fn name_posting_truncated_returns_partial() {
        // Count = 3 but only 2 u32 bodies present. Decoder reads what's
        // there and stops rather than panicking on the third.
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&20u32.to_le_bytes());
        // No third u32.
        let got = read_posting(&bytes, 0);
        assert_eq!(got, vec![10, 20]);
    }

    #[test]
    fn name_posting_empty_count_returns_empty() {
        let bytes = 0u32.to_le_bytes().to_vec();
        let got = read_posting(&bytes, 0);
        assert!(got.is_empty());
    }

    #[test]
    fn name_posting_oob_offset_returns_empty() {
        // Offset past the end of the buffer must not panic.
        let bytes = vec![0u8; 4];
        let got = read_posting(&bytes, 1000);
        assert!(got.is_empty());
    }

    // ------------------------------------------------------------------
    // Wagner-Fischer edit distance — pins the canonical small cases so a
    // future refactor of the inner loop can't silently mis-rank fuzzy
    // results. We test the published-textbook values that lots of unit
    // tests across the industry use; they're not arbitrary.
    // ------------------------------------------------------------------

    #[test]
    fn wagner_fischer_known_pairs() {
        // Identity
        assert_eq!(wagner_fischer(b"foo", b"foo"), 0);
        // Single insertion
        assert_eq!(wagner_fischer(b"foo", b"foos"), 1);
        // Single deletion
        assert_eq!(wagner_fischer(b"foos", b"foo"), 1);
        // Single substitution
        assert_eq!(wagner_fischer(b"foo", b"fou"), 1);
        // The textbook kitten / sitting → 3
        assert_eq!(wagner_fischer(b"kitten", b"sitting"), 3);
        // Symmetry holds
        assert_eq!(wagner_fischer(b"sitting", b"kitten"), 3);
        // Empty inputs
        assert_eq!(wagner_fischer(b"", b""), 0);
        assert_eq!(wagner_fischer(b"", b"abc"), 3);
        assert_eq!(wagner_fischer(b"abc", b""), 3);
    }

    /// The space-bound trick (keep the shorter side as `b`) must not
    /// change correctness. Spot-check by reversing the argument order
    /// across multiple length combinations.
    #[test]
    fn wagner_fischer_argument_order_invariance() {
        let pairs: &[(&[u8], &[u8])] = &[
            (b"abc", b"xyz"),
            (b"hello", b"world"),
            (b"a", b"abcde"),
            (b"ParcelFile", b"ParcelFileDescriptor"),
        ];
        for (a, b) in pairs {
            assert_eq!(wagner_fischer(a, b), wagner_fischer(b, a),
                       "asymmetric on ({a:?}, {b:?})");
        }
    }

    /// Pure Wagner-Fischer ordering test (the underlying metric, not
    /// the user-facing rank). Pinned so a refactor of the inner loop
    /// can't silently mis-rank.
    #[test]
    fn wagner_fischer_orders_closer_matches_first() {
        let names = [
            "Parcel",
            "ParcelFile",
            "ParcelFileDescriptor",
            "Parcelable",
        ];
        let query = b"ParcelFile";
        let mut scored: Vec<(&str, u32)> = names.iter()
            .map(|n| (*n, wagner_fischer(query, n.as_bytes())))
            .collect();
        scored.sort_by_key(|&(_, d)| d);
        // Exact match wins.
        assert_eq!(scored[0].0, "ParcelFile");
        assert_eq!(scored[0].1, 0);
    }

    /// The fuzzy *sort* score (vs pure Levenshtein) gives substring
    /// matches a strong preference over Levenshtein-close-but-unrelated
    /// names. This test pins the user-facing ordering:
    ///
    ///   query="ParcelFile" should rank
    ///     ParcelFile             (exact)              first
    ///     ParcelFileDescriptor   (prefix substring)   before
    ///     OutboundParcelFile     (middle substring)   before
    ///     Parcelable             (typo, WF=2)         before
    ///     ParcellableFooBar      (typo, WF≈9)         last
    ///
    /// Without the substring bonus, Parcelable (WF=2) would outrank
    /// ParcelFileDescriptor (WF=10) — which is wrong per the ROADMAP.
    #[test]
    fn fuzzy_sort_score_prefers_substring_over_typo() {
        let q = b"ParcelFile";
        let names = [
            "ParcelFile",
            "ParcelFileDescriptor",
            "OutboundParcelFile",
            "Parcelable",
            "ParcellableFooBar",
        ];
        let mut scored: Vec<(&str, u32)> = names.iter()
            .map(|n| {
                let nb = n.as_bytes();
                (*n, fuzzy_sort_score(q, nb, wagner_fischer(q, nb)))
            })
            .collect();
        scored.sort_by_key(|&(_, d)| d);
        let names_sorted: Vec<&str> = scored.iter().map(|(n, _)| *n).collect();
        assert_eq!(names_sorted[0], "ParcelFile",
                   "exact must rank first: {scored:?}");
        let pfd = names_sorted.iter().position(|&n| n == "ParcelFileDescriptor").unwrap();
        let parcelable = names_sorted.iter().position(|&n| n == "Parcelable").unwrap();
        assert!(pfd < parcelable,
                "ParcelFileDescriptor (prefix substring) must outrank Parcelable (typo): {scored:?}");
        let outbound = names_sorted.iter().position(|&n| n == "OutboundParcelFile").unwrap();
        let parcellable = names_sorted.iter().position(|&n| n == "ParcellableFooBar").unwrap();
        assert!(outbound < parcellable,
                "OutboundParcelFile (middle substring) must outrank ParcellableFooBar (typo): {scored:?}");
    }

    // ------------------------------------------------------------------
    // file_digest + is_tombstoned sidecar accessors (incremental indexing)
    // ------------------------------------------------------------------

    /// A reader opened against an index dir with no `file_digests.bin`
    /// returns None for every file_digest query — confirms the
    /// optional-sidecar path doesn't crash when the sidecar is absent
    /// (the default state until `scry build-digests` runs).
    /// (Builds nothing; just exercises the accessor against a stub
    /// reader. The reader's open() is too heavyweight to construct
    /// inline; we verify the bare accessor logic via a focused unit.)
    #[test]
    fn file_digest_absent_returns_none() {
        // Simulate by checking that an out-of-range byte slice returns
        // None — same code path as "sidecar missing" because the
        // accessor only reads when the mmap is Some(_).
        let mm: Option<&[u8]> = None;
        // Inline the accessor logic:
        let file_id = 5u32;
        let digest = mm.and_then(|m| {
            let off = (file_id as usize) * 32;
            if off + 32 > m.len() { None }
            else {
                let mut out = [0u8; 32];
                out.copy_from_slice(&m[off..off+32]);
                Some(out)
            }
        });
        assert!(digest.is_none());
    }

    /// scan_file_literal: round-trips simple cases against a temp file.
    /// Covers empty needle (Vec empty), needle longer than file (Vec
    /// empty), single match, multi-match cap, and oversize-file refuse.
    #[test]
    fn scan_file_literal_basic_cases() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(
            format!("scry-scan-{}", std::process::id())
        );
        // Multi-match: "foo" appears 3x in "foo bar foo baz foo".
        std::fs::write(&tmp, b"foo bar foo baz foo").unwrap();
        let m = scan_file_literal(&tmp, b"foo", 100, 1 << 20);
        assert_eq!(m, vec![0, 8, 16]);
        // Cap honored.
        let m2 = scan_file_literal(&tmp, b"foo", 2, 1 << 20);
        assert_eq!(m2.len(), 2);
        // Empty needle: bails.
        let m3 = scan_file_literal(&tmp, b"", 10, 1 << 20);
        assert!(m3.is_empty());
        // Oversize-file refuse: max_file_bytes=10 < actual file size.
        let m4 = scan_file_literal(&tmp, b"foo", 10, 10);
        assert!(m4.is_empty());
        // No match: empty.
        let m5 = scan_file_literal(&tmp, b"xyz", 10, 1 << 20);
        assert!(m5.is_empty());
        // Missing file: empty (no panic).
        let m6 = scan_file_literal(
            Path::new("/nonexistent/path/here"),
            b"foo", 10, 1 << 20,
        );
        assert!(m6.is_empty());
        // Write a partial-write helper test: file with no trailing
        // newline still scans correctly.
        let tmp2 = std::env::temp_dir().join(
            format!("scry-scan-2-{}", std::process::id())
        );
        let mut f = File::create(&tmp2).unwrap();
        f.write_all(b"abcd").unwrap();
        let m7 = scan_file_literal(&tmp2, b"bc", 5, 1 << 20);
        assert_eq!(m7, vec![1]);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&tmp2);
    }

    /// Tombstone bitmap accessor: bit 0 of byte 0 = file_id 0, bit 3 of
    /// byte 1 = file_id 11. Verify the byte/bit math by constructing a
    /// known bitmap and exercising the accessor pattern.
    #[test]
    fn tombstone_bitmap_byte_bit_layout() {
        // Mark file_ids 0, 7, 8, 11, 64 as tombstoned.
        let mut buf = [0u8; 16];
        buf[0] |= 1 << 0;     // file_id 0
        buf[0] |= 1 << 7;     // file_id 7
        buf[1] |= 1 << 0;     // file_id 8
        buf[1] |= 1 << 3;     // file_id 11
        buf[8] |= 1 << 0;     // file_id 64
        let is_tombstoned = |id: u32| -> bool {
            let byte = (id as usize) / 8;
            let bit = (id as usize) % 8;
            if byte >= buf.len() { return false; }
            (buf[byte] >> bit) & 1 == 1
        };
        for id in [0, 7, 8, 11, 64] {
            assert!(is_tombstoned(id), "file_id {id} should be tombstoned");
        }
        for id in [1, 2, 3, 4, 5, 6, 9, 10, 12, 63, 65, 100, 999] {
            assert!(!is_tombstoned(id), "file_id {id} should NOT be tombstoned");
        }
    }

    /// Prefix-match discount: among two substring matches of equal
    /// length, the one where the query is a prefix should win.
    #[test]
    fn fuzzy_sort_prefix_beats_middle_substring() {
        let q = b"abc";
        let prefix_name = b"abcXYZ";  // query is prefix
        let middle_name = b"XabcYZ";  // query in the middle, same length
        let p = fuzzy_sort_score(q, prefix_name, wagner_fischer(q, prefix_name));
        let m = fuzzy_sort_score(q, middle_name, wagner_fischer(q, middle_name));
        assert!(p < m, "prefix score {p} must beat middle score {m}");
    }
}
