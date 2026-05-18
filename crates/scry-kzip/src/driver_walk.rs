//! Phase 1 of the kzip driver: walk the kzip and bucket every CU by
//! `IndexerKind`.
//!
//! Split out of `driver.rs` so the orchestrator stays focused on
//! phase coordination — here we own the per-walk filtering rules
//! (language allow-list, path include/exclude, resumed-SHA skip,
//! `max_units` early break) plus the bucket data structure phases
//! 2 / 3 consume.

use crate::dispatch::{self, IndexerKind};
use crate::walker::{self, KzipUnit};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

/// One per-language CU bucket, keyed by `IndexerKind::label()` (or
/// `skip-<source-language>` for `Skip(_)` kinds). The driver reads
/// the `kind` to decide whether to spawn an indexer or short-circuit
/// to the skip-counter; `units` is the worklist for phase 3; and
/// `skip_reason` populates the per-language summary line.
pub(crate) struct BucketEntry {
    pub(crate) kind: IndexerKind,
    pub(crate) units: Vec<KzipUnit>,
    /// Populated only when kind is `Skip(_)`.
    pub(crate) skip_reason: String,
}

/// Walk the kzip and bucket every unit by `IndexerKind`. Always
/// uses the serial walker; `max_units` enables an early break.
/// See the crate-level rationale on `driver.rs` for why we don't
/// fan the walk across rayon workers.
///
/// `done_shas` is the set of CU SHAs that have already been recorded
/// by a previous, checkpointed run. Units whose SHA is in the set are
/// dropped here so phase 2 / 3 never spawn an indexer for them.
pub(crate) fn walk_and_bucket(
    kzip: &Path,
    langs_filter: Option<&HashSet<String>>,
    max_units: Option<usize>,
    path_include: Option<&Vec<String>>,
    path_exclude: Option<&Vec<String>>,
    done_shas: Option<&HashSet<String>>,
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
    let mut dropped_by_resume = 0usize;
    let mut seen = 0usize;
    let t_walk_phase = Instant::now();
    for unit_res in walker::walk_units_serial(kzip, langs_filter)? {
        let unit = unit_res.context("walk kzip units")?;
        seen += 1;
        if seen % 5000 == 0 {
            eprintln!(
                "[scry-kzip] phase 1/6: walked {} CUs ({:.1}s, {} bucketed, {} excluded, {} include-dropped, {} resume-skipped)",
                seen, t_walk_phase.elapsed().as_secs_f64(),
                yielded, dropped_by_exclude, dropped_by_include, dropped_by_resume,
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
        if let Some(done) = done_shas {
            if done.contains(&unit.unit_sha) {
                dropped_by_resume += 1;
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
    if dropped_by_resume > 0 {
        eprintln!(
            "[scry-kzip] phase 1/6: {} CUs skipped because their SHA was in the resumed checkpoint",
            dropped_by_resume,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::UnitEncoding;

    /// `bucket_unit` keys skip kinds by `skip-<source language>` and
    /// runnable kinds by the indexer label. Two units of different
    /// runnable kinds land in different buckets; two skip units of
    /// the same source language coalesce.
    #[test]
    fn bucket_unit_keys_skip_by_language() {
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

    /// `primary_path_matches` empty paths never match; non-empty
    /// prefixes match on string-start only.
    #[test]
    fn primary_path_matches_basic() {
        let prefixes = vec!["frameworks/".to_string(), "system/".to_string()];
        assert!(primary_path_matches("frameworks/base/X.java", &prefixes));
        assert!(primary_path_matches("system/core/Y.cc", &prefixes));
        assert!(!primary_path_matches("external/A.cc", &prefixes));
        assert!(!primary_path_matches("", &prefixes));
    }
}
