//! `scry-bridge` — build-system → indexer bridge.
//!
//! Bridges between native build systems (Soong, GN, Bazel, Cargo,
//! plain compile_commands.json) and per-language structured-symbol
//! indexers (libclang for C/C++, scip-java + semanticdb-kotlinc for
//! JVM, rust-analyzer for Rust, scip-go for Go, scip-typescript for
//! TS, scip-python for Python).
//!
//! # Kythe parity
//!
//! Each [`BuildSystem`] yields a stream of [`Compilation`] units —
//! the same level of abstraction Kythe's extractors operate on
//! (`CompilationUnit` proto). Each compilation has everything an
//! indexer needs to reproduce the build's view of one source file
//! or module: source roots, source files, classpath / include
//! paths, predefined macros, bootclasspath. The orchestrator
//! dispatches each compilation to its [`LanguageIndexer`] and
//! merges the outputs into a single per-language sidecar consumed
//! by scry's strict-precision filter.
//!
//! # Layout
//!
//! - [`soong`] — Soong (AOSP) implementation: streaming ninja parser
//!   plus per-javac-rule extractor. Handles the `out/soong/build*.ninja`
//!   files that AOSP emits + their per-module `.jar.rsp` source lists.
//! - [`compile_commands`] — already covered by the existing
//!   `scry-clang` crate; this module re-exports its API behind the
//!   trait so the orchestrator can treat all C/C++ inputs uniformly.
//! - Future: `gn`, `bazel`, `cargo`.

#![forbid(unsafe_code)]

/// Where scry drops its scratch output (per-target `.scip` shards,
/// per-compilation `.semanticdb` files). `$SCRY_TMP_DIR` overrides
/// the default of `/mnt/agent/tmp` — preferred over `$TMPDIR` /
/// `std::env::temp_dir()` because semanticdb shards and per-shard
/// classes/ trees can be tens of GB on AOSP and `/tmp` is often a
/// small tmpfs.
pub fn scry_tmp_dir() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("SCRY_TMP_DIR") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from("/mnt/agent/tmp")
}

/// Resolve an indexer binary by name. Two sources, in priority
/// order — no heuristics, no hardcoded distro paths, no `npm config`
/// probing. If the operator's environment isn't set up, the caller
/// gets a clean "binary not found" error and a pointer at the CLI
/// override.
///
///   1. `$SCRY_INDEXER_<NAME>` — per-binary override (e.g.
///      `SCRY_INDEXER_SCIP_JAVA=/opt/sg/bin/scip-java`). Dashes in
///      `name` map to underscores. Lets operators pin one indexer
///      without touching PATH.
///   2. The first match on `$PATH`.
///
/// If neither resolves, the bare `name` is returned so the eventual
/// spawn error mentions the binary. Every CLI command that takes
/// an indexer binary also exposes an explicit `--<name>` flag with
/// higher priority than this resolver — that's the canonical way to
/// override.
pub fn resolve_indexer_binary(name: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    let exists = |p: &PathBuf| p.is_file();
    let env_key = format!(
        "SCRY_INDEXER_{}",
        name.to_ascii_uppercase().replace('-', "_"),
    );
    if let Some(val) = std::env::var_os(&env_key) {
        let p = PathBuf::from(val);
        if exists(&p) { return p; }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&paths) {
            let p = d.join(name);
            if exists(&p) { return p; }
        }
    }
    PathBuf::from(name)
}

/// Extract every srcjar into a per-compilation scratch dir and
/// return the absolute paths of every extracted `.java` file. Used by
/// the Java and Kotlin indexers to fold Soong's AIDL stubs + KAPT
/// generated sources into the same javac/kotlinc invocation that
/// processes the user's hand-written source — exactly mirroring what
/// Soong's build pipeline does. Re-running on the same target dir is
/// idempotent.
///
/// Shells out to `unzip -q -o` rather than pulling in a zip crate;
/// the binary is universally present and the per-srcjar cost is
/// dominated by I/O anyway.
pub fn extract_srcjars(
    srcjars: &[std::path::PathBuf],
    target_dir: &std::path::Path,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    use anyhow::Context as _;
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if srcjars.is_empty() { return Ok(out); }
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("create srcjar dir {}", target_dir.display()))?;
    for jar in srcjars {
        if !jar.is_file() { continue; }
        let status = std::process::Command::new("unzip")
            .arg("-q").arg("-o")
            .arg(jar)
            .arg("-d").arg(target_dir)
            .status()
            .with_context(|| format!("spawn unzip for {}", jar.display()))?;
        if !status.success() {
            // Soft-fail per jar — a corrupt/empty srcjar shouldn't
            // poison the whole compilation. Other srcjars + the
            // user's hand-written sources may still produce useful
            // semanticdb output.
            eprintln!("[scry-bridge] unzip {} exited with {}",
                      jar.display(), status);
            continue;
        }
    }
    // Walk the extracted tree for .java files.
    for entry in ignore::WalkBuilder::new(target_dir)
        .standard_filters(false)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && entry.path().extension().is_some_and(|e| e == "java")
        {
            out.push(entry.into_path());
        }
    }
    Ok(out)
}

pub mod cmake;
pub mod gn;
pub mod java_indexer;
pub mod kbuild;
pub mod kotlin_indexer;
pub mod polyglot;
pub mod soong;

/// Source language of a compilation unit. Determines which
/// [`LanguageIndexer`] handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Language {
    /// C / C++ / Objective-C — libclang → clang_usrs.bin sidecar.
    CFamily,
    /// Java — scip-java → scip_index.bin sidecar.
    Java,
    /// Kotlin — semanticdb-kotlinc → scip_index.bin sidecar.
    Kotlin,
    /// Rust — rust-analyzer scip → scip_index.bin.
    Rust,
    /// Go — scip-go → scip_index.bin.
    Go,
    /// TypeScript — scip-typescript → scip_index.bin.
    TypeScript,
    /// Python — scip-python → scip_index.bin.
    Python,
}

/// One compilation unit (Kythe's `CompilationUnit` analogue).
///
/// Each compilation produces ONE indexer output (a `.scip` shard, a
/// `.semanticdb` file, a set of USRs). The orchestrator runs the
/// indexer, captures the output, then merges all shards into a
/// single per-language sidecar.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Compilation {
    /// Stable module name for grouping + diagnostics.
    pub module: String,
    pub language: Language,
    /// Absolute path to the project root. SCIP / clang USR paths are
    /// emitted relative to this so scry can match them to its
    /// indexed file_ids.
    pub source_root: std::path::PathBuf,
    /// Source files in this compilation, relative to `source_root`.
    /// Order matters for Java / Kotlin (annotation processors observe
    /// compilation order); we preserve the build system's order.
    pub sources: Vec<String>,
    /// Compile-time classpath as javac-ready tokens, e.g.
    /// `["-classpath", "/abs/a.jar:/abs/b.jar"]`. Stored as a token
    /// stream — not a typed jar list — because Soong sometimes
    /// emits flag forms we shouldn't second-guess (e.g.
    /// `--module-path=...` for module-aware compilations).
    /// Resolved to absolute paths so the indexer doesn't depend on
    /// its own cwd.
    pub classpath: Vec<String>,
    /// Bootclasspath / system-modules tokens (Java/Kotlin only).
    /// Same format as `classpath` — most modules use
    /// `["-bootclasspath", "/abs/boot.jar"]` but AOSP libcore/art
    /// modules use `["--system=/abs/system-modules-dir"]` which is
    /// mutually exclusive with `--release`. We round-trip whatever
    /// the build system specified.
    pub bootclasspath: Vec<String>,
    /// Predefined macros / `-D` flags (C-family) or javac `-A`
    /// processor args (Java). Pre-split into `(key, value)`.
    pub defines: Vec<(String, Option<String>)>,
    /// Source-level version the build system requested for this
    /// compilation, e.g. `"21"`. The indexer maps this to javac's
    /// `--release=N` when no `--system=` / `-bootclasspath` is
    /// present. None ⇒ indexer falls back to its config default.
    pub java_version: Option<String>,
    /// Raw extra args the build system passed to the compiler that
    /// don't fit the structured slots above. We round-trip them
    /// verbatim so the indexer sees the same view the build did.
    pub extra_args: Vec<String>,
    /// Absolute paths to generated-source jars that should be
    /// extracted and compiled as part of THIS compilation. AIDL
    /// stubs (`gen/aidl/aidl0.srcjar`), AutoValue/Hilt KAPT outputs
    /// (`kapt/kapt-sources.jar`), and other annotation-processor
    /// outputs live here. The indexer extracts each one into a
    /// per-compilation scratch dir and appends its `.java` files to
    /// the source argfile so javac/kotlinc resolves the generated
    /// symbols (e.g. `IUwbAdapterStateCallbacks`, `Hilt_MainActivity`)
    /// at compile time, matching what Soong's build does.
    #[serde(default)]
    pub generated_srcjars: Vec<std::path::PathBuf>,
}

/// Trait implemented by build systems that can enumerate their
/// compilations. Each `extract_compilations` call is read-only —
/// no compilation actually runs at this stage. The caller drives
/// the indexer.
pub trait BuildSystem {
    /// Walk the build's manifest / ninja / WORKSPACE and return one
    /// `Compilation` per compiler invocation. Returns an empty
    /// stream when the build directory doesn't match this build
    /// system's shape — callers can fan out across multiple bridges
    /// without explicit detection.
    fn extract_compilations(
        &self,
        build_dir: &std::path::Path,
    ) -> anyhow::Result<Vec<Compilation>>;
}
