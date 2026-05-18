//! Crash-safe per-CU checkpoint for `PackedEmitter`.
//!
//! AOSP-scale ingests take 3–12 hours; without a checkpoint a SIGTERM
//! mid-phase-3 throws away every per-CU record because the buckets
//! live only in `PackedEmitter`'s in-memory `HashMap`s and the
//! `clang_usrs.bin` / `scip_index.bin` sidecars aren't written until
//! phase 4. This module owns the on-disk state that survives a kill:
//!
//! * A per-language append-only **record log** under
//!   `<out>/kythe-logs/checkpoint/{cxx,scip}.records.log`. Each frame
//!   carries one CU's records. Bincode-encoded payload, length-
//!   prefixed so a tail truncation (process killed mid-write) is
//!   detectable and recoverable.
//! * A **manifest** at `<out>/kythe-logs/checkpoint/manifest.json`
//!   carrying the fingerprint of the kzip + env + kythe-root the
//!   checkpoint was built against, plus the set of compilation-unit
//!   SHAs that have been fully recorded. Atomic write (tmp + rename),
//!   refreshed every `SCRY_KZIP_CHECKPOINT_EVERY` CUs and at clean
//!   shutdown.
//!
//! ## Why explicit-flag (no auto-resume)
//!
//! `build_packed_from_kzip` only touches the checkpoint dir when the
//! caller passes `resume: true`. Auto-resume would let a user re-run
//! against the same out-dir and silently inherit half-finished state,
//! getting results that don't match a fresh run. Production tools
//! (kubelet, terraform, bazel) require explicit opt-in; we do the
//! same. See [`crate::driver::build_packed_from_kzip`] for the
//! three-state validator.
//!
//! ## Crash-safety story
//!
//! The records log is append-only. Each frame is:
//!
//! ```text
//!   [u64 cu_sha_hash][u32 record_count][u32 byte_len][bincode records]
//! ```
//!
//! A SIGKILL between `write` and the next CU's `write` truncates at
//! frame boundary — replay reads complete frames and stops on the
//! first incomplete tail (with a warning). No CRC: bincode + the
//! length prefix is enough to detect truncation; per-record CRC would
//! cost CPU we don't need (the underlying filesystem already protects
//! the bytes).
//!
//! The manifest is the bookkeeping side-channel: which CUs have been
//! committed. It's written atomically (`manifest.json.tmp` + rename),
//! refreshed periodically. If the manifest is behind the log on
//! resume (we crashed between log-append and manifest-refresh), the
//! replay rebuilds the done-set from the log frames directly — the
//! manifest's `done_shas` is a hint, not the source of truth.

use crate::dispatch::IndexerKind;
use crate::entries::{DecodedRecord, Role};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Manifest schema version. Bumped only on incompatible layout
/// changes; a mismatch refuses to resume (the user must delete the
/// checkpoint dir).
pub const MANIFEST_VERSION: u32 = 1;

/// Sub-directory under `<out>/kythe-logs/` that holds the records
/// log + manifest. Kept after a successful sidecar flush so the user
/// has forensic access (and can `rm -rf` it when they're done).
pub const CHECKPOINT_SUBDIR: &str = "kythe-logs/checkpoint";

/// Default value for `SCRY_KZIP_CHECKPOINT_EVERY` — how often the
/// manifest is rewritten in terms of CU completions. The log itself
/// is appended every CU; this knob only controls the manifest
/// refresh cadence (the manifest is recoverable from the log).
pub const DEFAULT_MANIFEST_EVERY: usize = 100;

/// Which per-language record-log file a record stream belongs to.
/// Mirrors `PackedEmitter`'s `cxx` vs `scip` bucket split — the same
/// routing the sidecar writers use, so the log files map cleanly to
/// the eventual sidecars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogKind {
    Cxx,
    Scip,
}

impl LogKind {
    /// Filename under `<checkpoint-dir>/` for this kind.
    pub fn file_name(self) -> &'static str {
        match self {
            LogKind::Cxx => "cxx.records.log",
            LogKind::Scip => "scip.records.log",
        }
    }

    /// Route an `IndexerKind` to a `LogKind`. Mirrors
    /// `PackedEmitter::record_decoded` — `Cxx` is its own sidecar,
    /// everything-runnable-else goes to SCIP, `Skip(_)` never reaches
    /// the log path (the caller short-circuits).
    pub fn for_indexer(kind: IndexerKind) -> Option<Self> {
        match kind {
            IndexerKind::Cxx => Some(LogKind::Cxx),
            IndexerKind::Skip(_) => None,
            _ => Some(LogKind::Scip),
        }
    }
}

/// Serialisable record payload — what we bincode into one frame's
/// body. `IndexerKind` doesn't go through serde directly (it carries
/// a `'static str` for `Skip` variants which serde can't round-trip),
/// so we shrink to the four fields the bucket merge needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogRecord {
    pub file_path: String,
    pub byte_offset: u32,
    pub symbol: String,
    pub role: u8,
}

impl LogRecord {
    /// Build from a freshly-decoded `DecodedRecord`. The role byte
    /// matches `PackedEmitter::record_decoded`'s mapping so replay is
    /// a no-op identity on the role channel.
    pub fn from_decoded(rec: &DecodedRecord) -> Self {
        Self {
            file_path: rec.file_path.clone(),
            byte_offset: rec.start,
            symbol: rec.target_symbol.clone(),
            role: match rec.role {
                Role::Decl => 0,
                Role::Ref => 1,
                Role::Call => 2,
            },
        }
    }
}

/// Fingerprint inputs we lock the checkpoint to. Two runs with a
/// different fingerprint refuse to resume into each other.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KzipFingerprint {
    pub path: String,
    pub size_bytes: u64,
    /// `mtime` in nanoseconds since the Unix epoch. Whole-nanosecond
    /// precision matches `std::fs::Metadata`'s `modified` resolution
    /// on every filesystem we ingest from.
    pub mtime_nanos: i128,
}

impl KzipFingerprint {
    /// Probe `kzip` for size + mtime. Returns an error if the file
    /// is missing or unreadable.
    pub fn probe(kzip: &Path) -> Result<Self> {
        let md = std::fs::metadata(kzip)
            .with_context(|| format!("stat {}", kzip.display()))?;
        let mtime = md
            .modified()
            .with_context(|| format!("mtime {}", kzip.display()))?;
        let dur = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i128)
            .unwrap_or_else(|e| -(e.duration().as_nanos() as i128));
        Ok(Self {
            path: kzip.display().to_string(),
            size_bytes: md.len(),
            mtime_nanos: dur,
        })
    }
}

/// Subset of process env that participates in the fingerprint. Any
/// var that changes which CUs are processed (`SCRY_KZIP_LANGS`,
/// `SCRY_KZIP_PATH_PREFIX`, `SCRY_KZIP_PATH_EXCLUDE`,
/// `SCRY_KZIP_STRIP_WRAPPERS`, `SCRY_KZIP_MAX_UNITS`) MUST be here —
/// different values mean the checkpoint covers a different slice of
/// the kzip and resuming would silently produce a mixed-scope sidecar.
pub const FINGERPRINTED_ENV_KEYS: &[&str] = &[
    "SCRY_KZIP_LANGS",
    "SCRY_KZIP_PATH_PREFIX",
    "SCRY_KZIP_PATH_EXCLUDE",
    "SCRY_KZIP_STRIP_WRAPPERS",
    "SCRY_KZIP_MAX_UNITS",
];

/// Snapshot the fingerprinted env into a sorted, stable map. Missing
/// vars land as the empty string so the fingerprint distinguishes
/// "unset" from "set to empty" only on caller-visible diffs (both
/// route through `parse_..._env` to `None`, so equal-to-the-emitter
/// either way).
pub fn snapshot_env() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = FINGERPRINTED_ENV_KEYS
        .iter()
        .map(|k| {
            let v = std::env::var(k).unwrap_or_default();
            ((*k).to_string(), v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// On-disk manifest. JSON for human inspection; the hot path (records
/// log) stays bincode-binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub version: u32,
    pub kzip: KzipFingerprint,
    /// Sorted `[(key, value)]` to keep JSON shape diff-friendly.
    pub env: Vec<(String, String)>,
    pub kythe_root: String,
    /// Source root prepended to Kythe corpus-relative paths before
    /// they hit the on-disk sidecar. Empty string when no root was
    /// passed (legacy manifests / tests). Part of the fingerprint so
    /// you can't accidentally resume a checkpoint built with one
    /// source root into a process configured for another — the
    /// resulting sidecar would carry mixed paths.
    #[serde(default)]
    pub source_root: String,
    /// All CU SHAs whose records have been appended to the log AND
    /// reflected in the manifest. The log itself is the source of
    /// truth on resume (this list may be stale by up to
    /// `SCRY_KZIP_CHECKPOINT_EVERY` CUs).
    pub done_shas: Vec<String>,
}

impl CheckpointManifest {
    /// Build a fresh manifest with an empty done-set.
    pub fn fresh(
        kzip: KzipFingerprint,
        env: Vec<(String, String)>,
        kythe_root: &Path,
        source_root: Option<&Path>,
    ) -> Self {
        Self {
            version: MANIFEST_VERSION,
            kzip,
            env,
            kythe_root: kythe_root.display().to_string(),
            source_root: source_root
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            done_shas: Vec::new(),
        }
    }

    /// True if `other` covers the same kzip + env + kythe-root +
    /// source-root.
    pub fn fingerprint_matches(&self, other: &Self) -> bool {
        self.version == other.version
            && self.kzip == other.kzip
            && self.env == other.env
            && self.kythe_root == other.kythe_root
            && self.source_root == other.source_root
    }

    /// Human-readable summary of which fields disagree. Used in the
    /// `resume + mismatch` error.
    pub fn fingerprint_diff(&self, other: &Self) -> String {
        let mut diffs = Vec::new();
        if self.version != other.version {
            diffs.push(format!(
                "manifest version (checkpoint={}, current={})",
                self.version, other.version,
            ));
        }
        if self.kzip != other.kzip {
            diffs.push(format!(
                "kzip ({}@{}B/{}ns vs {}@{}B/{}ns)",
                self.kzip.path, self.kzip.size_bytes, self.kzip.mtime_nanos,
                other.kzip.path, other.kzip.size_bytes, other.kzip.mtime_nanos,
            ));
        }
        if self.env != other.env {
            diffs.push(format!(
                "env (checkpoint={:?}, current={:?})", self.env, other.env,
            ));
        }
        if self.kythe_root != other.kythe_root {
            diffs.push(format!(
                "kythe_root (checkpoint={}, current={})",
                self.kythe_root, other.kythe_root,
            ));
        }
        if self.source_root != other.source_root {
            diffs.push(format!(
                "source_root (checkpoint={:?}, current={:?})",
                self.source_root, other.source_root,
            ));
        }
        diffs.join("; ")
    }

    /// Read from `path`. Returns an error if missing, malformed, or
    /// carries an unknown schema version.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read manifest {}", path.display()))?;
        let m: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse manifest {}", path.display()))?;
        if m.version != MANIFEST_VERSION {
            return Err(anyhow!(
                "manifest at {} has version {}, expected {}",
                path.display(), m.version, MANIFEST_VERSION,
            ));
        }
        Ok(m)
    }

    /// Atomic write: `<path>.tmp` + rename. Caller is responsible
    /// for ensuring `path.parent()` exists.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .context("serialize manifest")?;
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("write tmp manifest {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!(
                "rename {} -> {}", tmp.display(), path.display(),
            ))?;
        Ok(())
    }
}

/// One per-language append-only record log. Append is buffered through
/// an internal mutex so concurrent CU completions serialize on the
/// file write — sub-millisecond per CU, negligible vs the indexer's
/// per-CU runtime.
pub struct RecordLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl RecordLog {
    /// Open (or create) the log file in append mode. Existing
    /// contents are preserved; `replay` reads them back.
    pub fn open(dir: &Path, kind: LogKind) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("mkdir {}", dir.display()))?;
        let path = dir.join(kind.file_name());
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .with_context(|| format!("open log {}", path.display()))?;
        Ok(Self { path, file: Mutex::new(file) })
    }

    /// Append one CU's records as a single framed payload. The frame
    /// is written with a single `write_all` so a SIGKILL mid-write
    /// produces a truncated tail (caught by `replay`) rather than a
    /// scrambled-middle log.
    pub fn append_cu(&self, cu_sha: &str, records: &[LogRecord]) -> Result<()> {
        let payload = bincode::serialize(records)
            .context("bincode serialize log records")?;
        let mut frame = Vec::with_capacity(16 + payload.len());
        frame.extend_from_slice(&hash_sha(cu_sha).to_le_bytes());
        frame.extend_from_slice(&(records.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        let mut f = self.file.lock().unwrap();
        f.write_all(&frame)
            .with_context(|| format!("append frame to {}", self.path.display()))?;
        // We deliberately do not fsync per CU. Crash-safety story:
        // - Bincode + length prefix detects partial tails on replay.
        // - The host fs (ext4 / xfs / tmpfs) flushes its journal on
        //   close; cargo / OS kill paths still flush page cache.
        // - fsync-per-CU would add ~5-10ms to every CU completion
        //   (~hundreds of seconds total on a 100K-CU run) for
        //   protection against a class of failure (kernel panic with
        //   pages still in dirty cache) that we don't promise.
        Ok(())
    }

    /// Read every complete frame back. Returns `(cu_shas_seen,
    /// total_records)` along with the records replayed via `sink`.
    /// On a truncated tail, logs a single warning and stops reading.
    pub fn replay<F>(&self, mut sink: F) -> Result<ReplayReport>
    where
        F: FnMut(&[LogRecord]),
    {
        let path = self.path.clone();
        let mut f = self.file.lock().unwrap();
        // Replay needs the full contents from byte 0; we leave the
        // file cursor at end-of-file afterwards so subsequent appends
        // land cleanly (the OpenOptions `append` flag makes every
        // write go to the current end regardless of cursor position
        // on Linux, but we restore the seek for cross-platform clarity).
        f.seek(SeekFrom::Start(0))
            .with_context(|| format!("rewind {} for replay", path.display()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .with_context(|| format!("read log body {}", path.display()))?;
        f.seek(SeekFrom::End(0)).ok();
        let mut cursor = 0usize;
        let mut report = ReplayReport::default();
        while cursor < buf.len() {
            if cursor + 16 > buf.len() {
                eprintln!(
                    "[scry-kzip] checkpoint: truncated tail at {} (have {} bytes, need 16-byte header) — discarding",
                    path.display(), buf.len() - cursor,
                );
                break;
            }
            let mut h8 = [0u8; 8];
            h8.copy_from_slice(&buf[cursor..cursor + 8]);
            let cu_hash = u64::from_le_bytes(h8);
            let mut c4 = [0u8; 4];
            c4.copy_from_slice(&buf[cursor + 8..cursor + 12]);
            let n_records = u32::from_le_bytes(c4) as usize;
            c4.copy_from_slice(&buf[cursor + 12..cursor + 16]);
            let byte_len = u32::from_le_bytes(c4) as usize;
            let body_start = cursor + 16;
            let body_end = body_start + byte_len;
            if body_end > buf.len() {
                eprintln!(
                    "[scry-kzip] checkpoint: truncated tail at {} (frame wants {} body bytes, have {}) — discarding",
                    path.display(), byte_len, buf.len() - body_start,
                );
                break;
            }
            let records: Vec<LogRecord> =
                match bincode::deserialize(&buf[body_start..body_end]) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!(
                            "[scry-kzip] checkpoint: bincode decode failed at byte {} of {} ({e}) — discarding tail",
                            cursor, path.display(),
                        );
                        break;
                    }
                };
            if records.len() != n_records {
                eprintln!(
                    "[scry-kzip] checkpoint: frame at byte {} of {} claimed {} records but decoded {} — discarding tail",
                    cursor, path.display(), n_records, records.len(),
                );
                break;
            }
            report.cu_hashes.push(cu_hash);
            report.total_records += records.len();
            sink(&records);
            cursor = body_end;
        }
        if cursor < buf.len() {
            report.truncated_bytes = buf.len() - cursor;
        }
        Ok(report)
    }
}

/// Per-language replay result, returned by `RecordLog::replay`.
#[derive(Debug, Default)]
pub struct ReplayReport {
    /// One hashed CU SHA per replayed frame (FxHash-shaped — see
    /// [`hash_sha`]). Useful for cross-referencing manifest's
    /// `done_shas` (the manifest stores SHA strings; both shapes
    /// round-trip through `hash_sha`).
    pub cu_hashes: Vec<u64>,
    pub total_records: usize,
    /// Bytes at the tail that didn't form a complete frame.
    /// Zero on a clean log.
    pub truncated_bytes: usize,
}

/// Hash a CU SHA string to a 64-bit identifier. We use
/// `std::collections::hash_map::DefaultHasher` for stdlib stability;
/// the hash is only used as a frame-level identifier (the manifest's
/// `done_shas` keeps the original strings for forensic readback).
pub fn hash_sha(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn tmp_dir(label: &str) -> PathBuf {
        let p = scry_bridge::scry_tmp_dir().join(format!(
            "scry-kzip-checkpoint-{}-{}-{:?}",
            label,
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rec(path: &str, off: u32, sym: &str, role: u8) -> LogRecord {
        LogRecord {
            file_path: path.to_string(),
            byte_offset: off,
            symbol: sym.to_string(),
            role,
        }
    }

    /// Round-trip: write 3 frames, replay reads exactly 3.
    #[test]
    fn append_then_replay_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let log = RecordLog::open(&dir, LogKind::Cxx).unwrap();
        log.append_cu("sha1", &[rec("/A.cc", 10, "kythe:foo", 0)]).unwrap();
        log.append_cu("sha2", &[
            rec("/B.cc", 20, "kythe:bar", 0),
            rec("/B.cc", 30, "kythe:baz", 1),
        ]).unwrap();
        log.append_cu("sha3", &[rec("/C.cc", 40, "kythe:qux", 2)]).unwrap();
        drop(log);

        let log2 = RecordLog::open(&dir, LogKind::Cxx).unwrap();
        let mut all: Vec<LogRecord> = Vec::new();
        let rep = log2.replay(|recs| all.extend_from_slice(recs)).unwrap();
        assert_eq!(rep.cu_hashes.len(), 3);
        assert_eq!(rep.total_records, 4);
        assert_eq!(rep.truncated_bytes, 0);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].symbol, "kythe:foo");
        assert_eq!(all[3].symbol, "kythe:qux");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Truncate the log mid-frame-4. Replay reads the first 3 frames,
    /// reports the truncated tail size, doesn't error.
    #[test]
    fn truncated_tail_recovers_complete_frames() {
        let dir = tmp_dir("trunc");
        let log = RecordLog::open(&dir, LogKind::Scip).unwrap();
        log.append_cu("sha1", &[rec("/a.go", 1, "s1", 0)]).unwrap();
        log.append_cu("sha2", &[rec("/b.go", 2, "s2", 1)]).unwrap();
        log.append_cu("sha3", &[rec("/c.go", 3, "s3", 2)]).unwrap();
        log.append_cu("sha4", &[
            rec("/d.go", 4, "s4a", 0),
            rec("/d.go", 5, "s4b", 1),
        ]).unwrap();
        drop(log);

        // Now truncate ~12 bytes off the end (kills frame 4's tail).
        let path = dir.join(LogKind::Scip.file_name());
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > 12);
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 12).unwrap();
        drop(f);

        let log2 = RecordLog::open(&dir, LogKind::Scip).unwrap();
        let mut all: Vec<LogRecord> = Vec::new();
        let rep = log2.replay(|recs| all.extend_from_slice(recs)).unwrap();
        // Three good frames, frame 4 dropped.
        assert_eq!(rep.cu_hashes.len(), 3);
        assert_eq!(rep.total_records, 3);
        assert!(rep.truncated_bytes > 0);
        assert_eq!(all.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Append after replay: the file cursor is restored to EOF so a
    /// resumed run can keep adding frames without scrambling the log.
    #[test]
    fn append_after_replay_preserves_old_frames() {
        let dir = tmp_dir("append-after-replay");
        let log = RecordLog::open(&dir, LogKind::Cxx).unwrap();
        log.append_cu("sha1", &[rec("/A.cc", 10, "s1", 0)]).unwrap();
        let mut all: Vec<LogRecord> = Vec::new();
        log.replay(|recs| all.extend_from_slice(recs)).unwrap();
        assert_eq!(all.len(), 1);
        log.append_cu("sha2", &[rec("/B.cc", 20, "s2", 1)]).unwrap();
        drop(log);

        let log2 = RecordLog::open(&dir, LogKind::Cxx).unwrap();
        let mut all2: Vec<LogRecord> = Vec::new();
        let rep = log2.replay(|recs| all2.extend_from_slice(recs)).unwrap();
        assert_eq!(rep.cu_hashes.len(), 2);
        assert_eq!(all2.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Manifest save + load round-trip with a stable on-disk shape.
    #[test]
    fn manifest_roundtrip() {
        let dir = tmp_dir("manifest");
        let fp = KzipFingerprint {
            path: "/x/y.kzip".to_string(),
            size_bytes: 12345,
            mtime_nanos: 1_700_000_000_000_000_000,
        };
        let env = vec![
            ("SCRY_KZIP_LANGS".to_string(), "cxx".to_string()),
        ];
        let m = CheckpointManifest::fresh(
            fp.clone(), env.clone(), Path::new("/opt/kythe"),
            Some(Path::new("/home/zim/dev/aosp")),
        );
        let path = dir.join("manifest.json");
        m.save_atomic(&path).unwrap();
        let m2 = CheckpointManifest::load(&path).unwrap();
        assert_eq!(m.kzip, m2.kzip);
        assert_eq!(m.env, m2.env);
        assert_eq!(m.kythe_root, m2.kythe_root);
        assert_eq!(m.source_root, m2.source_root);
        assert_eq!(m.source_root, "/home/zim/dev/aosp");
        assert_eq!(m.done_shas, m2.done_shas);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fingerprint mismatch surfaces a human-readable diff. The
    /// source_root field is part of the fingerprint too — flipping
    /// it must be detected so a checkpoint built with one root
    /// can't be silently mixed with a different root.
    #[test]
    fn fingerprint_diff_lists_changed_fields() {
        let env = vec![("SCRY_KZIP_LANGS".to_string(), "cxx".to_string())];
        let m_a = CheckpointManifest::fresh(
            KzipFingerprint { path: "/a".to_string(), size_bytes: 1, mtime_nanos: 1 },
            env.clone(), Path::new("/k1"), None,
        );
        let m_b = CheckpointManifest::fresh(
            KzipFingerprint { path: "/a".to_string(), size_bytes: 2, mtime_nanos: 1 },
            env.clone(), Path::new("/k1"), None,
        );
        assert!(!m_a.fingerprint_matches(&m_b));
        let diff = m_a.fingerprint_diff(&m_b);
        assert!(diff.contains("kzip"), "diff should mention kzip: {diff}");

        // Source-root change alone must also be detected.
        let m_c = CheckpointManifest::fresh(
            KzipFingerprint { path: "/a".to_string(), size_bytes: 1, mtime_nanos: 1 },
            env.clone(), Path::new("/k1"), Some(Path::new("/root/one")),
        );
        let m_d = CheckpointManifest::fresh(
            KzipFingerprint { path: "/a".to_string(), size_bytes: 1, mtime_nanos: 1 },
            env, Path::new("/k1"), Some(Path::new("/root/two")),
        );
        assert!(!m_c.fingerprint_matches(&m_d));
        let diff_root = m_c.fingerprint_diff(&m_d);
        assert!(diff_root.contains("source_root"),
            "diff should mention source_root: {diff_root}");
    }

    /// `LogKind::for_indexer` mirrors the bucket routing in emit.rs.
    #[test]
    fn log_kind_routing_matches_emitter_buckets() {
        assert_eq!(LogKind::for_indexer(IndexerKind::Cxx), Some(LogKind::Cxx));
        assert_eq!(LogKind::for_indexer(IndexerKind::Go), Some(LogKind::Scip));
        assert_eq!(LogKind::for_indexer(IndexerKind::JavaSource), Some(LogKind::Scip));
        assert_eq!(LogKind::for_indexer(IndexerKind::JvmBytecode), Some(LogKind::Scip));
        assert_eq!(LogKind::for_indexer(IndexerKind::Proto), Some(LogKind::Scip));
        assert_eq!(LogKind::for_indexer(IndexerKind::TextProto), Some(LogKind::Scip));
        assert_eq!(LogKind::for_indexer(IndexerKind::Skip("x")), None);
    }

    /// Hashes are deterministic across calls (same string → same hash
    /// in a single process). Cross-process stability isn't required:
    /// the hash is only used inside one ingest run.
    #[test]
    fn hash_sha_is_deterministic_within_process() {
        assert_eq!(hash_sha("abc"), hash_sha("abc"));
        assert_ne!(hash_sha("abc"), hash_sha("abd"));
    }

    /// `HashSet`-shape access is the expected lookup pattern.
    #[test]
    fn done_shas_lookup_is_set_friendly() {
        let mut s: HashSet<String> = HashSet::new();
        s.insert("sha1".to_string());
        s.insert("sha2".to_string());
        assert!(s.contains("sha1"));
        assert!(!s.contains("sha3"));
    }
}
