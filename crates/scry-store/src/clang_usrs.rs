//! Reader for the `clang_usrs.bin` sidecar produced by
//! `scry-clang-index`. Optional; missing → `ClangUsrIndex::open` returns
//! `Ok(None)`. Present → mmaps the packed sidecar (see
//! [`crate::precision_packed`] for the on-disk layout) and exposes
//! `(abs_path, byte_offset)` lookups.
//!
//! The on-disk shape is owned by `precision_packed`; this module only
//! adds the USR-flavoured public API names (`usr_for`,
//! `usr_for_window`, `usr_count`, …) so callers keep their
//! C/C++/ObjC-specific naming.

use crate::precision_packed::{self, PrecisionPacked, Record};
use anyhow::Result;
use std::path::Path;

/// One symbol decl / ref / call site, keyed by absolute path +
/// byte offset within the file. Writer-side carrier — the on-disk
/// shape interns `usr_table[usr_id]` into a single symbol blob.
#[derive(Debug, Clone)]
pub struct UsrRecord {
    pub abs_path: String,
    pub byte_offset: u32,
    pub usr_id: u32,
    /// 0 = decl, 1 = ref, 2 = call.
    pub kind: u8,
}

/// Mmap'd USR sidecar. All accessors borrow from the mmap, so per-
/// query cost is the algorithmic cost only.
#[derive(Debug)]
pub struct ClangUsrIndex {
    packed: PrecisionPacked,
}

impl ClangUsrIndex {
    /// Open the sidecar at `<index_dir>/clang_usrs.bin`. Returns
    /// `Ok(None)` if the file is absent.
    pub fn open(path: &Path) -> Result<Option<Self>> {
        Ok(PrecisionPacked::open(path, precision_packed::MAGIC_CLANG_USR)?
            .map(|packed| Self { packed }))
    }

    /// Look up the USR for a (path, offset) pair. Returns None if no
    /// clang record covers that exact site.
    pub fn usr_for(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
        self.packed.symbol_at(abs_path, byte_offset)
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
        self.packed.symbol_for_window(abs_path, byte_offset, window)
    }

    pub fn len(&self) -> usize { self.packed.record_count() }
    pub fn is_empty(&self) -> bool { self.packed.is_empty() }
    pub fn usr_count(&self) -> usize { self.packed.symbol_count() }

    /// Iterate the interned USR table (string at id 0, 1, …). Used by
    /// `scry sidecar-inspect` to show a sample.
    pub fn iter_usrs(&self) -> impl Iterator<Item = &str> {
        (0..self.packed.symbol_count() as u32)
            .filter_map(|i| self.packed.symbol(i))
    }

    /// Build a [`ByFileLookup`] indexed by scry's `FileEntry::id`, so
    /// per-ref lookups in a query loop become a `Vec::get(file_id)` +
    /// binary search instead of `HashMap<String, …>::get(&display_path)`
    /// per ref. `paths_by_file_id` must yield each `(file_id, abs_path)`
    /// pair at most once.
    ///
    /// Designed for `apply_precision_filter`: the caller walks
    /// `StoreReader::files` once, materialises `display_path` once per
    /// file, then drives 2k–100k+ per-ref lookups against the result.
    pub fn precompute_by_file_ids<'a>(
        &'a self,
        paths_by_file_id: impl Iterator<Item = (u32, &'a str)>,
        file_count: usize,
    ) -> ByFileLookup<'a> {
        ByFileLookup {
            inner: self.packed.precompute_by_file_ids(paths_by_file_id, file_count),
        }
    }
}

/// Per-query precomputed lookup keyed by `file_id`. Built once by
/// [`ClangUsrIndex::precompute_by_file_ids`] and consulted per ref
/// inside the precision filter. Borrows from the parent
/// `ClangUsrIndex` so no record / table data is cloned.
pub struct ByFileLookup<'a> {
    inner: precision_packed::ByFileLookup<'a>,
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
        self.inner.symbol_for_window(file_id, byte_offset, window)
    }
}

/// Write the sidecar in packed format. The writer side keeps owned
/// `String`s in `usr_table` and indexes them by `usr_id`; we expand
/// each record to a borrowed (path, symbol) pair for
/// [`precision_packed::write`] to intern.
pub fn write(path: &Path, usr_table: &[String], records: &[UsrRecord]) -> Result<()> {
    let pp_records: Vec<Record<'_>> = records
        .iter()
        .map(|r| Record {
            abs_path: r.abs_path.as_str(),
            byte_offset: r.byte_offset,
            symbol: usr_table
                .get(r.usr_id as usize)
                .map(String::as_str)
                .unwrap_or(""),
            kind: r.kind,
        })
        .collect();
    precision_packed::write(path, precision_packed::MAGIC_CLANG_USR, &pp_records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write(&tmp, &[], &[]).unwrap();
        let opened = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        assert_eq!(opened.len(), 0);
        assert_eq!(opened.usr_count(), 0);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn lookup_by_loc() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-lookup-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let usr_table = vec!["c:@F@foo".to_string(), "c:@F@bar".to_string()];
        let records = vec![
            UsrRecord { abs_path: "/x/y.cc".to_string(), byte_offset: 42, usr_id: 0, kind: 0 },
            UsrRecord { abs_path: "/x/z.cc".to_string(), byte_offset: 100, usr_id: 1, kind: 2 },
        ];
        write(&tmp, &usr_table, &records).unwrap();
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
        let usr_table = vec!["c:@S@A".into(), "c:@S@B".into(), "c:@S@C".into()];
        let records = vec![
            UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 100, usr_id: 0, kind: 0 },
            UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 110, usr_id: 1, kind: 0 },
            UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 500, usr_id: 2, kind: 0 },
        ];
        write(&tmp, &usr_table, &records).unwrap();
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
    fn rejects_wrong_magic() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-badmagic-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        // Write a SCIP-magic sidecar, try to open it as a USR sidecar → error.
        precision_packed::write(&tmp, precision_packed::MAGIC_SCIP, &[]).unwrap();
        let err = ClangUsrIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("bad magic"));
        std::fs::remove_file(&tmp).ok();
    }
}
