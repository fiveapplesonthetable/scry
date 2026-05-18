//! Spawn the right Kythe indexer for a per-CU kzip and collect the
//! delimited Entry-proto stream off stdout.
//!
//! Why per-CU and not "process the whole kzip in one go": the
//! Soong-emitted AOSP kzip ships **both** `root/pbunits/` (proto
//! encoding) and `root/units/` (JSON encoding) for every CU. Every
//! Kythe v0.0.75 indexer refuses such mixed-encoding kzips with
//! `INVALID_ARGUMENT: Malformed kzip: multiple unit encodings but
//! different entries`. Building a clean per-CU sub-kzip (just the
//! one pbunit + its `root/files/<sha>` blobs, no JSON twin) is the
//! cleanest workaround that doesn't require us to rewrite the
//! upstream kzip on disk.
//!
//! Per-CU has a second upside: we can parallelise across CUs with
//! rayon, and the JVM-based indexers (which open with a 200-300 MB
//! resident set even for tiny inputs) get bounded concurrency via
//! `num_cpus / 2` so we don't OOM mid-run.

use crate::dispatch::IndexerKind;
use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Wrap the source-kzip `File` in a `BufReader` before handing it
/// to `zip::ZipArchive`. The zip crate hands its inflate decoder
/// raw `Read` access; without buffering each entry open + decode
/// fans out into thousands of small `read()` syscalls. Sized to
/// match the walker's choice (see `walker::ZIP_READ_BUF`); they're
/// kept independent because indexer.rs opens fresh archives per
/// per-CU sub-kzip build and shouldn't import walker's plumbing.
const SRC_KZIP_READ_BUF: usize = 256 * 1024;

/// Resolve a Kythe indexer binary. Two sources, in priority order:
///   1. `SCRY_INDEXER_KYTHE_<NAME>` — per-binary override
///      (e.g. `SCRY_INDEXER_KYTHE_CXX=/opt/bin/cxx_indexer`).
///   2. `<kythe_root>/indexers/<name>` — the canonical layout
///      shipped by the Kythe v0.0.75 release tarball.
pub fn resolve_indexer(kythe_root: &Path, kind: IndexerKind) -> Result<PathBuf> {
    let name = match kind {
        IndexerKind::Cxx          => "cxx_indexer",
        IndexerKind::JavaSource   => "java_indexer.jar",
        IndexerKind::JvmBytecode  => "jvm_indexer.jar",
        IndexerKind::Go           => "go_indexer",
        IndexerKind::Proto        => "proto_indexer",
        IndexerKind::TextProto    => "textproto_indexer",
        IndexerKind::Skip(why) => return Err(anyhow!("indexer skipped: {why}")),
    };
    let env_key = format!(
        "SCRY_INDEXER_KYTHE_{}",
        name.trim_end_matches(".jar")
            .to_ascii_uppercase(),
    );
    if let Some(val) = std::env::var_os(&env_key) {
        let p = PathBuf::from(val);
        if p.is_file() { return Ok(p); }
    }
    let p = kythe_root.join("indexers").join(name);
    if !p.is_file() {
        return Err(anyhow!(
            "indexer binary missing: {} (override with {})",
            p.display(), env_key,
        ));
    }
    Ok(p)
}

/// Outcome of one indexer invocation. We keep stdout for the entry
/// stream and the stderr tail for the per-language log file.
pub struct IndexerRun {
    pub kind: IndexerKind,
    pub unit_sha: String,
    /// Raw stdout — length-delimited Kythe Entry protos.
    pub stdout: Vec<u8>,
    /// Trimmed stderr (last 4 KiB) — diagnostics for the log file.
    pub stderr_tail: Vec<u8>,
    pub exit_code: i32,
    pub wall_secs: f64,
}

impl IndexerRun {
    /// Did the run actually produce a non-empty entries stream?
    /// Indexers regularly fail on individual TUs but still emit
    /// entries for the ones that succeeded — we treat any non-zero
    /// stdout as "useful" and only count truly-empty runs as
    /// failures.
    pub fn produced_entries(&self) -> bool {
        !self.stdout.is_empty()
    }
}

/// Build a per-CU sub-kzip in `staging_dir` containing only the one
/// pbunit + its referenced `root/files/<sha>` blobs (no JSON `units/`
/// twin). Returns the path to the new kzip.
///
/// We use the `stored` (no-compress) zip mode and copy raw bytes
/// straight from the source kzip for the `root/files/<digest>` blobs
/// — repacking is O(unit-size + inputs-size), not O(full-kzip).
///
/// If the source CU is JSON-encoded under `root/units/<sha>` (the
/// AOSP cxx + java extractor norm), we parse it via
/// `protobuf-json-mapping` and re-serialize to proto3 binary before
/// writing it as `root/pbunits/<sha>` in the per-CU sub-kzip. Every
/// Kythe v0.0.75 indexer accepts proto-encoded units; emitting only
/// the proto form sidesteps the mixed-encoding-rejection check that
/// trips when both `pbunits/` and `units/` are present.
pub fn build_per_cu_kzip(
    src_kzip: &Path,
    unit_sha: &str,
    staging_dir: &Path,
) -> Result<PathBuf> {
    use crate::proto::analysis::IndexedCompilation;
    use protobuf::Message;

    let src = File::open(src_kzip)
        .with_context(|| format!("open source kzip {}", src_kzip.display()))?;
    let src_buf = BufReader::with_capacity(SRC_KZIP_READ_BUF, src);
    let mut zin = zip::ZipArchive::new(src_buf)
        .with_context(|| format!("read source kzip header {}", src_kzip.display()))?;

    // Locate the source unit (try `root/pbunits/<sha>` then
    // `root/units/<sha>`) and grab its bytes.
    let (src_entry_name, src_encoding) = locate_unit_entry(&mut zin, unit_sha)?;
    let mut src_unit_bytes = Vec::new();
    zin.by_name(&src_entry_name)
        .with_context(|| format!("open unit {src_entry_name}"))?
        .read_to_end(&mut src_unit_bytes)
        .with_context(|| format!("read unit {src_entry_name}"))?;

    // Decode the unit into the generated proto type regardless of
    // source encoding. The JSON path goes through
    // `protobuf-json-mapping` (accepts snake_case + camelCase field
    // names per the proto3-JSON spec).
    let indexed: IndexedCompilation = match src_encoding {
        SrcEncoding::Proto => IndexedCompilation::parse_from_bytes(&src_unit_bytes)
            .with_context(|| format!("decode IndexedCompilation {unit_sha}"))?,
        SrcEncoding::Json => {
            let text = std::str::from_utf8(&src_unit_bytes).with_context(|| {
                format!("JSON unit {unit_sha} is not valid UTF-8")
            })?;
            protobuf_json_mapping::parse_from_str_with_options(
                text,
                &crate::walker::lenient_json_opts(),
            )
            .with_context(|| format!("parse JSON IndexedCompilation {unit_sha}"))?
        }
    };
    // Always write proto into the per-CU sub-kzip — the indexers
    // accept it uniformly and mixed-encoding kzips are rejected.
    let proto_unit_bytes = indexed
        .write_to_bytes()
        .with_context(|| format!("re-serialize IndexedCompilation {unit_sha}"))?;
    let cu = indexed
        .unit
        .into_option()
        .ok_or_else(|| anyhow!("{unit_sha}: IndexedCompilation.unit absent"))?;
    let mut wanted: Vec<String> = Vec::with_capacity(cu.required_input.len());
    for fi in &cu.required_input {
        if let Some(info) = fi.info.as_ref() {
            if !info.digest.is_empty() {
                wanted.push(format!("root/files/{}", info.digest));
            }
        }
    }
    // Dedup — kzips often list the same blob digest multiple times.
    wanted.sort();
    wanted.dedup();

    // Write a per-CU kzip.
    std::fs::create_dir_all(staging_dir)
        .with_context(|| format!("mkdir staging {}", staging_dir.display()))?;
    let out_path = staging_dir.join(format!("cu-{unit_sha}.kzip"));
    let mut zout = zip::ZipWriter::new(
        File::create(&out_path)
            .with_context(|| format!("create per-CU kzip {}", out_path.display()))?,
    );
    let opts = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    // Top-level skeleton — some indexers (notably java_indexer.jar's
    // Apache-Commons-zip reader) look for directory markers.
    zout.add_directory("root/", opts).ok();
    zout.add_directory("root/pbunits/", opts).ok();
    zout.add_directory("root/files/", opts).ok();
    zout.start_file(format!("root/pbunits/{unit_sha}"), opts)?;
    zout.write_all(&proto_unit_bytes)?;
    let avail: std::collections::HashSet<String> = zin
        .file_names()
        .map(|s| s.to_string())
        .collect();
    for w in &wanted {
        if !avail.contains(w) {
            // Source kzip is missing the blob; skip — the indexer
            // will report its own error if it actually needs it.
            continue;
        }
        let mut buf = Vec::new();
        zin.by_name(w)?.read_to_end(&mut buf)?;
        zout.start_file(w, opts)?;
        zout.write_all(&buf)?;
    }
    zout.finish()?;
    Ok(out_path)
}

/// On-disk encoding of the source unit we just located. Mirrors
/// `walker::UnitEncoding` but kept private here so `indexer.rs` doesn't
/// need a public dep on the walker module's enum surface.
#[derive(Debug, Clone, Copy)]
enum SrcEncoding {
    Proto,
    Json,
}

/// Find the in-zip entry name carrying this unit's bytes, along with
/// its on-disk encoding. Prefers proto (`root/pbunits/<sha>`) when both
/// encodings exist for the same SHA — the same precedence the walker
/// applies.
fn locate_unit_entry(
    zin: &mut zip::ZipArchive<BufReader<File>>,
    unit_sha: &str,
) -> Result<(String, SrcEncoding)> {
    for (cand, enc) in [
        (format!("root/pbunits/{unit_sha}"), SrcEncoding::Proto),
        (format!("root/units/{unit_sha}"),   SrcEncoding::Json),
    ] {
        if zin.by_name(&cand).is_ok() {
            return Ok((cand, enc));
        }
    }
    Err(anyhow!("unit sha {unit_sha} not found in kzip"))
}

/// Spawn the indexer for one CU and capture (stdout, stderr).
/// Honours per-binary env overrides (`SCRY_INDEXER_KYTHE_<NAME>`).
///
/// `cu_kzip_path` must be a CLEAN per-CU sub-kzip (built by
/// [`build_per_cu_kzip`]) — feeding the upstream mixed-encoding
/// AOSP kzip directly will trip every indexer's encoding-mismatch
/// check.
pub fn run_indexer(
    cu_kzip_path: &Path,
    unit_sha: &str,
    kind: IndexerKind,
    kythe_root: &Path,
) -> Result<IndexerRun> {
    let bin = resolve_indexer(kythe_root, kind)?;
    // JVM-based indexers need a writable temp directory to unpack the
    // JDK system modules (`--system <jdk_image>` in the CU's compiler
    // args). Without it, CompilationUnitPathFileManager.setSystemOption
    // raises IllegalArgumentException and the indexer emits zero
    // entries — silently, because java_indexer.jar still exits 0.
    // We allocate one per CU under the same staging dir as `cu_kzip`
    // so the dir lives on `/mnt/agent/tmp` (via `scry_tmp_dir`) and
    // gets removed alongside the rest of the staging tree.
    let jvm_tmp_dir = jvm_temp_dir_for(cu_kzip_path, unit_sha, kind);
    if let Some(dir) = jvm_tmp_dir.as_ref() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("mkdir java_indexer temp_directory {}", dir.display()))?;
    }
    let mut cmd = build_command(&bin, kind, cu_kzip_path, jvm_tmp_dir.as_deref());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let t = std::time::Instant::now();
    let output = cmd
        .output()
        .with_context(|| format!("spawn {} on {}", bin.display(), cu_kzip_path.display()))?;
    let wall_secs = t.elapsed().as_secs_f64();
    let exit_code = output.status.code().unwrap_or(-1);
    let stderr = output.stderr;
    let tail_start = stderr.len().saturating_sub(4096);
    // Best-effort cleanup: the staging tree gets wiped at the end of
    // the run regardless, this just keeps it from ballooning if a
    // long phase 3 processes hundreds of thousands of java CUs.
    if let Some(dir) = jvm_tmp_dir.as_ref() {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(IndexerRun {
        kind,
        unit_sha: unit_sha.to_string(),
        stdout: output.stdout,
        stderr_tail: stderr[tail_start..].to_vec(),
        exit_code,
        wall_secs,
    })
}

/// Per-CU `--temp_directory` path for JVM indexers, or `None` for
/// indexers that don't take that flag. Sits next to `cu_kzip` in the
/// staging dir so it inherits `/mnt/agent/tmp` from the caller.
fn jvm_temp_dir_for(
    cu_kzip: &Path,
    unit_sha: &str,
    kind: IndexerKind,
) -> Option<PathBuf> {
    match kind {
        IndexerKind::JavaSource | IndexerKind::JvmBytecode => {
            let parent = cu_kzip.parent()?;
            Some(parent.join(format!("cu-{unit_sha}-jvm-tmp")))
        }
        _ => None,
    }
}

/// Build the right `Command` for each indexer kind. Each indexer
/// expects a slightly different argv form — see the per-binary help
/// output for the source of truth.
fn build_command(
    bin: &Path,
    kind: IndexerKind,
    cu_kzip: &Path,
    jvm_tmp_dir: Option<&Path>,
) -> Command {
    match kind {
        IndexerKind::Cxx | IndexerKind::Go => {
            // Both take the kzip as a positional argument and emit
            // delimited Entry protos on stdout.
            let mut c = Command::new(bin);
            c.arg(cu_kzip);
            c
        }
        IndexerKind::JavaSource | IndexerKind::JvmBytecode => {
            // `java <heap> -jar <indexer>.jar --temp_directory <dir> <kzip>`.
            // The `--temp_directory` flag is mandatory whenever the
            // CU's compiler args carry `--system <jdk_image>` (every
            // modern AOSP build does) — without it java_indexer
            // silently emits zero entries on those CUs.
            //
            // **Heap sizing.** Default `-Xmx8g`. Earlier `-Xmx2g` was
            // chosen as a "conservative ceiling so one CU can't claim
            // the host"; AOSP's services.core `am/` batch (Activity-
            // ManagerService, BroadcastQueueImpl, BroadcastRecord,
            // ProcessRecord ...) blows past 2g just building javac
            // line maps and OOMs with `java.lang.OutOfMemoryError:
            // Java heap space`. The wrapper exits 0 silently — the
            // CU shows as "empty" in the summary, masking the bug.
            // 8g handles every observed AOSP batch with margin.
            // `SCRY_KZIP_JVM_HEAP=N` (e.g. `16g`) overrides for
            // pathological corpora; 16 workers × 8g = 128 GB peak
            // worst-case, comfortable on the 157 GiB host.
            let heap = std::env::var("SCRY_KZIP_JVM_HEAP")
                .unwrap_or_else(|_| "8g".to_string());
            let mut c = Command::new("java");
            c.arg(format!("-Xmx{heap}"))
                .arg("-jar")
                .arg(bin)
                .arg("--ignore_empty_kzip");
            if let Some(dir) = jvm_tmp_dir {
                c.arg("--temp_directory").arg(dir);
            }
            c.arg(cu_kzip);
            c
        }
        IndexerKind::Proto => {
            // proto_indexer uses single-dash flags (gflags).
            let mut c = Command::new(bin);
            c.arg(format!("-index_file={}", cu_kzip.display()));
            c
        }
        IndexerKind::TextProto => {
            // textproto_indexer uses double-dash flags.
            let mut c = Command::new(bin);
            c.arg(format!("--index_file={}", cu_kzip.display()));
            c
        }
        IndexerKind::Skip(_) => Command::new(bin),
    }
}

/// Recommended worker count for parallel indexer dispatch. JVM-based
/// indexers (Java, JVM-bytecode) keep a 200-300 MB resident set even
/// for trivial inputs, so we cap at half the CPU count.
pub fn recommended_workers() -> usize {
    let n = num_cpus::get();
    std::cmp::max(1, n / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_missing_indexer_errors() {
        let tmp = scry_bridge::scry_tmp_dir().join(format!(
            "scry-kzip-noindexer-{}", std::process::id()));
        let err = resolve_indexer(&tmp, IndexerKind::Cxx).unwrap_err();
        assert!(format!("{err}").contains("missing"));
    }

    #[test]
    fn skip_kind_errors() {
        let tmp = scry_bridge::scry_tmp_dir().join("any");
        let err = resolve_indexer(&tmp, IndexerKind::Skip("test reason")).unwrap_err();
        assert!(format!("{err}").contains("skipped"));
    }

    /// JVM kinds get a per-CU temp dir sibling to the cu_kzip; other
    /// kinds skip the allocation entirely. The path encodes the unit
    /// sha so concurrent workers don't collide.
    #[test]
    fn jvm_temp_dir_only_for_jvm_kinds() {
        let cu = Path::new("/mnt/agent/tmp/staging/cu-abc123.kzip");
        let sha = "abc123";
        assert_eq!(
            jvm_temp_dir_for(cu, sha, IndexerKind::JavaSource),
            Some(PathBuf::from("/mnt/agent/tmp/staging/cu-abc123-jvm-tmp")),
        );
        assert_eq!(
            jvm_temp_dir_for(cu, sha, IndexerKind::JvmBytecode),
            Some(PathBuf::from("/mnt/agent/tmp/staging/cu-abc123-jvm-tmp")),
        );
        assert_eq!(jvm_temp_dir_for(cu, sha, IndexerKind::Cxx), None);
        assert_eq!(jvm_temp_dir_for(cu, sha, IndexerKind::Go), None);
        assert_eq!(jvm_temp_dir_for(cu, sha, IndexerKind::Proto), None);
        assert_eq!(jvm_temp_dir_for(cu, sha, IndexerKind::TextProto), None);
        assert_eq!(jvm_temp_dir_for(cu, sha, IndexerKind::Skip("x")), None);
    }

    /// Java indexer commands MUST include `--temp_directory` whenever
    /// a tmp dir is provided. Regression test for the services.core
    /// "empty" CU bug: without this flag, java_indexer.jar silently
    /// returns zero entries for every CU whose compiler args include
    /// `--system <jdk_image>` (i.e. every modern Java CU).
    #[test]
    fn java_command_includes_temp_directory() {
        let bin = Path::new("/fake/java_indexer.jar");
        let cu = Path::new("/mnt/agent/tmp/staging/cu-x.kzip");
        let tmp = Path::new("/mnt/agent/tmp/staging/cu-x-jvm-tmp");
        let cmd = build_command(bin, IndexerKind::JavaSource, cu, Some(tmp));
        let args: Vec<String> = cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = args.iter().position(|a| a == "--temp_directory")
            .expect("--temp_directory must be present");
        assert_eq!(args.get(pos + 1).map(String::as_str), Some(tmp.to_str().unwrap()),
            "--temp_directory must be followed by the tmp dir path; got args: {args:?}");
        assert!(args.iter().any(|a| a == "--ignore_empty_kzip"),
            "--ignore_empty_kzip preserved");
        assert!(args.last().map(String::as_str) == Some(cu.to_str().unwrap()),
            "cu_kzip must come last (positional); got args: {args:?}");
    }

    /// Belt-and-suspenders: Cxx command stays unchanged (no
    /// `--temp_directory`, just the positional kzip).
    #[test]
    fn cxx_command_has_no_temp_directory() {
        let bin = Path::new("/fake/cxx_indexer");
        let cu = Path::new("/mnt/agent/tmp/staging/cu-y.kzip");
        let cmd = build_command(bin, IndexerKind::Cxx, cu, None);
        let args: Vec<String> = cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a == "--temp_directory"),
            "cxx_indexer takes no --temp_directory; got args: {args:?}");
        assert_eq!(args, vec![cu.to_string_lossy().into_owned()]);
    }
}
