//! CLI handlers for the build-system bridge subcommands. Each
//! subcommand owns one build-system + indexer pairing and produces
//! a precision sidecar consumable by the strict-precision filter.

use anyhow::{Context, Result};
use scry_bridge::{BuildSystem, soong::Soong};
use scry_bridge::java_indexer::{JavaIndexerConfig, run as run_javac, merge as merge_scip};
use scry_store::StoreReader;
use std::path::PathBuf;
use std::time::Instant;

use crate::default_index_dir;

/// `scry build-java-scip` — Soong → scip-java pipeline.
///
/// 1. Walk Soong intermediates for javac compilations.
/// 2. Replay each compilation with semanticdb-javac to emit .semanticdb shards.
/// 3. Merge all shards into one SCIP file via `scip-java index-semanticdb`.
/// 4. Import the merged SCIP into the scry sidecar (scip_index.bin).
///
/// Side effect: writes to `<targetroot>/<module>/META-INF/semanticdb/…`
/// during stage 2, then to `<index>/scip_index.bin` after stage 4.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_build_java_scip(
    source_root: PathBuf,
    build_dir: Option<PathBuf>,
    index: Option<PathBuf>,
    javac: Option<PathBuf>,
    scip_java: Option<PathBuf>,
    semanticdb_javac_jar: Option<PathBuf>,
    targetroot: Option<PathBuf>,
    only_module: Option<String>,
    max_compilations: Option<usize>,
) -> Result<()> {
    let t_total = Instant::now();
    let build_dir = build_dir.unwrap_or_else(|| source_root.join("out/soong"));
    let index_dir = index.unwrap_or_else(default_index_dir);

    eprintln!("[build-java-scip] source_root: {}", source_root.display());
    eprintln!("[build-java-scip] build_dir:   {}", build_dir.display());
    eprintln!("[build-java-scip] index_dir:   {}", index_dir.display());

    // Stage 1: extract compilations.
    let t = Instant::now();
    let bridge = Soong::new(&source_root);
    let mut compilations = bridge.extract_compilations(&build_dir)
        .context("extract Soong compilations")?;
    eprintln!(
        "[build-java-scip] extracted {} compilations in {:.2}s",
        compilations.len(), t.elapsed().as_secs_f64(),
    );
    if let Some(filter) = only_module {
        let before = compilations.len();
        compilations.retain(|c| c.module.contains(&filter));
        eprintln!(
            "[build-java-scip] --only-module {filter:?}: {before} → {} compilations",
            compilations.len(),
        );
    }
    if let Some(cap) = max_compilations {
        if compilations.len() > cap {
            eprintln!(
                "[build-java-scip] --max-compilations: trimming {} → {cap}",
                compilations.len(),
            );
            compilations.truncate(cap);
        }
    }
    if compilations.is_empty() {
        anyhow::bail!(
            "no compilations to process. Try `--soong-build-dir {}` or \
             confirm AOSP has been built for this target.",
            build_dir.display(),
        );
    }

    // Stage 2: javac + semanticdb-javac.
    let mut cfg = JavaIndexerConfig::default();
    if let Some(p) = javac { cfg.javac = p; }
    if let Some(p) = scip_java { cfg.scip_java = p; }
    if let Some(p) = semanticdb_javac_jar { cfg.semanticdb_javac_jar = p; }
    if let Some(p) = targetroot { cfg.targetroot = p; }
    eprintln!("[build-java-scip] javac:               {}", cfg.javac.display());
    eprintln!("[build-java-scip] semanticdb-javac:    {}", cfg.semanticdb_javac_jar.display());
    eprintln!("[build-java-scip] scip-java:           {}", cfg.scip_java.display());
    eprintln!("[build-java-scip] targetroot:          {}", cfg.targetroot.display());

    let t = Instant::now();
    let report = run_javac(&compilations, &cfg)
        .context("javac+semanticdb dispatch")?;
    eprintln!(
        "[build-java-scip] javac: {} OK, {} partial, {} no-output ({} .semanticdb files) in {:.2}s",
        report.javac_ok,
        report.javac_failed_but_partial,
        report.javac_failed_no_output,
        report.semanticdb_files_written,
        t.elapsed().as_secs_f64(),
    );
    if report.semanticdb_files_written == 0 {
        anyhow::bail!(
            "no .semanticdb files produced. Check javac + semanticdb-javac \
             setup; first-failure stderr was logged above per module.",
        );
    }

    // Stage 3: merge to .scip.
    let scip_out = index_dir.join("merged_java.scip");
    let t = Instant::now();
    merge_scip(&cfg, &scip_out)
        .context("scip-java index-semanticdb merge")?;
    eprintln!(
        "[build-java-scip] merged → {} in {:.2}s",
        scip_out.display(), t.elapsed().as_secs_f64(),
    );

    // Stage 4: import the merged SCIP into scry's sidecar.
    //
    // We pass `source_root` as the project_root override because
    // scip-java sets the SCIP project_root from its own cwd at merge
    // time, not from the sourceroot it was told to use per-shard.
    // Without the override, scip-import would try to open paths like
    // `<scry cwd>/libcore/.../Foo.java` and fail with "file missing
    // on disk" — losing every document.
    let t = Instant::now();
    scry_scip::import_scip(&scip_out, &index_dir, Some(source_root.as_path()))
        .context("import merged SCIP into scry sidecar")?;
    eprintln!(
        "[build-java-scip] imported into {} in {:.2}s",
        index_dir.join("scip_index.bin").display(), t.elapsed().as_secs_f64(),
    );

    // Final sanity check: open the sidecar and report its record count
    // so the operator can verify the pipeline at a glance.
    let reader = StoreReader::open(&index_dir)
        .context("reopen index after import")?;
    let sidecar = scry_store::scip_index::ScipIndex::open(&reader.paths.scip_index())?;
    if let Some(sidx) = sidecar {
        eprintln!(
            "[build-java-scip] sidecar: {} symbols, {} records",
            sidx.symbol_count(), sidx.len(),
        );
    }

    eprintln!(
        "[build-java-scip] ALL STAGES OK in {:.2}s",
        t_total.elapsed().as_secs_f64(),
    );
    Ok(())
}
