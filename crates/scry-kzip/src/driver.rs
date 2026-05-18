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

use crate::dispatch::{self, IndexerKind};
use crate::emit::{EmitReport, PackedEmitter};
use crate::entries::decode_stream;
use crate::indexer::{
    build_per_cu_kzip, recommended_workers, resolve_indexer, run_indexer,
};
use crate::walker::{self, KzipUnit};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
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

    let t_walk = Instant::now();
    let by_kind = walk_and_bucket(
        kzip, langs_filter.as_ref(), max_units,
        path_include.as_ref(), path_exclude.as_ref(),
    )?;
    let walk_secs = t_walk.elapsed().as_secs_f64();
    let walked_total: usize = by_kind.values().map(|b| b.units.len()).sum();
    eprintln!(
        "[scry-kzip] phase 1/6: bucketed {} CUs across {} kinds in {:.2}s",
        walked_total, by_kind.len(), walk_secs,
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

/// Walk the kzip and bucket every unit by `IndexerKind`. Always
/// uses the serial walker; `max_units` enables an early break.
/// See the crate-level rationale on `driver.rs` for why we don't
/// fan the walk across rayon workers.
fn walk_and_bucket(
    kzip: &Path,
    langs_filter: Option<&HashSet<String>>,
    max_units: Option<usize>,
    path_include: Option<&Vec<String>>,
    path_exclude: Option<&Vec<String>>,
) -> Result<HashMap<String, BucketEntry>> {
    if let Some(cap) = max_units {
        eprintln!(
            "[scry-kzip] phase 1/6: walking {} (serial, max_units={})",
            kzip.display(), cap,
        );
    } else {
        eprintln!(
            "[scry-kzip] phase 1/6: walking {} (serial, full ingest)",
            kzip.display(),
        );
    }
    let mut by_kind: HashMap<String, BucketEntry> = HashMap::new();
    let mut yielded = 0usize;
    let mut dropped_by_include = 0usize;
    let mut dropped_by_exclude = 0usize;
    let mut seen = 0usize;
    let t_walk_phase = Instant::now();
    for unit_res in walker::walk_units_serial(kzip, langs_filter)? {
        let unit = unit_res.context("walk kzip units")?;
        seen += 1;
        if seen % 5000 == 0 {
            eprintln!(
                "[scry-kzip] phase 1/6: walked {} CUs ({:.1}s, {} bucketed, {} excluded, {} include-dropped)",
                seen, t_walk_phase.elapsed().as_secs_f64(),
                yielded, dropped_by_exclude, dropped_by_include,
            );
        }
        // Path filters apply only to CUs that would actually run an
        // indexer; Skip-kind CUs preserve their per-language skip
        // tally in the summary regardless of path scope.
        let kind = dispatch::choose_for(&unit);
        if matches!(kind, IndexerKind::Skip(_)) {
            bucket_unit(&mut by_kind, unit);
            yielded += 1;
            if let Some(cap) = max_units {
                if yielded >= cap { break; }
            }
            continue;
        }
        if let Some(excludes) = path_exclude {
            if primary_path_matches(&unit.primary_path, excludes) {
                dropped_by_exclude += 1;
                continue;
            }
        }
        if let Some(prefixes) = path_include {
            if !primary_path_matches(&unit.primary_path, prefixes) {
                dropped_by_include += 1;
                continue;
            }
        }
        bucket_unit(&mut by_kind, unit);
        yielded += 1;
        if let Some(cap) = max_units {
            if yielded >= cap {
                eprintln!(
                    "[scry-kzip] phase 1/6: serial walk hit max_units cap ({})",
                    cap,
                );
                break;
            }
        }
    }
    if dropped_by_include > 0 {
        eprintln!(
            "[scry-kzip] phase 1/6: {} CUs dropped by SCRY_KZIP_PATH_PREFIX",
            dropped_by_include,
        );
    }
    if dropped_by_exclude > 0 {
        eprintln!(
            "[scry-kzip] phase 1/6: {} CUs dropped by SCRY_KZIP_PATH_EXCLUDE",
            dropped_by_exclude,
        );
    }
    Ok(by_kind)
}

/// True if `primary_path` starts with any of the given prefixes.
/// Empty primary paths (CU with no required inputs) never match.
fn primary_path_matches(primary_path: &str, prefixes: &[String]) -> bool {
    !primary_path.is_empty() && prefixes.iter().any(|p| primary_path.starts_with(p))
}

/// Bucket one unit into the per-kind map. Skip buckets get a
/// `skip-<source language>` label so the summary log distinguishes
/// per-language skip reasons.
fn bucket_unit(by_kind: &mut HashMap<String, BucketEntry>, unit: KzipUnit) {
    let kind = dispatch::choose_for(&unit);
    let key = match kind {
        IndexerKind::Skip(_) => format!("skip-{}", unit.language),
        _ => kind.label().to_string(),
    };
    let entry = by_kind.entry(key).or_insert_with(|| BucketEntry {
        kind,
        units: Vec::new(),
        skip_reason: match kind {
            IndexerKind::Skip(msg) => msg.to_string(),
            _ => String::new(),
        },
    });
    entry.units.push(unit);
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

struct BucketEntry {
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

    /// `bucket_unit` keys skip kinds by `skip-<source language>` and
    /// runnable kinds by the indexer label. Two units of different
    /// runnable kinds land in different buckets; two skip units of
    /// the same source language coalesce.
    #[test]
    fn bucket_unit_keys_skip_by_language() {
        use crate::walker::UnitEncoding;
        let mk = |lang: &str, sha: &str| KzipUnit {
            kzip_path: Path::new("/x.kzip").to_path_buf(),
            unit_sha: sha.to_string(),
            encoding: UnitEncoding::Proto,
            language: lang.to_string(),
            has_class_input: false,
            primary_path: String::new(),
        };
        let mut by_kind = HashMap::new();
        bucket_unit(&mut by_kind, mk("c++", "a"));
        bucket_unit(&mut by_kind, mk("go", "b"));
        bucket_unit(&mut by_kind, mk("rust", "c"));
        bucket_unit(&mut by_kind, mk("rust", "d"));
        bucket_unit(&mut by_kind, mk("kotlin", "e")); // no .class → skip
        assert_eq!(by_kind.get("cxx").map(|b| b.units.len()), Some(1));
        assert_eq!(by_kind.get("go").map(|b| b.units.len()), Some(1));
        assert_eq!(by_kind.get("skip-rust").map(|b| b.units.len()), Some(2));
        assert_eq!(by_kind.get("skip-kotlin").map(|b| b.units.len()), Some(1));
    }
}
