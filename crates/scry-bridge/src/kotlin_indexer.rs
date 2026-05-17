//! Per-[`Compilation`] dispatch to kotlinc+semanticdb-kotlinc.
//!
//! Symmetric to [`crate::java_indexer`] but for Kotlin source. Runs
//! kotlinc (via the embeddable jar — see [`KotlinIndexerConfig::kotlinc`]
//! for why) with the SemanticDB Kotlin plugin attached, dumping
//! per-source `.semanticdb` files into the shared targetroot. The
//! final SCIP merge is done by the same `scip-java index-semanticdb`
//! pass that the Java indexer's outputs go through — semanticdb
//! is the lingua-franca of the JVM SCIP world, so one merge step
//! handles both.
//!
//! ## Embeddable vs non-embeddable
//!
//! Sourcegraph's `semanticdb-kotlinc` plugin was built against the
//! Kotlin compiler's **embeddable** artifact (the shaded
//! distribution that JetBrains publishes to Maven Central). Its
//! bytecode references classes at
//! `org.jetbrains.kotlin.com.intellij.*` — the shaded relocation
//! prefix the embeddable jar uses. The non-embeddable
//! `kotlin-compiler.jar` that ships in the official `kotlinc` ZIP
//! dropped that shading in Kotlin 2.0+, so plugins built against
//! the embeddable surface crash with `NoClassDefFoundError` when
//! loaded by the stock launcher. The [`install_indexers.sh`] script
//! installs `kotlinc-embeddable` — a shell wrapper that launches
//! the same compiler with the embeddable jar — and we default to
//! it here.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Compilation, Language};

/// Configuration for the Kotlin SCIP indexer dispatch.
#[derive(Debug, Clone)]
pub struct KotlinIndexerConfig {
    /// Path to the kotlinc launcher. **Must** load the embeddable
    /// jar — the install script provides this as `kotlinc-embeddable`.
    /// See the module docs for why the stock `kotlinc` doesn't work.
    pub kotlinc: PathBuf,
    /// Path to the `semanticdb-kotlinc` plugin jar (built from
    /// sourcegraph/scip-kotlin via `sbt kotlinc/publishM2`).
    pub semanticdb_kotlinc_jar: PathBuf,
    /// Where the per-compilation `.semanticdb` shards land. Shared
    /// with [`crate::java_indexer::JavaIndexerConfig::targetroot`]
    /// so one `scip-java index-semanticdb` pass handles both
    /// languages.
    pub targetroot: PathBuf,
    /// `-jvm-target` (kotlinc's analogue of javac's `-target`).
    /// Falls back to the compilation's `java_version` field when
    /// the build provided one (Soong's `kotlinJvmTarget` binding).
    pub jvm_target: String,
    /// Don't fail the dispatch when kotlinc emits errors. Same
    /// rationale as the Java indexer's analogous knob: the plugin
    /// writes `.semanticdb` files per source as analysis succeeds,
    /// so we still recover symbol identities from a TU that later
    /// failed name resolution.
    pub tolerate_kotlinc_errors: bool,
}

impl Default for KotlinIndexerConfig {
    fn default() -> Self {
        Self {
            kotlinc: PathBuf::from("kotlinc-embeddable"),
            semanticdb_kotlinc_jar: dirs_semanticdb_kotlinc()
                .unwrap_or_else(|| PathBuf::from("semanticdb-kotlinc.jar")),
            targetroot: crate::scry_tmp_dir().join("scry-semanticdb"),
            jvm_target: "21".into(),
            tolerate_kotlinc_errors: true,
        }
    }
}

/// Best-effort lookup for the highest installed semanticdb-kotlinc
/// jar under `~/.m2`. The Maven layout is one version-subdir per
/// release, and we want the most recent.
fn dirs_semanticdb_kotlinc() -> Option<PathBuf> {
    let m2 = std::env::var_os("HOME")
        .map(|h| Path::new(&h)
            .join(".m2/repository/com/sourcegraph/semanticdb-kotlinc"))?;
    let mut found: Vec<PathBuf> = Vec::new();
    let read = std::fs::read_dir(&m2).ok()?;
    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() { continue; }
        if let Ok(inner) = std::fs::read_dir(&dir) {
            for e in inner.flatten() {
                let p = e.path();
                let name = p.file_name()?.to_string_lossy().to_string();
                if name.starts_with("semanticdb-kotlinc-")
                    && name.ends_with(".jar")
                    && !name.ends_with("-sources.jar")
                    && !name.ends_with("-javadoc.jar")
                {
                    found.push(p);
                }
            }
        }
    }
    found.sort();
    found.pop()
}

/// Result of running [`run`] over a set of compilations. Mirrors the
/// Java indexer's report so the orchestrator can pretty-print both
/// in one consistent shape.
#[derive(Debug, Clone, Default)]
pub struct DispatchReport {
    pub total: usize,
    pub kotlinc_ok: usize,
    pub kotlinc_failed_but_partial: usize,
    pub kotlinc_failed_no_output: usize,
    pub semanticdb_files_written: usize,
}

/// Run kotlinc + semanticdb-kotlinc for every Kotlin compilation in
/// the input slice. Compilations execute in parallel via Rayon.
/// Each compilation writes to its own per-module targetroot subdir
/// to keep parallel writers off the same destination tree.
pub fn run(
    compilations: &[Compilation],
    cfg: &KotlinIndexerConfig,
) -> Result<DispatchReport> {
    std::fs::create_dir_all(&cfg.targetroot)
        .with_context(|| format!("create targetroot {}", cfg.targetroot.display()))?;

    let outcomes: Vec<CompilationOutcome> = compilations
        .par_iter()
        .filter(|c| matches!(c.language, Language::Kotlin))
        .map(|c| run_one(c, cfg))
        .collect();

    let mut report = DispatchReport {
        total: outcomes.len(),
        ..Default::default()
    };
    for o in outcomes {
        report.semanticdb_files_written += o.semanticdb_files;
        match o.status {
            CompilationStatus::Ok => report.kotlinc_ok += 1,
            CompilationStatus::PartialOnError => report.kotlinc_failed_but_partial += 1,
            CompilationStatus::NoOutput => report.kotlinc_failed_no_output += 1,
        }
    }
    Ok(report)
}

#[derive(Debug)]
enum CompilationStatus {
    Ok,
    PartialOnError,
    NoOutput,
}

#[derive(Debug)]
struct CompilationOutcome {
    status: CompilationStatus,
    semanticdb_files: usize,
}

fn run_one(compilation: &Compilation, cfg: &KotlinIndexerConfig) -> CompilationOutcome {
    let slug = compilation.module.replace('/', "_");
    let per_target = cfg.targetroot.join(&slug);
    let classes_dir = per_target.join("classes");
    if let Err(e) = std::fs::create_dir_all(&classes_dir) {
        eprintln!("[scry-bridge] {}: mkdir {} failed: {e}",
                  compilation.module, classes_dir.display());
        return CompilationOutcome {
            status: CompilationStatus::NoOutput,
            semanticdb_files: 0,
        };
    }

    // Resolve every source path to absolute. kotlinc's classpath
    // resolution depends on cwd, but the file args do not — pinning
    // them to absolute paths lets us run from anywhere.
    let abs_sources: Vec<PathBuf> = compilation.sources.iter()
        .map(|s| compilation.source_root.join(s))
        .collect();

    let jvm_target = compilation.java_version
        .clone().unwrap_or_else(|| cfg.jvm_target.clone());

    let mut cmd = Command::new(&cfg.kotlinc);
    cmd.arg(format!("-Xplugin={}", cfg.semanticdb_kotlinc_jar.display()))
       .arg("-P").arg(format!(
           "plugin:semanticdb-kotlinc:sourceroot={}",
           compilation.source_root.display(),
       ))
       .arg("-P").arg(format!(
           "plugin:semanticdb-kotlinc:targetroot={}",
           per_target.display(),
       ))
       .arg("-d").arg(&classes_dir)
       .arg("-no-stdlib")
       .arg("-jvm-target").arg(&jvm_target);

    // Soong-derived classpath tokens. The kotlinc-side classpath
    // shape is `-classpath J1:J2:...` which the soong bridge
    // already emits.
    for tok in &compilation.classpath {
        cmd.arg(tok);
    }

    // Sources via @argfile to dodge ARG_MAX on big Kotlin modules.
    let argfile = per_target.join("sources.args");
    let args_body: String = abs_sources.iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>().join("\n");
    if std::fs::write(&argfile, args_body).is_err() {
        return CompilationOutcome {
            status: CompilationStatus::NoOutput, semanticdb_files: 0,
        };
    }
    cmd.arg(format!("@{}", argfile.display()));

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[scry-bridge] {}: spawn kotlinc failed: {e}", compilation.module);
            return CompilationOutcome {
                status: CompilationStatus::NoOutput, semanticdb_files: 0,
            };
        }
    };

    let semanticdb_files = count_semanticdb_files(&per_target);
    let ok = output.status.success();
    let status = if ok {
        CompilationStatus::Ok
    } else if semanticdb_files > 0 && cfg.tolerate_kotlinc_errors {
        CompilationStatus::PartialOnError
    } else {
        CompilationStatus::NoOutput
    };
    if matches!(status, CompilationStatus::NoOutput) && !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("");
        eprintln!("[scry-bridge] {}: kotlinc no output: {}",
                  compilation.module, first);
    }
    CompilationOutcome { status, semanticdb_files }
}

fn count_semanticdb_files(root: &Path) -> usize {
    let mut count = 0;
    for entry in ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && entry.file_name().to_string_lossy().ends_with(".semanticdb")
        {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_finds_semanticdb_kotlinc_if_installed() {
        let cfg = KotlinIndexerConfig::default();
        let _ = cfg.semanticdb_kotlinc_jar;
    }

    #[test]
    fn count_semanticdb_files_counts_only_target_files() {
        let tmp = std::env::temp_dir().join(format!(
            "scry-bridge-kotlin-count-{}", std::process::id()));
        let _ = std::fs::create_dir_all(tmp.join("META-INF/semanticdb"));
        std::fs::write(
            tmp.join("META-INF/semanticdb/Foo.kt.semanticdb"), b"x").unwrap();
        std::fs::write(tmp.join("README.md"), b"hi").unwrap();
        assert_eq!(count_semanticdb_files(&tmp), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
