//! `scry finalize` — one-shot post-index sidecar pipeline.
//!
//! Composes the individual `scry build-*` sub-commands plus the
//! per-language indexer importers (clang USR sidecar, SCIP import)
//! into one discoverable CI hook. Each stage reports its own timing
//! and propagates errors via `?` so the caller can fail-fast.
//!
//! Also home to `pick_single_auto_discovered` — the walker that
//! finds `compile_commands.json` / `*.scip` artifacts inside the
//! indexed source roots and the `--build-out` paths. See
//! `docs/BUILD_AWARE.md` for the per-language Quick start that
//! explains how end users wire build outputs into scry.

use anyhow::{Context, Result};
use scry_store::StoreReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{
    cmd_build_file_refs, cmd_build_file_symbols, cmd_build_modgraph,
    cmd_build_offsets, cmd_build_resolutions, cmd_build_trigrams,
    default_index_dir,
};

/// `scry finalize` — one-shot post-index sidecar pipeline.
///
/// Composition of existing build-* commands; the value here is
/// discoverability + one-stop CI hook. Each stage reports its
/// own timing and exits non-zero on failure so the caller can
/// fail-fast.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_finalize(
    index: Option<PathBuf>,
    build_soong: Option<PathBuf>,
    build_kernel: Option<PathBuf>,
    build_gn: Option<PathBuf>,
    build_bazel: Option<PathBuf>,
    build_cargo: Option<PathBuf>,
    scip: Option<PathBuf>,
    clang_compile_commands: Option<PathBuf>,
    clang_root: Option<PathBuf>,
    build_out: Vec<PathBuf>,
    workers: usize,
) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    let t_total = Instant::now();
    eprintln!("[finalize] index: {}", dir.display());

    fn stage<R, F: FnOnce() -> Result<R>>(name: &str, f: F) -> Result<R> {
        let t = Instant::now();
        eprintln!("[finalize] === {name} ===");
        let r = f().with_context(|| format!("finalize stage {name}"))?;
        eprintln!("[finalize] {name} done in {} ms", t.elapsed().as_millis());
        Ok(r)
    }

    stage("build-offsets", || cmd_build_offsets(Some(dir.clone())))?;
    stage("build-file-symbols", || cmd_build_file_symbols(Some(dir.clone())))?;
    stage("build-file-refs", || cmd_build_file_refs(Some(dir.clone())))?;
    stage("build-trigrams", || cmd_build_trigrams(Some(dir.clone()), Some(workers), 5 * 1024 * 1024))?;
    stage("build-resolutions", || cmd_build_resolutions(Some(dir.clone())))?;

    // Module-graph. Pick one source — first non-None wins, with a
    // clear log line stating the choice. Multiple precision sidecars
    // for the same index don't compose: a single module_graph.json
    // lives in the dir.
    let mg_out = dir.join("module_graph.json");
    let mg_source = build_soong.as_ref().map(|r| ("soong", r))
        .or_else(|| build_kernel.as_ref().map(|r| ("kernel", r)))
        .or_else(|| build_gn.as_ref().map(|r| ("gn", r)))
        .or_else(|| build_bazel.as_ref().map(|r| ("bazel", r)))
        .or_else(|| build_cargo.as_ref().map(|r| ("cargo", r)));
    if let Some((kind, root)) = mg_source {
        let kind_str = kind.to_string();
        let root = root.clone();
        let out = mg_out.clone();
        stage(&format!("build-modgraph {kind_str}"),
              || cmd_build_modgraph(&kind_str, &root, &out))?;
    } else {
        eprintln!("[finalize] (no --build-<kind> flag — skipping module_graph)");
    }

    if let Some(scip_path) = scip.as_ref() {
        let dir2 = dir.clone();
        let sp = scip_path.clone();
        stage("SCIP import", || scry_scip::import_scip(&sp, &dir2, None))?;
    }

    if let Some(cc_path) = clang_compile_commands.as_ref() {
        let dir2 = dir.clone();
        let cc = cc_path.clone();
        let root = clang_root.clone();
        let workers_for_clang = workers;
        stage("clang USR sidecar", || scry_clang::build_clang_usrs(
            &cc, &dir2, root.as_deref(), workers_for_clang, 4 * 1024 * 1024,
        ))?;
    }

    // Auto-discover compile_commands.json and .scip artifacts inside
    // each indexed root and stage them as clang / SCIP runs.
    // This is the Kythe-style "indexer composition" path: scry
    // consumes existing compiler-backed indexer output as the resolver
    // of record when present, and falls back to tree-sitter for the
    // rest. Skipped when the user already passed --clang-compile-
    // commands / --scip explicitly.
    //
    // Coverage by artifact:
    //   compile_commands.json  →  C / C++ / ObjC (consumed by libclang)
    //   *.scip                 →  Java         (scip-java)
    //                             Kotlin       (scip-java / scip-kotlin)
    //                             Rust         (rust-analyzer scip)
    //                             TypeScript   (scip-typescript)
    //                             Go           (scip-go)
    //                             Python       (scip-python)
    // See docs/BUILD_AWARE.md for per-language instructions.
    //
    // Single-artifact contract: build_clang_usrs and import_scip each
    // write a single sidecar that the previous run would overwrite.
    // If multiple cc.json or .scip files are found across the indexed
    // roots, auto-discovery would silently drop all but the last —
    // so we refuse to guess. The user must pick one by passing
    // --clang-compile-commands / --scip explicitly.
    if clang_compile_commands.is_none() {
        if let Some(picked) = pick_single_auto_discovered(
            &dir, "compile_commands.json", false, &build_out,
        )? {
            let dir2 = dir.clone();
            let cc = picked.clone();
            let root = clang_root.clone();
            let workers_for_clang = workers;
            let stage_name = format!("clang USR sidecar (auto: {})", cc.display());
            // Soft-fail: a missing libclang shouldn't break the whole
            // finalize run for users who happen to have a cc.json in
            // a root. Explicit --clang-compile-commands stays strict
            // because the user opted in.
            let res = stage(&stage_name, || scry_clang::build_clang_usrs(
                &cc, &dir2, root.as_deref(), workers_for_clang, 4 * 1024 * 1024,
            ));
            if let Err(e) = res {
                eprintln!(
                    "[finalize] WARN: auto clang USR sidecar failed; \
                     continuing without clang USR sidecar: {e}"
                );
            }
        }
    }
    if scip.is_none() {
        if let Some(picked) = pick_single_auto_discovered(
            &dir, ".scip", true, &build_out,
        )? {
            let dir2 = dir.clone();
            let sp = picked.clone();
            let stage_name = format!("SCIP import (auto: {})", sp.display());
            let res = stage(&stage_name, || scry_scip::import_scip(&sp, &dir2, None));
            if let Err(e) = res {
                eprintln!(
                    "[finalize] WARN: auto SCIP import failed; \
                     continuing without SCIP sidecar: {e}"
                );
            }
        }
    }

    eprintln!(
        "[finalize] ALL STAGES OK in {} sec — run `scry health --index {}` \
         to verify sidecar shapes.",
        t_total.elapsed().as_secs(), dir.display(),
    );
    Ok(())
}

/// Walk indexed roots + `--build-out` paths looking for files whose
/// name == `pattern` (or whose name ends with `pattern` when
/// `match_ext_suffix` is true, for `.scip`-style suffix matching).
/// Returns `Ok(Some(path))` only when EXACTLY one file is found.
/// On zero or multiple matches, returns `Ok(None)`; multiple
/// matches emit a candidate warning so the user knows to pick one
/// via `--clang-compile-commands` / `--scip` explicitly.
///
/// **Priority of search paths:**
///
/// If `--build-out` paths are provided, ONLY those are searched.
/// `--build-out` is a user-supplied authoritative signal pointing
/// at the canonical build directory; competing matches inside the
/// indexed source roots (e.g. LLVM ships a test-fixture
/// `external/clang/test/Index/compile_commands.json` inside AOSP)
/// would otherwise stuff the candidate pool with files the user
/// didn't mean to nominate.
///
/// If no `--build-out` is given, fall back to walking the indexed
/// source roots — that covers in-tree builds (CMake with the
/// compdb landing in the source tree, `bear -- make` from the
/// project root, scip-typescript writing `index.scip` to cwd).
///
/// **Walker policy:**
///
///  - Indexed-root walk: depth-capped (5), hit-capped (8),
///    `.gitignore` honored (vendored artifacts skip).
///  - `--build-out` walk: deeper cap (10), same hit cap,
///    `.gitignore` OFF. Build outputs (`out/soong/...`, `build/`,
///    `target/`) are exactly what `.gitignore` hides, so a
///    filter-respecting walker would miss them — that's the whole
///    reason `--build-out` exists as a distinct signal.
fn pick_single_auto_discovered(
    index_dir: &Path,
    pattern: &str,
    match_ext_suffix: bool,
    build_out: &[PathBuf],
) -> Result<Option<PathBuf>> {
    const SRC_MAX_DEPTH: usize = 5;
    const OUT_MAX_DEPTH: usize = 10;
    const MAX_HITS: usize = 8;
    let mut hits: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    let match_name = |name: &str| -> bool {
        if match_ext_suffix { name.ends_with(pattern) } else { name == pattern }
    };
    let push_hit = |hits: &mut Vec<PathBuf>,
                    seen: &mut std::collections::HashSet<PathBuf>,
                    p: PathBuf| -> bool {
        let key = p.canonicalize().unwrap_or_else(|_| p.clone());
        if seen.insert(key) {
            hits.push(p);
        }
        hits.len() >= MAX_HITS
    };
    if !build_out.is_empty() {
        'outer2: for out_root in build_out {
            let mut walker = ignore::WalkBuilder::new(out_root);
            walker.max_depth(Some(OUT_MAX_DEPTH));
            walker.standard_filters(false);
            for entry in walker.build().flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) { continue; }
                if match_name(&entry.file_name().to_string_lossy())
                    && push_hit(&mut hits, &mut seen, entry.into_path())
                {
                    break 'outer2;
                }
            }
        }
    } else {
        let roots: Vec<PathBuf> = match StoreReader::open(index_dir) {
            Ok(r) => r.roots.iter().map(|root| PathBuf::from(&root.path)).collect(),
            Err(e) => {
                eprintln!("[finalize] WARN: cannot open index for auto-discovery: {e}");
                Vec::new()
            }
        };
        'outer: for root in &roots {
            let mut walker = ignore::WalkBuilder::new(root);
            walker.max_depth(Some(SRC_MAX_DEPTH));
            walker.standard_filters(true);
            for entry in walker.build().flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) { continue; }
                if match_name(&entry.file_name().to_string_lossy())
                    && push_hit(&mut hits, &mut seen, entry.into_path())
                {
                    break 'outer;
                }
            }
        }
    }
    match hits.len() {
        0 => Ok(None),
        1 => Ok(Some(hits.remove(0))),
        n => {
            eprintln!(
                "[finalize] auto-discovery: found {n} {pattern} files; \
                 skipping (pass an explicit flag to pick one). Candidates:"
            );
            for h in &hits {
                eprintln!("[finalize]   {}", h.display());
            }
            Ok(None)
        }
    }
}
