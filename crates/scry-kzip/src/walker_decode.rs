//! Per-entry "read bytes off the zip → `KzipUnit`" mechanics.
//!
//! Split out of `walker.rs` so the orchestrator (serial iterator +
//! parallel fan-out) stays focused on coordination; here we own the
//! raw-zip-entry-to-decoded-CU pipeline plus the filter pre-peek
//! short-circuit.

use crate::proto::analysis::{CompilationUnit, IndexedCompilation};
use crate::walker::{KzipArchive, KzipUnit, LangFilter, UnitEncoding, UnitEntry};
use crate::walker_peek::{peek_matches_filter, peek_unit_language};
use anyhow::{anyhow, Context, Result};
use protobuf::Message;
use std::io::Read as _;
use std::path::Path;

/// Bytes consumed for the cheap language pre-peek (see
/// [`read_one_entry`]). Large enough to cover the
/// `unit.v_name.language` field in every AOSP-extractor unit we've
/// seen (the field lives in the first ~256 bytes of the JSON, well
/// inside this cap); small enough that we don't pay for the multi-MB
/// decompression of CUs the filter rejects.
const PEEK_PREFIX_BYTES: usize = 4096;

/// Pull one entry's bytes out of `zip` and turn them into a
/// `KzipUnit`. Honours the optional language pre-peek filter and
/// short-circuits the full decode whenever it isn't needed.
///
/// Decode-skip rules:
///
/// * `filter` rejects the peeked language → return `None` without
///   reading past the peek prefix.
/// * Peeked language is one of the "no-class-introspection" set
///   (every non-JVM language: cxx, go, proto, textproto, rust, …) →
///   construct the `KzipUnit` directly from the peek + entry
///   metadata, no full decompression. `has_class_input` is fixed
///   `false` because dispatch never consults it for these languages.
/// * Otherwise (kotlin / java / no-language unit) → drain the full
///   entry and run the structured decode, since we need
///   `required_input` to compute `has_class_input` or to fall back
///   to `infer_language_from_inputs`.
///
/// In an AOSP-shape kzip ~3 K of 115 K JSON units are cxx; the
/// short-circuit means phase 1 skips ~325 GB of cumulative JSON
/// parse work that would otherwise dominate the walk.
///
/// Returns:
/// * `Ok(Some(unit))` — kept.
/// * `Ok(None)` — filtered out by the language peek.
/// * `Err(_)` — the entry couldn't be read or decoded.
pub(crate) fn read_one_entry(
    zip: &mut KzipArchive,
    entry: &UnitEntry,
    kzip: &Path,
    filter: LangFilter<'_>,
) -> Result<Option<KzipUnit>> {
    // Phase 1: read peek prefix and probe language.
    let mut buf = Vec::new();
    let total = {
        let mut zf = zip
            .by_name(&entry.name)
            .with_context(|| format!("open kzip entry {}", entry.name))?;
        let total = zf.size() as usize;
        let peek_len = total.min(PEEK_PREFIX_BYTES);
        buf.resize(peek_len, 0);
        zf.read_exact(&mut buf)
            .with_context(|| format!("peek-read kzip entry {}", entry.name))?;
        if filter.is_some()
            && !peek_matches_filter(&buf, entry.encoding, false, filter.unwrap())
        {
            return Ok(None);
        }
        // Continue reading the rest of the body iff we'll need the full
        // CU to compute `has_class_input` or infer the language. For the
        // languages that don't need either, we skip the rest of the
        // decompress.
        let peeked_lang = peek_unit_language(&buf, entry.encoding).unwrap_or_default();
        if language_skips_full_decode(&peeked_lang) {
            let unit = KzipUnit {
                kzip_path: kzip.to_path_buf(),
                unit_sha: entry.sha.clone(),
                encoding: entry.encoding,
                language: peeked_lang,
                has_class_input: false,
            };
            if let Some(allowed) = filter {
                let kind = crate::dispatch::choose(&unit.language, unit.has_class_input);
                if !allowed.contains(kind.label()) { return Ok(None); }
            }
            return Ok(Some(unit));
        }
        // Need full decode — drain the rest of the body.
        buf.reserve(total.saturating_sub(peek_len));
        zf.read_to_end(&mut buf)
            .with_context(|| format!("read kzip entry {}", entry.name))?;
        total
    };
    let _ = total;
    let unit = match entry.encoding {
        UnitEncoding::Proto => decode_proto_unit(&buf, &entry.name, kzip, &entry.sha)?,
        UnitEncoding::Json  => decode_json_unit(&buf, &entry.name, kzip, &entry.sha)?,
    };
    if let Some(allowed) = filter {
        let kind = crate::dispatch::choose(&unit.language, unit.has_class_input);
        if !allowed.contains(kind.label()) {
            return Ok(None);
        }
    }
    Ok(Some(unit))
}

/// Languages where peek alone gives us everything we need: dispatch
/// doesn't consult `has_class_input` for them, and the language is
/// already known (non-empty), so `infer_language_from_inputs` is
/// not required. Listing them as a closed set keeps this routine
/// in lockstep with `dispatch::choose` — if a future language gets
/// a class-introspection fork, it must NOT appear here.
fn language_skips_full_decode(lang: &str) -> bool {
    matches!(
        lang,
        "c++" | "c" | "objc" | "go" | "protobuf" | "proto" | "textproto" | "rust",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keep the decode-skip allow-list in step with the dispatcher.
    /// Languages NOT in this list must be the ones whose dispatch
    /// kind consults `has_class_input` (kotlin) or whose v_name
    /// language string is empty (forces input-suffix inference).
    #[test]
    fn skip_set_excludes_kotlin_java_and_empty() {
        for lang in ["c++", "c", "objc", "go", "protobuf", "proto", "textproto", "rust"] {
            assert!(language_skips_full_decode(lang), "should skip full decode for {lang}");
        }
        for lang in ["kotlin", "java", "", "haskell"] {
            assert!(!language_skips_full_decode(lang), "must NOT skip full decode for {lang}");
        }
    }
}

fn decode_proto_unit(
    buf: &[u8],
    entry_name: &str,
    kzip: &Path,
    sha: &str,
) -> Result<KzipUnit> {
    let indexed = IndexedCompilation::parse_from_bytes(buf)
        .with_context(|| format!("decode IndexedCompilation at {entry_name}"))?;
    let cu = indexed
        .unit
        .into_option()
        .ok_or_else(|| anyhow!("{entry_name}: IndexedCompilation.unit absent"))?;
    Ok(kzip_unit_from_cu(kzip, sha, UnitEncoding::Proto, &cu))
}

fn decode_json_unit(
    buf: &[u8],
    entry_name: &str,
    kzip: &Path,
    sha: &str,
) -> Result<KzipUnit> {
    let text = std::str::from_utf8(buf)
        .with_context(|| format!("{entry_name}: JSON unit is not valid UTF-8"))?;
    let indexed: IndexedCompilation = protobuf_json_mapping::parse_from_str_with_options(
        text,
        &lenient_json_opts(),
    )
    .with_context(|| format!("parse JSON IndexedCompilation at {entry_name}"))?;
    let cu = indexed
        .unit
        .into_option()
        .ok_or_else(|| anyhow!("{entry_name}: IndexedCompilation.unit absent"))?;
    Ok(kzip_unit_from_cu(kzip, sha, UnitEncoding::Json, &cu))
}

/// Project an already-decoded `CompilationUnit` plus its source kzip
/// coordinates into a `KzipUnit`. Shared between the proto and JSON
/// reader paths so they yield identical shape regardless of encoding.
fn kzip_unit_from_cu(
    kzip: &Path,
    sha: &str,
    encoding: UnitEncoding,
    cu: &CompilationUnit,
) -> KzipUnit {
    let language = cu
        .v_name
        .as_ref()
        .map(|v| v.language.clone())
        .unwrap_or_default();
    let language = if language.is_empty() {
        infer_language_from_inputs(cu).unwrap_or_default()
    } else {
        language
    };
    // We need real bytecode for jvm_indexer to do anything useful;
    // a .jar that ships .java source files (kotlinc's srcjar
    // output, the AOSP norm) trips the indexer's NPE on missing
    // JarDetails. Only count actual .class inputs as JVM-bytecode
    // signal — .jar alone isn't enough.
    let has_class_input = cu.required_input.iter().any(|fi| {
        fi.info
            .as_ref()
            .map(|i| i.path.as_str().ends_with(".class"))
            .unwrap_or(false)
    });
    KzipUnit {
        kzip_path: kzip.to_path_buf(),
        unit_sha: sha.to_string(),
        encoding,
        language,
        has_class_input,
    }
}

/// Shared JSON parse options for the JSON-units path.
///
/// `ignore_unknown_fields = true` is mandatory: AOSP's cxx_extractor
/// + java_extractor write a `details: [google.protobuf.Any]` field
/// (and a few others) that we deliberately strip from our embedded
/// `analysis.proto` subset to keep the codegen surface minimal.
/// The strict proto3-JSON default would reject every real-world AOSP
/// JSON unit on `Unknown field name: 'details'`.
pub(crate) fn lenient_json_opts() -> protobuf_json_mapping::ParseOptions {
    protobuf_json_mapping::ParseOptions {
        ignore_unknown_fields: true,
        ..Default::default()
    }
}

/// Best-effort language inference for units whose `v_name.language`
/// is empty. We sample the first source-looking input — extractors
/// that omit the v_name language still record real `.cc` / `.java` /
/// `.go` / `.kt` paths in `required_input[*].info.path`.
fn infer_language_from_inputs(cu: &CompilationUnit) -> Option<String> {
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
