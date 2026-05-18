//! Optional phase 4b: corpus-wide cross-CU Java resolution.
//!
//! After the per-CU indexers have run and tee'd each indexer's stdout
//! into `<staging>/entries/cu-<sha>.entries`, this module concatenates
//! every per-CU file into a single sorted+deduplicated stream and
//! hands it to `kythe write_tables` to build a LevelDB serving table.
//! The serving table is what scry's phase-5 importer (separate task)
//! walks to resolve `services.core` → `Binder.clearCallingIdentity`
//! and other cross-CU framework-bytecode references that intra-CU
//! resolution alone can't bridge.
//!
//! Runs only when `SCRY_KZIP_SERVING_DIR=<output-dir>` is set. The
//! caller wires the per-CU tee dir + the desired output dir.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};

/// Build a Kythe serving LevelDB at `serving_dir` from every `*.entries`
/// file under `entries_dir`. Pipeline matches what the Kythe tooling
/// documents:
///
/// ```text
/// cat <entries_dir>/cu-*.entries
///   | kythe entrystream --read_format=delimited --unique --sort
///   | kythe write_tables --entries - --out <serving_dir>
/// ```
///
/// `kythe_root` is the directory containing `tools/entrystream` and
/// `tools/write_tables` (same convention as the rest of scry-kzip).
///
/// On a clean run the function leaves the serving LevelDB at
/// `serving_dir`. The intermediate sorted-entries blob is streamed
/// directly between subprocesses so disk usage stays close to (raw
/// entries) + (serving table) rather than 3x.
pub fn build_kythe_serving_table(
    entries_dir: &Path,
    serving_dir: &Path,
    kythe_root: &Path,
) -> Result<()> {
    let entrystream = kythe_root.join("tools").join("entrystream");
    let write_tables = kythe_root.join("tools").join("write_tables");
    for bin in [&entrystream, &write_tables] {
        if !bin.exists() {
            return Err(anyhow!(
                "missing Kythe tool {} (check --kythe-root)",
                bin.display(),
            ));
        }
    }

    // Refuse to silently overwrite an existing serving LevelDB — we
    // never want phase 4b to clobber the operator's prior table mid-
    // crash. Caller is expected to point us at a fresh path.
    if serving_dir.exists() {
        return Err(anyhow!(
            "serving dir already exists at {} — refusing to overwrite",
            serving_dir.display(),
        ));
    }
    let entry_files = collect_entry_files(entries_dir)
        .with_context(|| format!("scan {}", entries_dir.display()))?;
    if entry_files.is_empty() {
        return Err(anyhow!(
            "no *.entries files under {} — phase 3 produced no \
             tee'd output, check SCRY_KZIP_SERVING_DIR was set before \
             the indexer pass",
            entries_dir.display(),
        ));
    }
    eprintln!(
        "[scry-kzip] phase 4b/6: feeding {} per-CU entry files to entrystream",
        entry_files.len(),
    );

    // Spawn: entrystream --read_format=delimited --unique --sort
    //   reads from stdin, writes delimited entries to stdout sorted
    //   in GraphStore order with duplicates collapsed.
    let mut es = Command::new(&entrystream)
        .arg("--read_format=delimited")
        .arg("--write_format=delimited")
        .arg("--unique")
        .arg("--sort")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", entrystream.display()))?;

    // Spawn: write_tables --entries=/dev/stdin --out=<serving_dir>.
    //   Bazel-built write_tables accepts `-` as the entries path to
    //   mean stdin; the released binary uses /dev/stdin on Linux.
    let stdin_arg = "/dev/stdin";
    let mut wt = Command::new(&write_tables)
        .arg(format!("--entries={stdin_arg}"))
        .arg(format!("--out={}", serving_dir.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", write_tables.display()))?;

    // Pump entrystream's stdout → write_tables' stdin in a background
    // thread so we can keep feeding entrystream's stdin without
    // blocking.
    let mut es_out = es.stdout.take().expect("piped");
    let mut wt_in = wt.stdin.take().expect("piped");
    let pipe_handle = std::thread::spawn(move || -> std::io::Result<u64> {
        std::io::copy(&mut es_out, &mut wt_in)
    });

    // Pour every cu-<sha>.entries file into entrystream's stdin.
    {
        let mut es_in = BufWriter::with_capacity(
            1024 * 1024, es.stdin.take().expect("piped"),
        );
        let mut total_bytes: u64 = 0;
        let mut buf = vec![0u8; 256 * 1024];
        for file in &entry_files {
            let f = std::fs::File::open(file)
                .with_context(|| format!("open {}", file.display()))?;
            let mut r = BufReader::with_capacity(256 * 1024, f);
            loop {
                let n = r.read(&mut buf)
                    .with_context(|| format!("read {}", file.display()))?;
                if n == 0 { break; }
                es_in.write_all(&buf[..n])
                    .context("write to entrystream stdin")?;
                total_bytes += n as u64;
            }
        }
        es_in.flush().context("flush entrystream stdin")?;
        // Closing es_in (drop) signals EOF to entrystream so it can
        // finalize its sort and stream the unique-sorted output to
        // write_tables.
        eprintln!(
            "[scry-kzip] phase 4b/6: piped {:.1} MiB of raw entries into entrystream",
            total_bytes as f64 / (1024.0 * 1024.0),
        );
    }

    // Wait for pipe + both children. Surface stderr tails from any
    // failure so the operator has actionable diagnostics.
    let pipe_res = pipe_handle.join()
        .map_err(|_| anyhow!("entrystream→write_tables pump thread panicked"))?;
    pipe_res.context("entrystream→write_tables byte pump")?;

    let es_status = es.wait().context("wait entrystream")?;
    let wt_status = wt.wait().context("wait write_tables")?;

    // Drain stderr (small): write_tables prints "Writing CrossReferences"
    // etc. on success; on failure it dumps a stack trace.
    let mut es_err = String::new();
    if let Some(mut s) = es.stderr.take() {
        let _ = s.read_to_string(&mut es_err);
    }
    let mut wt_err = String::new();
    if let Some(mut s) = wt.stderr.take() {
        let _ = s.read_to_string(&mut wt_err);
    }

    if !es_status.success() {
        return Err(anyhow!(
            "entrystream exited {} — stderr tail: {}",
            es_status.code().unwrap_or(-1),
            tail(&es_err, 4096),
        ));
    }
    if !wt_status.success() {
        return Err(anyhow!(
            "write_tables exited {} — stderr tail: {}",
            wt_status.code().unwrap_or(-1),
            tail(&wt_err, 4096),
        ));
    }
    eprintln!(
        "[scry-kzip] phase 4b/6: serving table written ({} LevelDB files)",
        std::fs::read_dir(serving_dir).map(|i| i.count()).unwrap_or(0),
    );
    Ok(())
}

/// Walk the entries dir, return sorted list of `.entries` file paths.
/// Sort makes the resulting concatenation order stable across runs
/// (helpful for reproducible serving-table builds, even though
/// entrystream's --unique --sort makes byte-level reproducibility a
/// matter of input dedup).
fn collect_entry_files(entries_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(entries_dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("entries") {
            paths.push(p);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Tail at most `n` chars of `s` for log messages. Lossy but bounded.
fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        s[s.len() - n..].to_string()
    }
}
