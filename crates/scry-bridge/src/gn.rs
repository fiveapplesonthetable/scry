//! GN (Chromium / Fuchsia / Perfetto) — `compile_commands.json` locator.
//!
//! GN has one `compile_commands.json` for C-family code (when
//! `gn gen --export-compile-commands` was passed) plus an `args.gn`
//! file that identifies the build directory. The downstream consumer
//! (libclang via `scry-clang`) walks the compile_commands.json
//! directly. Non-C-family languages in a GN project live in
//! subdirectories that [`crate::polyglot`] handles.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// GN build directory marker. `args.gn` is GN's per-build config
/// file — every GN build dir has one.
const GN_BUILD_MARKER: &str = "args.gn";

/// Locate the compile_commands.json inside a GN build directory.
/// Returns `Ok(None)` if the directory looks like a GN build dir
/// (`args.gn` present) but the compile_commands.json is missing —
/// the caller can then regenerate it via `gn gen`.
pub fn locate_compile_commands(build_dir: &Path) -> Result<Option<PathBuf>> {
    let args_gn = build_dir.join(GN_BUILD_MARKER);
    if !args_gn.exists() {
        anyhow::bail!(
            "{} is not a GN build directory ({} not found)",
            build_dir.display(), GN_BUILD_MARKER,
        );
    }
    let cc = build_dir.join("compile_commands.json");
    if cc.exists() {
        Ok(Some(cc))
    } else {
        Ok(None)
    }
}

/// Run `gn gen --export-compile-commands BUILD_DIR` from SOURCE_ROOT.
/// `gn` must be in PATH or pointed at via the `gn` arg.
///
/// Returns the absolute path to the freshly-generated
/// `compile_commands.json` on success.
pub fn regenerate_compile_commands(
    gn_binary: &Path,
    source_root: &Path,
    build_dir: &Path,
) -> Result<PathBuf> {
    let status = std::process::Command::new(gn_binary)
        .arg("gen")
        .arg("--export-compile-commands")
        .arg(build_dir)
        .current_dir(source_root)
        .status()
        .with_context(|| format!("spawn {} gen", gn_binary.display()))?;
    if !status.success() {
        anyhow::bail!("gn gen failed with exit {status}");
    }
    let cc = build_dir.join("compile_commands.json");
    if !cc.exists() {
        anyhow::bail!(
            "gn gen --export-compile-commands ran but {} not found",
            cc.display(),
        );
    }
    Ok(cc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_finds_existing_compile_commands() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-gn-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("args.gn"), b"is_debug=false\n").unwrap();
        std::fs::write(tmp.join("compile_commands.json"), b"[]").unwrap();
        let cc = locate_compile_commands(&tmp).unwrap();
        assert!(cc.is_some());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn locate_returns_none_when_missing_compile_commands() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-gn-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("args.gn"), b"is_debug=false\n").unwrap();
        let cc = locate_compile_commands(&tmp).unwrap();
        assert!(cc.is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn locate_errors_for_non_gn_directory() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-gn-nope-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let r = locate_compile_commands(&tmp);
        assert!(r.is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
