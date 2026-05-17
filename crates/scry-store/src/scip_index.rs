//! Reader for the `scip_index.bin` sidecar produced by
//! `scry scip-import`. Mirrors the shape of [`super::clang_usrs`] —
//! both wire SCIP-style symbol-identity into scry's query path, but
//! `scip_index` is fed by external SCIP tools (scip-java, gopls,
//! rust-analyzer, …) while `clang_usrs` is produced by the in-tree
//! `scry clang-index` helper for C/C++/ObjC.
//!
//! On-disk layout is owned by [`crate::precision_packed`]; this
//! module is a thin wrapper that supplies the SCIP-flavoured method
//! names (`symbol_for`, `symbol_for_window`, `symbol_count`, …).

use crate::precision_packed::{self, PrecisionPacked, Record};
use anyhow::Result;
use std::path::Path;

/// One occurrence (decl, ref, or write/read access) at an exact
/// source location. Writer-side carrier; the on-disk format interns
/// `symbol_table[symbol_id]` into a single blob.
#[derive(Debug, Clone)]
pub struct ScipRecord {
    /// Absolute path the SCIP indexer recorded (resolved against the
    /// SCIP file's `project_root` at import time).
    pub abs_path: String,
    /// Byte offset within the file. Computed at import from
    /// `(line, col)` by reading the source — matches tree-sitter's
    /// `byte_start` for occurrences on the same identifier.
    pub byte_offset: u32,
    /// Index into the writer's `symbol_table`.
    pub symbol_id: u32,
    /// Low 8 bits of SCIP's `symbol_roles` bitmap:
    /// `0x01` = Definition, `0x02` = Import, `0x04` = WriteAccess,
    /// `0x08` = ReadAccess, `0x10` = Generated, `0x20` = Test,
    /// `0x40` = ForwardDefinition. 0 = pure reference.
    pub role: u8,
}

/// Mmap'd SCIP sidecar. All accessors borrow from the mmap.
#[derive(Debug)]
pub struct ScipIndex {
    packed: PrecisionPacked,
}

impl ScipIndex {
    /// Open `scip_index.bin`; missing → `Ok(None)`.
    pub fn open(path: &Path) -> Result<Option<Self>> {
        Ok(PrecisionPacked::open(path, precision_packed::MAGIC_SCIP)?
            .map(|packed| Self { packed }))
    }

    pub fn symbol_for(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
        self.packed.symbol_at(abs_path, byte_offset)
    }

    /// Windowed lookup — same shape as
    /// [`super::clang_usrs::ClangUsrIndex::usr_for_window`]; see
    /// that doc for why the window is needed (parser cursor vs
    /// identifier-start alignment).
    pub fn symbol_for_window(
        &self,
        abs_path: &str,
        byte_offset: u32,
        window: u32,
    ) -> Option<&str> {
        self.packed.symbol_for_window(abs_path, byte_offset, window)
    }

    pub fn len(&self) -> usize { self.packed.record_count() }
    pub fn is_empty(&self) -> bool { self.packed.is_empty() }
    pub fn symbol_count(&self) -> usize { self.packed.symbol_count() }

    /// Iterate the interned SCIP symbol table (string at id 0, 1, …).
    /// Used by `scry sidecar-inspect` for sample output.
    pub fn iter_symbols(&self) -> impl Iterator<Item = &str> {
        (0..self.packed.symbol_count() as u32)
            .filter_map(|i| self.packed.symbol(i))
    }

    /// Stream every `(abs_path, byte_offset, symbol, role)` row in the
    /// sidecar. Used by `scry scip-import --append` to seed the
    /// rebuild with the existing rows. Path order matches on-disk
    /// (sorted by path_id then byte_offset), so callers get
    /// per-path runs together.
    pub fn iter_records(&self) -> impl Iterator<Item = (&str, u32, &str, u8)> {
        (0..self.packed.path_count() as u32).flat_map(move |pid| {
            let path = self.packed.path_of(pid).unwrap_or("");
            let (start, count) = self.packed.path_records_range(pid).unwrap_or((0, 0));
            (start..start + count).filter_map(move |rid| {
                let (bo, sid, role) = self.packed.record(rid)?;
                let sym = self.packed.symbol(sid)?;
                Some((path, bo, sym, role))
            })
        })
    }

    /// Sibling of [`super::clang_usrs::ClangUsrIndex::precompute_by_file_ids`].
    /// See that doc for rationale — same fast-path for the SCIP
    /// sidecar's lookup loop inside `apply_precision_filter`.
    pub fn precompute_by_file_ids<'a>(
        &'a self,
        paths_by_file_id: impl Iterator<Item = (u32, &'a str)>,
        file_count: usize,
    ) -> ByFileSymbolLookup<'a> {
        ByFileSymbolLookup {
            inner: self.packed.precompute_by_file_ids(paths_by_file_id, file_count),
        }
    }
}

/// Per-query precomputed lookup keyed by `file_id` for the SCIP
/// sidecar. Mirror of `clang_usrs::ByFileLookup`.
pub struct ByFileSymbolLookup<'a> {
    inner: precision_packed::ByFileLookup<'a>,
}

impl<'a> ByFileSymbolLookup<'a> {
    pub fn symbol_for_window(
        &self,
        file_id: u32,
        byte_offset: u32,
        window: u32,
    ) -> Option<&'a str> {
        self.inner.symbol_for_window(file_id, byte_offset, window)
    }
}

/// Write the sidecar in packed format. Writer side keeps owned
/// `String`s in `symbol_table` and indexes them by `symbol_id`; we
/// expand each record to a borrowed (path, symbol) pair for
/// [`precision_packed::write`] to intern.
pub fn write(path: &Path, symbol_table: &[String], records: &[ScipRecord]) -> Result<()> {
    let pp_records: Vec<Record<'_>> = records
        .iter()
        .map(|r| Record {
            abs_path: r.abs_path.as_str(),
            byte_offset: r.byte_offset,
            symbol: symbol_table
                .get(r.symbol_id as usize)
                .map(String::as_str)
                .unwrap_or(""),
            kind: r.role,
        })
        .collect();
    precision_packed::write(path, precision_packed::MAGIC_SCIP, &pp_records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write(&tmp, &[], &[]).unwrap();
        let opened = ScipIndex::open(&tmp).unwrap().unwrap();
        assert!(opened.is_empty());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn lookup_exact_and_window() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-look-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let symbol_table = vec![
            "scip-java . . jvm 0 com/Foo#".to_string(),
            "scip-java . . jvm 0 com/Bar#".to_string(),
        ];
        let records = vec![
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 100, symbol_id: 0, role: 1 },
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 120, symbol_id: 1, role: 0 },
        ];
        write(&tmp, &symbol_table, &records).unwrap();
        let idx = ScipIndex::open(&tmp).unwrap().unwrap();
        // Exact.
        assert_eq!(idx.symbol_for("/x/A.java", 100), Some("scip-java . . jvm 0 com/Foo#"));
        // Window prefers closer.
        assert_eq!(idx.symbol_for_window("/x/A.java", 105, 32), Some("scip-java . . jvm 0 com/Foo#"));
        assert_eq!(idx.symbol_for_window("/x/A.java", 118, 32), Some("scip-java . . jvm 0 com/Bar#"));
        // Out of window.
        assert_eq!(idx.symbol_for_window("/x/A.java", 200, 32), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn rejects_wrong_magic() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-badmagic-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        // Write a USR-magic sidecar, try to open it as SCIP → error.
        precision_packed::write(&tmp, precision_packed::MAGIC_CLANG_USR, &[]).unwrap();
        let err = ScipIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("bad magic"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn missing_returns_none() {
        let nope = crate::scry_tmp_dir().join(format!("scry-scip-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&nope);
        assert!(ScipIndex::open(&nope).unwrap().is_none());
    }

    #[test]
    fn iter_records_round_trips() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-iter-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let symbol_table = vec!["s1".into(), "s2".into(), "s3".into()];
        let records = vec![
            ScipRecord { abs_path: "/x/B.java".into(), byte_offset: 50, symbol_id: 1, role: 0 },
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 10, symbol_id: 0, role: 1 },
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 30, symbol_id: 2, role: 2 },
        ];
        write(&tmp, &symbol_table, &records).unwrap();
        let idx = ScipIndex::open(&tmp).unwrap().unwrap();
        let collected: Vec<_> = idx
            .iter_records()
            .map(|(p, bo, s, r)| (p.to_string(), bo, s.to_string(), r))
            .collect();
        assert_eq!(collected.len(), 3);
        // Verify every input row survived (order is by path_id then byte_offset;
        // writer interns in insertion order so /x/B.java has the lower path_id).
        let pairs: std::collections::HashSet<_> = collected
            .iter()
            .map(|(p, bo, s, r)| (p.as_str(), *bo, s.as_str(), *r))
            .collect();
        assert!(pairs.contains(&("/x/B.java", 50, "s2", 0)));
        assert!(pairs.contains(&("/x/A.java", 10, "s1", 1)));
        assert!(pairs.contains(&("/x/A.java", 30, "s3", 2)));
        std::fs::remove_file(&tmp).ok();
    }
}
