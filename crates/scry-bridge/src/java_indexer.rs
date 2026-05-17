//! Per-[`Compilation`] dispatch to javac+semanticdb-javac.
//!
//! Runs javac with the SemanticDB compiler plugin attached, dumping
//! per-source `.semanticdb` files into a shared targetroot. After
//! all compilations finish, a single `scip-java index-semanticdb`
//! pass walks the targetroot and emits one merged `.scip` file —
//! which scry imports as its `scip_index.bin` sidecar.
//!
//! This is the same two-stage architecture Kythe uses: per-compile
//! extractor → batch indexer. We split the stages because
//! semanticdb-javac is per-javac-invocation (it observes the
//! compiler), while SCIP-shape merging is a separate offline pass
//! over the captured shards.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Compilation, Language};

/// Configuration for the Java SCIP indexer dispatch.
#[derive(Debug, Clone)]
pub struct JavaIndexerConfig {
    /// Path to the `javac` binary. AOSP ships its own at
    /// `prebuilts/jdk/jdk21/linux-x86/bin/javac`; non-AOSP users
    /// pass their system javac.
    pub javac: PathBuf,
    /// Path to the `semanticdb-javac` jar (Sourcegraph's javac
    /// plugin). Lives at
    /// `~/.m2/repository/com/sourcegraph/semanticdb-javac/<ver>/semanticdb-javac-<ver>.jar`
    /// when installed via `install_indexers.sh`.
    pub semanticdb_javac_jar: PathBuf,
    /// Path to the `scip-java` binary — used in the merge step to
    /// convert the per-source `.semanticdb` files into one SCIP
    /// index.
    pub scip_java: PathBuf,
    /// Directory where per-source `.semanticdb` files land. One
    /// per compilation gets a subdir to prevent cross-compilation
    /// path collisions on duplicate source files (e.g. when two
    /// AOSP variants share the same .java file).
    pub targetroot: PathBuf,
    /// Source-version flag passed to javac (`-source N`). AOSP
    /// modules range from 8 to 21; defaults to 11 which compiles
    /// most of the corpus and is the SemanticDB plugin's lowest
    /// tested target.
    pub source_version: String,
    /// `-target N` flag. Matches `source_version` by default.
    pub target_version: String,
    /// Don't fail the dispatch when javac emits errors. AOSP modules
    /// can depend on platform-injected classpaths we don't fully
    /// reproduce (e.g. `@SystemApi` annotation classes shipped as
    /// resources); semanticdb-javac still writes partial output for
    /// every source it parsed before the first symbol error, which
    /// is good enough for symbol-identity narrowing in practice.
    pub tolerate_javac_errors: bool,
}

impl Default for JavaIndexerConfig {
    fn default() -> Self {
        Self {
            javac: PathBuf::from("javac"),
            semanticdb_javac_jar: dirs_semanticdb_javac()
                .unwrap_or_else(|| PathBuf::from("semanticdb-javac.jar")),
            scip_java: PathBuf::from("scip-java"),
            targetroot: std::env::temp_dir().join("scry-semanticdb"),
            source_version: "11".into(),
            target_version: "11".into(),
            tolerate_javac_errors: true,
        }
    }
}

/// Best-effort lookup for the highest installed semanticdb-javac jar
/// in `~/.m2`. Avoids forcing every caller to pin a version.
fn dirs_semanticdb_javac() -> Option<PathBuf> {
    let m2 = std::env::var_os("HOME")
        .map(|h| Path::new(&h).join(".m2/repository/com/sourcegraph/semanticdb-javac"))?;
    let mut found: Vec<PathBuf> = Vec::new();
    let read = std::fs::read_dir(&m2).ok()?;
    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() { continue; }
        if let Ok(inner) = std::fs::read_dir(&dir) {
            for e in inner.flatten() {
                let p = e.path();
                let name = p.file_name()?.to_string_lossy().to_string();
                if name.starts_with("semanticdb-javac-")
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

/// Result of running [`run`] over a set of compilations.
#[derive(Debug, Clone, Default)]
pub struct DispatchReport {
    pub total: usize,
    pub javac_ok: usize,
    pub javac_failed_but_partial: usize,
    pub javac_failed_no_output: usize,
    pub semanticdb_files_written: usize,
}

/// Run javac+semanticdb-javac for every Java/Kotlin compilation in
/// the input slice. Output `.semanticdb` files land under
/// `cfg.targetroot/<module>/`. Returns a report so the caller can
/// decide whether the coverage is acceptable.
///
/// Compilations execute in parallel via Rayon. Each compilation
/// produces its own per-module targetroot subdir so the parallel
/// runs don't write into the same destination, eliminating any
/// inter-compilation file races.
pub fn run(compilations: &[Compilation], cfg: &JavaIndexerConfig) -> Result<DispatchReport> {
    std::fs::create_dir_all(&cfg.targetroot)
        .with_context(|| format!("create targetroot {}", cfg.targetroot.display()))?;

    let outcomes: Vec<CompilationOutcome> = compilations
        .par_iter()
        .filter(|c| matches!(c.language, Language::Java))
        .map(|c| run_one(c, cfg))
        .collect();

    let mut report = DispatchReport {
        total: outcomes.len(),
        ..Default::default()
    };
    for o in outcomes {
        report.semanticdb_files_written += o.semanticdb_files;
        match o.status {
            CompilationStatus::Ok => report.javac_ok += 1,
            CompilationStatus::PartialOnError => report.javac_failed_but_partial += 1,
            CompilationStatus::NoOutput => report.javac_failed_no_output += 1,
        }
    }
    Ok(report)
}

/// Merge the per-compilation `.semanticdb` files into one SCIP file
/// using `scip-java index-semanticdb`. The output path is written
/// verbatim — callers point `scry scip-import` at it to land the
/// `scip_index.bin` sidecar.
pub fn merge(cfg: &JavaIndexerConfig, output_scip: &Path) -> Result<()> {
    let status = Command::new(&cfg.scip_java)
        .arg("index-semanticdb")
        .arg("--output").arg(output_scip)
        .arg(&cfg.targetroot)
        .status()
        .with_context(|| format!("spawn scip-java at {}", cfg.scip_java.display()))?;
    if !status.success() {
        anyhow::bail!("scip-java index-semanticdb exited with {status}");
    }
    Ok(())
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

fn run_one(compilation: &Compilation, cfg: &JavaIndexerConfig) -> CompilationOutcome {
    // Per-compilation targetroot under the shared root. Path-safe
    // module slug avoids collisions between sibling variants of the
    // same module.
    let slug = compilation.module.replace('/', "_");
    let per_target = cfg.targetroot.join(&slug);
    if let Err(e) = std::fs::create_dir_all(&per_target) {
        eprintln!("[scry-bridge] {}: mkdir {} failed: {e}",
                  compilation.module, per_target.display());
        return CompilationOutcome {
            status: CompilationStatus::NoOutput,
            semanticdb_files: 0,
        };
    }

    // Build javac command. Order is load-bearing:
    //   1. Source/target levels — semanticdb plugin needs them set.
    //   2. -processorpath — registers the plugin jar.
    //   3. -Xplugin — activates the SemanticdbPlugin and configures
    //      its sourceroot / targetroot / text emission.
    //   4. -classpath / -bootclasspath — from the Soong rule.
    //   5. -d <classes-dir> — javac needs to write class files
    //      somewhere even though we discard them.
    //   6. sources — fed via @argfile so we don't blow past the
    //      OS argv limit on big modules.
    let classes_dir = per_target.join("classes");
    let _ = std::fs::create_dir_all(&classes_dir);

    let mut cmd = Command::new(&cfg.javac);
    cmd.arg("-processorpath").arg(&cfg.semanticdb_javac_jar)
       .arg(format!(
           "-Xplugin:semanticdb -sourceroot:{} -targetroot:{} -text:on",
           compilation.source_root.display(),
           per_target.display(),
       ))
       .arg("-d").arg(&classes_dir);

    // Pass the build-system's classpath / bootclasspath tokens
    // verbatim. They're already javac-ready (one token per arg).
    for tok in &compilation.classpath {
        cmd.arg(tok);
    }
    for tok in &compilation.bootclasspath {
        cmd.arg(tok);
    }

    // Decide on the source/target version knob. javac refuses to
    // combine `--release` with `--system=` or `-bootclasspath`, so
    // when the build provided a custom bootclasspath we fall back to
    // -source/-target. Otherwise --release is the modern single-knob
    // that pins the system modules too — load-bearing on JDK21+ where
    // -source 11 alone can't resolve `java.lang.String`.
    let uses_custom_boot = compilation.bootclasspath.iter().any(|t| {
        t.starts_with("--system") || t.starts_with("-bootclasspath")
    });
    let release_version = compilation.java_version
        .clone().unwrap_or_else(|| cfg.source_version.clone());
    if uses_custom_boot {
        cmd.arg(format!("-source")).arg(&release_version)
           .arg(format!("-target")).arg(&release_version);
    } else {
        cmd.arg(format!("--release={release_version}"));
    }

    // We don't pass -Werror or other lint flags from Soong's
    // `javacFlags`. semanticdb-javac runs as a compiler PLUGIN, not
    // an annotation processor — it observes every source the compiler
    // successfully parsed, then writes .semanticdb files irrespective
    // of later type-resolution errors. So a javac that aborts mid-TU
    // still leaves us with the symbols it had already resolved. Lint
    // flags only ever turn that into FEWER outputs.
    // Sources from an @argfile to dodge ARG_MAX (~2MB Linux default).
    let argfile = per_target.join("sources.args");
    let args_body: String = compilation.sources.iter()
        .map(|s| compilation.source_root.join(s).display().to_string())
        .collect::<Vec<_>>().join("\n");
    if std::fs::write(&argfile, args_body).is_err() {
        return CompilationOutcome {
            status: CompilationStatus::NoOutput, semanticdb_files: 0,
        };
    }
    cmd.arg(format!("@{}", argfile.display()));

    // Don't let extra_args fold in here — Soong's `javacFlags` often
    // include `-Werror`, `-Xlint:all`, and module-specific settings
    // that conflict with our `-source/-target` and the plugin
    // invocation. Production-grade integration would whitelist a
    // safe subset; for symbol-identity output the defaults are fine.
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[scry-bridge] {}: spawn javac failed: {e}", compilation.module);
            return CompilationOutcome {
                status: CompilationStatus::NoOutput, semanticdb_files: 0,
            };
        }
    };

    let semanticdb_files = count_semanticdb_files(&per_target);
    let ok = output.status.success();
    let status = if ok {
        CompilationStatus::Ok
    } else if semanticdb_files > 0 && cfg.tolerate_javac_errors {
        CompilationStatus::PartialOnError
    } else {
        CompilationStatus::NoOutput
    };
    if matches!(status, CompilationStatus::NoOutput) && !output.stderr.is_empty() {
        // Surface the first stderr line so the operator can see what
        // went wrong without combing through per-module logs.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("");
        eprintln!("[scry-bridge] {}: javac no output: {}",
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
    fn count_semanticdb_files_counts_only_target_files() {
        let tmp = std::env::temp_dir().join(format!(
            "scry-bridge-count-{}", std::process::id()));
        let _ = std::fs::create_dir_all(tmp.join("META-INF/semanticdb"));
        std::fs::write(
            tmp.join("META-INF/semanticdb/Foo.java.semanticdb"), b"x").unwrap();
        std::fs::write(tmp.join("README.md"), b"hi").unwrap();
        assert_eq!(count_semanticdb_files(&tmp), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn config_default_finds_semanticdb_javac_if_installed() {
        // Side-effect free — just exercises the path-walking logic.
        let cfg = JavaIndexerConfig::default();
        // Don't assert presence; the lookup may legitimately miss
        // on a host that hasn't installed the jar yet.
        let _ = cfg.semanticdb_javac_jar;
    }
}
