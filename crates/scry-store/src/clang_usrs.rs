//! Reader for the `clang_usrs.bin` sidecar produced by
//! `scry-clang-index`. Optional; missing → `ClangUsrIndex::open` returns
//! `Ok(None)`. Present → loads the full sidecar into memory (one alloc
//! for the USR table, one for the records) and exposes lookups by
//! `(abs_path, byte_offset)`.
//!
//! The sidecar wire format must match the writer in
//! `crates/scry-clang-index/src/main.rs` — both crates ship in the
//! same version, so the schema is locked per release.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// One symbol decl / ref / call site, keyed by absolute path +
/// byte offset within the file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UsrRecord {
    pub abs_path: String,
    pub byte_offset: u32,
    pub usr_id: u32,
    /// 0 = decl, 1 = ref, 2 = call.
    pub kind: u8,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct UsrSidecar {
    pub version: u32,
    pub usr_table: Vec<String>,
    pub records: Vec<UsrRecord>,
}

/// In-memory representation with a (path, offset) → record_idx index
/// for O(1) lookups.
#[derive(Debug)]
pub struct ClangUsrIndex {
    pub sidecar: UsrSidecar,
    /// (abs_path, byte_offset) → usr_id. One entry per record; if the
    /// same (path, offset) appears with multiple kinds (rare; e.g.
    /// definition-as-reference), the first record wins.
    by_loc: HashMap<(String, u32), u32>,
    /// abs_path → sorted Vec<(byte_offset, usr_id)>. Used by
    /// `usr_for_window` for fuzzy lookups when scry's byte_start
    /// doesn't exactly match clang's cursor offset (e.g. scry's
    /// struct identifier vs clang's struct keyword position).
    by_path: HashMap<String, Vec<(u32, u32)>>,
}

impl ClangUsrIndex {
    /// Open the sidecar at `<index_dir>/clang_usrs.bin`. Returns
    /// `Ok(None)` if the file is absent.
    pub fn open(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let buf = std::fs::read(path)
            .with_context(|| format!("read {}", path.display()))?;
        let sidecar: UsrSidecar = bincode::deserialize(&buf)
            .with_context(|| format!("decode {}", path.display()))?;
        if sidecar.version != 1 {
            anyhow::bail!(
                "{}: unsupported clang_usrs.bin version {} (this scry expects v1)",
                path.display(),
                sidecar.version,
            );
        }
        let mut by_loc: HashMap<(String, u32), u32> =
            HashMap::with_capacity(sidecar.records.len());
        let mut by_path: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        for r in &sidecar.records {
            by_loc
                .entry((r.abs_path.clone(), r.byte_offset))
                .or_insert(r.usr_id);
            by_path
                .entry(r.abs_path.clone())
                .or_default()
                .push((r.byte_offset, r.usr_id));
        }
        // Sort each per-path list by offset so usr_for_window can
        // binary-search.
        for v in by_path.values_mut() {
            v.sort_by_key(|x| x.0);
        }
        Ok(Some(Self { sidecar, by_loc, by_path }))
    }

    /// Look up the USR for a (path, offset) pair. Returns None if no
    /// clang record covers that exact site.
    pub fn usr_for(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
        self.by_loc
            .get(&(abs_path.to_string(), byte_offset))
            .and_then(|&id| self.sidecar.usr_table.get(id as usize).map(String::as_str))
    }

    /// Look up the USR for a site within ±`window` bytes of
    /// `byte_offset`. Use this when the query and clang's cursor
    /// disagree on whether to point at the keyword vs identifier
    /// (e.g. tree-sitter struct decl byte_start = identifier, clang
    /// CXCursor_StructDecl = keyword). Returns the USR of the
    /// CLOSEST record within the window, or None if none.
    pub fn usr_for_window(
        &self,
        abs_path: &str,
        byte_offset: u32,
        window: u32,
    ) -> Option<&str> {
        let entries = self.by_path.get(abs_path)?;
        let lo = byte_offset.saturating_sub(window);
        let hi = byte_offset.saturating_add(window);
        let start = entries.partition_point(|(o, _)| *o < lo);
        let mut best: Option<(u32, u32)> = None;
        for (o, id) in entries.iter().skip(start) {
            if *o > hi { break; }
            let d = o.abs_diff(byte_offset);
            if best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, *id));
            }
        }
        best.and_then(|(_, id)| {
            self.sidecar.usr_table.get(id as usize).map(String::as_str)
        })
    }

    pub fn len(&self) -> usize { self.sidecar.records.len() }
    pub fn is_empty(&self) -> bool { self.sidecar.records.is_empty() }
    pub fn usr_count(&self) -> usize { self.sidecar.usr_table.len() }

    /// Build a [`ByFileLookup`] indexed by scry's `FileEntry::id`, so
    /// per-ref lookups in a query loop become a `Vec::get(file_id)` +
    /// binary search instead of `HashMap<String, …>::get(&display_path)`
    /// per ref. `paths_by_file_id` must yield each `(file_id, abs_path)`
    /// pair at most once.
    ///
    /// Designed for `apply_precision_filter`: the caller walks
    /// `StoreReader::files` once, materialises `display_path` once per
    /// file, then drives 2k–100k+ per-ref lookups against the result.
    /// Eliminates both the per-ref `display_path` allocation and the
    /// per-ref HashMap hash+strcmp.
    pub fn precompute_by_file_ids<'a>(
        &'a self,
        paths_by_file_id: impl Iterator<Item = (u32, &'a str)>,
        file_count: usize,
    ) -> ByFileLookup<'a> {
        let mut entries: Vec<Option<&[(u32, u32)]>> = vec![None; file_count];
        for (fid, path) in paths_by_file_id {
            if (fid as usize) >= entries.len() { continue; }
            if let Some(v) = self.by_path.get(path) {
                entries[fid as usize] = Some(v.as_slice());
            }
        }
        ByFileLookup { entries, usr_table: &self.sidecar.usr_table }
    }
}

/// Per-query precomputed lookup keyed by `file_id`. Built once by
/// [`ClangUsrIndex::precompute_by_file_ids`] and consulted per ref
/// inside the precision filter. Borrows from the parent
/// `ClangUsrIndex` so no record / table data is cloned.
pub struct ByFileLookup<'a> {
    /// Indexed by scry's `FileEntry::id`. `None` means the file
    /// isn't in the clang sidecar (e.g. headers we didn't parse,
    /// non-C-family files).
    entries: Vec<Option<&'a [(u32, u32)]>>,
    usr_table: &'a [String],
}

impl<'a> ByFileLookup<'a> {
    /// Same window semantics as [`ClangUsrIndex::usr_for_window`].
    /// Returns `None` if no record falls inside `[byte_offset±window]`.
    pub fn usr_for_window(
        &self,
        file_id: u32,
        byte_offset: u32,
        window: u32,
    ) -> Option<&'a str> {
        let entries = (*self.entries.get(file_id as usize)?)?;
        let lo = byte_offset.saturating_sub(window);
        let hi = byte_offset.saturating_add(window);
        let start = entries.partition_point(|(o, _)| *o < lo);
        let mut best: Option<(u32, u32)> = None;
        for (o, id) in entries.iter().skip(start) {
            if *o > hi { break; }
            let d = o.abs_diff(byte_offset);
            if best.map_or(true, |(bd, _)| d < bd) {
                best = Some((d, *id));
            }
        }
        best.and_then(|(_, id)| self.usr_table.get(id as usize).map(String::as_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = UsrSidecar { version: 1, usr_table: vec![], records: vec![] };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let opened = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        assert_eq!(opened.len(), 0);
        assert_eq!(opened.usr_count(), 0);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn lookup_by_loc() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-lookup-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = UsrSidecar {
            version: 1,
            usr_table: vec!["c:@F@foo".to_string(), "c:@F@bar".to_string()],
            records: vec![
                UsrRecord { abs_path: "/x/y.cc".to_string(), byte_offset: 42, usr_id: 0, kind: 0 },
                UsrRecord { abs_path: "/x/z.cc".to_string(), byte_offset: 100, usr_id: 1, kind: 2 },
            ],
        };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let idx = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        assert_eq!(idx.usr_for("/x/y.cc", 42), Some("c:@F@foo"));
        assert_eq!(idx.usr_for("/x/z.cc", 100), Some("c:@F@bar"));
        assert_eq!(idx.usr_for("/x/y.cc", 99), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn missing_returns_none() {
        let nope = crate::scry_tmp_dir().join(format!(
            "scry-cusr-missing-{}", std::process::id(),
        ));
        let _ = std::fs::remove_file(&nope);
        assert!(ClangUsrIndex::open(&nope).unwrap().is_none());
    }

    #[test]
    fn window_lookup_finds_closest_within_range() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-win-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = UsrSidecar {
            version: 1,
            usr_table: vec!["c:@S@A".into(), "c:@S@B".into(), "c:@S@C".into()],
            records: vec![
                UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 100, usr_id: 0, kind: 0 },
                UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 110, usr_id: 1, kind: 0 },
                UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 500, usr_id: 2, kind: 0 },
            ],
        };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let idx = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        // Exact hits still work via window.
        assert_eq!(idx.usr_for_window("/x/a.cc", 100, 64), Some("c:@S@A"));
        // Within window: 105 is closer to 100 than 110.
        assert_eq!(idx.usr_for_window("/x/a.cc", 105, 64), Some("c:@S@A"));
        // 107 is closer to 110 than 100 (dist 3 vs 7).
        assert_eq!(idx.usr_for_window("/x/a.cc", 107, 64), Some("c:@S@B"));
        // Outside window from 500: nothing.
        assert_eq!(idx.usr_for_window("/x/a.cc", 200, 64), None);
        // Unknown path → None.
        assert_eq!(idx.usr_for_window("/x/none.cc", 100, 64), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn rejects_wrong_version() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-badv-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = UsrSidecar { version: 99, ..Default::default() };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let err = ClangUsrIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("v1"));
        std::fs::remove_file(&tmp).ok();
    }
}
