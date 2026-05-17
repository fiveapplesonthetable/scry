//! Linux Kbuild → [`Compilation`] bridge.
//!
//! The Linux kernel doesn't have a per-module compilation manifest;
//! Kbuild drops one `.cmd` file per object next to it inside the
//! build output tree. The kernel ships a Python helper —
//! `scripts/clang-tools/gen_compile_commands.py` — that walks the
//! out dir, parses those `.cmd` files, and emits a single
//! `compile_commands.json` covering every TU built. We shell out to
//! that script (rather than reimplementing it in Rust) because it
//! tracks the kernel's evolving `.cmd` format directly with
//! upstream — no version-skew bug surface to maintain ourselves.
//!
//! Once the compile_commands.json exists, the scry clang-index
//! pipeline consumes it like any other compdb. The kbuild-specific
//! work in this module is just locating + (re)generating the
//! manifest.
//!
//! ## Module-level reachability
//!
//! Distinct from this file — handled by `scry-cli::build_adapter::kernel`
//! which attributes every file to a top-level subsystem
//! (`drivers/net`, `fs/btrfs`, …) and emits a module_graph.json
//! that scry's `--reachable` filter consumes. The two pieces compose:
//! clang USRs give symbol identity, module_graph gives call-across-
//! subsystem reachability.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Kernel build directory marker. Every Kbuild out dir has a
/// generated `.config` file at the top.
pub const KBUILD_MARKER: &str = ".config";

/// Kernel source root marker. The kernel source root has both a
/// top-level `Makefile` and `Kconfig` (and `scripts/`).
pub const KSOURCE_MARKERS: &[&str] = &["Makefile", "Kconfig"];

/// Locate compile_commands.json inside a kernel build directory.
/// Returns `Ok(None)` if it's a kernel out dir (`.config` present)
/// but compile_commands.json hasn't been generated yet — the
/// caller can then run [`regenerate_compile_commands`].
pub fn locate_compile_commands(build_dir: &Path) -> Result<Option<PathBuf>> {
    let dot_config = build_dir.join(KBUILD_MARKER);
    if !dot_config.exists() {
        anyhow::bail!(
            "{} is not a kernel build directory ({} not found)",
            build_dir.display(), KBUILD_MARKER,
        );
    }
    let cc = build_dir.join("compile_commands.json");
    if cc.exists() {
        Ok(Some(cc))
    } else {
        Ok(None)
    }
}

/// Run the kernel's bundled
/// `scripts/clang-tools/gen_compile_commands.py` to produce a
/// compile_commands.json from the build's `.cmd` files. The script
/// lives in the SOURCE tree and walks the build dir.
///
/// Some kernel checkouts also keep the script at
/// `scripts/gen_compile_commands.py` (older layout). We try the
/// modern path first, then the older one. Returns the absolute
/// path to the freshly-generated `compile_commands.json` on success.
pub fn regenerate_compile_commands(
    source_root: &Path,
    build_dir: &Path,
) -> Result<PathBuf> {
    let candidates = [
        source_root.join("scripts/clang-tools/gen_compile_commands.py"),
        source_root.join("scripts/gen_compile_commands.py"),
    ];
    let script = candidates.iter()
        .find(|p| p.exists())
        .with_context(|| format!(
            "no gen_compile_commands.py found under {} \
             (tried scripts/clang-tools/ and scripts/)",
            source_root.display(),
        ))?;
    let status = std::process::Command::new("python3")
        .arg(script)
        .arg("-d").arg(build_dir)
        .arg("-o").arg(build_dir.join("compile_commands.json"))
        .current_dir(source_root)
        .status()
        .with_context(|| format!("spawn python3 {}", script.display()))?;
    if !status.success() {
        anyhow::bail!("gen_compile_commands.py failed with exit {status}");
    }
    let cc = build_dir.join("compile_commands.json");
    if !cc.exists() {
        anyhow::bail!(
            "gen_compile_commands.py ran but {} not found",
            cc.display(),
        );
    }
    Ok(cc)
}

/// Sanity check that a path looks like a kernel source root. Used
/// by orchestrators that take a `--source-root` for kbuild work.
pub fn looks_like_kernel_source(root: &Path) -> bool {
    KSOURCE_MARKERS.iter().all(|m| root.join(m).exists())
        && root.join("scripts").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_finds_existing_compile_commands() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-kbuild-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join(".config"), b"CONFIG_X=y\n").unwrap();
        std::fs::write(tmp.join("compile_commands.json"), b"[]").unwrap();
        let cc = locate_compile_commands(&tmp).unwrap();
        assert!(cc.is_some());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn locate_returns_none_when_missing() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-kbuild-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join(".config"), b"CONFIG_X=y\n").unwrap();
        let cc = locate_compile_commands(&tmp).unwrap();
        assert!(cc.is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn locate_errors_for_non_kbuild_directory() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-kbuild-nope-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let r = locate_compile_commands(&tmp);
        assert!(r.is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn looks_like_kernel_source_detects_marker_files() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-kbuild-src-{}", std::process::id()));
        let _ = std::fs::create_dir_all(tmp.join("scripts"));
        std::fs::write(tmp.join("Makefile"), b"obj-y :=\n").unwrap();
        std::fs::write(tmp.join("Kconfig"), b"mainmenu \"Linux\"\n").unwrap();
        assert!(looks_like_kernel_source(&tmp));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
