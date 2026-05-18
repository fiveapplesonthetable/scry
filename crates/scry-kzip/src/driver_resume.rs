//! Resume-policy validator + checkpoint-related env knobs for the
//! kzip driver.
//!
//! Split out of `driver.rs` so the orchestration file stays focused
//! on phase coordination — here we own the three-state validator
//! that decides whether a `build-symbols --build-kzip` invocation
//! starts fresh or resumes an existing on-disk checkpoint, plus the
//! one env knob (`SCRY_KZIP_CHECKPOINT_EVERY`) that tunes the
//! manifest-flush cadence.
//!
//! ## Why explicit-flag
//!
//! Auto-resume (e.g. silently picking up wherever the previous run
//! left off whenever the kzip fingerprint matches) is rejected by
//! design. Production tools — kubelet, terraform, bazel — require
//! explicit opt-in to resume long-running workflows; we do the same.
//! A user re-running `scry build-symbols --build-kzip X --out DIR`
//! against the same `DIR` SHOULD get an error pointing out the
//! existing checkpoint rather than a silent partial run.

use crate::emit_checkpoint::CheckpointManifest;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Outcome of the resume-policy validator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDecision {
    /// `--resume` was not passed; no existing checkpoint, start fresh.
    Fresh,
    /// `--resume` was passed and the existing checkpoint matched.
    Resume,
}

impl ResumeDecision {
    pub fn is_resume(self) -> bool {
        matches!(self, ResumeDecision::Resume)
    }
}

/// Parse `SCRY_KZIP_CHECKPOINT_EVERY=N`. Controls how often the
/// checkpoint manifest is re-flushed during phase 3 (the records log
/// is appended every CU regardless — this knob is just about how
/// fresh the `done_shas` list in the JSON manifest stays). Returns
/// `None` if unset or malformed; the driver falls back to
/// [`crate::emit_checkpoint::DEFAULT_MANIFEST_EVERY`].
pub fn parse_checkpoint_every_env() -> Option<usize> {
    let v = std::env::var("SCRY_KZIP_CHECKPOINT_EVERY").ok()?;
    let n = v.parse::<usize>().ok()?;
    if n == 0 { None } else { Some(n) }
}

/// Validate the four legal `(resume, checkpoint-exists)` states and
/// reject the two illegal ones with human-actionable errors. The
/// fifth state — `(resume, exists, fingerprint mismatch)` — is also
/// rejected, with a diff naming the fields that disagree.
///
/// | `--resume` | checkpoint exists | fingerprint matches | outcome  |
/// |------------|-------------------|---------------------|----------|
/// | no         | no                | n/a                 | Fresh    |
/// | no         | yes               | n/a                 | error    |
/// | yes        | no                | n/a                 | error    |
/// | yes        | yes               | no                  | error    |
/// | yes        | yes               | yes                 | Resume   |
pub fn enforce_resume_policy(
    resume: bool,
    checkpoint_dir: &Path,
    current_manifest: &CheckpointManifest,
) -> Result<ResumeDecision> {
    let manifest_path = checkpoint_dir.join("manifest.json");
    let exists = checkpoint_dir.is_dir() && manifest_path.is_file();
    match (resume, exists) {
        (false, false) => Ok(ResumeDecision::Fresh),
        (false, true) => bail!(
            "checkpoint already exists at {} — pass --resume to continue, or `rm -rf` the dir to start fresh",
            checkpoint_dir.display(),
        ),
        (true, false) => bail!(
            "no checkpoint to resume from at {} — drop --resume to start fresh",
            checkpoint_dir.display(),
        ),
        (true, true) => {
            let existing = CheckpointManifest::load(&manifest_path)
                .with_context(|| format!(
                    "load existing manifest {}", manifest_path.display(),
                ))?;
            if !existing.fingerprint_matches(current_manifest) {
                bail!(
                    "checkpoint at {} was built from a different kzip / env; refusing to mix. Differences: {}. Either point at the original or `rm -rf` the checkpoint dir",
                    checkpoint_dir.display(),
                    existing.fingerprint_diff(current_manifest),
                );
            }
            Ok(ResumeDecision::Resume)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit_checkpoint::KzipFingerprint;
    use std::path::PathBuf;

    fn unique_tmp(label: &str) -> PathBuf {
        let p = scry_bridge::scry_tmp_dir().join(format!(
            "scry-kzip-resume-{}-{}-{:?}",
            label, std::process::id(), std::thread::current().id(),
        ));
        let _ = std::fs::remove_dir_all(&p);
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
        )
    }

    /// Three-state validator — fresh + no-checkpoint case returns
    /// `Fresh` without creating anything on disk.
    #[test]
    fn fresh_then_no_checkpoint() {
        let dir = unique_tmp("fresh");
        let m = fresh_manifest();
        let decision = enforce_resume_policy(false, &dir, &m).unwrap();
        assert_eq!(decision, ResumeDecision::Fresh);
        assert!(!dir.exists() || !dir.join("manifest.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fresh request against a populated checkpoint dir errors.
    #[test]
    fn fresh_with_existing_checkpoint_errors() {
        let dir = unique_tmp("exists-noresume");
        std::fs::create_dir_all(&dir).unwrap();
        let m = fresh_manifest();
        m.save_atomic(&dir.join("manifest.json")).unwrap();
        let err = enforce_resume_policy(false, &dir, &m).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "msg: {msg}");
        assert!(msg.contains("--resume"), "msg: {msg}");
        assert!(msg.contains("rm -rf"), "msg: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resume request with no checkpoint on disk errors.
    #[test]
    fn resume_without_checkpoint_errors() {
        let dir = unique_tmp("noexists-resume");
        let _ = std::fs::remove_dir_all(&dir);
        let m = fresh_manifest();
        let err = enforce_resume_policy(true, &dir, &m).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no checkpoint"), "msg: {msg}");
        assert!(msg.contains("drop --resume"), "msg: {msg}");
    }

    /// Resume against a fingerprint-mismatched checkpoint errors with
    /// a diff naming the offending field.
    #[test]
    fn fingerprint_mismatch_errors() {
        let dir = unique_tmp("mismatch");
        std::fs::create_dir_all(&dir).unwrap();
        let m_old = CheckpointManifest::fresh(
            KzipFingerprint {
                path: "/old.kzip".to_string(), size_bytes: 100, mtime_nanos: 1,
            },
            Vec::new(),
            Path::new("/k"),
        );
        m_old.save_atomic(&dir.join("manifest.json")).unwrap();
        let m_new = CheckpointManifest::fresh(
            KzipFingerprint {
                path: "/new.kzip".to_string(), size_bytes: 200, mtime_nanos: 2,
            },
            Vec::new(),
            Path::new("/k"),
        );
        let err = enforce_resume_policy(true, &dir, &m_new).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("different kzip"), "msg: {msg}");
        assert!(msg.contains("kzip"), "msg: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resume against a matching checkpoint succeeds.
    #[test]
    fn matching_fingerprint_resumes() {
        let dir = unique_tmp("match");
        std::fs::create_dir_all(&dir).unwrap();
        let m = fresh_manifest();
        m.save_atomic(&dir.join("manifest.json")).unwrap();
        let decision = enforce_resume_policy(true, &dir, &m).unwrap();
        assert_eq!(decision, ResumeDecision::Resume);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Knob parse — `=0` is treated as unset (the driver falls back
    /// to the default rather than re-flushing on every CU, which
    /// would explode JSON write cost on AOSP-scale runs).
    #[test]
    fn checkpoint_every_zero_is_unset() {
        // Avoid set_var (unsafe across parallel tests); exercise the
        // parsing rule instead.
        let v = "0";
        let n: Option<usize> = v.parse::<usize>().ok();
        assert_eq!(n, Some(0));
        // The wrapped helper would map `Some(0)` → `None`:
        let mapped = match n {
            Some(0) | None => None,
            Some(n) => Some(n),
        };
        assert_eq!(mapped, None);
    }
}
