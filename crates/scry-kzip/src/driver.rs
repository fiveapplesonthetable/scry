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
//! ## Walk strategy
//!
//! Phase 1 streams entries through `walker::walk_units_serial`:
//!
//! * **Bounded** (`SCRY_KZIP_MAX_UNITS=N`) — early `break` after N
//!   accepted units. The smoke test hits a `--max-units 3` ceiling
//!   so paying for the full walk would be pure waste.
//! * **Unbounded** — drain the iterator end to end.
//!
//! A language pre-peek (cheap O(small constant) probe over the raw
//! unit bytes — see [`walker_peek`](crate::walker_peek)) skips CUs
//! whose dispatched indexer label isn't in `SCRY_KZIP_LANGS`
//! *before* paying the full proto / JSON decode cost.
//!
//! We deliberately do NOT fan the walk out across rayon workers,
//! even though [`walker::walk_units_parallel`] exists. On large
//! AOSP-shaped kzips (~600 K zip entries, ~120 K compilation units)
//! parallel access to `zip::ZipArchive` is dominated by per-entry
//! `seek + read` syscalls — each worker holds its own `BufReader`,
//! but the underlying file is randomly seeked tens of thousands of
//! times and the kernel can't prefetch. The parallel path measured
//! 2–10x SLOWER than serial at every worker count from 2 to 72 on
//! the cuttlefish AOSP kzip. Serial with a 256 KiB `BufReader` per
//! the walker's plumbing (see `walker::ZIP_READ_BUF`) walks 118 K
//! unit entries in ~20 s; phase 3's indexer dispatch is where the
//! real CPU parallelism lives.
//!
//! `walker::walk_units_parallel` is retained as a public API for
//! callers (test suite, future smaller-kzip ingest paths) where the
//! contention profile may differ.
//!
//! ## Env knobs (testing / dev)
//!
//! * `SCRY_KZIP_PATH_PREFIX=frameworks/base/,frameworks/native/` —
//!   comma-separated list of path prefixes. Only CUs whose
//!   `primary_path` (the first source-extension `required_input`)
//!   starts with at least one prefix are kept. Used to scope an
//!   ingest to a subtree of the repo (e.g. just `frameworks/base/`
//!   for a faster targeted run).
//! * `SCRY_KZIP_PATH_EXCLUDE=external/,prebuilts/` — comma-separated
//!   list of path prefixes that drop matching CUs. Evaluated BEFORE
//!   the include filter, so excludes win. Useful to skip vendor /
//!   third-party code without listing every wanted prefix explicitly.
//! * `SCRY_KZIP_LANGS=cxx,go` — comma-separated list. Only CUs whose
//!   indexer kind label appears here will be processed; everything
//!   else is dropped from the run before phase 2.
//! * `SCRY_KZIP_MAX_UNITS=50` — cap total CUs (first N after the
//!   language filter). Used by the smoke test; also forces the
//!   serial walk path.

use crate::dispatch::IndexerKind;
use crate::driver_resume::{enforce_resume_policy, parse_checkpoint_every_env};
use crate::driver_walk::walk_and_bucket;
use crate::emit::{EmitReport, PackedEmitter};
use crate::emit_checkpoint::{
    self, CheckpointManifest, CHECKPOINT_SUBDIR, DEFAULT_MANIFEST_EVERY,
};
// `DecodedRecord`s are produced by `crate::entries::decode_stream`,
// which `run_indexer` calls internally; this file only handles the
// already-decoded vector via `IndexerRun.decoded`.
use crate::indexer::{
    build_per_cu_kzip, recommended_workers, resolve_indexer, run_indexer,
};
use crate::walker::KzipUnit;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashSet;
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
    /// Distinct label per bucket. For runnable buckets this is the
    /// `IndexerKind` label (`cxx`, `java`, …). For skip buckets it's
    /// `skip-<source language>` so the summary shows separate
    /// `skip-rust` and `skip-kotlin` rows.
    pub label: String,
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
///
/// `resume = true` opts into reading an existing on-disk checkpoint at
/// `<out_dir>/kythe-logs/checkpoint/` and continuing it; `resume =
/// false` insists on a fresh checkpoint and refuses to overwrite one
/// that already exists. See [`enforce_resume_policy`] for the
/// three-state validator (fresh / mismatched / matched).
///
/// `workers` overrides the phase-3 rayon pool size. `None` falls back
/// to `SCRY_KZIP_WORKERS` if set, then [`recommended_workers`] (capped
/// at `num_cpus/2` to keep JVM-based indexers from OOM'ing the host).
/// `Some(N)` is honored verbatim — callers who pass a value have
/// taken responsibility for the resident-set arithmetic.
///
/// `source_root` is the on-disk prefix to prepend to Kythe's
/// corpus-relative paths (`frameworks/base/...` →
/// `<source_root>/frameworks/base/...`). MANDATORY for the sidecars
/// to be usable by `scry ref` / `callers` queries — without it the
/// stored `abs_path` is corpus-relative while queries pass absolute
/// filesystem paths, so every precision lookup misses.
pub fn build_packed_from_kzip(
    kzip: &Path,
    out_dir: &Path,
    kythe_root: &Path,
    resume: bool,
    workers: Option<usize>,
    source_root: &Path,
) -> Result<KzipBuildReport> {
    let t_total = Instant::now();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("mkdir {}", out_dir.display()))?;
    let logs_dir = out_dir.join("kythe-logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("mkdir {}", logs_dir.display()))?;

    // Three-state checkpoint validator runs BEFORE phase 1 so a
    // misconfigured run fails in microseconds, not after a half-hour
    // walk.
    let checkpoint_dir = out_dir.join(CHECKPOINT_SUBDIR);
    let current_manifest = CheckpointManifest::fresh(
        emit_checkpoint::KzipFingerprint::probe(kzip)?,
        emit_checkpoint::snapshot_env(),
        kythe_root,
        Some(source_root),
    );
    let resume_decision = enforce_resume_policy(
        resume, &checkpoint_dir, &current_manifest,
    )?;

    let langs_filter = parse_kzip_langs_env();
    let max_units = parse_kzip_max_units_env();
    let path_include = parse_kzip_path_prefix_env();
    let path_exclude = parse_kzip_path_exclude_env();
    if let Some(prefixes) = &path_include {
        eprintln!(
            "[scry-kzip] phase 1/6: SCRY_KZIP_PATH_PREFIX active ({} prefixes)",
            prefixes.len(),
        );
    }
    if let Some(prefixes) = &path_exclude {
        eprintln!(
            "[scry-kzip] phase 1/6: SCRY_KZIP_PATH_EXCLUDE active ({} prefixes)",
            prefixes.len(),
        );
    }

    // Build the emitter. Resuming attaches the existing checkpoint;
    // a fresh run creates the checkpoint dir + files lazily on the
    // first CU commit (but we open the log handles up front so a
    // permission failure surfaces before phase 1).
    //
    // When resuming we hand `build_checkpoint_state` the ON-DISK
    // manifest (which carries the committed `done_shas` list) rather
    // than the fresh-from-this-process one. Both have the same
    // fingerprint (the validator just confirmed that), but the
    // on-disk one is the only place the committed-CU SHAs survive
    // across processes.
    let manifest_every = parse_checkpoint_every_env().unwrap_or(DEFAULT_MANIFEST_EVERY);
    let seed_manifest = if resume_decision.is_resume() {
        CheckpointManifest::load(&checkpoint_dir.join("manifest.json"))
            .with_context(|| format!(
                "load on-disk manifest at {}", checkpoint_dir.display(),
            ))?
    } else {
        current_manifest.clone()
    };
    let checkpoint_state = PackedEmitter::build_checkpoint_state(
        &checkpoint_dir, seed_manifest, manifest_every,
    )?;
    let emitter = PackedEmitter::with_checkpoint(checkpoint_state)
        .with_source_root(source_root.to_path_buf());

    // On resume, replay the on-disk record log into the in-memory
    // buckets BEFORE phase 1 so the done-set is populated when the
    // walker filter is applied.
    let resumed_skip_set: HashSet<String> = if resume_decision.is_resume() {
        let counts = emitter.replay_from_checkpoint()?;
        eprintln!(
            "[scry-kzip] checkpoint: replayed {} cxx CUs ({} records), {} scip CUs ({} records); manifest done_shas={}{}",
            counts.cxx_cus, counts.cxx_records, counts.scip_cus, counts.scip_records,
            emitter.checkpoint().map(|c| c.done_shas.lock().unwrap().len())
                .unwrap_or(0),
            if counts.cxx_truncated_bytes + counts.scip_truncated_bytes > 0 {
                format!(" (warning: {} truncated tail bytes discarded)",
                    counts.cxx_truncated_bytes + counts.scip_truncated_bytes)
            } else { String::new() },
        );
        emitter.checkpoint().unwrap().done_shas.lock().unwrap().clone()
    } else {
        HashSet::new()
    };
    let resumed_skip_set_for_walk = if resumed_skip_set.is_empty() {
        None
    } else {
        Some(&resumed_skip_set)
    };

    let t_walk = Instant::now();
    let by_kind = walk_and_bucket(
        kzip, langs_filter.as_ref(), max_units,
        path_include.as_ref(), path_exclude.as_ref(),
        resumed_skip_set_for_walk,
    )?;
    let walk_secs = t_walk.elapsed().as_secs_f64();
    let walked_total: usize = by_kind.values().map(|b| b.units.len()).sum();
    eprintln!(
        "[scry-kzip] phase 1/6: bucketed {} CUs across {} kinds in {:.2}s{}",
        walked_total, by_kind.len(), walk_secs,
        if resume_decision.is_resume() {
            format!(" (resume: {} CUs skipped from checkpoint)", resumed_skip_set.len())
        } else { String::new() },
    );

    // Phase 2: pre-resolve each indexer binary so we fail fast if
    // any are missing. Skipped buckets short-circuit.
    eprintln!(
        "[scry-kzip] phase 2/6: dispatching {} kinds ({})",
        by_kind.len(),
        by_kind.iter().map(|(k, b)| format!("{}={}", k, b.units.len()))
            .collect::<Vec<_>>().join(", "),
    );
    for (key, bucket) in &by_kind {
        if !bucket.kind.is_runnable() { continue; }
        if let Err(e) = resolve_indexer(kythe_root, bucket.kind) {
            return Err(e).with_context(|| format!(
                "resolve indexer for kind {}", key,
            ));
        }
    }

    // Phase 3: per-CU indexer dispatch, streaming decode into the
    // shared emitter. Default pool size is capped at num_cpus/2 so
    // JVM-based indexers don't OOM the host. Caller / env override
    // wins when set — at that point the operator has chosen to take
    // responsibility for the resident-set arithmetic.
    let workers = workers
        .or_else(parse_kzip_workers_env)
        .unwrap_or_else(recommended_workers);
    eprintln!(
        "[scry-kzip] phase 3/6: running indexers with {} workers",
        workers,
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .context("build rayon worker pool")?;
    let lang_reports: Mutex<Vec<LangReport>> = Mutex::new(Vec::new());

    // Where per-CU sub-kzips land. Cleaned up at the end of the run.
    let staging = scry_bridge::scry_tmp_dir()
        .join(format!("scry-kzip-staging-{}", std::process::id()));
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("mkdir staging {}", staging.display()))?;

    pool.install(|| {
        for (display_label, bucket) in &by_kind {
            let t_lang = Instant::now();
            let log_path = logs_dir.join(format!("{display_label}.log"));
            if !bucket.kind.is_runnable() {
                lang_reports.lock().unwrap().push(LangReport {
                    label: display_label.clone(),
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
                    display_label, bucket.units.len(), bucket.skip_reason,
                );
                continue;
            }
            eprintln!(
                "[scry-kzip] phase 3/6: {} ({} CUs)", display_label, bucket.units.len(),
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
                    &emitter, &log_file, &counters, i + 1, n_units, display_label,
                );
            });
            let (ok, empty, failed) = *counters.lock().unwrap();
            lang_reports.lock().unwrap().push(LangReport {
                label: display_label.clone(),
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
                display_label, ok, empty, failed, t_lang.elapsed().as_secs_f64(),
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
        v.sort_by(|a, b| a.label.cmp(&b.label));
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

/// Parse `SCRY_KZIP_LANGS=cxx,go` into a set of indexer labels.
/// Returns `None` if the env var is unset or empty.
fn parse_kzip_langs_env() -> Option<HashSet<String>> {
    let spec = std::env::var("SCRY_KZIP_LANGS").ok()?;
    let allowed: HashSet<String> = spec
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if allowed.is_empty() { None } else { Some(allowed) }
}

/// Parse `SCRY_KZIP_MAX_UNITS=50`. Returns `None` if unset or
/// malformed.
fn parse_kzip_max_units_env() -> Option<usize> {
    std::env::var("SCRY_KZIP_MAX_UNITS").ok()?.parse::<usize>().ok()
}

/// Parse `SCRY_KZIP_WORKERS=N`. Override for the phase-3 rayon pool
/// size when the caller hasn't passed `--kzip-workers`. Returns
/// `None` if unset, malformed, or `=0` (zero means "use the default",
/// not "spawn zero threads"). Out-of-range values are surfaced via
/// the rayon pool builder, not pre-validated here.
pub(crate) fn parse_kzip_workers_env() -> Option<usize> {
    let n = std::env::var("SCRY_KZIP_WORKERS").ok()?.parse::<usize>().ok()?;
    if n == 0 { None } else { Some(n) }
}

/// Parse `SCRY_KZIP_PATH_PREFIX=frameworks/base/,frameworks/native/`
/// into a list of path prefixes. A CU is kept iff its `primary_path`
/// (the first source-extension `required_input` — `.cc`, `.java`,
/// etc.) starts with any entry. Returns `None` if the env var is
/// unset or empty after trimming.
fn parse_kzip_path_prefix_env() -> Option<Vec<String>> {
    parse_prefix_list("SCRY_KZIP_PATH_PREFIX")
}

/// Parse `SCRY_KZIP_PATH_EXCLUDE=external/,prebuilts/` into a list
/// of path prefixes. A CU is dropped iff its `primary_path` starts
/// with any entry. Evaluated BEFORE the include filter, so excludes
/// always win over includes. Returns `None` if unset or empty.
fn parse_kzip_path_exclude_env() -> Option<Vec<String>> {
    parse_prefix_list("SCRY_KZIP_PATH_EXCLUDE")
}

fn parse_prefix_list(var: &str) -> Option<Vec<String>> {
    let spec = std::env::var(var).ok()?;
    let prefixes: Vec<String> = spec
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if prefixes.is_empty() { None } else { Some(prefixes) }
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
        "[{} {}/{}] sha={} exit={} wall={:.2}s stdout={}B entries={} records={} stderr_tail={}B\n",
        label, seq, total, &run.unit_sha, run.exit_code, run.wall_secs,
        run.stdout_bytes, run.entry_count, run.decoded.len(), run.stderr_tail.len(),
    ));
    if !run.stderr_tail.is_empty() {
        log_to(log_file, format!(
            "    stderr: {}\n",
            String::from_utf8_lossy(&run.stderr_tail).replace('\n', " | "),
        ));
    }
    if !run.produced_entries() {
        // No records: either a hard fail (exit != 0), a stream-decode
        // error (already prepended to stderr_tail by run_indexer),
        // or a clean run that produced no symbols. The third branch
        // hides a class of silent failures — `java_indexer.jar`
        // exits 0 even when javac aborts mid-parse with
        // OutOfMemoryError or a SEVERE-logged IllegalStateException,
        // just emits nothing. Detect those patterns in the stderr
        // tail and reclassify as failed so the summary's "empty"
        // bucket only contains genuinely empty CUs (no source,
        // all skipped).
        let stderr_tail_str = String::from_utf8_lossy(&run.stderr_tail);
        let silent_failure = run.exit_code == 0 && (
            stderr_tail_str.contains("OutOfMemoryError")
            || stderr_tail_str.contains("java.lang.IllegalStateException")
            || stderr_tail_str.contains("SEVERE: Unexpected error")
            || stderr_tail_str.contains("decode_stream error")
        );
        if silent_failure {
            log_to(log_file, format!(
                "[{} {}/{}] silent-fail (exit=0 but stderr names OOM/SEVERE/decode-error) — counting as failed\n",
                label, seq, total,
            ));
        }
        if run.exit_code != 0 || silent_failure {
            counters.lock().unwrap().2 += 1;
        } else {
            counters.lock().unwrap().1 += 1;
        }
        return;
    }
    // The IndexerRun already carries decoded records; commit them
    // atomically. Stream decoding inside `run_indexer` lets peak
    // per-CU memory stay bounded regardless of how many GB the
    // indexer wrote to stdout — previously the parent buffered the
    // full stdout in a Vec<u8> before decoding, which OOM'd on
    // big CUs like CarSystemUIRavenTests (10+ GB of Kythe entries).
    if let Err(e) = emitter.commit_cu(&run.unit_sha, kind, &run.decoded) {
        log_to(log_file, format!(
            "[{} {}/{}] checkpoint commit failed: {e}\n", label, seq, total,
        ));
        counters.lock().unwrap().2 += 1;
        return;
    }
    counters.lock().unwrap().0 += 1;
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

    /// `parse_kzip_langs_env` returns `None` when the var is unset
    /// and a populated set otherwise. We don't actually mutate env
    /// here — `std::env::set_var` is unsafe to share with parallel
    /// tests in the same process, so just verify the parse helper
    /// behaviour over its input.
    #[test]
    fn langs_env_parse_is_lowercase_and_trimmed() {
        // We can't read SCRY_KZIP_LANGS sanely from a parallel test,
        // but the parsing rule is straightforward — exercise it via
        // a clone of the inner logic.
        let spec = " Cxx ,Go,,JAVA";
        let allowed: HashSet<String> = spec
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(allowed.contains("cxx"));
        assert!(allowed.contains("go"));
        assert!(allowed.contains("java"));
        assert_eq!(allowed.len(), 3);
    }

    /// `SCRY_KZIP_WORKERS=0` must collapse to `None` — zero would
    /// give rayon a zero-thread pool, which builds but never makes
    /// progress. The driver chains `.unwrap_or_else(recommended_workers)`
    /// so `None` is the right "use the default" sentinel.
    #[test]
    fn kzip_workers_zero_collapses_to_none() {
        fn rule(spec: &str) -> Option<usize> {
            let n = spec.parse::<usize>().ok()?;
            if n == 0 { None } else { Some(n) }
        }
        assert_eq!(rule("0"), None);
        assert_eq!(rule(""), None);
        assert_eq!(rule("notanumber"), None);
        assert_eq!(rule("16"), Some(16));
        assert_eq!(rule("72"), Some(72));
    }
}
