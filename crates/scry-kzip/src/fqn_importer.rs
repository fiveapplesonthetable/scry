//! Phase 5: FQN-canonical cross-CU sidecar importer.
//!
//! Reads the per-CU raw entry files tee'd by phase 3 (under
//! `<staging>/entries/cu-*.entries`), walks them twice in pure
//! streaming fashion, and emits one packed sidecar
//! `scip_index_fqn.bin` whose records are keyed on canonical JVM
//! FQN strings (e.g. `kythe:jvm:<corpus>##android.os.Binder.clearCallingIdentity()J`).
//!
//! Pipeline:
//! 1. **Pass 1 — named-edge collection.** Stream every entry; whenever
//!    a `/kythe/edge/named` edge points at a language=jvm VName,
//!    record `(source_vname.symbol_string -> target_vname.symbol_string)`
//!    in a deduplicated in-memory map. The map size is bounded by the
//!    corpus's unique entity count (~1-5M on full AOSP, ~200 MB-1 GB
//!    of strings); we emit a soft warning if it exceeds a configurable
//!    cap so the operator can detect memory pressure ahead of OOM.
//! 2. **Pass 2 — anchor emit.** Stream every entry a second time;
//!    reuse [`crate::entries::decode_stream`] to fold anchor facts +
//!    edges into [`crate::entries::DecodedRecord`] per file. For each
//!    record whose `target_symbol` appears in the named-map, emit a
//!    [`scry_store::precision_packed::Record`] with `symbol = jvm_fqn`
//!    (canonical) instead of the per-CU opaque hash. Records without
//!    a bridge are skipped — they're already captured by the per-CU
//!    `scip_index.bin` from phase 4 and don't need the cross-CU sidecar.
//!
//! The two-pass design keeps peak per-CU memory bounded to the decode
//! accumulator + the named-map. We never load the full corpus's
//! anchor records into RAM at once during pass 1, and pass 2's
//! per-CU records are released after each file finishes streaming.
//! Final assembly into the packed sidecar IS bounded by the total
//! cross-CU record count — that's the structural ceiling, and we
//! document it as the L7 followup if it ever bites.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::entries::{
    decode_stream, walk_entries, DecodedRecord, Role, VName,
};
use scry_store::precision_packed::{self, MAGIC_SCIP};

/// Stats reported after a successful import.
#[derive(Debug, Clone)]
pub struct FqnImportReport {
    /// Number of `.entries` files walked.
    pub entry_files: usize,
    /// Distinct (source-VName → JVM-FQN) bridge entries collected
    /// in pass 1 (post-dedup).
    pub named_bridges: usize,
    /// Anchor records emitted to the canonical sidecar (only those
    /// whose target VName was in the bridge map).
    pub canonical_records: usize,
    /// Anchor records seen in pass 2 but skipped because no bridge
    /// existed for their target VName.
    pub skipped_no_bridge: usize,
    /// Distinct JVM FQNs that ended up as `symbol` strings in the
    /// canonical sidecar. Sanity-check: roughly the number of
    /// resolvable cross-CU entities.
    pub distinct_fqns: usize,
    /// Total wall time.
    pub elapsed_secs: f64,
    /// Where the sidecar was written.
    pub sidecar_path: PathBuf,
}

/// Build the FQN-canonical packed sidecar at
/// `<out_dir>/scip_index_fqn.bin`. Walks every `cu-*.entries` file
/// under `entries_dir` in two streaming passes.
pub fn build_fqn_sidecar(
    entries_dir: &Path,
    out_dir: &Path,
) -> Result<FqnImportReport> {
    let t = Instant::now();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("mkdir {}", out_dir.display()))?;

    let entry_files = collect_entry_files(entries_dir)
        .with_context(|| format!("scan {}", entries_dir.display()))?;
    eprintln!(
        "[scry-kzip] phase 5/6: FQN importer — {} per-CU entry files",
        entry_files.len(),
    );
    if entry_files.is_empty() {
        anyhow::bail!(
            "no *.entries files under {} — phase 3 didn't tee anything; \
             confirm SCRY_KZIP_SERVING_DIR was set",
            entries_dir.display(),
        );
    }

    // Pass 1: build (source_vname.symbol -> jvm_fqn) bridge map.
    let bridges = collect_named_bridges(&entry_files)
        .context("pass 1 (named-edge collection)")?;
    eprintln!(
        "[scry-kzip] phase 5/6: pass 1 done — {} named-edge bridges",
        bridges.len(),
    );

    // Pass 2: emit canonical records.
    let (records, distinct_fqns, skipped) = emit_canonical_records(&entry_files, &bridges)
        .context("pass 2 (canonical record emit)")?;

    // Write the packed sidecar. The Record<'a> takes string refs into
    // the owned `records` Vec.
    let sidecar_path = out_dir.join("scip_index_fqn.bin");
    let pp_records: Vec<precision_packed::Record<'_>> = records.iter()
        .map(|r| precision_packed::Record {
            abs_path: &r.path,
            byte_offset: r.byte_offset,
            symbol: &r.symbol,
            kind: r.kind,
        })
        .collect();
    precision_packed::write(&sidecar_path, MAGIC_SCIP, &pp_records)
        .with_context(|| format!("write {}", sidecar_path.display()))?;

    let report = FqnImportReport {
        entry_files: entry_files.len(),
        named_bridges: bridges.len(),
        canonical_records: records.len(),
        skipped_no_bridge: skipped,
        distinct_fqns,
        elapsed_secs: t.elapsed().as_secs_f64(),
        sidecar_path,
    };
    eprintln!(
        "[scry-kzip] phase 5/6: pass 2 done — {} canonical records ({} distinct FQNs), \
         {} skipped (no bridge) in {:.1}s",
        report.canonical_records, report.distinct_fqns, report.skipped_no_bridge,
        report.elapsed_secs,
    );
    Ok(report)
}

/// Owned record we accumulate during pass 2 before handing to
/// `precision_packed::write`. Owned strings (not `&str`) because the
/// VName-derived symbol strings outlive the per-CU decode batch.
struct OwnedRecord {
    path: String,
    byte_offset: u32,
    symbol: String,
    kind: u8,
}

/// Soft warning threshold for the named-bridge map. Picked at 200 MB
/// of estimated retained string memory (~1 M entries × 200 B average).
/// Above this, the importer prints a one-line warning so the operator
/// notices before the host swaps.
const NAMED_BRIDGE_WARN_ENTRIES: usize = 1_000_000;

/// Pass 1: walk every entry file, collect dedup'd named bridges.
fn collect_named_bridges(entry_files: &[PathBuf]) -> Result<HashMap<String, String>> {
    let mut bridges: HashMap<String, String> = HashMap::new();
    let mut total_entries: u64 = 0;
    let mut warned = false;
    for file in entry_files {
        let f = File::open(file)
            .with_context(|| format!("open {}", file.display()))?;
        let r = BufReader::with_capacity(256 * 1024, f);
        let count = walk_entries(r, |entry| {
            // Only interested in `/kythe/edge/named` edges to a
            // language=jvm VName (Kythe's canonical cross-language
            // identity surface).
            if entry.edge_kind != "/kythe/edge/named" { return; }
            let Some(source) = entry.source.as_ref() else { return };
            let Some(target) = entry.target.as_ref() else { return };
            if target.language != "jvm" { return; }
            if target.signature.is_empty() { return; }
            let source_vn = VName::from_proto(source);
            let target_vn = VName::from_proto(target);
            // First-writer-wins on the rare case of conflicting named
            // edges; Kythe Java's emitter is single-source-of-truth
            // per CU, but the corpus-wide concat can theoretically
            // see two different mappings for the same source. Picking
            // first is deterministic given our sorted file walk.
            bridges.entry(source_vn.to_symbol_string())
                .or_insert_with(|| target_vn.to_symbol_string());
        }).with_context(|| format!("walk_entries({})", file.display()))?;
        total_entries += count;
        if bridges.len() > NAMED_BRIDGE_WARN_ENTRIES && !warned {
            eprintln!(
                "[scry-kzip] phase 5/6: warning — named-bridge map exceeded {}; \
                 monitor RSS",
                NAMED_BRIDGE_WARN_ENTRIES,
            );
            warned = true;
        }
    }
    eprintln!(
        "[scry-kzip] phase 5/6: pass 1 scanned {} entries across {} files",
        total_entries, entry_files.len(),
    );
    Ok(bridges)
}

/// Pass 2: walk entries again, decode anchors, look up target in
/// bridge map, accumulate owned canonical records.
fn emit_canonical_records(
    entry_files: &[PathBuf],
    bridges: &HashMap<String, String>,
) -> Result<(Vec<OwnedRecord>, usize, usize)> {
    let mut out: Vec<OwnedRecord> = Vec::new();
    let mut distinct: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut skipped: usize = 0;
    for file in entry_files {
        let f = File::open(file)
            .with_context(|| format!("open {}", file.display()))?;
        let r = BufReader::with_capacity(256 * 1024, f);
        // Reuse decode_stream's anchor-folding logic so each emitted
        // DecodedRecord already has file_path + byte_start + role.
        decode_stream(r, |rec: DecodedRecord| {
            if let Some(fqn) = bridges.get(&rec.target_symbol) {
                distinct.insert(fqn.clone());
                out.push(OwnedRecord {
                    path: rec.file_path,
                    byte_offset: rec.start,
                    symbol: fqn.clone(),
                    kind: role_to_kind(rec.role),
                });
            } else {
                skipped += 1;
            }
        }).with_context(|| format!("decode_stream({})", file.display()))?;
    }
    Ok((out, distinct.len(), skipped))
}

fn role_to_kind(r: Role) -> u8 {
    match r {
        Role::Decl => 0,
        Role::Ref  => 1,
        Role::Call => 2,
    }
}

/// Walk the entries dir for `.entries` files. Sorted for repro.
fn collect_entry_files(entries_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(entries_dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("entries") {
            paths.push(p);
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::storage::{Entry, VName as PVName};
    use protobuf::Message;

    /// Build a length-delimited Kythe Entry stream by serializing
    /// each entry and prefixing with a base-128 varint length. Same
    /// wire format the indexers emit. Returned bytes can be written
    /// to a `.entries` file the importer reads.
    fn write_stream(entries: &[Entry]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for e in entries {
            let bytes = e.write_to_bytes().expect("serialize Entry");
            let mut len = bytes.len() as u64;
            while len >= 0x80 {
                out.push(((len & 0x7f) | 0x80) as u8);
                len >>= 7;
            }
            out.push((len & 0x7f) as u8);
            out.extend_from_slice(&bytes);
        }
        out
    }

    fn vn(sig: &str, path: &str) -> PVName {
        let mut v = PVName::new();
        v.signature = sig.to_string();
        v.path = path.to_string();
        v.corpus = "test-corpus".to_string();
        v.language = "java".to_string();
        v
    }

    /// Language=jvm canonical VName.
    fn jvm_vn(fqn: &str) -> PVName {
        let mut v = PVName::new();
        v.signature = fqn.to_string();
        v.language = "jvm".to_string();
        v.corpus = "test-corpus".to_string();
        v
    }

    fn fact(source: PVName, name: &str, value: &[u8]) -> Entry {
        let mut e = Entry::new();
        e.source = Some(source).into();
        e.fact_name = name.to_string();
        e.fact_value = value.to_vec();
        e
    }

    fn edge(source: PVName, target: PVName, kind: &str) -> Entry {
        let mut e = Entry::new();
        e.source = Some(source).into();
        e.target = Some(target).into();
        e.edge_kind = kind.to_string();
        e.fact_name = "/".to_string();
        e
    }

    /// End-to-end small-scale test. Two synthetic CUs:
    /// - `cu-def.entries`: Binder.java's standalone CU. Defines a
    ///   method whose def-target VName has a named-edge to
    ///   `android.os.Binder.clearCallingIdentity()J`.
    /// - `cu-caller.entries`: services.core's CU. A call-anchor's
    ///   target VName *also* has a named-edge to the SAME JVM FQN
    ///   (this is what our Kythe Patch 4 makes happen on the real
    ///   corpus).
    ///
    /// Expected: pass 1 collects two bridges → the same JVM FQN;
    /// pass 2 emits two records both keyed on the FQN — one decl
    /// from Binder.java, one call from services.core. The packed
    /// sidecar can then be read and verified to contain the
    /// canonical FQN as the symbol for both anchor sites.
    #[test]
    fn two_cu_bridge_resolves_through_jvm_fqn() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let entries_dir = tmp.path().join("entries");
        std::fs::create_dir_all(&entries_dir).unwrap();

        let fqn = "android.os.Binder.clearCallingIdentity()J";

        // CU 1: Binder.java standalone — def anchor + named bridge.
        let def_anchor = vn("def-anchor-binder", "Binder.java");
        let def_target = vn("Binder#clearCallingIdentity()", "Binder.java");
        let jvm = jvm_vn(fqn);
        let cu_def = write_stream(&[
            fact(def_anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(def_anchor.clone(), "/kythe/loc/start", b"100"),
            fact(def_anchor.clone(), "/kythe/loc/end",   b"120"),
            edge(def_anchor.clone(), def_target.clone(), "/kythe/edge/defines/binding"),
            edge(def_target.clone(), jvm.clone(), "/kythe/edge/named"),
        ]);
        std::fs::write(entries_dir.join("cu-aaa.entries"), &cu_def).unwrap();

        // CU 2: caller — call anchor + (different) call-target VName,
        // but that target ALSO has a named-edge to the same JVM FQN.
        // This is exactly what the post-patch java_indexer emits.
        let call_anchor = vn("call-anchor-svc", "services.core/Caller.java");
        let call_target = vn("opaque-hash-of-classpath-resolve", "");
        let cu_call = write_stream(&[
            fact(call_anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(call_anchor.clone(), "/kythe/loc/start", b"500"),
            fact(call_anchor.clone(), "/kythe/loc/end",   b"520"),
            edge(call_anchor.clone(), call_target.clone(), "/kythe/edge/ref/call"),
            edge(call_target.clone(), jvm.clone(), "/kythe/edge/named"),
        ]);
        std::fs::write(entries_dir.join("cu-bbb.entries"), &cu_call).unwrap();

        // Run the importer.
        let out_dir = tmp.path().join("out");
        let report = build_fqn_sidecar(&entries_dir, &out_dir)
            .expect("build_fqn_sidecar");

        // Bridges: two distinct source VNames mapped to one JVM FQN.
        assert_eq!(report.named_bridges, 2, "two named bridges expected, got {report:?}");
        // Records: one decl (Binder) + one call (services.core), both
        // canonicalized to the FQN.
        assert_eq!(report.canonical_records, 2, "{report:?}");
        assert_eq!(report.distinct_fqns, 1, "{report:?}");
        assert_eq!(report.skipped_no_bridge, 0, "{report:?}");

        // Read the sidecar back and check the canonical symbol shows
        // up at both anchor locations.
        let pp = precision_packed::PrecisionPacked::open(&report.sidecar_path, MAGIC_SCIP)
            .expect("open sidecar")
            .expect("sidecar exists");
        assert_eq!(pp.record_count(), 2);
        let expected_symbol = format!("kythe:jvm:test-corpus###{fqn}");
        // Both records' symbols should be the FQN-encoded JVM VName.
        let def_sym = pp.symbol_at("Binder.java", 100).expect("def at 100");
        let call_sym = pp.symbol_at("services.core/Caller.java", 500).expect("call at 500");
        assert_eq!(def_sym, expected_symbol);
        assert_eq!(call_sym, expected_symbol);
    }

    /// Anchors whose target has no named-edge bridge get skipped (not
    /// emitted) — they're already in the per-CU `scip_index.bin` and
    /// don't need the cross-CU sidecar. Records: 0, skipped: 1.
    #[test]
    fn anchor_without_bridge_is_skipped() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let entries_dir = tmp.path().join("entries");
        std::fs::create_dir_all(&entries_dir).unwrap();

        let anchor = vn("a1", "F.java");
        let target = vn("unbridged-target", "F.java");
        let cu = write_stream(&[
            fact(anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(anchor.clone(), "/kythe/loc/start", b"7"),
            fact(anchor.clone(), "/kythe/loc/end",   b"10"),
            edge(anchor.clone(), target.clone(), "/kythe/edge/ref"),
        ]);
        std::fs::write(entries_dir.join("cu-x.entries"), &cu).unwrap();

        let out_dir = tmp.path().join("out");
        let report = build_fqn_sidecar(&entries_dir, &out_dir).unwrap();
        assert_eq!(report.named_bridges, 0);
        assert_eq!(report.canonical_records, 0);
        assert_eq!(report.skipped_no_bridge, 1);
    }
}
