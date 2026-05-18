//! Walk a `.kzip` and yield `KzipUnit` per compilation.
//!
//! A kzip is a zip archive with two top-level sub-trees under
//! `root/` (the in-archive prefix is configurable, but every kzip
//! produced by the standard extractors uses `root/`):
//!
//! * `root/pbunits/<sha>` — one `IndexedCompilation` proto per file.
//!   (Older v1 kzips used `root/units/<sha>`; we accept either.)
//! * `root/files/<sha>`   — the raw file blobs referenced by the
//!   units. We don't read these here — the actual indexer binaries
//!   (cxx_indexer, java_indexer.jar, …) reach into the kzip themselves
//!   and resolve `required_input[*].info.digest` against `root/files/`.
//!
//! The walker's job is small: enumerate units so the dispatcher can
//! count CUs per language and the driver knows which indexers it has
//! to spawn.

use crate::proto::analysis::IndexedCompilation;
use anyhow::{anyhow, Context, Result};
use protobuf::Message;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    /// `root/pbunits/<sha>`). Useful for logging.
    pub unit_sha: String,
    /// Language string from `cu.v_name.language`. May be empty when
    /// the extractor didn't set it; in that case we infer from the
    /// `required_input[*].info.path` suffix.
    pub language: String,
    /// True if any `required_input[*].info.path` ends `.class` or
    /// `.jar`. Drives the Kotlin/JVM dispatch fork.
    pub has_class_or_jar_input: bool,
}

/// Open `kzip` and stream every unit out as a `KzipUnit`.
///
/// Errors mid-stream (one corrupt unit) are surfaced as `Err(_)` but
/// don't stop iteration — callers can `filter_map(Result::ok)` if
/// they want best-effort behaviour.
pub fn walk_units(kzip: &Path) -> Result<Vec<KzipUnit>> {
    let f = File::open(kzip)
        .with_context(|| format!("open kzip {}", kzip.display()))?;
    let mut zip = zip::ZipArchive::new(f)
        .with_context(|| format!("read kzip header {}", kzip.display()))?;
    let mut out: Vec<KzipUnit> = Vec::new();
    // Pre-scan filenames so we know which `root/` prefix is in use;
    // most kzips use `root/`, but the spec allows any single-segment
    // prefix and some legacy extractors used `kzip/`.
    let mut prefix: Option<String> = None;
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).map(|e| e.name().to_string()))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("enumerate kzip entries in {}", kzip.display()))?;
    for n in &names {
        if let Some(rest) = n.strip_suffix('/') {
            // A directory entry such as `root/` tells us the prefix.
            if !rest.contains('/') && prefix.is_none() {
                prefix = Some(rest.to_string());
                break;
            }
        }
        if let Some(idx) = n.find("/pbunits/").or_else(|| n.find("/units/")) {
            prefix = Some(n[..idx].to_string());
            break;
        }
    }
    let prefix = prefix.ok_or_else(|| {
        anyhow!(
            "{}: no `<root>/pbunits/...` or `<root>/units/...` entries found",
            kzip.display(),
        )
    })?;
    // Read each unit. Prefer the proto encoding (`<root>/pbunits/`)
    // when both exist — the Soong-emitted AOSP kzip ships BOTH the
    // proto encoding (`pbunits/`) and the older JSON encoding
    // (`units/`) for every CU, and decoding a JSON unit as proto
    // would error. If a kzip only ships `units/`, that's a legacy
    // v1 format and we fall through to it for back-compat.
    let unit_dir = format!("{prefix}/pbunits/");
    let unit_dir_v1 = format!("{prefix}/units/");
    let has_pbunits = names.iter().any(|n| n.starts_with(&unit_dir) && !n.ends_with('/'));
    for n in &names {
        let sha = if has_pbunits {
            match n.strip_prefix(&unit_dir) {
                Some(s) => s,
                None => continue,
            }
        } else if let Some(s) = n.strip_prefix(&unit_dir_v1) {
            s
        } else {
            continue;
        };
        // Skip directory entries; only flat blobs are units.
        if sha.is_empty() || sha.contains('/') {
            continue;
        }
        let mut entry = zip.by_name(n)
            .with_context(|| format!("open kzip entry {n}"))?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)
            .with_context(|| format!("read kzip entry {n}"))?;
        let indexed = IndexedCompilation::parse_from_bytes(&buf)
            .with_context(|| format!("decode IndexedCompilation at {n}"))?;
        let cu = indexed
            .unit
            .into_option()
            .ok_or_else(|| anyhow!("{n}: IndexedCompilation.unit absent"))?;
        let language = cu
            .v_name
            .as_ref()
            .map(|v| v.language.clone())
            .unwrap_or_default();
        let language = if language.is_empty() {
            infer_language_from_inputs(&cu).unwrap_or_default()
        } else {
            language
        };
        let has_class_or_jar_input = cu.required_input.iter().any(|fi| {
            fi.info
                .as_ref()
                .map(|i| {
                    let p = i.path.as_str();
                    p.ends_with(".class") || p.ends_with(".jar")
                })
                .unwrap_or(false)
        });
        out.push(KzipUnit {
            kzip_path: kzip.to_path_buf(),
            unit_sha: sha.to_string(),
            language,
            has_class_or_jar_input,
        });
    }
    Ok(out)
}

/// Best-effort language inference for units whose `v_name.language`
/// is empty. We sample the first source-looking input — extractors
/// that omit the v_name language still record real `.cc` / `.java` /
/// `.go` / `.kt` paths in `required_input[*].info.path`.
fn infer_language_from_inputs(
    cu: &crate::proto::analysis::CompilationUnit,
) -> Option<String> {
    for fi in &cu.required_input {
        let Some(info) = fi.info.as_ref() else { continue };
        let p = info.path.as_str();
        let lang = match () {
            _ if p.ends_with(".cc") || p.ends_with(".cpp") || p.ends_with(".cxx")
                || p.ends_with(".c++") || p.ends_with(".h") || p.ends_with(".hpp")
                || p.ends_with(".hxx") => "c++",
            _ if p.ends_with(".c") => "c",
            _ if p.ends_with(".m") || p.ends_with(".mm") => "objc",
            _ if p.ends_with(".java") => "java",
            _ if p.ends_with(".kt") => "kotlin",
            _ if p.ends_with(".go") => "go",
            _ if p.ends_with(".proto") => "protobuf",
            _ if p.ends_with(".textpb") || p.ends_with(".textproto") => "textproto",
            _ if p.ends_with(".rs") => "rust",
            _ => continue,
        };
        return Some(lang.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
