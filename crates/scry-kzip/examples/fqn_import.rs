//! Run Phase 5 (FQN-canonical sidecar) standalone against a
//! pre-populated entries dir (produced by an earlier
//! `SCRY_KZIP_SERVING_DIR=<dir>` build-symbols run that was
//! interrupted before Phase 5 completed).
//!
//! Useful as an escape-hatch when the optional Phase 6 LevelDB build
//! ran long and the user wants the scip_index_fqn.bin without
//! re-doing Phase 3's indexer dispatch.
//!
//! Usage:
//!   cargo run --release --example fqn_import -- <entries-dir> <out-dir> <source-root>
//!
//! `<source-root>` is the on-disk prefix prepended to corpus-relative
//! anchor paths (e.g. `/home/zim/dev/aosp`) — required for query-time
//! `apply_precision_filter` to cover the sidecar's records.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let entries_dir = PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("missing <entries-dir>"))?,
    );
    let out_dir = PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("missing <out-dir>"))?,
    );
    let source_root = PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("missing <source-root>"))?,
    );
    std::fs::create_dir_all(&out_dir)?;
    eprintln!("[fqn_import] entries:     {}", entries_dir.display());
    eprintln!("[fqn_import] out:         {}", out_dir.display());
    eprintln!("[fqn_import] source-root: {}", source_root.display());
    let t = std::time::Instant::now();
    let report = scry_kzip::fqn_importer::build_fqn_sidecar(
        &entries_dir, &out_dir, Some(&source_root),
    )?;
    eprintln!(
        "[fqn_import] wrote {} ({} records, {} distinct FQNs, \
         {} skipped no-bridge) in {:.1}s",
        report.sidecar_path.display(),
        report.canonical_records,
        report.distinct_fqns,
        report.skipped_no_bridge,
        t.elapsed().as_secs_f64(),
    );
    Ok(())
}
