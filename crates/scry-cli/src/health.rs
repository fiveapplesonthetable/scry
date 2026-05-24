//! `scry health` — index integrity check.
//!
//! Validates the on-disk index dir: required artifacts present,
//! optional sidecars accounted for, structured-sidecar versions
//! sane, live `StoreReader::open` + spot-decode works, scry_version
//! matches.
//!
//! Exits 2 if any required artifact is missing/invalid; otherwise
//! 0 (with "healthy" / sidecar status lines printed). `--json`
//! returns the full per-check array for programmatic consumers.

use anyhow::Result;
use scry_store::StoreReader;
use std::path::PathBuf;

use crate::default_index_dir;

pub(crate) fn cmd_health(index: Option<PathBuf>, json: bool) -> Result<()> {
    let index_dir = index.unwrap_or_else(default_index_dir);
    let paths = scry_store::StorePaths::new(index_dir.clone());

    struct Check { name: &'static str, status: String, required: bool, ok: bool }
    let mut checks: Vec<Check> = Vec::new();

    // Required artifacts: anything an open() of StoreReader would
    // fail without.
    let required_files: &[(&str, PathBuf)] = &[
        ("manifest.json", paths.manifest()),
        ("roots.bin",     paths.roots()),
        ("files_packed.bin", paths.files_packed()),
        ("symbols.bin",   paths.symbols()),
        ("refs.bin",      paths.refs()),
        ("names.fst",     paths.names_fst()),
        ("name_postings.bin", paths.name_postings()),
        ("ref_names.fst", paths.ref_names_fst()),
        ("ref_postings.bin", paths.ref_postings()),
    ];
    for (name, path) in required_files {
        let (ok, status) = match std::fs::metadata(path) {
            Ok(m) if m.len() > 0 => (true, format!("OK ({} bytes)", m.len())),
            Ok(_) => (false, "EMPTY".to_string()),
            Err(e) => (false, format!("MISSING: {e}")),
        };
        checks.push(Check { name, status, required: true, ok });
    }

    // Optional sidecars: report state but don't fail.
    let optional_files: &[(&str, PathBuf)] = &[
        ("trigrams.fst",         paths.trigram_fst()),
        ("trigram_postings.bin", paths.trigram_postings()),
        ("symbols_offsets.bin",  paths.symbol_offsets()),
        ("refs_offsets.bin",     paths.ref_offsets()),
        ("file_symbols.bin",     paths.file_symbols()),
        ("file_symbols_offsets.bin", paths.file_symbols_offsets()),
        // file_refs sidecar powers `scry uses` — listed alongside
        // file_symbols (which powers `scry outline`/`tldr`) for the
        // symmetric report. Absent ⇒ `uses` falls back to a slow
        // linear scan; missing here would silently hide that.
        ("file_refs.bin",        paths.file_refs()),
        ("file_refs_offsets.bin", paths.file_refs_offsets()),
        ("ref_resolutions.bin",  paths.ref_resolutions()),
        ("file_digests.bin",     paths.file_digests()),
        ("tombstones.bin",       paths.tombstones()),
        // Build-system module graph (used by --reachable). Has its own
        // structured status line below; this just reports file presence.
        ("module_graph.json",    paths.module_graph_json()),
    ];
    for (name, path) in optional_files {
        let (ok, status) = match std::fs::metadata(path) {
            Ok(m) if m.len() > 0 => (true, format!("OK ({} bytes)", m.len())),
            Ok(_) => (true, "empty (treated absent)".to_string()),
            Err(_) => (true, "absent (optional)".to_string()),
        };
        checks.push(Check { name, status, required: false, ok });
    }

    // Live-open spot-check: try to open the reader and decode the
    // first few records. Catches "files exist but are corrupted" cases
    // that pure existence checks miss.
    let mut reader_ok = false;
    let reader_status;
    match StoreReader::open(&index_dir) {
        Ok(r) => {
            // Decode the first symbol + ref via the lazy paths; both
            // should succeed on any healthy index with >= 1 record.
            let s_ok = r.n_symbols() == 0 || r.get_symbol_raw(0).is_some();
            let r_ok = r.n_refs() == 0 || r.get_ref_raw(0).is_some();
            // Look up a known-no-such-name to exercise the FST.
            let fst_ok = r.lookup_exact("__definitely_not_a_real_symbol__").is_empty();
            if s_ok && r_ok && fst_ok {
                reader_ok = true;
                reader_status = format!(
                    "open OK ({} files, {} symbols, {} refs)",
                    r.file_count(), r.n_symbols(), r.n_refs(),
                );
            } else {
                reader_status = format!(
                    "open OK but spot-check failed (sym={s_ok} ref={r_ok} fst={fst_ok})"
                );
            }
        }
        Err(e) => {
            reader_status = format!("open FAILED: {e:#}");
        }
    }
    checks.push(Check {
        name: "open + spot-decode", status: reader_status,
        required: true, ok: reader_ok,
    });

    // Build-system module graph (used by `--reachable`): report version +
    // counts, or absent. Drift surfaces as a soft warn.
    let mg_status = match std::fs::read(paths.module_graph_json()) {
        Err(_) => "absent (run `scry build-modgraph`)".to_string(),
        Ok(raw) => match serde_json::from_slice::<scry_store::modgraph::ModuleGraphJsonV1>(&raw) {
            Ok(v) if v.version == 1 => format!(
                "v1, {} modules, {} dep edges, {} file attributions",
                v.modules.len(), v.deps.len(), v.files.len(),
            ),
            Ok(v) => format!("present but unsupported version {}", v.version),
            Err(e) => format!("present but FAILED to parse: {e:#}"),
        },
    };
    checks.push(Check { name: "module_graph", status: mg_status, required: false, ok: true });

    // Layer 2 resolution coverage (v0.1.59). Surface the resolved/total
    // ratio so users know up-front whether `--def-in` / `--strict` will
    // be useful on this index.
    let resolution_status = match StoreReader::open(&index_dir).ok()
        .and_then(|r| r.count_resolved_refs().map(|n| (n, r.n_refs())))
    {
        None => "absent (scry is tree-sitter only; no resolution sidecar — \
                 resolved_to is always null)".to_string(),
        Some((_, 0)) => "no refs in index (sidecar dimension unknown)".to_string(),
        Some((resolved, total)) => {
            let pct = (resolved as f64) * 100.0 / (total as f64);
            format!("v1, {resolved}/{total} refs resolved ({pct:.1}%)")
        }
    };
    checks.push(Check {
        name: "refs_resolved", status: resolution_status,
        required: false, ok: true,
    });

    // Version-skew check.
    let manifest_version = std::fs::read_to_string(paths.manifest()).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("scry_version").and_then(|x| x.as_str()).map(String::from));
    let running_version = env!("CARGO_PKG_VERSION").to_string();
    let (version_ok, version_status) = match manifest_version.as_deref() {
        Some(mv) if mv == running_version => (true,
            format!("index built with scry {mv} (matches running)")),
        Some(mv) => (true,
            format!("index built with scry {mv}; running {running_version} \
                     (rebuild recommended if you see odd query results)")),
        None => (true, format!("manifest missing scry_version field; running {running_version}")),
    };
    checks.push(Check {
        name: "scry_version", status: version_status,
        required: false, ok: version_ok,
    });

    let any_required_failed = checks.iter().any(|c| c.required && !c.ok);

    if json {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let arr: Vec<serde_json::Value> = checks.iter().map(|c| serde_json::json!({
            "artifact": c.name,
            "required": c.required,
            "ok": c.ok,
            "status": c.status,
        })).collect();
        writeln!(out, "{}", serde_json::json!({
            "index": index_dir.display().to_string(),
            "healthy": !any_required_failed,
            "checks": arr,
        }))?;
    } else {
        println!("scry health — index: {}", index_dir.display());
        for c in &checks {
            let tag = if c.ok { " OK" } else { "FAIL" };
            let req = if c.required { " (required)" } else { "" };
            println!("  [{tag}] {:<30} {}{}", c.name, c.status, req);
        }
        println!();
        if any_required_failed {
            println!("OVERALL: UNHEALTHY (one or more required artifacts missing/invalid)");
        } else {
            println!("OVERALL: healthy");
        }
    }
    if any_required_failed {
        std::process::exit(2);
    }
    Ok(())
}
