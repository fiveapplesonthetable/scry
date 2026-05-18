//! Flush decoded Kythe records into scry's mmap-packed precision
//! sidecars.
//!
//! Two output flavours, both backed by `scry_store::precision_packed`:
//!
//! * `clang_usrs.bin` — C/C++/ObjC (magic `SCRYUP01`). Fed by the
//!   `cxx_indexer` arm of the dispatcher; symbol strings are the
//!   Kythe-formatted vname (kept stable so the libclang-USR reader
//!   and the kzip-derived USR reader can use the same query path).
//! * `scip_index.bin` — Java / Kotlin (via JVM) / Go / proto /
//!   textproto (magic `SCRYSP01`). Symbol-string-keyed, matches the
//!   existing SCIP importer's reader.
//!
//! Both writers interleave via the wrapper macro in
//! `precision_packed.rs` — we only need to construct the typed
//! input record (`UsrRecord` / `ScipRecord`) and the symbol-table
//! `Vec<String>`, then call the writer.
//!
//! ## Aggregation
//!
//! Each per-CU decode call hands us records keyed by file path.
//! Same path may be touched by multiple CUs (kotlin srcjar shared
//! across compilations, for instance), so we dedup on
//! `(abs_path, byte_offset, symbol_id, role)` while accumulating —
//! the sidecar reader's `symbol_for_window` would happily return the
//! same record twice otherwise.

use crate::dispatch::IndexerKind;
use crate::entries::{DecodedRecord, Role};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One bucket of accumulated records — either Cxx-flavoured (writes
/// `clang_usrs.bin`) or SCIP-flavoured (writes `scip_index.bin`).
#[derive(Debug, Default)]
struct Bucket {
    symbol_table: Vec<String>,
    /// `symbol -> symbol_id`.
    sym_index: HashMap<String, u32>,
    /// `(path, offset, sym, role) -> ()` dedup.
    seen: std::collections::HashSet<(String, u32, u32, u8)>,
    /// Aggregated rows, ready to hand to the writer at finalize-time.
    rows: Vec<(String, u32, u32, u8)>,
}

impl Bucket {
    fn record(&mut self, path: &str, offset: u32, sym: &str, role: u8) {
        let sid = match self.sym_index.get(sym) {
            Some(&id) => id,
            None => {
                let id = self.symbol_table.len() as u32;
                self.symbol_table.push(sym.to_string());
                self.sym_index.insert(sym.to_string(), id);
                id
            }
        };
        let key = (path.to_string(), offset, sid, role);
        if self.seen.insert(key.clone()) {
            self.rows.push(key);
        }
    }
}

/// Routes per-CU `DecodedRecord`s into the right (cxx vs scip)
/// bucket. Thread-safe: indexer dispatch runs across rayon workers,
/// each calling `record_decoded` concurrently.
pub struct PackedEmitter {
    cxx: Mutex<Bucket>,
    scip: Mutex<Bucket>,
}

impl Default for PackedEmitter {
    fn default() -> Self { Self::new() }
}

impl PackedEmitter {
    pub fn new() -> Self {
        Self {
            cxx: Mutex::new(Bucket::default()),
            scip: Mutex::new(Bucket::default()),
        }
    }

    /// Push one decoded record. `source_kind` selects the sidecar
    /// — `Cxx` → clang_usrs.bin, everything-runnable-else →
    /// scip_index.bin. `Skip(_)` records are dropped (the dispatcher
    /// wouldn't have called us, but we belt-and-suspender it).
    pub fn record_decoded(&self, source_kind: IndexerKind, rec: &DecodedRecord) {
        let role_byte = match rec.role {
            Role::Decl => 0,
            Role::Ref  => 1,
            Role::Call => 2,
        };
        let bucket = match source_kind {
            IndexerKind::Cxx => &self.cxx,
            IndexerKind::Skip(_) => return,
            _ => &self.scip,
        };
        bucket.lock().unwrap().record(
            &rec.file_path, rec.start, &rec.target_symbol, role_byte,
        );
    }

    /// Flush both buckets to `out_dir`. Writes are atomic
    /// (tmp + rename) so concurrent readers never see a partial
    /// sidecar. Returns per-flavour record counts for the report.
    pub fn finalize(self, out_dir: &Path) -> Result<EmitReport> {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;

        let cxx = self.cxx.into_inner().unwrap();
        let scip = self.scip.into_inner().unwrap();

        let cxx_records = write_cxx(out_dir, &cxx)?;
        let scip_records = write_scip(out_dir, &scip)?;

        Ok(EmitReport {
            cxx_records,
            cxx_symbols: cxx.symbol_table.len(),
            scip_records,
            scip_symbols: scip.symbol_table.len(),
        })
    }
}

/// Counts after a `PackedEmitter::finalize`. Hands back to the
/// driver for the per-phase summary line.
#[derive(Debug, Clone, Copy)]
pub struct EmitReport {
    pub cxx_records: usize,
    pub cxx_symbols: usize,
    pub scip_records: usize,
    pub scip_symbols: usize,
}

fn write_cxx(out_dir: &Path, bucket: &Bucket) -> Result<usize> {
    use scry_store::clang_usrs::{self, UsrRecord};
    let out = out_dir.join("clang_usrs.bin");
    let tmp = tmp_path(&out);
    let recs: Vec<UsrRecord> = bucket.rows.iter().map(|(p, off, sid, k)| {
        UsrRecord {
            abs_path: p.clone(),
            byte_offset: *off,
            usr_id: *sid,
            kind: *k,
        }
    }).collect();
    clang_usrs::write(&tmp, &bucket.symbol_table, &recs)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))?;
    Ok(recs.len())
}

fn write_scip(out_dir: &Path, bucket: &Bucket) -> Result<usize> {
    use scry_store::scip_index::{self, ScipRecord};
    let out = out_dir.join("scip_index.bin");
    let tmp = tmp_path(&out);
    let recs: Vec<ScipRecord> = bucket.rows.iter().map(|(p, off, sid, k)| {
        ScipRecord {
            abs_path: p.clone(),
            byte_offset: *off,
            symbol_id: *sid,
            role: *k,
        }
    }).collect();
    scip_index::write(&tmp, &bucket.symbol_table, &recs)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))?;
    Ok(recs.len())
}

/// Tmp filename next to `dst`, used for atomic tmp+rename writes.
fn tmp_path(dst: &Path) -> PathBuf {
    dst.with_extension(format!(
        "{}.tmp",
        dst.extension().map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entries::DecodedRecord;

    fn dec(path: &str, start: u32, sym: &str, role: Role) -> DecodedRecord {
        DecodedRecord {
            file_path: path.to_string(),
            start,
            end: start + 1,
            target_symbol: sym.to_string(),
            role,
        }
    }

    #[test]
    fn deduplicates_identical_records() {
        let e = PackedEmitter::new();
        let r = dec("/x/A.cc", 100, "kythe:c++:foo", Role::Decl);
        e.record_decoded(IndexerKind::Cxx, &r);
        e.record_decoded(IndexerKind::Cxx, &r); // identical → drop
        let out = scry_bridge::scry_tmp_dir()
            .join(format!("scry-kzip-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let rep = e.finalize(&out).unwrap();
        assert_eq!(rep.cxx_records, 1);
        assert_eq!(rep.cxx_symbols, 1);
        // Cleanup.
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn routes_cxx_to_usr_and_else_to_scip() {
        let e = PackedEmitter::new();
        e.record_decoded(IndexerKind::Cxx, &dec("/A.cc", 10, "csym", Role::Decl));
        e.record_decoded(IndexerKind::Go,  &dec("/A.go", 20, "gsym", Role::Ref));
        e.record_decoded(IndexerKind::JavaSource,
                         &dec("/A.java", 30, "jsym", Role::Call));
        let out = scry_bridge::scry_tmp_dir()
            .join(format!("scry-kzip-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let rep = e.finalize(&out).unwrap();
        assert_eq!(rep.cxx_records, 1);
        assert_eq!(rep.scip_records, 2);
        // Sidecar files exist
        assert!(out.join("clang_usrs.bin").exists());
        assert!(out.join("scip_index.bin").exists());
        let _ = std::fs::remove_dir_all(&out);
    }
}
