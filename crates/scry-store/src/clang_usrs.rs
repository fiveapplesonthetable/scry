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
        for r in &sidecar.records {
            by_loc
                .entry((r.abs_path.clone(), r.byte_offset))
                .or_insert(r.usr_id);
        }
        Ok(Some(Self { sidecar, by_loc }))
    }

    /// Look up the USR for a (path, offset) pair. Returns None if no
    /// clang record covers that exact site.
    pub fn usr_for(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
        self.by_loc
            .get(&(abs_path.to_string(), byte_offset))
            .and_then(|&id| self.sidecar.usr_table.get(id as usize).map(String::as_str))
    }

    pub fn len(&self) -> usize { self.sidecar.records.len() }
    pub fn is_empty(&self) -> bool { self.sidecar.records.is_empty() }
    pub fn usr_count(&self) -> usize { self.sidecar.usr_table.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = std::env::temp_dir().join(format!("scry-cusr-empty-{}", std::process::id()));
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
        let tmp = std::env::temp_dir().join(format!("scry-cusr-lookup-{}", std::process::id()));
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
        let nope = std::env::temp_dir().join(format!(
            "scry-cusr-missing-{}", std::process::id(),
        ));
        let _ = std::fs::remove_file(&nope);
        assert!(ClangUsrIndex::open(&nope).unwrap().is_none());
    }

    #[test]
    fn rejects_wrong_version() {
        let tmp = std::env::temp_dir().join(format!("scry-cusr-badv-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = UsrSidecar { version: 99, ..Default::default() };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let err = ClangUsrIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("v1"));
        std::fs::remove_file(&tmp).ok();
    }
}
