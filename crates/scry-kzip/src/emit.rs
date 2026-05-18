//! Flush decoded Kythe records into scry's mmap-packed precision
//! sidecars.
//!
//! Two output flavours, both backed by `scry_store::precision_packed`:
//!
//! * `clang_usrs.bin` — C/C++/ObjC (magic `SCRYUP01`). Fed by the
//!   `cxx_indexer` arm of the dispatcher; symbol strings are the
//!   Kythe-formatted vname (kept stable so the libclang-USR reader
//!   and the kzip-derived USR reader can use the same query path).
//! * `scip_index.bin` — Java / Kotlin (via JVM) / Go / proto /
//!   textproto (magic `SCRYSP01`). Symbol-string-keyed, matches the
//!   existing SCIP importer's reader.
//!
//! Both writers interleave via the wrapper macro in
//! `precision_packed.rs` — we only need to construct the typed
//! input record (`UsrRecord` / `ScipRecord`) and the symbol-table
//! `Vec<String>`, then call the writer.
//!
//! ## Aggregation
//!
//! Each per-CU decode call hands us records keyed by file path.
//! Same path may be touched by multiple CUs (kotlin srcjar shared
//! across compilations, for instance), so we dedup on
//! `(abs_path, byte_offset, symbol_id, role)` while accumulating —
//! the sidecar reader's `symbol_for_window` would happily return the
//! same record twice otherwise.
//!
//! ## Crash-safety
//!
//! When the driver constructs the emitter via [`PackedEmitter::with_checkpoint`]
//! every committed CU's records also land in an append-only on-disk
//! log under `<out>/kythe-logs/checkpoint/` (see
//! [`crate::emit_checkpoint`]). A SIGTERM mid-phase-3 leaves the log
//! intact; the next `--resume` run replays the log back into the
//! in-memory buckets via [`PackedEmitter::replay_from_checkpoint`]
//! and the walker skips CUs whose SHAs are already in the done-set.
//!
//! The non-checkpoint constructor [`PackedEmitter::new`] is kept for
//! tests and for callers that don't need crash-safety.

use crate::dispatch::IndexerKind;
use crate::emit_checkpoint::{
    CheckpointManifest, LogKind, LogRecord, RecordLog,
};
use crate::entries::{DecodedRecord, Role};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One bucket of accumulated records — either Cxx-flavoured (writes
/// `clang_usrs.bin`) or SCIP-flavoured (writes `scip_index.bin`).
#[derive(Debug, Default)]
struct Bucket {
    symbol_table: Vec<String>,
    /// `symbol -> symbol_id`.
    sym_index: HashMap<String, u32>,
    /// `(path, offset, sym, role) -> ()` dedup.
    seen: HashSet<(String, u32, u32, u8)>,
    /// Aggregated rows, ready to hand to the writer at finalize-time.
    rows: Vec<(String, u32, u32, u8)>,
}

impl Bucket {
    fn record(&mut self, path: &str, offset: u32, sym: &str, role: u8) {
        let sid = match self.sym_index.get(sym) {
            Some(&id) => id,
            None => {
                let id = self.symbol_table.len() as u32;
                self.symbol_table.push(sym.to_string());
                self.sym_index.insert(sym.to_string(), id);
                id
            }
        };
        let key = (path.to_string(), offset, sid, role);
        if self.seen.insert(key.clone()) {
            self.rows.push(key);
        }
    }
}

/// Per-emitter checkpoint state: open log handles for each
/// per-language record log, plus the bookkeeping needed to rewrite
/// the manifest. Owned by `PackedEmitter` when constructed via
/// [`PackedEmitter::with_checkpoint`].
pub struct CheckpointState {
    pub dir: PathBuf,
    pub manifest: Mutex<CheckpointManifest>,
    pub cxx_log: RecordLog,
    pub scip_log: RecordLog,
    /// CU SHAs we've already recorded — used by the driver to skip
    /// repeat work on resume. Updated under the same lock that does
    /// the manifest mutation so the manifest is never ahead of the
    /// in-memory set.
    pub done_shas: Mutex<HashSet<String>>,
    /// How many `commit_cu` calls between manifest rewrites. The log
    /// is the source of truth — the manifest is just a hint to keep
    /// `done_shas` close to the on-disk reality without paying a JSON
    /// rewrite every CU. See
    /// [`crate::emit_checkpoint::DEFAULT_MANIFEST_EVERY`].
    pub manifest_every: usize,
    /// CU commits since the last manifest rewrite. Reset on flush.
    commits_since_manifest_flush: Mutex<usize>,
}

impl CheckpointState {
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    /// Update the manifest's `done_shas` list from the in-memory set
    /// and write it atomically.
    pub fn flush_manifest(&self) -> Result<()> {
        let mut m = self.manifest.lock().unwrap();
        let snap = {
            let s = self.done_shas.lock().unwrap();
            let mut v: Vec<String> = s.iter().cloned().collect();
            v.sort();
            v
        };
        m.done_shas = snap;
        m.save_atomic(&self.dir.join("manifest.json"))?;
        *self.commits_since_manifest_flush.lock().unwrap() = 0;
        Ok(())
    }
}

/// Routes per-CU `DecodedRecord`s into the right (cxx vs scip)
/// bucket. Thread-safe: indexer dispatch runs across rayon workers,
/// each calling `commit_cu` concurrently.
///
/// **Source root**. Kythe VName paths are corpus-relative (e.g.
/// `frameworks/base/services/core/.../BroadcastQueueImpl.java`).
/// scry's query layer expects absolute filesystem paths
/// (`/home/zim/dev/aosp/frameworks/base/...`). Without an explicit
/// source root, every precision lookup misses with "uncovered TU"
/// because the abs_path on the SCIP/USR record never matches what
/// the query passes in. `source_root` is the on-disk prefix to
/// prepend to relative paths; paths that already start with `/`
/// (e.g. `/kythe_builtins/`) are passed through unchanged — they're
/// synthetic Kythe paths that don't exist on disk anyway and won't
/// match any real query.
pub struct PackedEmitter {
    source_root: Option<PathBuf>,
    cxx: Mutex<Bucket>,
    scip: Mutex<Bucket>,
    /// `None` when the caller opted out of crash-safety (tests, ad-hoc
    /// callers). `Some` enables the per-CU record log + manifest.
    checkpoint: Option<CheckpointState>,
}

impl Default for PackedEmitter {
    fn default() -> Self { Self::new() }
}

impl PackedEmitter {
    /// Construct an in-memory-only emitter — no on-disk checkpoint,
    /// no source root (paths stored verbatim, only useful for tests).
    pub fn new() -> Self {
        Self {
            source_root: None,
            cxx: Mutex::new(Bucket::default()),
            scip: Mutex::new(Bucket::default()),
            checkpoint: None,
        }
    }

    /// Construct an emitter that mirrors every successful CU's
    /// records into the on-disk checkpoint at `checkpoint.dir`. The
    /// driver wires this up when an out-dir resume policy is in
    /// effect (see [`crate::driver::build_packed_from_kzip`]).
    pub fn with_checkpoint(checkpoint: CheckpointState) -> Self {
        Self {
            source_root: None,
            cxx: Mutex::new(Bucket::default()),
            scip: Mutex::new(Bucket::default()),
            checkpoint: Some(checkpoint),
        }
    }

    /// Builder: set the source root that absolutizes Kythe
    /// corpus-relative paths before they hit the bucket. Without
    /// this, every precision query against AOSP / kernel returns
    /// "uncovered TU" because the abs_path on the record is
    /// `frameworks/...` while the query has `/home/.../frameworks/...`.
    pub fn with_source_root(mut self, source_root: PathBuf) -> Self {
        self.source_root = Some(source_root);
        self
    }

    /// Borrow the configured source root for fingerprinting /
    /// diagnostics.
    pub fn source_root(&self) -> Option<&Path> {
        self.source_root.as_deref()
    }

    /// Prepend `source_root` to `p` when `p` is corpus-relative.
    /// Paths that already start with `/` (e.g. `/kythe_builtins/...`)
    /// are returned unchanged — those are synthetic Kythe paths that
    /// don't exist on disk and won't match any real query. Paths
    /// stay borrowed when no transformation is needed (hot path).
    fn absolutize<'a>(&self, p: &'a str) -> std::borrow::Cow<'a, str> {
        match (self.source_root.as_ref(), p.starts_with('/')) {
            (Some(root), false) => {
                let mut s = root.to_string_lossy().into_owned();
                if !s.ends_with('/') { s.push('/'); }
                s.push_str(p);
                std::borrow::Cow::Owned(s)
            }
            _ => std::borrow::Cow::Borrowed(p),
        }
    }

    /// Borrow the checkpoint state (None when the emitter was built
    /// without one). Used by the driver to flush the manifest on a
    /// successful run-completion.
    pub fn checkpoint(&self) -> Option<&CheckpointState> {
        self.checkpoint.as_ref()
    }

    /// Commit one CU's complete set of decoded records. Single atomic
    /// step: appends them to the per-language record log (when a
    /// checkpoint is enabled), merges them into the in-memory bucket,
    /// and notes the CU SHA as done. Returns `Ok(())` after the merge
    /// succeeds; the log append happens BEFORE the in-memory merge so
    /// a crash mid-merge isn't visible to a later resume.
    ///
    /// `Skip(_)` indexer kinds are dropped (the driver shouldn't be
    /// calling us in that case — defence in depth).
    pub fn commit_cu(
        &self,
        cu_sha: &str,
        kind: IndexerKind,
        records: &[DecodedRecord],
    ) -> Result<()> {
        let log_kind = match LogKind::for_indexer(kind) {
            Some(k) => k,
            None => return Ok(()),
        };
        // Step 1: persist to the log (if enabled).
        if let Some(cp) = self.checkpoint.as_ref() {
            let log_records: Vec<LogRecord> =
                records.iter().map(LogRecord::from_decoded).collect();
            let log = match log_kind {
                LogKind::Cxx => &cp.cxx_log,
                LogKind::Scip => &cp.scip_log,
            };
            log.append_cu(cu_sha, &log_records)
                .with_context(|| format!(
                    "append CU {} to {} record log", cu_sha, log_kind.file_name(),
                ))?;
        }
        // Step 2: merge into the in-memory bucket.
        let bucket = match log_kind {
            LogKind::Cxx => &self.cxx,
            LogKind::Scip => &self.scip,
        };
        {
            let mut b = bucket.lock().unwrap();
            for rec in records {
                let role_byte = role_byte_of(rec.role);
                let abs = self.absolutize(&rec.file_path);
                b.record(&abs, rec.start, &rec.target_symbol, role_byte);
            }
        }
        // Step 3: bookkeeping.
        if let Some(cp) = self.checkpoint.as_ref() {
            {
                let mut s = cp.done_shas.lock().unwrap();
                s.insert(cu_sha.to_string());
            }
            let should_flush = {
                let mut n = cp.commits_since_manifest_flush.lock().unwrap();
                *n += 1;
                *n >= cp.manifest_every
            };
            if should_flush {
                cp.flush_manifest()
                    .context("refresh checkpoint manifest")?;
            }
        }
        Ok(())
    }

    /// Replay records from the on-disk checkpoint into the in-memory
    /// buckets. Only meaningful when [`PackedEmitter::with_checkpoint`]
    /// was used. Returns a per-kind `(cu_count, record_count)` tuple
    /// so the driver can log how much state survived the crash.
    pub fn replay_from_checkpoint(&self) -> Result<ReplayCounts> {
        let cp = match self.checkpoint.as_ref() {
            Some(c) => c,
            None => return Ok(ReplayCounts::default()),
        };
        let mut counts = ReplayCounts::default();
        // Cxx log.
        {
            let mut b = self.cxx.lock().unwrap();
            let rep = cp.cxx_log.replay(|recs| {
                for r in recs {
                    let abs = self.absolutize(&r.file_path);
                    b.record(&abs, r.byte_offset, &r.symbol, r.role);
                }
            }).context("replay cxx record log")?;
            counts.cxx_cus = rep.cu_hashes.len();
            counts.cxx_records = rep.total_records;
            counts.cxx_truncated_bytes = rep.truncated_bytes;
            // Repopulate done_shas from the manifest's persisted
            // strings, falling back to leaving the hash-only side
            // untouched. (The manifest is the only place we have the
            // original SHA strings; the log only carries their hash.)
        }
        // Scip log.
        {
            let mut b = self.scip.lock().unwrap();
            let rep = cp.scip_log.replay(|recs| {
                for r in recs {
                    let abs = self.absolutize(&r.file_path);
                    b.record(&abs, r.byte_offset, &r.symbol, r.role);
                }
            }).context("replay scip record log")?;
            counts.scip_cus = rep.cu_hashes.len();
            counts.scip_records = rep.total_records;
            counts.scip_truncated_bytes = rep.truncated_bytes;
        }
        // Seed the done-set from the manifest. The manifest may be
        // up to `manifest_every` CUs behind the log; the residual
        // CUs will be re-processed on resume (idempotent — same
        // records produce the same bucket entries thanks to the dedup
        // key).
        {
            let m = cp.manifest.lock().unwrap();
            let mut s = cp.done_shas.lock().unwrap();
            for sha in &m.done_shas {
                s.insert(sha.clone());
            }
        }
        Ok(counts)
    }

    /// Flush both buckets to `out_dir`. Writes are atomic
    /// (tmp + rename) so concurrent readers never see a partial
    /// sidecar. Returns per-flavour record counts for the report.
    pub fn finalize(self, out_dir: &Path) -> Result<EmitReport> {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("mkdir {}", out_dir.display()))?;
        // Best-effort final manifest flush so the on-disk done-set
        // matches what the sidecar carries.
        if let Some(cp) = self.checkpoint.as_ref() {
            if let Err(e) = cp.flush_manifest() {
                eprintln!(
                    "[scry-kzip] checkpoint: final manifest flush failed ({e}) — log itself is still consistent",
                );
            }
        }

        let cxx = self.cxx.into_inner().unwrap();
        let scip = self.scip.into_inner().unwrap();

        let cxx_records = write_cxx(out_dir, &cxx)?;
        let scip_records = write_scip(out_dir, &scip)?;

        Ok(EmitReport {
            cxx_records,
            cxx_symbols: cxx.symbol_table.len(),
            scip_records,
            scip_symbols: scip.symbol_table.len(),
        })
    }

    /// Construct a `CheckpointState` for the given out-dir, opening
    /// (and creating if missing) the two record-log files. Pure
    /// helper; the driver calls this once the resume-policy
    /// validation has accepted the (resume, checkpoint-exists) state.
    pub fn build_checkpoint_state(
        dir: &Path,
        manifest: CheckpointManifest,
        manifest_every: usize,
    ) -> Result<CheckpointState> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("mkdir checkpoint {}", dir.display()))?;
        let cxx_log = RecordLog::open(dir, LogKind::Cxx)?;
        let scip_log = RecordLog::open(dir, LogKind::Scip)?;
        Ok(CheckpointState {
            dir: dir.to_path_buf(),
            manifest: Mutex::new(manifest),
            cxx_log,
            scip_log,
            done_shas: Mutex::new(HashSet::new()),
            manifest_every: manifest_every.max(1),
            commits_since_manifest_flush: Mutex::new(0),
        })
    }
}

fn role_byte_of(role: Role) -> u8 {
    match role {
        Role::Decl => 0,
        Role::Ref => 1,
        Role::Call => 2,
    }
}

/// Per-kind replay report. `cxx_cus + scip_cus` is the number of CUs
/// the resumed run will SKIP in phase 1 / 2 / 3.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplayCounts {
    pub cxx_cus: usize,
    pub cxx_records: usize,
    pub cxx_truncated_bytes: usize,
    pub scip_cus: usize,
    pub scip_records: usize,
    pub scip_truncated_bytes: usize,
}

impl ReplayCounts {
    pub fn total_records(&self) -> usize {
        self.cxx_records + self.scip_records
    }
}

/// Counts after a `PackedEmitter::finalize`. Hands back to the
/// driver for the per-phase summary line.
#[derive(Debug, Clone, Copy)]
pub struct EmitReport {
    pub cxx_records: usize,
    pub cxx_symbols: usize,
    pub scip_records: usize,
    pub scip_symbols: usize,
}

fn write_cxx(out_dir: &Path, bucket: &Bucket) -> Result<usize> {
    use scry_store::clang_usrs::{self, UsrRecord};
    let out = out_dir.join("clang_usrs.bin");
    let tmp = tmp_path(&out);
    let recs: Vec<UsrRecord> = bucket.rows.iter().map(|(p, off, sid, k)| {
        UsrRecord {
            abs_path: p.clone(),
            byte_offset: *off,
            usr_id: *sid,
            kind: *k,
        }
    }).collect();
    clang_usrs::write(&tmp, &bucket.symbol_table, &recs)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))?;
    Ok(recs.len())
}

fn write_scip(out_dir: &Path, bucket: &Bucket) -> Result<usize> {
    use scry_store::scip_index::{self, ScipRecord};
    let out = out_dir.join("scip_index.bin");
    let tmp = tmp_path(&out);
    let recs: Vec<ScipRecord> = bucket.rows.iter().map(|(p, off, sid, k)| {
        ScipRecord {
            abs_path: p.clone(),
            byte_offset: *off,
            symbol_id: *sid,
            role: *k,
        }
    }).collect();
    scip_index::write(&tmp, &bucket.symbol_table, &recs)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))?;
    Ok(recs.len())
}

/// Tmp filename next to `dst`, used for atomic tmp+rename writes.
fn tmp_path(dst: &Path) -> PathBuf {
    dst.with_extension(format!(
        "{}.tmp",
        dst.extension().map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit_checkpoint::{
        CheckpointManifest, DEFAULT_MANIFEST_EVERY, KzipFingerprint,
    };
    use crate::entries::DecodedRecord;

    fn dec(path: &str, start: u32, sym: &str, role: Role) -> DecodedRecord {
        DecodedRecord {
            file_path: path.to_string(),
            start,
            end: start + 1,
            target_symbol: sym.to_string(),
            role,
        }
    }

    fn tmp_dir(label: &str) -> PathBuf {
        let p = scry_bridge::scry_tmp_dir().join(format!(
            "scry-kzip-emit-{}-{}-{:?}", label,
            std::process::id(), std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fresh_manifest() -> CheckpointManifest {
        CheckpointManifest::fresh(
            KzipFingerprint {
                path: "/x.kzip".to_string(),
                size_bytes: 1,
                mtime_nanos: 1,
            },
            Vec::new(),
            Path::new("/k"),
            None,
        )
    }

    /// Kythe paths are corpus-relative; the emitter must prepend the
    /// configured source root before storing so query-time absolute
    /// paths can match. Paths that already start with `/` (synthetic
    /// Kythe paths like `/kythe_builtins/...`) are passed through
    /// unchanged — they don't exist on disk and won't match any
    /// real query, but we keep them for storage round-trip.
    /// Without this, every precision lookup against AOSP returned 0
    /// hits ("uncovered TU" for every file) regardless of corpus
    /// coverage. Regression for that bug.
    #[test]
    fn absolutize_prepends_source_root_for_relative_only() {
        let e = PackedEmitter::new().with_source_root(PathBuf::from("/home/zim/dev/aosp"));
        assert_eq!(
            &*e.absolutize("frameworks/base/services/core/X.java"),
            "/home/zim/dev/aosp/frameworks/base/services/core/X.java",
        );
        // Trailing-slash safe.
        let e2 = PackedEmitter::new().with_source_root(PathBuf::from("/home/zim/dev/aosp/"));
        assert_eq!(
            &*e2.absolutize("frameworks/base/X.java"),
            "/home/zim/dev/aosp/frameworks/base/X.java",
        );
        // Absolute-prefixed Kythe paths pass through.
        assert_eq!(&*e.absolutize("/kythe_builtins/include/stdint.h"),
            "/kythe_builtins/include/stdint.h");
        // No source root → no transformation.
        let e3 = PackedEmitter::new();
        assert_eq!(&*e3.absolutize("frameworks/base/X.java"), "frameworks/base/X.java");
    }

    /// End-to-end via commit_cu: the bucket row's abs_path field
    /// must reflect the absolutized form.
    #[test]
    fn commit_cu_stores_absolutized_path() {
        let e = PackedEmitter::new().with_source_root(PathBuf::from("/home/zim/dev/aosp"));
        let r = dec(
            "frameworks/base/services/core/X.java",
            100, "kythe:java:X", Role::Decl,
        );
        e.commit_cu("cu-1", IndexerKind::JavaSource, &[r]).unwrap();
        let out = tmp_dir("abs");
        e.finalize(&out).unwrap();
        let bytes = std::fs::read(out.join("scip_index.bin")).unwrap();
        let abs = b"/home/zim/dev/aosp/frameworks/base/services/core/X.java";
        let rel = b"frameworks/base/services/core/X.java";
        // Sidecar should carry the absolute form, never the bare relative.
        // (The relative form is a substring of the absolute, so just
        // checking presence is ambiguous — check absence of a path that
        // starts at a byte boundary right after a length prefix would
        // be the strict version; here we settle for: the absolute path
        // is present, and the source_root prefix is too.)
        assert!(memmem(&bytes, abs).is_some(),
            "absolutized path must appear in the sidecar");
        // The bare relative path's first byte should be preceded by '/aosp/'
        // every time (i.e. it never appears un-prefixed). We can verify
        // this by counting: every occurrence of `rel` should be inside
        // the abs string.
        let rel_count = memmem_count(&bytes, rel);
        let abs_count = memmem_count(&bytes, abs);
        assert_eq!(rel_count, abs_count,
            "every relative-path occurrence should be inside an absolutized one; \
             rel={rel_count} abs={abs_count}");
    }

    fn memmem(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() { return None; }
        (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
    }
    fn memmem_count(hay: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() { return 0; }
        let mut i = 0; let mut n = 0;
        while i + needle.len() <= hay.len() {
            if &hay[i..i + needle.len()] == needle { n += 1; i += needle.len(); }
            else { i += 1; }
        }
        n
    }

    #[test]
    fn deduplicates_identical_records() {
        let e = PackedEmitter::new();
        let r = dec("/x/A.cc", 100, "kythe:c++:foo", Role::Decl);
        e.commit_cu("cu-sha-1", IndexerKind::Cxx, std::slice::from_ref(&r)).unwrap();
        e.commit_cu("cu-sha-2", IndexerKind::Cxx, &[r]).unwrap();
        let out = tmp_dir("dedup");
        let rep = e.finalize(&out).unwrap();
        assert_eq!(rep.cxx_records, 1);
        assert_eq!(rep.cxx_symbols, 1);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn routes_cxx_to_usr_and_else_to_scip() {
        let e = PackedEmitter::new();
        e.commit_cu("cu1", IndexerKind::Cxx,
            &[dec("/A.cc", 10, "csym", Role::Decl)]).unwrap();
        e.commit_cu("cu2", IndexerKind::Go,
            &[dec("/A.go", 20, "gsym", Role::Ref)]).unwrap();
        e.commit_cu("cu3", IndexerKind::JavaSource,
            &[dec("/A.java", 30, "jsym", Role::Call)]).unwrap();
        let out = tmp_dir("route");
        let rep = e.finalize(&out).unwrap();
        assert_eq!(rep.cxx_records, 1);
        assert_eq!(rep.scip_records, 2);
        assert!(out.join("clang_usrs.bin").exists());
        assert!(out.join("scip_index.bin").exists());
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Crash-safety round-trip: commit 3 CUs against an emitter with
    /// a checkpoint, drop the emitter (simulates kill), open a fresh
    /// emitter against the same checkpoint dir, replay, verify all
    /// records present.
    #[test]
    fn commit_then_drop_then_replay_recovers_all_records() {
        let dir = tmp_dir("commit-replay");
        let ckpt_dir = dir.join("kythe-logs/checkpoint");
        let manifest = fresh_manifest();
        let cp = PackedEmitter::build_checkpoint_state(
            &ckpt_dir, manifest.clone(), DEFAULT_MANIFEST_EVERY,
        ).unwrap();
        let e = PackedEmitter::with_checkpoint(cp);
        e.commit_cu("sha-A", IndexerKind::Cxx, &[
            dec("/a.cc", 1, "k:foo", Role::Decl),
            dec("/a.cc", 5, "k:bar", Role::Ref),
        ]).unwrap();
        e.commit_cu("sha-B", IndexerKind::Cxx, &[
            dec("/b.cc", 9, "k:baz", Role::Call),
        ]).unwrap();
        e.commit_cu("sha-C", IndexerKind::Go, &[
            dec("/c.go", 2, "k:goo", Role::Decl),
        ]).unwrap();
        // Force a manifest write so the second-run done-set seed is
        // populated; in production the periodic flush would have done
        // this for us.
        e.checkpoint().unwrap().flush_manifest().unwrap();
        drop(e);

        // Second emitter against the same checkpoint dir.
        let manifest2 = CheckpointManifest::load(
            &ckpt_dir.join("manifest.json"),
        ).unwrap();
        let cp2 = PackedEmitter::build_checkpoint_state(
            &ckpt_dir, manifest2, DEFAULT_MANIFEST_EVERY,
        ).unwrap();
        let e2 = PackedEmitter::with_checkpoint(cp2);
        let counts = e2.replay_from_checkpoint().unwrap();
        assert_eq!(counts.cxx_cus, 2);
        assert_eq!(counts.cxx_records, 3);
        assert_eq!(counts.scip_cus, 1);
        assert_eq!(counts.scip_records, 1);
        let done = e2.checkpoint().unwrap().done_shas.lock().unwrap();
        assert!(done.contains("sha-A"));
        assert!(done.contains("sha-B"));
        assert!(done.contains("sha-C"));
        drop(done);

        // Finalising the second emitter without any new commits
        // should still emit a fully-formed sidecar carrying all
        // replayed records.
        let out = dir.join("out");
        let rep = e2.finalize(&out).unwrap();
        assert_eq!(rep.cxx_records, 3);
        assert_eq!(rep.scip_records, 1);
        assert!(out.join("clang_usrs.bin").exists());
        assert!(out.join("scip_index.bin").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Commits stay deduped across the replay boundary — a CU
    /// reprocessed after replay (e.g. one that landed between the
    /// last manifest flush and the crash) doesn't double-count
    /// records.
    #[test]
    fn replay_then_recommit_same_cu_is_idempotent() {
        let dir = tmp_dir("idempotent");
        let ckpt_dir = dir.join("kythe-logs/checkpoint");
        let cp1 = PackedEmitter::build_checkpoint_state(
            &ckpt_dir, fresh_manifest(), DEFAULT_MANIFEST_EVERY,
        ).unwrap();
        let e1 = PackedEmitter::with_checkpoint(cp1);
        e1.commit_cu("sha-X", IndexerKind::Cxx, &[
            dec("/x.cc", 100, "k:sym", Role::Decl),
        ]).unwrap();
        drop(e1);

        let manifest2 = fresh_manifest();
        let cp2 = PackedEmitter::build_checkpoint_state(
            &ckpt_dir, manifest2, DEFAULT_MANIFEST_EVERY,
        ).unwrap();
        let e2 = PackedEmitter::with_checkpoint(cp2);
        e2.replay_from_checkpoint().unwrap();
        // Manifest was never flushed in the first session, so done_shas
        // is empty; re-commit the same CU and verify the bucket dedup
        // keeps the record count at 1.
        e2.commit_cu("sha-X", IndexerKind::Cxx, &[
            dec("/x.cc", 100, "k:sym", Role::Decl),
        ]).unwrap();
        let out = dir.join("out");
        let rep = e2.finalize(&out).unwrap();
        assert_eq!(rep.cxx_records, 1, "duplicate record should be deduped");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
