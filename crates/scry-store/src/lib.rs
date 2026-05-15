//! scry-store: on-disk index format for symbols, files, and roots.
//!
//! Phase 1: a simple bincode-serialized store plus an FST over symbol names
//! for prefix/fuzzy lookup. Phase 4 replaces this with a custom mmap'd
//! columnar layout; the public API stays.

use anyhow::{anyhow, Context, Result};
use scry_walker::{FileKind, Profile};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

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
}

impl StoreWriter {
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self {
            paths: StorePaths::new(root),
            roots: Vec::new(),
            files: Vec::new(),
            symbols: Vec::new(),
            refs: Vec::new(),
        }
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
