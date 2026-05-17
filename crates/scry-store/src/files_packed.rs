//! Mmap-backed sidecar that holds the file table for an index.
//! The whole on-disk layout is one tightly packed file with three
//! sections (header + fixed-size record table + concatenated
//! relpath blob), opened via [`safe_mmap`] so every accessor is
//! a couple of byte loads with no allocation.
//!
//! Replaces the bincode-encoded `Vec<FileEntry>` that earlier
//! versions of scry kept in RAM. On the 1M-file AOSP+Linux index
//! the decode of that vec dominated CLI cold-start (~325 ms,
//! mostly the 1M per-record `String` allocations bincode emits);
//! the packed sidecar drops cold open to <1 ms.
//!
//! On-disk layout (little-endian throughout):
//!
//! ```text
//! [ 0.. 8]  magic    = b"SCRYFP01"
//! [ 8..12]  version  = 1            u32
//! [12..16]  n_files                  u32
//! [16..24]  records_off              u64  (always 32 in v1)
//! [24..32]  paths_blob_off           u64
//! [32..32 + n*24]  records, 24 bytes each:
//!     [ 0.. 1] root_id               u8
//!     [ 1.. 2] kind_tag              u8  (stable enum mapping below)
//!     [ 2.. 4] reserved              [u8;2]
//!     [ 4.. 8] path_off              u32  (offset into paths_blob)
//!     [ 8..12] path_len              u32
//!     [12..20] size                  u64
//!     [20..24] reserved              [u8;4]
//! [paths_blob_off ..]  concatenated relpath bytes (no separators).
//! ```
//!
//! The reader exposes per-file random-access helpers that borrow
//! into the mmap — no allocation per call. Reader-side callers
//! that want a struct-shaped record use `StoreReader::file_view`,
//! which yields a `FileView<'_>` borrowed from the mmap; nothing
//! materialises a full `Vec` anymore.
//!
//! ## Stable kind mapping
//!
//! `FileKind` is `#[derive(Serialize, Deserialize)]`, so its
//! bincode discriminant is tied to declaration order — fragile for
//! a wire format. We pin a stable u8 mapping here. Adding a new
//! `FileKind` variant means appending a row to `KIND_TABLE`; never
//! reorder or delete rows or readers of older sidecars will
//! mis-attribute language tags. Unknown bytes (older readers
//! against newer writers) fall back to `None`.

use anyhow::{anyhow, Context, Result};
use memmap2::Mmap;
use scry_walker::FileKind;
use std::fs::File;
use std::io::Write;
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"SCRYFP01";
pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 32;
pub const RECORD_LEN: usize = 24;

const KIND_TABLE: &[(u8, FileKind)] = &[
    (0,  FileKind::C),
    (1,  FileKind::Cpp),
    (2,  FileKind::Header),
    (3,  FileKind::HeaderCpp),
    (4,  FileKind::Java),
    (5,  FileKind::Kotlin),
    (6,  FileKind::Rust),
    (7,  FileKind::Go),
    (8,  FileKind::Python),
    (9,  FileKind::Bash),
    (10, FileKind::TypeScript),
    (11, FileKind::Html),
    (12, FileKind::Css),
    (13, FileKind::Scss),
    (14, FileKind::Proto),
    (15, FileKind::Aidl),
    (16, FileKind::Hidl),
    (17, FileKind::Assembly),
    (18, FileKind::Soong),
    (19, FileKind::AndroidMk),
    (20, FileKind::Bazel),
    (21, FileKind::Bzl),
    (22, FileKind::CMake),
    (23, FileKind::Gn),
    (24, FileKind::Kconfig),
    (25, FileKind::Makefile),
    (26, FileKind::Gradle),
    (27, FileKind::FlagsFile),
    (28, FileKind::Jarjar),
    (29, FileKind::Toml),
    (30, FileKind::Aconfig),
    (31, FileKind::InitRc),
    (32, FileKind::Sepolicy),
    (33, FileKind::SepolicyOther),
    (34, FileKind::Manifest),
    (35, FileKind::ApiTxt),
    (36, FileKind::XmlOther),
    (37, FileKind::Json),
    (38, FileKind::Properties),
    (39, FileKind::Cfg),
    (40, FileKind::Yaml),
    (41, FileKind::Owners),
    (42, FileKind::Markdown),
    (43, FileKind::License),
];

pub fn kind_to_u8(k: FileKind) -> u8 {
    for &(tag, v) in KIND_TABLE {
        if v == k { return tag; }
    }
    // New variant we forgot to register — clamp to a sentinel so
    // the writer doesn't silently corrupt data. The reader treats
    // u8::MAX as "unknown" and falls back to Header (the most
    // ambiguous parser-input default), which is wrong but
    // legible — and a missing case here is a compile-time bug
    // we should fix by extending KIND_TABLE.
    u8::MAX
}

pub fn u8_to_kind(b: u8) -> Option<FileKind> {
    for &(tag, v) in KIND_TABLE {
        if tag == b { return Some(v); }
    }
    None
}

/// mmap-backed reader for `files_packed.bin`. Accessors return
/// `None` for out-of-range ids; callers should treat that the same
/// as an out-of-range `file_id`.
#[derive(Debug)]
pub struct FilesPacked {
    mmap: Mmap,
    n: u32,
    records_off: usize,
    paths_blob_off: usize,
}

impl FilesPacked {
    pub fn open(path: &Path) -> Result<Self> {
        let f = File::open(path)
            .with_context(|| format!("open {} for mmap", path.display()))?;
        // SAFETY: see module-level "Unsafe policy" in lib.rs. The
        // packed sidecar is written via tmp+rename by finalize, so
        // no concurrent truncation while live readers hold the mmap.
        let mmap = unsafe { Mmap::map(&f) }
            .with_context(|| format!("mmap {}", path.display()))?;
        if mmap.len() < HEADER_LEN {
            return Err(anyhow!("{}: too short for header", path.display()));
        }
        if &mmap[0..8] != MAGIC {
            return Err(anyhow!("{}: bad magic", path.display()));
        }
        let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(anyhow!(
                "{}: unsupported files_packed version {} (expected {})",
                path.display(), version, VERSION,
            ));
        }
        let n = u32::from_le_bytes(mmap[12..16].try_into().unwrap());
        let records_off = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;
        let paths_blob_off = u64::from_le_bytes(mmap[24..32].try_into().unwrap()) as usize;
        let need = records_off
            .checked_add((n as usize).checked_mul(RECORD_LEN).ok_or_else(||
                anyhow!("{}: n*RECORD_LEN overflow", path.display()))?)
            .ok_or_else(|| anyhow!("{}: records_off+n*RECORD_LEN overflow", path.display()))?;
        if mmap.len() < need || mmap.len() < paths_blob_off {
            return Err(anyhow!("{}: truncated body", path.display()));
        }
        Ok(Self { mmap, n, records_off, paths_blob_off })
    }

    pub fn len(&self) -> usize { self.n as usize }
    pub fn is_empty(&self) -> bool { self.n == 0 }

    #[inline]
    fn record(&self, id: u32) -> Option<&[u8]> {
        if id >= self.n { return None; }
        let base = self.records_off + (id as usize) * RECORD_LEN;
        self.mmap.get(base..base + RECORD_LEN)
    }

    pub fn root_id(&self, id: u32) -> Option<u8> {
        Some(self.record(id)?[0])
    }
    pub fn kind(&self, id: u32) -> Option<FileKind> {
        u8_to_kind(self.record(id)?[1])
    }
    pub fn size(&self, id: u32) -> Option<u64> {
        let r = self.record(id)?;
        Some(u64::from_le_bytes(r[12..20].try_into().ok()?))
    }
    pub fn relpath(&self, id: u32) -> Option<&str> {
        let r = self.record(id)?;
        let off = u32::from_le_bytes(r[4..8].try_into().ok()?) as usize;
        let len = u32::from_le_bytes(r[8..12].try_into().ok()?) as usize;
        let start = self.paths_blob_off.checked_add(off)?;
        let end = start.checked_add(len)?;
        let bytes = self.mmap.get(start..end)?;
        std::str::from_utf8(bytes).ok()
    }
}

/// Stream packed-files bytes to `path` from a borrowed-lifetime
/// iterator. Two passes: the first counts to size the record table
/// up front, the second emits records + blob. Both passes are
/// cheap (no allocation, no syscalls per file).
pub fn write_from_iter<'a>(
    path: &Path,
    entries: impl Iterator<Item = (u8, FileKind, &'a str, u64)> + Clone,
) -> Result<()> {
    let n: u32 = entries.clone().count().try_into()
        .map_err(|_| anyhow!("file count exceeds u32 (the packed sidecar uses u32 file ids)"))?;
    let records_off = HEADER_LEN as u64;
    let paths_blob_off = records_off + (n as u64) * (RECORD_LEN as u64);
    let mut f = std::io::BufWriter::with_capacity(
        1 << 20,
        File::create(path)
            .with_context(|| format!("create {}", path.display()))?,
    );
    // Header
    f.write_all(MAGIC)?;
    f.write_all(&VERSION.to_le_bytes())?;
    f.write_all(&n.to_le_bytes())?;
    f.write_all(&records_off.to_le_bytes())?;
    f.write_all(&paths_blob_off.to_le_bytes())?;
    // Record table — compute path_off as a running total.
    let mut path_off: u32 = 0;
    for (root_id, kind, relpath, size) in entries.clone() {
        let path_len: u32 = relpath.len().try_into()
            .map_err(|_| anyhow!("relpath > u32::MAX bytes"))?;
        let kind_tag = kind_to_u8(kind);
        // 24 bytes per record (see module header).
        let mut buf = [0u8; RECORD_LEN];
        buf[0] = root_id;
        buf[1] = kind_tag;
        buf[4..8].copy_from_slice(&path_off.to_le_bytes());
        buf[8..12].copy_from_slice(&path_len.to_le_bytes());
        buf[12..20].copy_from_slice(&size.to_le_bytes());
        f.write_all(&buf)?;
        path_off = path_off.checked_add(path_len)
            .ok_or_else(|| anyhow!("paths_blob size exceeds u32::MAX"))?;
    }
    // Paths blob.
    for (_, _, relpath, _) in entries {
        f.write_all(relpath.as_bytes())?;
    }
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-fp-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let entries: Vec<(u8, FileKind, &str, u64)> = vec![];
        write_from_iter(&tmp, entries.iter().cloned()).unwrap();
        let p = FilesPacked::open(&tmp).unwrap();
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn roundtrip_and_random_access() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-fp-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let entries: Vec<(u8, FileKind, &str, u64)> = vec![
            (0, FileKind::Cpp,    "frameworks/base/foo.cpp", 1234),
            (1, FileKind::Java,   "packages/x/y/Z.java",     5678),
            (0, FileKind::Header, "system/include/sys.h",    42),
        ];
        write_from_iter(&tmp, entries.iter().cloned()).unwrap();
        let p = FilesPacked::open(&tmp).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p.root_id(0), Some(0));
        assert_eq!(p.kind(0),    Some(FileKind::Cpp));
        assert_eq!(p.relpath(0), Some("frameworks/base/foo.cpp"));
        assert_eq!(p.size(0),    Some(1234));
        assert_eq!(p.root_id(1), Some(1));
        assert_eq!(p.kind(1),    Some(FileKind::Java));
        assert_eq!(p.relpath(1), Some("packages/x/y/Z.java"));
        assert_eq!(p.size(1),    Some(5678));
        assert_eq!(p.kind(2),    Some(FileKind::Header));
        assert_eq!(p.relpath(2), Some("system/include/sys.h"));
        // Out of range.
        assert_eq!(p.root_id(3), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-fp-bad-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        // Wrong magic.
        std::fs::write(&tmp, b"BADMAGIC\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
        assert!(FilesPacked::open(&tmp).is_err());
        // Wrong version.
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&99u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&(HEADER_LEN as u64).to_le_bytes());
        buf.extend_from_slice(&(HEADER_LEN as u64).to_le_bytes());
        std::fs::write(&tmp, &buf).unwrap();
        let err = FilesPacked::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("unsupported files_packed version"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn kind_table_round_trip_for_every_variant() {
        // Every (tag, kind) row must satisfy u8_to_kind(kind_to_u8(k)) == Some(k).
        // Catches the "added a variant, forgot KIND_TABLE row" footgun.
        for &(tag, kind) in KIND_TABLE {
            assert_eq!(kind_to_u8(kind), tag);
            assert_eq!(u8_to_kind(tag), Some(kind));
        }
    }
}
