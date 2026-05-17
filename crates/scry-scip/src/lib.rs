//! SCIP → scry sidecar importer.
//!
//! SCIP (https://github.com/sourcegraph/scip) is a portable, language-
//! agnostic code-graph format emitted by `scip-java`, `scip-kotlin`,
//! `rust-analyzer --output-scip`, `gopls scip`, `scip-typescript`,
//! `scip-python`, `lsif-clang`, and others.
//!
//! Path B (scry-clang) ships in-tree per-TU libclang precision for
//! C/C++/ObjC. Path C (this crate) takes pre-built SCIP indexes for
//! any language and projects them into the same sidecar shape so
//! `scry ref --scip-precise` can drop name-collision noise without
//! us needing per-language indexers.
//!
//! ## Pipeline
//!
//! 1. Read the SCIP `Index` protobuf from disk via the `scip` crate.
//! 2. Iterate `documents[]`, each carrying a relative path +
//!    `occurrences[]`. Each occurrence has a range, a `symbol`
//!    string (the SCIP-formatted identifier), and a role bitmap.
//! 3. For each occurrence we emit one `ScipRecord { abs_path,
//!    byte_offset, symbol_id, role }`. The symbol string is
//!    interned into a flat table to keep the sidecar compact.
//! 4. Byte offsets: SCIP's ranges are `[start_line, start_col,
//!    end_line, end_col]` zero-indexed. We compute `byte_offset`
//!    from the source file on disk (one read per document).
//!
//! ## Roles
//!
//! - `0` = reference (default; SCIP role bit 0)
//! - `1` = definition (SCIP role bit 1)
//! - `2` = write_access (bit 2)
//! - `3` = read_access (bit 3)
//!
//! Higher SCIP role bits (generated, test, etc.) are preserved verbatim
//! in the lower 8 bits so future query filters can read them.

use anyhow::{anyhow, Context, Result};
use protobuf::Message;
use scip::types::Index;
use scry_store::scip_index::{ScipRecord, ScipSidecar};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Build the `scip_index.bin` sidecar in `index_dir` from a SCIP
/// protobuf file at `scip_path`. `root_override`, if set, replaces
/// the SCIP index's `project_root` for path resolution — useful when
/// the SCIP file was generated in a different working tree (e.g.
/// CI vs local checkout).
///
/// Replace mode (calls [`import_scip_with_mode`] with `append=false`).
pub fn import_scip(
    scip_path: &Path,
    index_dir: &Path,
    root_override: Option<&Path>,
) -> Result<()> {
    import_scip_with_mode(scip_path, index_dir, root_override, false)
}

/// Like [`import_scip`] but with explicit append-vs-replace semantics.
///
/// `append=true`: load the existing `scip_index.bin` (if any) and
/// merge the new SCIP file into it. Symbol-table IDs are remapped so
/// the resulting bin's `symbol_table` is the union of both. Records
/// are deduped on `(abs_path, byte_offset, symbol_id)` so re-running
/// the same language pipeline doesn't bloat the sidecar.
///
/// `append=false` (default for [`import_scip`]): replace the existing
/// sidecar with the imported content. Use this when you've re-merged
/// every input from scratch (e.g. the JVM pipeline that always produces
/// one mega-SCIP from a fresh `scip-java index-semanticdb` pass).
///
/// Append mode is what makes per-language indexer composition work:
/// run scip-go, then rust-analyzer scip, then scip-python, etc. Each
/// imports into the same sidecar additively.
pub fn import_scip_with_mode(
    scip_path: &Path,
    index_dir: &Path,
    root_override: Option<&Path>,
    append: bool,
) -> Result<()> {
    let t = Instant::now();
    let raw = std::fs::read(scip_path)
        .with_context(|| format!("read {}", scip_path.display()))?;
    let index: Index = Index::parse_from_bytes(&raw)
        .with_context(|| format!("parse SCIP protobuf at {}", scip_path.display()))?;

    let project_root = root_override
        .map(PathBuf::from)
        .or_else(|| {
            let s = &index.metadata.as_ref()?.project_root;
            if s.is_empty() { return None; }
            // SCIP's project_root is a `file://...` URI per spec.
            let stripped = s.strip_prefix("file://").unwrap_or(s);
            Some(PathBuf::from(stripped))
        })
        .ok_or_else(|| anyhow!(
            "SCIP index has no project_root and --root not provided; \
             cannot resolve document paths"
        ))?;

    // Seed the in-memory tables from the existing sidecar when appending.
    // We re-key existing symbols by string so the rest of the loop can
    // intern new symbols against the same map without an O(N) walk.
    let mut symbol_table: Vec<String> = Vec::new();
    let mut symbol_id: HashMap<String, u32> = HashMap::new();
    let mut records: Vec<ScipRecord> = Vec::new();
    let mut docs_processed = 0u64;
    let mut docs_skipped = 0u64;
    let mut starting_records = 0usize;
    if append {
        let existing_path = index_dir.join("scip_index.bin");
        if existing_path.exists() {
            let raw = std::fs::read(&existing_path)
                .with_context(|| format!("read existing {}", existing_path.display()))?;
            let existing: ScipSidecar = bincode::deserialize(&raw)
                .with_context(|| format!("decode existing {}", existing_path.display()))?;
            for (i, s) in existing.symbol_table.iter().enumerate() {
                symbol_id.insert(s.clone(), i as u32);
            }
            starting_records = existing.records.len();
            symbol_table = existing.symbol_table;
            records = existing.records;
        }
    }
    // Set used to dedup `(abs_path, byte_offset, symbol_id)` across
    // the existing + incoming records. Only populated in append mode.
    let mut seen: std::collections::HashSet<(String, u32, u32)> = if append {
        records.iter()
            .map(|r| (r.abs_path.clone(), r.byte_offset, r.symbol_id))
            .collect()
    } else {
        Default::default()
    };

    for doc in &index.documents {
        let abs = project_root.join(&doc.relative_path);
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(_) => {
                docs_skipped += 1;
                continue;
            }
        };
        // Build the line-start offset table once per document; SCIP
        // is line/column-based and we want byte_offset for the
        // scry sidecar to line up with tree-sitter byte_start.
        let line_starts = compute_line_starts(&bytes);
        let abs_str = abs.to_string_lossy().into_owned();
        for occ in &doc.occurrences {
            let (line, col) = match occ.range.as_slice() {
                [l, c, ..] => (*l as u32, *c as u32),
                _ => continue,
            };
            let Some(line_start) = line_starts.get(line as usize) else { continue };
            // byte_offset = start of line + col (SCIP cols are byte
            // offsets within the line per spec ≥ 0.3.0).
            let byte_offset = line_start.saturating_add(col);
            let id = match symbol_id.get(&occ.symbol) {
                Some(&id) => id,
                None => {
                    let id = symbol_table.len() as u32;
                    symbol_table.push(occ.symbol.clone());
                    symbol_id.insert(occ.symbol.clone(), id);
                    id
                }
            };
            // SCIP's `symbol_roles` is a bitmap. We pack the low 8
            // bits verbatim so readers can match by role kind.
            let role: u8 = (occ.symbol_roles & 0xFF) as u8;
            // Dedup in append mode — re-imports of the same SCIP file
            // shouldn't grow the sidecar.
            if append && !seen.insert((abs_str.clone(), byte_offset, id)) {
                continue;
            }
            records.push(ScipRecord {
                abs_path: abs_str.clone(),
                byte_offset,
                symbol_id: id,
                role,
            });
        }
        docs_processed += 1;
    }

    let sidecar = ScipSidecar { version: 1, symbol_table, records };
    let out = index_dir.join("scip_index.bin");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let buf = bincode::serialize(&sidecar).context("serialize ScipSidecar")?;
    let tmp = out.with_extension("bin.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))?;
    let mode = if append { "append" } else { "replace" };
    let added = sidecar.records.len().saturating_sub(starting_records);
    eprintln!(
        "[scip-import {mode}] {} docs processed ({} skipped: file missing on disk), \
         +{} occurrences (total {}), {} unique symbols, {} bytes → {} ({}s)",
        docs_processed,
        docs_skipped,
        added,
        sidecar.records.len(),
        sidecar.symbol_table.len(),
        buf.len(),
        out.display(),
        t.elapsed().as_secs(),
    );
    Ok(())
}

/// Byte offset (from file start) of the first character of each line.
/// `line_starts[0] = 0`; `line_starts[i]` = byte after the `i-1`'th
/// newline. Used to translate SCIP's (line, col) to byte_offset.
fn compute_line_starts(bytes: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(bytes.len() / 40);
    out.push(0u32);
    let mut acc: u32 = 0;
    for &b in bytes {
        acc = acc.saturating_add(1);
        if b == b'\n' {
            out.push(acc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_starts_three_line_file() {
        // "ab\nc\n\nde" → lines start at 0, 3, 5, 6.
        let starts = compute_line_starts(b"ab\nc\n\nde");
        assert_eq!(starts, vec![0, 3, 5, 6]);
    }

    #[test]
    fn line_starts_empty_file() {
        let starts = compute_line_starts(b"");
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn line_starts_no_trailing_newline() {
        let starts = compute_line_starts(b"abc");
        assert_eq!(starts, vec![0]);
    }

    /// E2E: build a 1-document SCIP index in memory, write it,
    /// import it via `import_scip`, then read the sidecar back and
    /// assert the (path, byte_offset, symbol) round-trip survived.
    /// Exercises the whole pipeline (protobuf decode, line→byte
    /// offset, root resolution, schema serialization).
    #[test]
    fn end_to_end_synthetic_index() {
        use scip::types::{Document, Index, Metadata, Occurrence, ToolInfo};
        use scry_store::scip_index::ScipIndex;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("scry-scip-e2e-{nanos}"));
        let src = base.join("src");
        let idx = base.join("idx");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::create_dir_all(&idx).unwrap();
        // 3 lines: "package a.b;\n" (13), "public class Foo {}\n" (20), "" (0).
        // Line 1, col 13 → "Foo" identifier starts at offset 13 + 13 = 26.
        std::fs::write(
            src.join("a/b/Foo.java"),
            "package a.b;\npublic class Foo {}\n",
        ).unwrap();
        let scip_path = base.join("test.scip");

        let mut occ = Occurrence::new();
        occ.range = vec![1, 13, 1, 16]; // line 1, cols 13..16 = "Foo"
        occ.symbol = "scip-java . maven/com.example/test jvm 0.1 a/b/Foo#".to_string();
        occ.symbol_roles = 1; // Definition

        let mut doc = Document::new();
        doc.relative_path = "a/b/Foo.java".to_string();
        doc.occurrences = vec![occ];

        let mut meta = Metadata::new();
        let mut tool = ToolInfo::new();
        tool.name = "test-scip-emitter".to_string();
        tool.version = "0".to_string();
        meta.tool_info = protobuf::MessageField::some(tool);
        meta.project_root = format!("file://{}", src.display());

        let mut index_msg = Index::new();
        index_msg.metadata = protobuf::MessageField::some(meta);
        index_msg.documents = vec![doc];

        let bytes = index_msg.write_to_bytes().unwrap();
        std::fs::write(&scip_path, &bytes).unwrap();

        import_scip(&scip_path, &idx, None).unwrap();

        let opened = ScipIndex::open(&idx.join("scip_index.bin"))
            .unwrap().unwrap();
        assert_eq!(opened.symbol_count(), 1);
        assert_eq!(opened.len(), 1);
        let abs = src.join("a/b/Foo.java");
        let abs_str = abs.to_string_lossy();
        // Line 1 starts at byte 13 (after "package a.b;\n"); col 13 = byte 26.
        let expected = "scip-java . maven/com.example/test jvm 0.1 a/b/Foo#";
        assert_eq!(opened.symbol_for(&abs_str, 26), Some(expected));
        // Window lookup succeeds within range, fails outside.
        assert_eq!(opened.symbol_for_window(&abs_str, 24, 8), Some(expected));
        assert_eq!(opened.symbol_for_window(&abs_str, 200, 8), None);

        std::fs::remove_dir_all(&base).ok();
    }
}
