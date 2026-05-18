//! End-to-end orchestration: kzip path in, packed sidecars + summary
//! log out.
//!
//! Six phases, each logged as `[scry-kzip] phase N/6: ...` to stderr:
//!
//! 1. walk kzip + bucket CUs by indexer kind
//! 2. spawn indexers in parallel (rayon, capped at num_cpus/2)
//! 3. decode each indexer's stdout into `DecodedRecord`s
//! 4. flush packed sidecars
//! 5. write `kythe-logs/summary.txt`
//! 6. return `KzipBuildReport`
//!
//! Steps 2 and 3 are interleaved per-CU — we don't buffer the whole
//! decoded record stream in memory.
//!
//! ## Env knobs (testing / dev)
//!
//! * `SCRY_KZIP_LANGS=cxx,go` — comma-separated list. Only CUs whose
//!   indexer kind label appears here will be processed; everything
//!   else is dropped from the run before phase 2.
//! * `SCRY_KZIP_MAX_UNITS=50` — cap total CUs (first N after the
//!   language filter). Used by the smoke test.

use crate::dispatch::{self, IndexerKind};
use crate::emit::{EmitReport, PackedEmitter};
use crate::entries::decode_stream;
use crate::indexer::{
    build_per_cu_kzip, recommended_workers, resolve_indexer, run_indexer,
};
use crate::walker::{walk_units, KzipUnit};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

/// What the driver hands back when it's done.
#[derive(Debug, Clone)]
pub struct KzipBuildReport {
    /// One entry per indexer label (`cxx`, `java`, `jvm`, `go`,
    /// `proto`, `textproto`, `skip`). Order is stable: insertion order
    /// of the dispatcher.
    pub per_lang: Vec<LangReport>,
    pub emit: EmitReport,
    pub wall_secs: f64,
}

/// Per-language slice of the report.
#[derive(Debug, Clone)]
pub struct LangReport {
    pub label: &'static str,
    pub cu_count: usize,
    pub indexer_ok: usize,
    pub indexer_empty: usize,
    pub indexer_failed: usize,
    pub wall_secs: f64,
    /// True if every CU under this label is being skipped on purpose
    /// (`IndexerKind::Skip(_)`).
    pub is_skipped_kind: bool,
    /// First Skip reason we saw, for the summary log. Empty if not
    /// applicable.
    pub skip_reason: String,
}

/// Build packed sidecars from `kzip`. `out_dir` is the scry index
/// directory the resulting `.bin` sidecars will live in.
///
/// Writes per-language indexer stderr to `out_dir/kythe-logs/<lang>.log`
/// and a summary to `out_dir/kythe-logs/summary.txt`.
pub fn build_packed_from_kzip(
    kzip: &Path,
    out_dir: &Path,
    kythe_root: &Path,
) -> Result<KzipBuildReport> {
    let t_total = Instant::now();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("mkdir {}", out_dir.display()))?;
    let logs_dir = out_dir.join("kythe-logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("mkdir {}", logs_dir.display()))?;

    eprintln!("[scry-kzip] phase 1/6: walking {}", kzip.display());
    let mut units = walk_units(kzip).context("walk kzip units")?;
    let total_units = units.len();
    eprintln!("[scry-kzip] phase 1/6: found {} CUs", total_units);

    // Apply env-knob filters.
    apply_env_filters(&mut units);
    let post_filter = units.len();
    if post_filter != total_units {
        eprintln!(
            "[scry-kzip] phase 1/6: after env filters ({} CUs left)",
            post_filter,
        );
    }

    // Bucket by indexer kind. Skipped kinds get their own bucket so
    // the summary still records "X CUs skipped (reason)".
    let mut by_kind: HashMap<&'static str, BucketEntry> = HashMap::new();
    for unit in units {
        let kind = dispatch::choose_for(&unit);
        let label = kind.label();
        let entry = by_kind.entry(label).or_insert_with(|| BucketEntry {
            label,
            kind,
            units: Vec::new(),
            skip_reason: match kind {
                IndexerKind::Skip(msg) => msg.to_string(),
                _ => String::new(),
            },
        });
        entry.units.push(unit);
    }

    // Phase 2: pre-resolve each indexer binary so we fail fast if
    // any are missing. Skipped buckets short-circuit.
    eprintln!(
        "[scry-kzip] phase 2/6: dispatching {} kinds ({})",
        by_kind.len(),
        by_kind.values().map(|b| format!("{}={}", b.label, b.units.len()))
            .collect::<Vec<_>>().join(", "),
    );
    for bucket in by_kind.values() {
        if !bucket.kind.is_runnable() { continue; }
        if let Err(e) = resolve_indexer(kythe_root, bucket.kind) {
            return Err(e).with_context(|| format!(
                "resolve indexer for kind {}", bucket.label,
            ));
        }
    }

    // Phase 3: per-CU indexer dispatch, streaming decode into the
    // shared emitter. Rayon worker pool is capped at num_cpus/2 so
    // JVM-based indexers don't OOM the host.
    let workers = recommended_workers();
    eprintln!(
        "[scry-kzip] phase 3/6: running indexers with {} workers",
        workers,
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .context("build rayon worker pool")?;
    let emitter = PackedEmitter::new();
    let lang_reports: Mutex<Vec<LangReport>> = Mutex::new(Vec::new());

    // Where per-CU sub-kzips land. Cleaned up at the end of the run.
    let staging = scry_bridge::scry_tmp_dir()
        .join(format!("scry-kzip-staging-{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("mkdir staging {}", staging.display()))?;

    pool.install(|| {
        for (label, bucket) in &by_kind {
            let t_lang = Instant::now();
            let log_path = logs_dir.join(format!("{label}.log"));
            if !bucket.kind.is_runnable() {
                lang_reports.lock().unwrap().push(LangReport {
                    label: bucket.label,
                    cu_count: bucket.units.len(),
                    indexer_ok: 0,
                    indexer_empty: 0,
                    indexer_failed: 0,
                    wall_secs: 0.0,
                    is_skipped_kind: true,
                    skip_reason: bucket.skip_reason.clone(),
                });
                let _ = std::fs::write(
                    &log_path,
                    format!(
                        "{} CUs skipped: {}\n", bucket.units.len(), bucket.skip_reason,
                    ),
                );
                eprintln!(
                    "[scry-kzip] phase 3/6: {} ({} CUs skipped — {})",
                    label, bucket.units.len(), bucket.skip_reason,
                );
                continue;
            }
            eprintln!(
                "[scry-kzip] phase 3/6: {} ({} CUs)", label, bucket.units.len(),
            );
            let log_file = Mutex::new(
                std::fs::File::create(&log_path)
                    .with_context(|| format!("create {}", log_path.display()))
                    .ok(),
            );
            let counters = Mutex::new((0usize, 0usize, 0usize));
            let n_units = bucket.units.len();
            bucket.units.par_iter().enumerate().for_each(|(i, unit)| {
                run_one_cu(
                    unit, bucket.kind, &staging, kythe_root,
                    &emitter, &log_file, &counters, i + 1, n_units, label,
                );
            });
            let (ok, empty, failed) = *counters.lock().unwrap();
            lang_reports.lock().unwrap().push(LangReport {
                label: bucket.label,
                cu_count: n_units,
                indexer_ok: ok,
                indexer_empty: empty,
                indexer_failed: failed,
                wall_secs: t_lang.elapsed().as_secs_f64(),
                is_skipped_kind: false,
                skip_reason: String::new(),
            });
            eprintln!(
                "[scry-kzip] phase 3/6: {} done ({} ok, {} empty, {} failed in {:.1}s)",
                label, ok, empty, failed, t_lang.elapsed().as_secs_f64(),
            );
        }
    });

    // Phase 4: flush sidecars.
    eprintln!("[scry-kzip] phase 4/6: writing packed sidecars");
    let emit = emitter
        .finalize(out_dir)
        .context("finalize packed sidecars")?;
    eprintln!(
        "[scry-kzip] phase 4/6: clang_usrs={} records ({} symbols), \
         scip_index={} records ({} symbols)",
        emit.cxx_records, emit.cxx_symbols,
        emit.scip_records, emit.scip_symbols,
    );

    // Phase 5: summary log.
    eprintln!("[scry-kzip] phase 5/6: writing summary log");
    let lang_reports_vec: Vec<LangReport> = {
        let mut v = lang_reports.into_inner().unwrap();
        v.sort_by_key(|r| r.label);
        v
    };
    write_summary_log(&logs_dir, kzip, &lang_reports_vec, &emit)?;

    // Best-effort cleanup of per-CU staging kzips.
    let _ = std::fs::remove_dir_all(&staging);

    let wall_secs = t_total.elapsed().as_secs_f64();
    eprintln!("[scry-kzip] phase 6/6: done in {:.1}s", wall_secs);
    Ok(KzipBuildReport {
        per_lang: lang_reports_vec,
        emit,
        wall_secs,
    })
}

/// Apply `SCRY_KZIP_LANGS` and `SCRY_KZIP_MAX_UNITS` env knobs.
/// Modifies `units` in place.
fn apply_env_filters(units: &mut Vec<KzipUnit>) {
    if let Ok(spec) = std::env::var("SCRY_KZIP_LANGS") {
        let allowed: std::collections::HashSet<String> = spec
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        units.retain(|u| {
            allowed.contains(dispatch::choose_for(u).label())
        });
    }
    if let Ok(s) = std::env::var("SCRY_KZIP_MAX_UNITS") {
        if let Ok(n) = s.parse::<usize>() {
            units.truncate(n);
        }
    }
}

struct BucketEntry {
    label: &'static str,
    kind: IndexerKind,
    units: Vec<KzipUnit>,
    /// Populated only when kind is `Skip(_)`.
    skip_reason: String,
}

#[allow(clippy::too_many_arguments)]
fn run_one_cu(
    unit: &KzipUnit,
    kind: IndexerKind,
    staging: &Path,
    kythe_root: &Path,
    emitter: &PackedEmitter,
    log_file: &Mutex<Option<std::fs::File>>,
    counters: &Mutex<(usize, usize, usize)>,
    seq: usize,
    total: usize,
    label: &str,
) {
    // Build the per-CU sub-kzip.
    let cu_kzip = match build_per_cu_kzip(&unit.kzip_path, &unit.unit_sha, staging) {
        Ok(p) => p,
        Err(e) => {
            log_to(log_file, format!("[{} {}/{}] build-cu-kzip failed: {e}\n",
                label, seq, total));
            counters.lock().unwrap().2 += 1;
            return;
        }
    };
    let run_res = run_indexer(&cu_kzip, &unit.unit_sha, kind, kythe_root);
    let _ = std::fs::remove_file(&cu_kzip);
    let run = match run_res {
        Ok(r) => r,
        Err(e) => {
            log_to(log_file, format!("[{} {}/{}] spawn failed: {e}\n",
                label, seq, total));
            counters.lock().unwrap().2 += 1;
            return;
        }
    };
    // Always note the run's exit/wall for the log.
    log_to(log_file, format!(
        "[{} {}/{}] sha={} exit={} wall={:.2}s stdout={}B stderr_tail={}B\n",
        label, seq, total, &run.unit_sha, run.exit_code, run.wall_secs,
        run.stdout.len(), run.stderr_tail.len(),
    ));
    if !run.stderr_tail.is_empty() {
        log_to(log_file, format!(
            "    stderr: {}\n",
            String::from_utf8_lossy(&run.stderr_tail).replace('\n', " | "),
        ));
    }
    if !run.produced_entries() {
        // No entries: either a hard fail (exit != 0) or a clean
        // run that produced no symbols (rare but possible).
        if run.exit_code != 0 { counters.lock().unwrap().2 += 1; }
        else { counters.lock().unwrap().1 += 1; }
        return;
    }
    // Stream-decode the entries into the emitter.
    let decoded = decode_stream(&run.stdout[..], |rec| {
        emitter.record_decoded(kind, &rec);
    });
    match decoded {
        Ok(_) => counters.lock().unwrap().0 += 1,
        Err(e) => {
            log_to(log_file, format!(
                "[{} {}/{}] decode failed: {e}\n", label, seq, total,
            ));
            counters.lock().unwrap().2 += 1;
        }
    }
    // Progress every 50 completions (not every 50th input index — with
    // rayon, items finish out of order, so a `seq % 50` check would
    // only fire when items #50, #100, ... happened to complete, which
    // may be very late). Sum the counters and print on multiples of 50.
    let _ = seq; // input-position-based progress would be misleading; see above
    let (ok, empty, failed) = *counters.lock().unwrap();
    let done = ok + empty + failed;
    if done % 50 == 0 || done == total {
        eprintln!(
            "[scry-kzip] phase 3/6: {} progress {}/{} ({} ok, {} empty, {} failed)",
            label, done, total, ok, empty, failed,
        );
    }
}

fn log_to(log_file: &Mutex<Option<std::fs::File>>, msg: String) {
    if let Some(f) = log_file.lock().unwrap().as_mut() {
        let _ = f.write_all(msg.as_bytes());
    }
}

fn write_summary_log(
    logs_dir: &Path,
    kzip: &Path,
    per_lang: &[LangReport],
    emit: &EmitReport,
) -> Result<()> {
    let out = logs_dir.join("summary.txt");
    let mut f = std::fs::File::create(&out)
        .with_context(|| format!("create {}", out.display()))?;
    writeln!(f, "scry-kzip summary")?;
    writeln!(f, "  source kzip: {}", kzip.display())?;
    writeln!(f, "  emitter:")?;
    writeln!(f, "    clang_usrs.bin: {} records, {} unique symbols",
        emit.cxx_records, emit.cxx_symbols)?;
    writeln!(f, "    scip_index.bin: {} records, {} unique symbols",
        emit.scip_records, emit.scip_symbols)?;
    writeln!(f, "  per-language:")?;
    for r in per_lang {
        if r.is_skipped_kind {
            writeln!(f, "    {:<10} {:>5} CUs  SKIPPED: {}",
                r.label, r.cu_count, r.skip_reason)?;
        } else {
            writeln!(
                f,
                "    {:<10} {:>5} CUs  {:>4} ok  {:>4} empty  {:>4} failed  {:>6.1}s",
                r.label, r.cu_count, r.indexer_ok, r.indexer_empty,
                r.indexer_failed, r.wall_secs,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `apply_env_filters` respects `SCRY_KZIP_LANGS` + `SCRY_KZIP_MAX_UNITS`
    /// in tandem. We can't easily test against a real kzip from a unit
    /// test, but we can build a tiny `Vec<KzipUnit>` and confirm the
    /// filter behaviour.
    #[test]
    fn env_filters_compose() {
        // Save + restore env so this test doesn't leak settings.
        let prev_langs = std::env::var("SCRY_KZIP_LANGS").ok();
        let prev_max = std::env::var("SCRY_KZIP_MAX_UNITS").ok();
        // Note: we set the envs and then iterate — this is best-effort
        // because Rust's test runner shares a process. The lock is
        // SetVar-then-immediately-read, which is racy across tests
        // but fine for the single assertion below.
        std::env::set_var("SCRY_KZIP_LANGS", "go");
        std::env::set_var("SCRY_KZIP_MAX_UNITS", "1");
        let mut u = vec![
            KzipUnit { kzip_path: PathBuf::from("/x.kzip"), unit_sha: "a".into(),
                language: "go".into(), has_class_or_jar_input: false },
            KzipUnit { kzip_path: PathBuf::from("/x.kzip"), unit_sha: "b".into(),
                language: "go".into(), has_class_or_jar_input: false },
            KzipUnit { kzip_path: PathBuf::from("/x.kzip"), unit_sha: "c".into(),
                language: "rust".into(), has_class_or_jar_input: false },
        ];
        apply_env_filters(&mut u);
        // langs=go → drop rust → 2 left. max=1 → truncate to 1.
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].unit_sha, "a");
        // Restore.
        match prev_langs {
            Some(v) => std::env::set_var("SCRY_KZIP_LANGS", v),
            None => std::env::remove_var("SCRY_KZIP_LANGS"),
        }
        match prev_max {
            Some(v) => std::env::set_var("SCRY_KZIP_MAX_UNITS", v),
            None => std::env::remove_var("SCRY_KZIP_MAX_UNITS"),
        }
    }
}
