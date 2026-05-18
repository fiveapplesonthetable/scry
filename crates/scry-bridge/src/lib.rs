//! `scry-bridge` — locate native build metadata for the precision
//! sidecar producers.
//!
//! Today this crate is small on purpose. Each module locates (and,
//! when needed, regenerates) the build-system artifact a downstream
//! producer consumes:
//!
//! - [`gn`] / [`kbuild`] / [`cmake`] — find / regenerate
//!   `compile_commands.json` for `scry-clang` to walk via libclang.
//! - [`polyglot`] — drive `rust-analyzer`, `gopls`, `scip-typescript`,
//!   `scip-python` over a source root and collect per-target `.scip`
//!   files.
//!
//! For Kythe-integrated builds (AOSP via Soong, Bazel, anything that
//! ships Kythe extractors), the canonical path is `scry build-symbols
//! --build-kzip PATH.kzip` — the compiler wrappers capture the exact
//! inputs every compile sees, and scry just ingests the resulting
//! kzip.

#![forbid(unsafe_code)]

/// Where scry drops its scratch output (per-target `.scip` shards,
/// any extracted per-compilation work). `$SCRY_TMP_DIR` overrides
/// the default of `/mnt/agent/tmp` — preferred over `$TMPDIR` /
/// `std::env::temp_dir()` because shards can be tens of GB on
/// AOSP-scale corpora and `/tmp` is often a small tmpfs.
pub fn scry_tmp_dir() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("SCRY_TMP_DIR") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from("/mnt/agent/tmp")
}

/// Resolve an indexer binary by name. Two sources, in priority
/// order:
///   1. `$SCRY_INDEXER_<NAME>` — per-binary override (e.g.
///      `SCRY_INDEXER_RUST_ANALYZER=/opt/ra/bin/rust-analyzer`).
///      Dashes in `name` map to underscores.
///   2. The first match on `$PATH`.
///
/// If neither resolves, the bare `name` is returned so the eventual
/// spawn error mentions the binary.
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

pub mod cmake;
pub mod gn;
pub mod kbuild;
pub mod polyglot;
