//! CLI handlers for the build-system bridge subcommands. Each
//! subcommand owns one build-system + indexer pairing and produces
//! a precision sidecar consumable by the strict-precision filter.

use anyhow::{Context, Result};
use scry_bridge::{BuildSystem, soong::Soong, Language};
use scry_bridge::{cmake, gn, kbuild};
use scry_bridge::java_indexer::{JavaIndexerConfig, run as run_javac, merge as merge_scip};
use scry_bridge::kotlin_indexer::{KotlinIndexerConfig, run as run_kotlinc};
use scry_bridge::polyglot::{
    PolyglotConfig, PolyglotKind, discover as polyglot_discover,
    run as polyglot_run,
};
use scry_store::StoreReader;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::default_index_dir;

/// Which build system the precision pipeline is acting on. Explicit
/// — `scry build-symbols` requires one of these flags, no
/// auto-detection (per the user policy: "no flaky guessing").
#[derive(Debug, Clone)]
pub(crate) enum BuildKind {
    /// Soong (AOSP). Walks the build dir for javac/kotlinc rules.
    Soong { build_dir: PathBuf },
    /// GN (Chromium / Perfetto / Fuchsia). Uses the build dir's
    /// compile_commands.json for C-family precision.
    Gn { build_dir: PathBuf, gn_binary: Option<PathBuf> },
    /// Linux kbuild. Locates / regenerates compile_commands.json
    /// from the kernel build out dir.
    Kbuild { build_dir: PathBuf },
    /// CMake. Same compile_commands.json shape as GN — locates /
    /// regenerates the manifest via `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`.
    Cmake { build_dir: PathBuf, cmake_binary: Option<PathBuf> },
    /// Cargo workspaces. No C-family precision step — the polyglot
    /// pass handles Rust via rust-analyzer scip. Listed here so the
    /// CLI takes one consistent shape across build systems.
    Cargo,
}

/// `scry build-jvm-scip` — Soong → scip-java + semanticdb-kotlinc.
///
/// Single command that handles BOTH Java and Kotlin compilations
/// emitted by Soong. Stages:
///   1. Walk Soong intermediates for javac and kotlinc rules.
///   2. Per compilation, run javac+semanticdb-javac (Java) or
///      kotlinc+semanticdb-kotlinc (Kotlin), dropping `.semanticdb`
///      shards in a shared targetroot.
///   3. One `scip-java index-semanticdb` merge pass turns the
///      shared targetroot into a single SCIP file (semanticdb is
///      the lingua-franca of the JVM SCIP world, so the same merger
///      handles both languages without distinction).
///   4. Import the merged SCIP into `<index>/scip_index.bin`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_build_jvm_scip(
    source_root: PathBuf,
    build_dir: Option<PathBuf>,
    index: Option<PathBuf>,
    javac: Option<PathBuf>,
    scip_java: Option<PathBuf>,
    semanticdb_javac_jar: Option<PathBuf>,
    kotlinc: Option<PathBuf>,
    semanticdb_kotlinc_jar: Option<PathBuf>,
    targetroot: Option<PathBuf>,
    only_module: Option<String>,
    max_compilations: Option<usize>,
    skip_kotlin: bool,
    skip_java: bool,
) -> Result<()> {
    let t_total = Instant::now();
    let build_dir = build_dir.unwrap_or_else(|| source_root.join("out/soong"));
    let index_dir = index.unwrap_or_else(default_index_dir);

    eprintln!("[build-jvm-scip] source_root: {}", source_root.display());
    eprintln!("[build-jvm-scip] build_dir:   {}", build_dir.display());
    eprintln!("[build-jvm-scip] index_dir:   {}", index_dir.display());

    // Stage 1: extract compilations.
    let t = Instant::now();
    let bridge = Soong::new(&source_root);
    let mut compilations = bridge.extract_compilations(&build_dir)
        .context("extract Soong compilations")?;
    let n_java = compilations.iter().filter(|c| matches!(c.language, Language::Java)).count();
    let n_kotlin = compilations.iter().filter(|c| matches!(c.language, Language::Kotlin)).count();
    eprintln!(
        "[build-jvm-scip] extracted {} compilations ({n_java} Java, {n_kotlin} Kotlin) in {:.2}s",
        compilations.len(), t.elapsed().as_secs_f64(),
    );
    if skip_java {
        compilations.retain(|c| !matches!(c.language, Language::Java));
    }
    if skip_kotlin {
        compilations.retain(|c| !matches!(c.language, Language::Kotlin));
    }
    if let Some(filter) = only_module {
        let before = compilations.len();
        compilations.retain(|c| c.module.contains(&filter));
        eprintln!(
            "[build-jvm-scip] --only-module {filter:?}: {before} → {} compilations",
            compilations.len(),
        );
    }
    if let Some(cap) = max_compilations {
        if compilations.len() > cap {
            eprintln!(
                "[build-jvm-scip] --max-compilations: trimming {} → {cap}",
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

    // Shared targetroot: both indexers write `.semanticdb` files
    // under here so the single merge step handles both languages.
    let shared_targetroot = targetroot.unwrap_or_else(||
        std::env::temp_dir().join("scry-semanticdb"));

    // Stage 2a: Java compilations → javac + semanticdb-javac.
    let mut java_cfg = JavaIndexerConfig::default();
    if let Some(p) = javac { java_cfg.javac = p; }
    if let Some(p) = scip_java.clone() { java_cfg.scip_java = p; }
    if let Some(p) = semanticdb_javac_jar { java_cfg.semanticdb_javac_jar = p; }
    java_cfg.targetroot = shared_targetroot.clone();
    eprintln!("[build-jvm-scip] javac:               {}", java_cfg.javac.display());
    eprintln!("[build-jvm-scip] semanticdb-javac:    {}", java_cfg.semanticdb_javac_jar.display());
    eprintln!("[build-jvm-scip] scip-java:           {}", java_cfg.scip_java.display());
    eprintln!("[build-jvm-scip] targetroot:          {}", java_cfg.targetroot.display());

    let java_compilations: Vec<_> = compilations.iter()
        .filter(|c| matches!(c.language, Language::Java))
        .cloned()
        .collect();
    if !java_compilations.is_empty() {
        let t = Instant::now();
        let r = run_javac(&java_compilations, &java_cfg)
            .context("javac+semanticdb dispatch")?;
        eprintln!(
            "[build-jvm-scip] javac:  {} OK, {} partial, {} no-output ({} .semanticdb files) in {:.2}s",
            r.javac_ok, r.javac_failed_but_partial, r.javac_failed_no_output,
            r.semanticdb_files_written, t.elapsed().as_secs_f64(),
        );
    }

    // Stage 2b: Kotlin compilations → kotlinc + semanticdb-kotlinc.
    let mut kotlin_cfg = KotlinIndexerConfig::default();
    if let Some(p) = kotlinc { kotlin_cfg.kotlinc = p; }
    if let Some(p) = semanticdb_kotlinc_jar { kotlin_cfg.semanticdb_kotlinc_jar = p; }
    kotlin_cfg.targetroot = shared_targetroot.clone();
    eprintln!("[build-jvm-scip] kotlinc:             {}", kotlin_cfg.kotlinc.display());
    eprintln!("[build-jvm-scip] semanticdb-kotlinc:  {}", kotlin_cfg.semanticdb_kotlinc_jar.display());

    let kotlin_compilations: Vec<_> = compilations.iter()
        .filter(|c| matches!(c.language, Language::Kotlin))
        .cloned()
        .collect();
    if !kotlin_compilations.is_empty() {
        let t = Instant::now();
        let r = run_kotlinc(&kotlin_compilations, &kotlin_cfg)
            .context("kotlinc+semanticdb dispatch")?;
        eprintln!(
            "[build-jvm-scip] kotlinc: {} OK, {} partial, {} no-output ({} .semanticdb files) in {:.2}s",
            r.kotlinc_ok, r.kotlinc_failed_but_partial, r.kotlinc_failed_no_output,
            r.semanticdb_files_written, t.elapsed().as_secs_f64(),
        );
    }

    // Stage 3: merge to .scip.
    let scip_out = index_dir.join("merged_jvm.scip");
    let t = Instant::now();
    merge_scip(&java_cfg, &scip_out)
        .context("scip-java index-semanticdb merge")?;
    eprintln!(
        "[build-jvm-scip] merged → {} in {:.2}s",
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
        "[build-jvm-scip] imported into {} in {:.2}s",
        index_dir.join("scip_index.bin").display(), t.elapsed().as_secs_f64(),
    );

    // Final sanity check.
    let reader = StoreReader::open(&index_dir)
        .context("reopen index after import")?;
    let sidecar = scry_store::scip_index::ScipIndex::open(&reader.paths.scip_index())?;
    if let Some(sidx) = sidecar {
        eprintln!(
            "[build-jvm-scip] sidecar: {} symbols, {} records",
            sidx.symbol_count(), sidx.len(),
        );
    }

    eprintln!(
        "[build-jvm-scip] ALL STAGES OK in {:.2}s",
        t_total.elapsed().as_secs_f64(),
    );
    Ok(())
}

/// `scry build-polyglot-scip` — Rust / Go / TypeScript / Python SCIP.
///
/// Walks the source root for project markers of each enabled
/// language and runs that language's native indexer per project.
/// Each indexer produces a `.scip` file that is then imported into
/// scry's sidecar in APPEND mode, so this command composes with
/// `build-jvm-scip` and `clang-index` (both of which already wrote
/// to `scip_index.bin` / `clang_usrs.bin`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_build_polyglot_scip(
    source_root: PathBuf,
    index: Option<PathBuf>,
    scip_out_dir: Option<PathBuf>,
    rust_analyzer: Option<PathBuf>,
    scip_go: Option<PathBuf>,
    scip_typescript: Option<PathBuf>,
    scip_python: Option<PathBuf>,
    no_rust: bool,
    no_go: bool,
    no_typescript: bool,
    no_python: bool,
    only_root: Option<String>,
    max_targets: Option<usize>,
) -> Result<()> {
    let t_total = Instant::now();
    let index_dir = index.unwrap_or_else(default_index_dir);
    let mut kinds: Vec<PolyglotKind> = Vec::new();
    if !no_rust       { kinds.push(PolyglotKind::Rust); }
    if !no_go         { kinds.push(PolyglotKind::Go); }
    if !no_typescript { kinds.push(PolyglotKind::TypeScript); }
    if !no_python     { kinds.push(PolyglotKind::Python); }
    if kinds.is_empty() {
        anyhow::bail!("all languages disabled — nothing to do");
    }

    eprintln!("[build-polyglot-scip] source_root: {}", source_root.display());
    eprintln!("[build-polyglot-scip] index_dir:   {}", index_dir.display());
    eprintln!("[build-polyglot-scip] languages:   {}",
        kinds.iter().map(|k| k.label()).collect::<Vec<_>>().join(", "));

    // Discover.
    let t = Instant::now();
    let mut targets = polyglot_discover(&source_root, &kinds)
        .context("discover polyglot project roots")?;
    eprintln!(
        "[build-polyglot-scip] discovered {} targets in {:.2}s",
        targets.len(), t.elapsed().as_secs_f64(),
    );
    if let Some(filter) = only_root {
        let before = targets.len();
        targets.retain(|t| t.root.to_string_lossy().contains(&filter));
        eprintln!(
            "[build-polyglot-scip] --only-root {filter:?}: {before} → {} targets",
            targets.len(),
        );
    }
    if let Some(cap) = max_targets {
        if targets.len() > cap {
            eprintln!(
                "[build-polyglot-scip] --max-targets: trimming {} → {cap}",
                targets.len(),
            );
            targets.truncate(cap);
        }
    }
    if targets.is_empty() {
        eprintln!("[build-polyglot-scip] no targets matched — nothing to do");
        return Ok(());
    }

    // Run indexers.
    let mut cfg = PolyglotConfig::default();
    if let Some(p) = rust_analyzer { cfg.rust_analyzer = p; }
    if let Some(p) = scip_go { cfg.scip_go = p; }
    if let Some(p) = scip_typescript { cfg.scip_typescript = p; }
    if let Some(p) = scip_python { cfg.scip_python = p; }
    if let Some(p) = scip_out_dir { cfg.scip_out_dir = p; }
    eprintln!("[build-polyglot-scip] rust-analyzer:   {}", cfg.rust_analyzer.display());
    eprintln!("[build-polyglot-scip] scip-go:         {}", cfg.scip_go.display());
    eprintln!("[build-polyglot-scip] scip-typescript: {}", cfg.scip_typescript.display());
    eprintln!("[build-polyglot-scip] scip-python:     {}", cfg.scip_python.display());

    let t = Instant::now();
    let report = polyglot_run(&targets, &cfg)
        .context("polyglot dispatch")?;
    eprintln!(
        "[build-polyglot-scip] {} OK, {} failed ({} .scip files) in {:.2}s",
        report.ok, report.failed, report.scip_files.len(),
        t.elapsed().as_secs_f64(),
    );

    // Import each .scip in append mode.
    let t = Instant::now();
    for scip in &report.scip_files {
        if let Err(e) = scry_scip::import_scip_with_mode(
            scip, &index_dir, Some(source_root.as_path()), true,
        ) {
            eprintln!("[build-polyglot-scip] import failed for {}: {e:#}",
                      scip.display());
        }
    }
    eprintln!(
        "[build-polyglot-scip] imported {} .scip shards in {:.2}s",
        report.scip_files.len(), t.elapsed().as_secs_f64(),
    );

    // Sanity.
    let reader = StoreReader::open(&index_dir)
        .context("reopen index after import")?;
    let sidecar = scry_store::scip_index::ScipIndex::open(&reader.paths.scip_index())?;
    if let Some(sidx) = sidecar {
        eprintln!(
            "[build-polyglot-scip] sidecar: {} symbols, {} records",
            sidx.symbol_count(), sidx.len(),
        );
    }
    eprintln!(
        "[build-polyglot-scip] ALL STAGES OK in {:.2}s",
        t_total.elapsed().as_secs_f64(),
    );
    Ok(())
}

/// Inputs to [`cmd_build_symbols`]. Grouped into one struct so the
/// CLI dispatch reads top-to-bottom without a 20-arg call site, and
/// new knobs slot in without churning every caller.
pub(crate) struct BuildSymbolsArgs {
    pub source_root: PathBuf,
    pub build: BuildKind,
    pub index: Option<PathBuf>,
    pub with_polyglot: bool,
    pub no_rust: bool,
    pub no_go: bool,
    pub no_typescript: bool,
    pub no_python: bool,
    pub workers: usize,
    pub targetroot: Option<PathBuf>,
    pub scip_out_dir: Option<PathBuf>,
    pub javac: Option<PathBuf>,
    pub scip_java: Option<PathBuf>,
    pub semanticdb_javac_jar: Option<PathBuf>,
    pub kotlinc: Option<PathBuf>,
    pub semanticdb_kotlinc_jar: Option<PathBuf>,
    pub only_module: Option<String>,
    pub max_compilations: Option<usize>,
    pub skip_kotlin: bool,
    pub skip_java: bool,
    pub rust_analyzer: Option<PathBuf>,
    pub scip_go: Option<PathBuf>,
    pub scip_typescript: Option<PathBuf>,
    pub scip_python: Option<PathBuf>,
    pub only_root: Option<String>,
    pub max_targets: Option<usize>,
}

/// `scry build-symbols` — unified Kythe-class precision pipeline.
///
/// Takes ONE explicit build-system flag (`--build-soong PATH`,
/// `--build-gn PATH`, `--build-kbuild PATH`), produces the right
/// per-language sidecars:
///   - Soong → SCIP via scip-java + semanticdb-kotlinc.
///   - GN    → clang USRs via libclang on the build's compile_commands.json.
///   - Kbuild → same as GN (kernel C only).
/// Optionally adds polyglot SCIP (Rust/Go/TS/Python) when the
/// `--with-polyglot` flag is on. Each output is imported into the
/// scry sidecar in append mode (SCIP) or replace mode (clang USR
/// — only one C-family pass per index).
pub(crate) fn cmd_build_symbols(args: BuildSymbolsArgs) -> Result<()> {
    let BuildSymbolsArgs {
        source_root, build, index, with_polyglot,
        no_rust, no_go, no_typescript, no_python,
        workers, targetroot, scip_out_dir,
        javac, scip_java, semanticdb_javac_jar,
        kotlinc, semanticdb_kotlinc_jar,
        only_module, max_compilations, skip_kotlin, skip_java,
        rust_analyzer, scip_go, scip_typescript, scip_python,
        only_root, max_targets,
    } = args;

    let t_total = Instant::now();
    let index_dir = index.clone().unwrap_or_else(default_index_dir);

    match &build {
        BuildKind::Soong { build_dir } => {
            eprintln!("[build-symbols] soong build_dir: {}", build_dir.display());
            cmd_build_jvm_scip(
                source_root.clone(), Some(build_dir.clone()), index.clone(),
                javac, scip_java, semanticdb_javac_jar,
                kotlinc, semanticdb_kotlinc_jar,
                targetroot, only_module, max_compilations,
                skip_kotlin, skip_java,
            )?;
        }
        BuildKind::Gn { build_dir, gn_binary } => {
            eprintln!("[build-symbols] gn build_dir: {}", build_dir.display());
            let cc = ensure_gn_compile_commands(build_dir, &source_root, gn_binary.as_deref())?;
            run_clang_on_cc(&cc, &source_root, &index_dir, workers)?;
        }
        BuildKind::Kbuild { build_dir } => {
            eprintln!("[build-symbols] kbuild build_dir: {}", build_dir.display());
            if !kbuild::looks_like_kernel_source(&source_root) {
                anyhow::bail!(
                    "{} doesn't look like a Linux kernel source tree \
                     (expected Makefile + Kconfig + scripts/ at top level)",
                    source_root.display(),
                );
            }
            let cc = ensure_kbuild_compile_commands(build_dir, &source_root)?;
            run_clang_on_cc(&cc, &source_root, &index_dir, workers)?;
        }
        BuildKind::Cmake { build_dir, cmake_binary } => {
            eprintln!("[build-symbols] cmake build_dir: {}", build_dir.display());
            let cc = ensure_cmake_compile_commands(build_dir, cmake_binary.as_deref())?;
            run_clang_on_cc(&cc, &source_root, &index_dir, workers)?;
        }
        BuildKind::Cargo => {
            // No C-family step; polyglot pass handles Rust via
            // rust-analyzer scip.  Force --with-polyglot on.
            eprintln!("[build-symbols] cargo: skipping C-family step, polyglot pass will handle Rust");
        }
    }

    // Cargo always needs the polyglot pass since it's pure-Rust.
    let auto_polyglot = matches!(&build, BuildKind::Cargo);
    if with_polyglot || auto_polyglot {
        eprintln!("[build-symbols] running polyglot pass over {}", source_root.display());
        cmd_build_polyglot_scip(
            source_root.clone(), index, scip_out_dir,
            rust_analyzer, scip_go, scip_typescript, scip_python,
            no_rust, no_go, no_typescript, no_python,
            only_root, max_targets,
        )?;
    }

    eprintln!(
        "[build-symbols] ALL STAGES OK in {:.2}s",
        t_total.elapsed().as_secs_f64(),
    );
    Ok(())
}

fn ensure_gn_compile_commands(
    build_dir: &Path,
    source_root: &Path,
    gn_binary: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(cc) = gn::locate_compile_commands(build_dir)? {
        eprintln!("[build-symbols] gn: reusing existing {}", cc.display());
        return Ok(cc);
    }
    let gn = gn_binary.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("gn"));
    eprintln!("[build-symbols] gn: regenerating compile_commands.json via `{} gen --export-compile-commands {}`",
        gn.display(), build_dir.display());
    gn::regenerate_compile_commands(&gn, source_root, build_dir)
}

fn ensure_kbuild_compile_commands(
    build_dir: &Path,
    source_root: &Path,
) -> Result<PathBuf> {
    if let Some(cc) = kbuild::locate_compile_commands(build_dir)? {
        eprintln!("[build-symbols] kbuild: reusing existing {}", cc.display());
        return Ok(cc);
    }
    eprintln!("[build-symbols] kbuild: regenerating compile_commands.json via gen_compile_commands.py");
    kbuild::regenerate_compile_commands(source_root, build_dir)
}

fn ensure_cmake_compile_commands(
    build_dir: &Path,
    cmake_binary: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(cc) = cmake::locate_compile_commands(build_dir)? {
        eprintln!("[build-symbols] cmake: reusing existing {}", cc.display());
        return Ok(cc);
    }
    let cmake_bin = cmake_binary.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("cmake"));
    eprintln!("[build-symbols] cmake: regenerating compile_commands.json via {}", cmake_bin.display());
    cmake::regenerate_compile_commands(&cmake_bin, build_dir)
}

fn run_clang_on_cc(
    compile_commands: &Path,
    source_root: &Path,
    index_dir: &Path,
    workers: usize,
) -> Result<()> {
    let t = Instant::now();
    scry_clang::build_clang_usrs(
        compile_commands, index_dir, Some(source_root), workers,
        4 * 1024 * 1024,
    )?;
    eprintln!(
        "[build-symbols] clang USRs landed in {} in {:.2}s",
        index_dir.join("clang_usrs.bin").display(),
        t.elapsed().as_secs_f64(),
    );
    Ok(())
}
