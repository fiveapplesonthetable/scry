//! Per-language SCIP indexers that don't go through a build system —
//! they walk the source tree for native project roots and run the
//! corresponding indexer in-place.
//!
//! Unlike [`crate::java_indexer`] / [`crate::kotlin_indexer`] (which
//! replay each compiler invocation Soong recorded), these indexers
//! take the project root as their unit of work. Each indexer:
//!
//!   1. Discover roots — Cargo.toml for Rust, go.mod for Go,
//!      tsconfig.json for TypeScript, the path itself for Python.
//!   2. Per root: invoke the indexer (`rust-analyzer scip`,
//!      `scip-go`, `scip-typescript`, `scip-python`) and capture
//!      the per-root `.scip` output.
//!   3. The orchestrator imports each `.scip` into scry's sidecar
//!      via [`scry_scip::import_scip_with_mode(append=true)`] so
//!      languages compose without overwriting each other.
//!
//! Errors per root are logged but don't abort the whole walk — a
//! single broken Cargo workspace shouldn't take out the rest of
//! the Rust corpus.

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One target the indexer can act on. `kind` selects the indexer;
/// `root` is the project directory passed to that indexer.
#[derive(Debug, Clone)]
pub struct PolyglotTarget {
    pub kind: PolyglotKind,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolyglotKind {
    Rust,
    Go,
    TypeScript,
    Python,
}

impl PolyglotKind {
    /// Human-readable label for logs / CLI flags.
    pub fn label(&self) -> &'static str {
        match self {
            PolyglotKind::Rust => "rust",
            PolyglotKind::Go => "go",
            PolyglotKind::TypeScript => "typescript",
            PolyglotKind::Python => "python",
        }
    }

    /// File / directory marker that identifies a project root for
    /// this language. `None` means "treat any directory containing
    /// the language's source files as a root" (Python).
    pub fn root_marker(&self) -> Option<&'static str> {
        match self {
            PolyglotKind::Rust => Some("Cargo.toml"),
            PolyglotKind::Go => Some("go.mod"),
            PolyglotKind::TypeScript => Some("tsconfig.json"),
            PolyglotKind::Python => None,
        }
    }
}

/// Walk `source_root` for project markers of every kind. Honors
/// `.gitignore` because vendored copies of indexed projects (e.g.
/// `external/rust/crates/...` in AOSP) appear under `out/` and would
/// otherwise double-index. Output is sorted+deduped per kind.
pub fn discover(
    source_root: &Path,
    kinds: &[PolyglotKind],
) -> Result<Vec<PolyglotTarget>> {
    let mut out: Vec<PolyglotTarget> = Vec::new();
    for kind in kinds {
        let Some(marker) = kind.root_marker() else {
            // Python — point the indexer at source_root itself.
            out.push(PolyglotTarget { kind: *kind, root: source_root.to_path_buf() });
            continue;
        };
        for entry in WalkBuilder::new(source_root)
            .standard_filters(true)
            .build()
            .flatten()
        {
            if entry.file_type().is_some_and(|t| t.is_file())
                && entry.file_name().to_string_lossy() == marker
            {
                if let Some(dir) = entry.path().parent() {
                    out.push(PolyglotTarget {
                        kind: *kind,
                        root: dir.to_path_buf(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| (a.kind as u8, &a.root).cmp(&(b.kind as u8, &b.root)));
    out.dedup_by(|a, b| a.kind == b.kind && a.root == b.root);
    Ok(out)
}

/// Configuration for the polyglot indexer dispatch. Each binary
/// override is optional — defaults assume the install_indexers.sh
/// install paths (`/mnt/agent/tools/bin/...` or `$PATH`).
#[derive(Debug, Clone)]
pub struct PolyglotConfig {
    pub rust_analyzer: PathBuf,
    pub scip_go: PathBuf,
    pub scip_typescript: PathBuf,
    pub scip_python: PathBuf,
    /// Per-target SCIP output files live under here, one per target.
    pub scip_out_dir: PathBuf,
    /// Per-invocation timeout in seconds. rust-analyzer scip on
    /// a moderately-sized crate runs in seconds; AOSP's bigger
    /// crates (`librustutils`, `keymint`) can take a minute. 600s
    /// is a generous upper bound that catches genuine hangs.
    pub per_target_timeout_secs: u64,
}

impl Default for PolyglotConfig {
    fn default() -> Self {
        Self {
            rust_analyzer: PathBuf::from("rust-analyzer"),
            scip_go: PathBuf::from("scip-go"),
            scip_typescript: PathBuf::from("scip-typescript"),
            scip_python: PathBuf::from("scip-python"),
            scip_out_dir: std::env::temp_dir().join("scry-polyglot-scip"),
            per_target_timeout_secs: 600,
        }
    }
}

/// Result of [`run`] over a target list.
#[derive(Debug, Clone, Default)]
pub struct DispatchReport {
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    pub scip_files: Vec<PathBuf>,
}

/// Run each target through its indexer in parallel. Returns the list
/// of produced `.scip` files for the orchestrator to import (in
/// append mode) into scry's sidecar.
pub fn run(targets: &[PolyglotTarget], cfg: &PolyglotConfig) -> Result<DispatchReport> {
    std::fs::create_dir_all(&cfg.scip_out_dir)
        .with_context(|| format!("create scip_out_dir {}", cfg.scip_out_dir.display()))?;

    let outcomes: Vec<Option<PathBuf>> = targets.par_iter()
        .map(|t| run_one(t, cfg))
        .collect();

    let mut report = DispatchReport {
        total: targets.len(),
        ..Default::default()
    };
    for o in outcomes {
        if let Some(p) = o {
            report.ok += 1;
            report.scip_files.push(p);
        } else {
            report.failed += 1;
        }
    }
    Ok(report)
}

fn run_one(target: &PolyglotTarget, cfg: &PolyglotConfig) -> Option<PathBuf> {
    let slug = target.root.to_string_lossy()
        .replace('/', "_").trim_start_matches('_').to_string();
    let out_path = cfg.scip_out_dir.join(format!(
        "{}-{slug}.scip", target.kind.label()));

    let mut cmd = match target.kind {
        PolyglotKind::Rust => {
            // rust-analyzer scip --output PATH (uses cwd as project root).
            let mut c = Command::new(&cfg.rust_analyzer);
            c.arg("scip")
             .arg(&target.root)
             .arg("--output").arg(&out_path)
             .current_dir(&target.root);
            c
        }
        PolyglotKind::Go => {
            // scip-go --output PATH --module-root .
            let mut c = Command::new(&cfg.scip_go);
            c.arg("--module-root").arg(".")
             .arg("--output").arg(&out_path)
             .current_dir(&target.root);
            c
        }
        PolyglotKind::TypeScript => {
            // scip-typescript index --output PATH
            let mut c = Command::new(&cfg.scip_typescript);
            c.arg("index")
             .arg("--output").arg(&out_path)
             .current_dir(&target.root);
            c
        }
        PolyglotKind::Python => {
            // scip-python index --project-name <slug> --output PATH .
            let mut c = Command::new(&cfg.scip_python);
            c.arg("index")
             .arg("--output").arg(&out_path)
             .arg("--project-name").arg(&slug)
             .arg(".")
             .current_dir(&target.root);
            c
        }
    };
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[scry-bridge] {} {}: spawn failed: {e}",
                      target.kind.label(), target.root.display());
            return None;
        }
    };
    if !output.status.success() || !out_path.exists() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("");
        eprintln!("[scry-bridge] {} {}: indexer failed (exit {}): {first}",
                  target.kind.label(), target.root.display(),
                  output.status.code().unwrap_or(-1));
        return None;
    }
    Some(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_cargo_roots() {
        let tmp = std::env::temp_dir().join(format!(
            "scry-bridge-polyglot-disco-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("foo")).unwrap();
        std::fs::create_dir_all(tmp.join("bar")).unwrap();
        std::fs::write(tmp.join("foo/Cargo.toml"), b"[package]\nname='foo'\n").unwrap();
        std::fs::write(tmp.join("bar/go.mod"), b"module bar\n").unwrap();

        let targets = discover(&tmp, &[PolyglotKind::Rust, PolyglotKind::Go]).unwrap();
        let rust_count = targets.iter().filter(|t| t.kind == PolyglotKind::Rust).count();
        let go_count = targets.iter().filter(|t| t.kind == PolyglotKind::Go).count();
        assert_eq!(rust_count, 1);
        assert_eq!(go_count, 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn discover_python_yields_single_root() {
        let tmp = std::env::temp_dir().join(format!(
            "scry-bridge-polyglot-py-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let targets = discover(&tmp, &[PolyglotKind::Python]).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].root, tmp);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
