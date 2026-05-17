//! CMake — `compile_commands.json` locator / regenerator.
//!
//! CMake's `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON` produces a
//! `compile_commands.json` in the build directory. From scry's
//! perspective this is identical to GN's compdb export: locate /
//! regenerate the manifest, then hand it to clang-index. The
//! bridge here is intentionally narrow and mirrors [`crate::gn`].
//!
//! Project marker: `CMakeCache.txt` lives in every CMake build
//! directory once configure has run. We refuse to operate on a
//! path that doesn't have one — explicit beats guessing.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// CMake build-directory marker — `cmake` writes this file during
/// configuration, so its presence reliably identifies a CMake
/// out-dir (not a source dir).
const CMAKE_BUILD_MARKER: &str = "CMakeCache.txt";

/// Locate compile_commands.json inside a CMake build directory.
/// Returns `Ok(None)` when the dir is a CMake build dir but the
/// manifest hasn't been exported — caller can then run
/// [`regenerate_compile_commands`].
pub fn locate_compile_commands(build_dir: &Path) -> Result<Option<PathBuf>> {
    let cache = build_dir.join(CMAKE_BUILD_MARKER);
    if !cache.exists() {
        anyhow::bail!(
            "{} is not a CMake build directory ({} not found)",
            build_dir.display(), CMAKE_BUILD_MARKER,
        );
    }
    let cc = build_dir.join("compile_commands.json");
    if cc.exists() {
        Ok(Some(cc))
    } else {
        Ok(None)
    }
}

/// Re-run `cmake` with `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON` to
/// produce a compile_commands.json. Preserves the existing
/// generator + cache state.
pub fn regenerate_compile_commands(
    cmake_binary: &Path,
    build_dir: &Path,
) -> Result<PathBuf> {
    let status = std::process::Command::new(cmake_binary)
        .arg("-DCMAKE_EXPORT_COMPILE_COMMANDS=ON")
        .arg(build_dir)
        .status()
        .with_context(|| format!("spawn {}", cmake_binary.display()))?;
    if !status.success() {
        anyhow::bail!("cmake re-configure failed with exit {status}");
    }
    let cc = build_dir.join("compile_commands.json");
    if !cc.exists() {
        anyhow::bail!(
            "cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON ran but {} not found",
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
            "scry-bridge-cmake-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("CMakeCache.txt"), b"//\n").unwrap();
        std::fs::write(tmp.join("compile_commands.json"), b"[]").unwrap();
        assert!(locate_compile_commands(&tmp).unwrap().is_some());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn locate_errors_for_non_cmake_directory() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-cmake-nope-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        assert!(locate_compile_commands(&tmp).is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
