//! Reader for the `scip_index.bin` sidecar produced by
//! `scry scip-import`. Mirrors the shape of [`super::clang_usrs`] —
//! both wire SCIP-style symbol-identity into scry's query path, but
//! `scip_index` is fed by external SCIP tools (scip-java, gopls,
//! rust-analyzer, …) while `clang_usrs` is produced by the in-tree
//! `scry clang-index` helper for C/C++/ObjC.
//!
//! The on-wire schema must match the writer in `scry-scip`; both ship
//! in the same release so the version field is the only knob.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// One occurrence (decl, ref, or write/read access) at an exact
/// source location.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScipRecord {
    /// Absolute path the SCIP indexer recorded (resolved against the
    /// SCIP file's `project_root` at import time).
    pub abs_path: String,
    /// Byte offset within the file. Computed at import from
    /// `(line, col)` by reading the source — matches tree-sitter's
    /// `byte_start` for occurrences on the same identifier.
    pub byte_offset: u32,
    /// Index into [`ScipSidecar::symbol_table`].
    pub symbol_id: u32,
    /// Low 8 bits of SCIP's `symbol_roles` bitmap:
    /// `0x01` = Definition, `0x02` = Import, `0x04` = WriteAccess,
    /// `0x08` = ReadAccess, `0x10` = Generated, `0x20` = Test,
    /// `0x40` = ForwardDefinition. 0 = pure reference.
    pub role: u8,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ScipSidecar {
    pub version: u32,
    /// Interned SCIP symbol strings (the structured language-agnostic
    /// IDs like `scip-java . maven/x/y jvm 1 com/example/Foo#bar().`).
    pub symbol_table: Vec<String>,
    pub records: Vec<ScipRecord>,
}

/// In-memory index over a SCIP sidecar. Exact + windowed lookups
/// mirror [`super::clang_usrs::ClangUsrIndex`] so the query side
/// can compose both filters uniformly.
#[derive(Debug)]
pub struct ScipIndex {
    pub sidecar: ScipSidecar,
    /// `(abs_path, byte_offset)` → `symbol_id`. First record wins.
    by_loc: HashMap<(String, u32), u32>,
    /// `abs_path` → sorted `Vec<(byte_offset, symbol_id)>` for
    /// `symbol_for_window`.
    by_path: HashMap<String, Vec<(u32, u32)>>,
}

impl ScipIndex {
    /// Open `scip_index.bin`; missing → `Ok(None)`.
    pub fn open(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let buf = std::fs::read(path)
            .with_context(|| format!("read {}", path.display()))?;
        let sidecar: ScipSidecar = bincode::deserialize(&buf)
            .with_context(|| format!("decode {}", path.display()))?;
        if sidecar.version != 1 {
            anyhow::bail!(
                "{}: unsupported scip_index.bin version {} (this scry expects v1)",
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
                .or_insert(r.symbol_id);
            by_path
                .entry(r.abs_path.clone())
                .or_default()
                .push((r.byte_offset, r.symbol_id));
        }
        for v in by_path.values_mut() {
            v.sort_by_key(|x| x.0);
        }
        Ok(Some(Self { sidecar, by_loc, by_path }))
    }

    pub fn symbol_for(&self, abs_path: &str, byte_offset: u32) -> Option<&str> {
        self.by_loc
            .get(&(abs_path.to_string(), byte_offset))
            .and_then(|&id| self.sidecar.symbol_table.get(id as usize).map(String::as_str))
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
            self.sidecar.symbol_table.get(id as usize).map(String::as_str)
        })
    }

    pub fn len(&self) -> usize { self.sidecar.records.len() }
    pub fn is_empty(&self) -> bool { self.sidecar.records.is_empty() }
    pub fn symbol_count(&self) -> usize { self.sidecar.symbol_table.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty_and_versioned() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = ScipSidecar { version: 1, symbol_table: vec![], records: vec![] };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let opened = ScipIndex::open(&tmp).unwrap().unwrap();
        assert!(opened.is_empty());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn lookup_exact_and_window() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-look-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = ScipSidecar {
            version: 1,
            symbol_table: vec!["scip-java . . jvm 0 com/Foo#".into(),
                               "scip-java . . jvm 0 com/Bar#".into()],
            records: vec![
                ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 100, symbol_id: 0, role: 1 },
                ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 120, symbol_id: 1, role: 0 },
            ],
        };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
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
    fn rejects_wrong_version() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-badv-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let s = ScipSidecar { version: 99, ..Default::default() };
        std::fs::write(&tmp, bincode::serialize(&s).unwrap()).unwrap();
        let err = ScipIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("v1"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn missing_returns_none() {
        let nope = crate::scry_tmp_dir().join(format!("scry-scip-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&nope);
        assert!(ScipIndex::open(&nope).unwrap().is_none());
    }
}
