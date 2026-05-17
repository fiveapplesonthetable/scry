//! Subcommands that build / inspect the build-symbol precision
//! sidecars: clang USRs (Path B) and SCIP symbol index (Path C).
//!
//! These mostly exist for direct CLI use and for `scry finalize`
//! to call internally; the default-on precision filter on `ref` /
//! `callers` queries reads the sidecars on its own once they
//! exist, so users typically don't invoke `scry clang-index` or
//! `scry scip-import` by hand — they let `scry finalize --build-out`
//! do it via auto-discovery. The stats/lookup variants are debugging
//! aids for verifying a sidecar landed with the expected shape.

use anyhow::Result;
use std::path::PathBuf;

use crate::default_index_dir;

/// `scry clang-index` — drive libclang per TU from a
/// `compile_commands.json` and write `<index>/clang_usrs.bin`.
pub(crate) fn cmd_clang_index(
    compile_commands: PathBuf,
    index: Option<PathBuf>,
    root: Option<PathBuf>,
    workers: usize,
    max_file_bytes: u64,
) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    let workers = if workers == 0 {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(8)
    } else {
        workers
    };
    scry_clang::build_clang_usrs(
        &compile_commands,
        &dir,
        root.as_deref(),
        workers,
        max_file_bytes,
    )
}

/// `scry clang-stats --index DIR` — report sidecar shape so users
/// can verify Path B is wired before issuing precise queries.
pub(crate) fn cmd_clang_stats(index: Option<PathBuf>) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    let sidecar_path = scry_store::StorePaths::new(dir.clone()).clang_usrs();
    match scry_store::clang_usrs::ClangUsrIndex::open(&sidecar_path)? {
        None => {
            println!(
                "no clang_usrs.bin found at {}. Run `scry-clang-index \
                 --compile-commands … --index {}` to generate it.",
                sidecar_path.display(),
                dir.display(),
            );
        }
        Some(idx) => {
            println!(
                "clang_usrs.bin: version 1, {} unique USRs, {} records ({} bytes file)",
                idx.usr_count(),
                idx.len(),
                std::fs::metadata(&sidecar_path)
                    .map(|m| m.len()).unwrap_or(0),
            );
            for u in idx.iter_usrs().take(3) {
                println!("  sample: {u}");
            }
        }
    }
    Ok(())
}

/// `scry clang-lookup --index DIR --path X --offset N` — look up a
/// USR by source location. Returns empty if absent (callable from
/// scripts; exit 0 either way).
pub(crate) fn cmd_clang_lookup(index: Option<PathBuf>, path: &str, offset: u32) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    let sidecar_path = scry_store::StorePaths::new(dir).clang_usrs();
    let idx = scry_store::clang_usrs::ClangUsrIndex::open(&sidecar_path)?
        .ok_or_else(|| anyhow::anyhow!(
            "no clang_usrs.bin at {} — run scry-clang-index first",
            sidecar_path.display(),
        ))?;
    if let Some(u) = idx.usr_for(path, offset) {
        println!("{u}");
    }
    Ok(())
}

/// `scry scip-import` — ingest an external SCIP index into the
/// scry sidecar so `--scip-precise` queries can filter by symbol
/// identity.
pub(crate) fn cmd_scip_import(
    scip: PathBuf,
    index: Option<PathBuf>,
    root: Option<PathBuf>,
) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    scry_scip::import_scip(&scip, &dir, root.as_deref())
}

pub(crate) fn cmd_scip_stats(index: Option<PathBuf>) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    let sidecar_path = scry_store::StorePaths::new(dir.clone()).scip_index();
    match scry_store::scip_index::ScipIndex::open(&sidecar_path)? {
        None => {
            println!(
                "no scip_index.bin found at {}. Run `scry scip-import \
                 --scip FILE.scip --index {}` to generate it.",
                sidecar_path.display(),
                dir.display(),
            );
        }
        Some(idx) => {
            println!(
                "scip_index.bin: version 1, {} unique symbols, {} occurrences \
                 ({} bytes file)",
                idx.symbol_count(),
                idx.len(),
                std::fs::metadata(&sidecar_path).map(|m| m.len()).unwrap_or(0),
            );
            for s in idx.iter_symbols().take(3) {
                println!("  sample: {s}");
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_scip_lookup(index: Option<PathBuf>, path: &str, offset: u32) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    let sidecar_path = scry_store::StorePaths::new(dir).scip_index();
    let idx = scry_store::scip_index::ScipIndex::open(&sidecar_path)?
        .ok_or_else(|| anyhow::anyhow!(
            "no scip_index.bin at {} — run scry scip-import first",
            sidecar_path.display(),
        ))?;
    if let Some(s) = idx.symbol_for(path, offset) {
        println!("{s}");
    }
    Ok(())
}
