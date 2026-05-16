//! scry-store: on-disk index format for symbols, files, and roots.
//!
//! Phase 1: a simple bincode-serialized store plus an FST over symbol names
//! for prefix/fuzzy lookup. Phase 4 replaces this with a custom mmap'd
//! columnar layout; the public API stays.

use anyhow::{anyhow, Context, Result};
use scry_walker::{FileKind, Profile};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub mod trigram;

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
        let mut n = std::mem::size_of::<Self>();
        n += self.name.capacity();
        n += self.scope_path.capacity() * std::mem::size_of::<String>();
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
    /// Cheap, deterministic estimate of how much RAM this record occupies
    /// when held in a `Vec<SymbolRecord>`. Used by the streaming indexer to
    /// decide when to flush a chunk to disk WITHOUT polling /proc/self/status
    /// (which lags real allocation by 100s of ms and counts shared pages).
    pub fn estimated_bytes(&self) -> usize {
        // fixed struct fields + String capacities + Vec<String> contents
        let mut n = std::mem::size_of::<Self>();
        n += self.name.capacity();
        if let Some(s) = self.fqn.as_ref() { n += s.capacity(); }
        n += self.scope_path.capacity() * std::mem::size_of::<String>();
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
    /// `<index>.tmp/`). When `None`, the writer is in legacy all-RAM mode and
    /// callers should invoke `finalize`. When `Some`, callers can invoke
    /// `flush_symbols_chunk`/`flush_refs_chunk` and finish via
    /// `finalize_streaming`.
    pub tmp_dir: Option<PathBuf>,
    /// Number of symbol chunks already flushed.
    pub symbol_chunk_count: u32,
    pub ref_chunk_count: u32,
    /// Per-chunk totals so we can stamp the final `Vec<T>` length without
    /// re-reading the chunks first.
    pub symbol_chunk_lens: Vec<u64>,
    pub ref_chunk_lens: Vec<u64>,
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
                let mut f = std::fs::File::open(&p)
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
        eprintln!(
            "[resume] reopening {} (existing chunks: {} symbol, {} ref)",
            tmp.display(), sym_chunks, ref_chunks,
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
        for r in self.refs.iter_mut() {
            let cands = match by_name.get(&r.name) {
                Some(c) if !c.is_empty() => c,
                _ => continue,
            };
            // Prefer same-lang match; otherwise unique; otherwise first.
            let chosen = cands.iter()
                .find(|&&idx| self.symbols[idx as usize].lang == r.lang)
                .copied()
                .or_else(|| if cands.len() == 1 { Some(cands[0]) } else { Some(cands[0]) });
            if let Some(idx) = chosen {
                r.resolved_to = Some(self.symbols[idx as usize].id);
            }
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

        let final_dir = self.paths.root.clone();
        let tmp = self
            .tmp_dir
            .clone()
            .ok_or_else(|| anyhow!("finalize_streaming requires streaming mode"))?;
        let tmp_paths = StorePaths::new(tmp.clone());

        write_bincode(&tmp_paths.roots(), &self.roots)?;
        write_bincode(&tmp_paths.files(), &self.files)?;

        // -- symbols.bin: concatenate chunks into a single bincode Vec<SymbolRecord> --
        let total_syms: u64 = self.symbol_chunk_lens.iter().sum();
        {
            let mut w = BufWriter::new(File::create(tmp_paths.symbols())?);
            // bincode 1.3 with default config encodes Vec<T> as u64-LE length
            // followed by each element. We stamp the length, then stream each
            // chunk's records back out one by one without rebuilding a Vec.
            w.write_all(&total_syms.to_le_bytes())?;
            for n in 0..self.symbol_chunk_count {
                let p = Self::chunk_path(&tmp, "symbols", n);
                let chunk: Vec<SymbolRecord> = read_bincode(&p)?;
                for s in &chunk {
                    bincode::serialize_into(&mut w, s)
                        .with_context(|| "stream symbol")?;
                }
            }
            w.flush()?;
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

        // -- refs.bin + ref_names.fst + ref_postings.bin --
        let total_refs: u64 = self.ref_chunk_lens.iter().sum();
        {
            let mut w = BufWriter::new(File::create(tmp_paths.refs())?);
            w.write_all(&total_refs.to_le_bytes())?;
            for n in 0..self.ref_chunk_count {
                let p = Self::chunk_path(&tmp, "refs", n);
                let chunk: Vec<RefRecord> = read_bincode(&p)?;
                for r in &chunk {
                    bincode::serialize_into(&mut w, r)
                        .with_context(|| "stream ref")?;
                }
            }
            w.flush()?;
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

        let manifest = Manifest {
            version: 1,
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
            version: 1,
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

/// Like `build_name_fst` but takes a pre-built (already-sorted via BTreeMap)
/// name -> indices map. Used by the streaming finalize so we don't have to
/// hold the raw records in RAM while emitting postings.
fn write_postings_and_fst(
    by_name: &BTreeMap<String, Vec<u32>>,
    fst_path: &Path,
    postings_path: &Path,
) -> Result<()> {
    let mut postings = BufWriter::new(File::create(postings_path)?);
    let mut pos: u64 = 0;
    let mut offsets: Vec<(&str, u64)> = Vec::with_capacity(by_name.len());
    for (name, idxs) in by_name.iter() {
        offsets.push((name.as_str(), pos));
        postings.write_all(&(idxs.len() as u32).to_le_bytes())?;
        pos += 4;
        for i in idxs {
            postings.write_all(&i.to_le_bytes())?;
            pos += 4;
        }
    }
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
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let hms = secs % 86_400;
    let (h, m, s) = (hms / 3600, (hms / 60) % 60, hms % 60);
    format!("{secs}-unixT{h:02}:{m:02}:{s:02}Z")
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub struct StoreReader {
    pub paths: StorePaths,
    pub manifest: Manifest,
    pub roots: Vec<RootEntry>,
    pub files: Vec<FileEntry>,
    pub symbols: Vec<SymbolRecord>,
    pub refs: Vec<RefRecord>,
    pub fst: fst::Map<memmap2::Mmap>,
    pub postings_mmap: memmap2::Mmap,
    pub ref_fst: fst::Map<memmap2::Mmap>,
    pub ref_postings_mmap: memmap2::Mmap,
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
        let symbols: Vec<SymbolRecord> = read_bincode(&paths.symbols())?;
        // refs.bin may be absent for old indexes
        let refs: Vec<RefRecord> = if paths.refs().exists() {
            read_bincode(&paths.refs())?
        } else {
            Vec::new()
        };
        let fst_file = File::open(paths.names_fst())?;
        let fst_mmap = unsafe { memmap2::Mmap::map(&fst_file)? };
        let fst = fst::Map::new(fst_mmap)?;
        let postings_file = File::open(paths.name_postings())?;
        let postings_mmap = unsafe { memmap2::Mmap::map(&postings_file)? };
        // Refs map is always written during finalize (even if empty). For
        // backwards compatibility with pre-Phase-2 indexes that don't have
        // it, callers should re-index.
        let rf = File::open(paths.ref_names_fst())
            .with_context(|| format!("open {} (re-run \"scry index\" if missing)", paths.ref_names_fst().display()))?;
        let ref_fst_mmap = unsafe { memmap2::Mmap::map(&rf)? };
        let ref_fst = fst::Map::new(ref_fst_mmap)?;
        let pf = File::open(paths.ref_postings())?;
        let ref_postings_mmap = unsafe { memmap2::Mmap::map(&pf)? };
        Ok(Self {
            paths, manifest, roots, files, symbols, refs,
            fst, postings_mmap, ref_fst, ref_postings_mmap,
        })
    }

    pub fn lookup_refs_exact(&self, name: &str) -> Vec<&RefRecord> {
        let off = match self.ref_fst.get(name.as_bytes()) {
            Some(v) => v,
            None => return Vec::new(),
        };
        read_posting(&self.ref_postings_mmap, off)
            .into_iter()
            .filter_map(|i| self.refs.get(i as usize))
            .collect()
    }

    pub fn lookup_exact(&self, name: &str) -> Vec<&SymbolRecord> {
        let off = match self.fst.get(name.as_bytes()) {
            Some(v) => v,
            None => return Vec::new(),
        };
        self.read_posting(off).into_iter()
            .filter_map(|i| self.symbols.get(i as usize))
            .collect()
    }

    pub fn lookup_prefix(&self, prefix: &str, limit: usize) -> Vec<&SymbolRecord> {
        use fst::IntoStreamer;
        use fst::Streamer;
        let mut out: Vec<&SymbolRecord> = Vec::new();
        let mut stream = self.fst.range().ge(prefix.as_bytes()).into_stream();
        while let Some((key, off)) = stream.next() {
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            for i in self.read_posting(off) {
                if let Some(s) = self.symbols.get(i as usize) {
                    out.push(s);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    pub fn lookup_substring(&self, substr: &str, limit: usize) -> Vec<&SymbolRecord> {
        use fst::Streamer;
        let mut out: Vec<&SymbolRecord> = Vec::new();
        let needle = substr.as_bytes();
        let mut stream = self.fst.stream();
        while let Some((key, off)) = stream.next() {
            if memchr::memmem::find(key, needle).is_some() {
                for i in self.read_posting(off) {
                    if let Some(s) = self.symbols.get(i as usize) {
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

    fn read_posting(&self, off: u64) -> Vec<u32> {
        read_posting(&self.postings_mmap, off)
    }
}

fn read_posting(buf: &[u8], off: u64) -> Vec<u32> {
    let start = off as usize;
    if start + 4 > buf.len() { return Vec::new(); }
    let count = u32::from_le_bytes(buf[start..start + 4].try_into().unwrap()) as usize;
    let mut v = Vec::with_capacity(count);
    let mut p = start + 4;
    for _ in 0..count {
        if p + 4 > buf.len() { break; }
        v.push(u32::from_le_bytes(buf[p..p + 4].try_into().unwrap()));
        p += 4;
    }
    v
}


fn read_bincode<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let f = BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    bincode::deserialize_from(f)
        .map_err(|e| anyhow!("decode {}: {e}", path.display()))
}
