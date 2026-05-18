//! Walk a `.kzip` and yield `KzipUnit` per compilation.
//!
//! A kzip is a zip archive with these top-level sub-trees under a
//! single prefix (almost always `root/`, but the spec allows any
//! single-segment prefix; a few legacy extractors used `kzip/`):
//!
//! * `root/pbunits/<sha>` — one `IndexedCompilation` proto per file
//!   (the newer binary encoding).
//! * `root/units/<sha>`   — one `IndexedCompilation` *JSON* per file
//!   (the older encoding; per proto3-JSON spec, accepts both snake_case
//!   and camelCase field names).
//! * `root/files/<sha>`   — the raw file blobs referenced by the
//!   units. We don't read these here — the actual indexer binaries
//!   (cxx_indexer, java_indexer.jar, …) reach into the kzip themselves
//!   and resolve `required_input[*].info.digest` against `root/files/`.
//!
//! The Soong-merged AOSP kzip ships **both** encodings: `pbunits/`
//! for newer extractors (go / kotlin / rust) and `units/` for the
//! historic cxx_extractor + java_extractor pipeline. We walk both
//! sub-trees and dedup by SHA so a CU that lives only in `units/`
//! still surfaces as a `KzipUnit`.
//!
//! ## Two entry points
//!
//! * [`walk_units_serial`] — single-threaded iterator. Use when a
//!   bound (`SCRY_KZIP_MAX_UNITS`) makes early termination the right
//!   strategy.
//! * [`walk_units_parallel`] — rayon fan-out, one `ZipArchive` per
//!   worker. Use when there's no bound — full ingest of a 100 K-unit
//!   kzip is dominated by the per-entry JSON decode, which trivially
//!   parallelises.
//!
//! Both honour an optional language filter — the read+decode helpers
//! live in [`super::walker_decode`] and pre-peek the first few KiB of
//! each entry so CUs that don't match the filter never pay full
//! decompression.
//!
//! The legacy [`walk_units`] wrapper collects the serial iterator
//! into a `Vec` — kept for the existing test suite and for callers
//! that genuinely want eager materialisation.

use crate::walker_decode::read_one_entry;
pub(crate) use crate::walker_decode::lenient_json_opts;
use anyhow::{anyhow, Context, Result};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Size of the per-worker `BufReader` wrapping the underlying kzip
/// `File` handle. The zip crate hands its decoder raw `Read` access,
/// which means flate2 + length-delimited reads round-trip through a
/// `read()` syscall per ~chunk. Without a buffer we measured ~8000
/// syscalls per JSON unit; with a 256 KiB buffer that drops by two
/// orders of magnitude. 256 KiB is enough to swallow the local-file
/// header + the entire compressed body of most JSON units in one
/// syscall.
const ZIP_READ_BUF: usize = 256 * 1024;

/// The reader flavour we wrap every kzip `File` in before handing it
/// to `zip::ZipArchive`. Exposed as a `pub(crate)` type alias so
/// downstream helpers (in `walker_decode`) can name the concrete
/// archive type without dragging the BufReader detail into their
/// signatures.
pub(crate) type KzipReader = BufReader<File>;
pub(crate) type KzipArchive = zip::ZipArchive<KzipReader>;

/// On-disk encoding of the unit blob inside the kzip. The Soong-
/// merged AOSP kzip carries both: newer extractors emit proto-encoded
/// units under `root/pbunits/`, while the historic cxx + java
/// extractors emit JSON-encoded units under `root/units/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitEncoding {
    /// Binary proto at `<prefix>/pbunits/<sha>`.
    Proto,
    /// proto3-JSON text at `<prefix>/units/<sha>`.
    Json,
}

impl UnitEncoding {
    /// The in-kzip sub-directory that holds units of this encoding,
    /// without the leading prefix (e.g. `"pbunits"`).
    pub fn subdir(self) -> &'static str {
        match self {
            UnitEncoding::Proto => "pbunits",
            UnitEncoding::Json  => "units",
        }
    }
}

/// One compilation unit pulled out of a kzip. We keep the parsed
/// `CompilationUnit`'s language + a hint about whether it has any
/// `.class` / `.jar` inputs (which drives the Kotlin-source-vs-
/// bytecode dispatcher), but the rest of the unit body is left on
/// the wire — the actual Kythe indexer will re-read the kzip.
#[derive(Debug, Clone)]
pub struct KzipUnit {
    /// The kzip file this unit came from. Recorded so the dispatcher
    /// can hand it to the indexer subprocess (every Kythe indexer
    /// takes a kzip path as a positional arg).
    pub kzip_path: PathBuf,
    /// The sha256 of the unit blob within the kzip (the basename of
    /// `<prefix>/pbunits/<sha>` or `<prefix>/units/<sha>`).
    pub unit_sha: String,
    /// Which on-disk encoding this unit uses inside the kzip. The
    /// per-CU sub-kzip builder needs this so it knows whether to copy
    /// the unit bytes verbatim (proto) or re-serialize them after
    /// parsing JSON (we always emit proto into the sub-kzip — every
    /// Kythe indexer accepts proto, but mixed-encoding kzips trip
    /// the indexers' encoding-mismatch check).
    pub encoding: UnitEncoding,
    /// Language string from `cu.v_name.language`. May be empty when
    /// the extractor didn't set it; in that case we infer from the
    /// `required_input[*].info.path` suffix.
    pub language: String,
    /// True if any `required_input[*].info.path` ends `.class`.
    /// Drives the Kotlin/JVM dispatch fork — jars without bytecode
    /// (e.g. kotlinc-emitted source jars) won't satisfy jvm_indexer.
    pub has_class_input: bool,
}

/// Optional pre-filter applied during the walk. When `Some`, every
/// unit is run through a cheap language peek (no full proto / JSON
/// decode) and dropped if its dispatched indexer label isn't in the
/// allow-set. CUs with an empty `v_name.language` always pass — the
/// caller's full-decode path will fall back to `infer_language_from
/// _inputs`.
pub type LangFilter<'a> = Option<&'a HashSet<String>>;

/// One classified zip entry. Used by both walk paths to share the
/// pbunits-vs-units bookkeeping. `pub(crate)` because the read
/// helpers in `walker_decode` need to consume it.
#[derive(Debug, Clone)]
pub(crate) struct UnitEntry {
    pub name: String,
    pub sha: String,
    pub encoding: UnitEncoding,
}

/// Eagerly walk every unit out of `kzip`. Internally streams via
/// [`walk_units_serial`] and collects. Retained for callers that want
/// a `Vec` and for the test suite.
pub fn walk_units(kzip: &Path) -> Result<Vec<KzipUnit>> {
    walk_units_serial(kzip, None)?.collect()
}

/// Stream every unit out of `kzip`. Single-threaded — pulls one entry
/// out of the central ZipArchive at a time. Use when the driver has
/// a bound (`SCRY_KZIP_MAX_UNITS`) that lets it `break` early.
///
/// `filter` short-circuits CUs whose language doesn't match before
/// paying the full decode cost. See [`LangFilter`] for the semantics.
pub fn walk_units_serial<'a>(
    kzip: &Path,
    filter: LangFilter<'a>,
) -> Result<Box<dyn Iterator<Item = Result<KzipUnit>> + 'a>> {
    let zip = open_archive(kzip)
        .with_context(|| format!("open kzip {}", kzip.display()))?;
    let entries = classify_entries(&zip, kzip)?;
    let kzip_buf = kzip.to_path_buf();
    let filter = filter.cloned();
    let it = SerialWalker { zip, entries, idx: 0, kzip: kzip_buf, filter };
    Ok(Box::new(it))
}

/// Parallel walk over every unit in `kzip`. Each rayon worker opens
/// its own `ZipArchive` once (via `for_each_init`) and reuses it
/// across all entries it's assigned — opening a 26 GB kzip is ~ms,
/// not seconds, but doing it 100 K times still adds up.
///
/// `on_unit` is called for every yielded `KzipUnit`, possibly from
/// multiple threads concurrently. It must be `Sync`.
///
/// `workers` caps the rayon pool size for this walk. The function
/// installs its own pool — it doesn't touch the global rayon pool —
/// so the caller can size walker concurrency independently from the
/// indexer dispatch concurrency downstream.
pub fn walk_units_parallel<F>(
    kzip: &Path,
    workers: usize,
    filter: LangFilter<'_>,
    on_unit: F,
) -> Result<()>
where
    F: Fn(KzipUnit) + Sync,
{
    let entries = {
        let zip = open_archive(kzip)
            .with_context(|| format!("open kzip {}", kzip.display()))?;
        classify_entries(&zip, kzip)?
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .thread_name(|i| format!("scry-kzip-walker-{i}"))
        .build()
        .context("build rayon walker pool")?;
    let kzip_buf = kzip.to_path_buf();
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    pool.install(|| {
        entries.par_iter().for_each_init(
            || open_archive(&kzip_buf),
            |archive_res, entry| {
                if first_err.lock().unwrap().is_some() {
                    return;
                }
                let archive = match archive_res {
                    Ok(z) => z,
                    Err(e) => {
                        *first_err.lock().unwrap() = Some(anyhow!(
                            "open kzip {}: {e}", kzip_buf.display(),
                        ));
                        return;
                    }
                };
                match read_one_entry(archive, entry, &kzip_buf, filter) {
                    Ok(Some(unit)) => on_unit(unit),
                    Ok(None) => {}
                    Err(e) => {
                        let mut slot = first_err.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                    }
                }
            },
        );
    });
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    Ok(())
}

fn open_archive(kzip: &Path) -> std::io::Result<KzipArchive> {
    let f = File::open(kzip)?;
    let buf = BufReader::with_capacity(ZIP_READ_BUF, f);
    zip::ZipArchive::new(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Build the ordered + deduped entry list shared by both walk paths.
/// Proto entries come first so the dedup pass keeps proto whenever a
/// SHA appears in both encodings (matches the original `walk_units`
/// semantics).
fn classify_entries(
    zip: &KzipArchive,
    kzip: &Path,
) -> Result<Vec<UnitEntry>> {
    let names: Vec<String> = zip.file_names().map(String::from).collect();
    let prefix = detect_prefix(&names).ok_or_else(|| {
        anyhow!(
            "{}: no `<root>/pbunits/...` or `<root>/units/...` entries found",
            kzip.display(),
        )
    })?;
    let pbunits_dir = format!("{prefix}/pbunits/");
    let units_dir   = format!("{prefix}/units/");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // Proto first.
    for n in &names {
        let Some(sha) = n.strip_prefix(&pbunits_dir) else { continue };
        if sha.is_empty() || sha.contains('/') { continue; }
        if !seen.insert(sha.to_string()) { continue; }
        out.push(UnitEntry {
            name: n.clone(),
            sha: sha.to_string(),
            encoding: UnitEncoding::Proto,
        });
    }
    let mut json_dup_skipped = 0usize;
    for n in &names {
        let Some(sha) = n.strip_prefix(&units_dir) else { continue };
        if sha.is_empty() || sha.contains('/') { continue; }
        if !seen.insert(sha.to_string()) {
            json_dup_skipped += 1;
            continue;
        }
        out.push(UnitEntry {
            name: n.clone(),
            sha: sha.to_string(),
            encoding: UnitEncoding::Json,
        });
    }
    if json_dup_skipped > 0 {
        eprintln!(
            "[scry-kzip] walker: {} JSON unit(s) had a proto twin and were skipped (proto wins)",
            json_dup_skipped,
        );
    }
    Ok(out)
}

/// Find the in-kzip prefix (`root` in the common case) by scanning
/// entry names for the first one with a recognised unit sub-directory.
fn detect_prefix(names: &[String]) -> Option<String> {
    for n in names {
        if let Some(rest) = n.strip_suffix('/') {
            // A directory entry such as `root/` tells us the prefix.
            if !rest.contains('/') {
                return Some(rest.to_string());
            }
        }
        if let Some(idx) = n.find("/pbunits/").or_else(|| n.find("/units/")) {
            return Some(n[..idx].to_string());
        }
    }
    None
}

/// Iterator returned by [`walk_units_serial`]. Streams one entry at
/// a time out of the central `ZipArchive`; the underlying file is
/// held open for the iterator's lifetime.
struct SerialWalker {
    zip: KzipArchive,
    entries: Vec<UnitEntry>,
    idx: usize,
    kzip: PathBuf,
    filter: Option<HashSet<String>>,
}

impl Iterator for SerialWalker {
    type Item = Result<KzipUnit>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.entries.get(self.idx)?;
            self.idx += 1;
            let filter_ref: LangFilter<'_> = self.filter.as_ref();
            match read_one_entry(&mut self.zip, entry, &self.kzip, filter_ref) {
                Ok(Some(unit)) => return Some(Ok(unit)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::analysis::{
        compilation_unit::FileInput, CompilationUnit, IndexedCompilation, VName,
    };
    use protobuf::{Message, MessageField};
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `walk_units` should error cleanly when given a non-zip file.
    #[test]
    fn rejects_non_zip() {
        let tmp = scry_bridge::scry_tmp_dir()
            .join(format!("scry-kzip-walk-bad-{}", std::process::id()));
        std::fs::create_dir_all(tmp.parent().unwrap()).ok();
        std::fs::write(&tmp, b"not a zip").unwrap();
        let err = walk_units(&tmp).unwrap_err();
        let _ = std::fs::remove_file(&tmp);
        assert!(format!("{err:?}").contains("kzip"));
    }

    /// Build a tiny one-unit `IndexedCompilation` we can drop into a
    /// synthetic kzip for the encoding-mix tests below.
    fn sample_indexed(language: &str, input_path: &str) -> IndexedCompilation {
        let mut vname = VName::new();
        vname.language = language.to_string();
        vname.corpus = "test".to_string();
        vname.signature = "sig".to_string();
        let mut fi_info = crate::proto::analysis::FileInfo::new();
        fi_info.path = input_path.to_string();
        fi_info.digest =
            "0000000000000000000000000000000000000000000000000000000000000000".to_string();
        let mut fi = FileInput::new();
        fi.info = MessageField::some(fi_info);
        let mut cu = CompilationUnit::new();
        cu.v_name = MessageField::some(vname);
        cu.required_input.push(fi);
        let mut ic = IndexedCompilation::new();
        ic.unit = MessageField::some(cu);
        ic
    }

    /// Assemble a kzip from a list of `(in-zip path, bytes)` entries.
    fn write_kzip(label: &str, entries: &[(String, Vec<u8>)]) -> PathBuf {
        // Per-test unique path: PID + thread id + caller-supplied label.
        // Avoids cross-test collisions when cargo test runs in parallel
        // (default).
        let tid = std::thread::current().id();
        let out = scry_bridge::scry_tmp_dir().join(format!(
            "scry-kzip-walk-{}-{:?}-{}.kzip",
            std::process::id(),
            tid,
            label,
        ));
        std::fs::create_dir_all(out.parent().unwrap()).ok();
        let _ = std::fs::remove_file(&out);
        let mut zw = zip::ZipWriter::new(File::create(&out).unwrap());
        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.add_directory("root/", opts).ok();
        for (name, bytes) in entries {
            zw.start_file(name.clone(), opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
        out
    }

    /// JSON-only unit (`root/units/<sha>`) round-trips through the
    /// walker with the same shape as a proto unit would. Mirrors the
    /// AOSP cxx_extractor / java_extractor output.
    #[test]
    fn walks_json_units() {
        let ic = sample_indexed("c++", "src/foo.cc");
        let json = protobuf_json_mapping::print_to_string(&ic).unwrap();
        let sha = "0000000000000000000000000000000000000000000000000000000000000001";
        let kzip = write_kzip(
            "json-only",
            &[(format!("root/units/{sha}"), json.into_bytes())],
        );
        let units = walk_units(&kzip).unwrap();
        let _ = std::fs::remove_file(&kzip);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].unit_sha, sha);
        assert_eq!(units[0].language, "c++");
        assert_eq!(units[0].encoding, UnitEncoding::Json);
        assert!(!units[0].has_class_input);
    }

    /// Proto unit (`root/pbunits/<sha>`) still works after the JSON
    /// branch was added; the encoding field reports `Proto`.
    #[test]
    fn walks_proto_units() {
        let ic = sample_indexed("go", "src/foo.go");
        let bytes = ic.write_to_bytes().unwrap();
        let sha = "0000000000000000000000000000000000000000000000000000000000000002";
        let kzip = write_kzip(
            "proto-only",
            &[(format!("root/pbunits/{sha}"), bytes)],
        );
        let units = walk_units(&kzip).unwrap();
        let _ = std::fs::remove_file(&kzip);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].unit_sha, sha);
        assert_eq!(units[0].language, "go");
        assert_eq!(units[0].encoding, UnitEncoding::Proto);
    }

    /// A SHA that appears in BOTH sub-trees yields exactly one
    /// `KzipUnit` and keeps the proto encoding (proto wins for dedup).
    #[test]
    fn dedup_prefers_proto_when_sha_in_both() {
        let sha = "0000000000000000000000000000000000000000000000000000000000000003";
        let proto_ic = sample_indexed("java", "Foo.java");
        let json_ic = sample_indexed("java", "Foo.java");
        let kzip = write_kzip("dup", &[
            (format!("root/pbunits/{sha}"), proto_ic.write_to_bytes().unwrap()),
            (
                format!("root/units/{sha}"),
                protobuf_json_mapping::print_to_string(&json_ic).unwrap().into_bytes(),
            ),
        ]);
        let units = walk_units(&kzip).unwrap();
        let _ = std::fs::remove_file(&kzip);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].encoding, UnitEncoding::Proto);
    }

    /// JSON + proto with disjoint SHAs both surface (the AOSP-kzip
    /// reality, where the two encodings cover non-overlapping
    /// languages).
    #[test]
    fn walks_mixed_encodings_disjoint_shas() {
        let proto_sha = "00000000000000000000000000000000000000000000000000000000000000aa";
        let json_sha  = "00000000000000000000000000000000000000000000000000000000000000bb";
        let proto_ic = sample_indexed("go", "foo.go");
        let json_ic = sample_indexed("c++", "foo.cc");
        let kzip = write_kzip("mixed", &[
            (
                format!("root/pbunits/{proto_sha}"),
                proto_ic.write_to_bytes().unwrap(),
            ),
            (
                format!("root/units/{json_sha}"),
                protobuf_json_mapping::print_to_string(&json_ic).unwrap().into_bytes(),
            ),
        ]);
        let mut units = walk_units(&kzip).unwrap();
        let _ = std::fs::remove_file(&kzip);
        units.sort_by(|a, b| a.unit_sha.cmp(&b.unit_sha));
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].unit_sha, proto_sha);
        assert_eq!(units[0].encoding, UnitEncoding::Proto);
        assert_eq!(units[0].language, "go");
        assert_eq!(units[1].unit_sha, json_sha);
        assert_eq!(units[1].encoding, UnitEncoding::Json);
        assert_eq!(units[1].language, "c++");
    }

    /// `walk_units_serial` yields the same shape as `walk_units` and
    /// honours the language filter: the cxx unit survives, the go
    /// unit is dropped before decode.
    #[test]
    fn serial_walker_honours_language_filter() {
        let cxx_sha  = "00000000000000000000000000000000000000000000000000000000000000c1";
        let go_sha   = "00000000000000000000000000000000000000000000000000000000000000c2";
        let cxx_ic = sample_indexed("c++", "src/foo.cc");
        let go_ic  = sample_indexed("go", "src/foo.go");
        let kzip = write_kzip("serial-filter", &[
            (format!("root/pbunits/{cxx_sha}"), cxx_ic.write_to_bytes().unwrap()),
            (format!("root/pbunits/{go_sha}"),  go_ic.write_to_bytes().unwrap()),
        ]);
        let mut allow = HashSet::new();
        allow.insert("cxx".to_string());
        let units: Vec<_> = walk_units_serial(&kzip, Some(&allow))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let _ = std::fs::remove_file(&kzip);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].language, "c++");
    }

    /// `walk_units_parallel` visits every unit exactly once and the
    /// language filter trims correctly under concurrent reads.
    #[test]
    fn parallel_walker_visits_each_unit_once() {
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..32 {
            let lang = if i % 2 == 0 { "c++" } else { "go" };
            let path = if i % 2 == 0 { "f.cc" } else { "f.go" };
            let sha = format!("{:064x}", i);
            entries.push((
                format!("root/pbunits/{sha}"),
                sample_indexed(lang, path).write_to_bytes().unwrap(),
            ));
        }
        let kzip = write_kzip("parallel-visit", &entries);
        let count = AtomicUsize::new(0);
        let cxx_count = AtomicUsize::new(0);
        let mut allow = HashSet::new();
        allow.insert("cxx".to_string());
        walk_units_parallel(&kzip, 4, Some(&allow), |unit| {
            count.fetch_add(1, Ordering::Relaxed);
            if unit.language == "c++" {
                cxx_count.fetch_add(1, Ordering::Relaxed);
            }
        })
        .unwrap();
        let _ = std::fs::remove_file(&kzip);
        // Filter accepted only the c++ half (16 of 32).
        assert_eq!(count.load(Ordering::Relaxed), 16);
        assert_eq!(cxx_count.load(Ordering::Relaxed), 16);
    }

    /// Parallel walk without a filter visits every unit.
    #[test]
    fn parallel_walker_no_filter_visits_all() {
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..10 {
            let sha = format!("{:064x}", i + 0xa0);
            entries.push((
                format!("root/pbunits/{sha}"),
                sample_indexed("go", "f.go").write_to_bytes().unwrap(),
            ));
        }
        let kzip = write_kzip("parallel-all", &entries);
        let count = AtomicUsize::new(0);
        walk_units_parallel(&kzip, 2, None, |_unit| {
            count.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        let _ = std::fs::remove_file(&kzip);
        assert_eq!(count.load(Ordering::Relaxed), 10);
    }
}
