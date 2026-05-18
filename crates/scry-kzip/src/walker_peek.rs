//! Cheap "what language is this unit?" probe that avoids paying the
//! full proto / proto3-JSON decode cost.
//!
//! Called from the walker before `read_proto_unit` / `read_json_unit`
//! when the driver has a `SCRY_KZIP_LANGS` filter set: if the unit
//! belongs to a language we're not running this pass, we short-circuit
//! and never construct the full `CompilationUnit`.
//!
//! ## Proto path
//!
//! `IndexedCompilation` (field 1 = `unit: CompilationUnit`)
//!   → `CompilationUnit` (field 1 = `v_name: VName`)
//!     → `VName` (field 5 = `language: string`)
//!
//! We walk the wire format by hand — read tag, dispatch on wire type,
//! descend through length-delimited sub-messages 1.1.5 and pull the
//! language string out. Skip every other field. Bounded cost: O(tag +
//! varint) per non-target field, O(string) for the language itself.
//!
//! ## JSON path
//!
//! We use serde with a minimal shape that captures only
//! `unit.v_name.language`. serde_json walks the tree once but only
//! materialises the fields we declared — the rest is skipped at the
//! deserializer level. Supports both `v_name` (snake_case, the AOSP
//! norm) and `vName` (camelCase, per the proto3-JSON spec).

use crate::walker::UnitEncoding;
use serde::Deserialize;

/// Best-effort language probe over the raw unit bytes.
///
/// Returns:
/// * `Some(lang)` when we successfully extracted `v_name.language`.
/// * `Some(String::new())` when the unit had a `v_name` but its
///   language field was empty / absent — the caller then needs to do
///   a full decode to fall back to `infer_language_from_inputs`.
/// * `None` when the bytes are malformed enough that peek can't
///   continue. The caller should fall back to a full decode, which
///   surfaces the real parse error.
pub(crate) fn peek_unit_language(bytes: &[u8], encoding: UnitEncoding) -> Option<String> {
    match encoding {
        UnitEncoding::Proto => peek_proto_language(bytes),
        UnitEncoding::Json  => peek_json_language(bytes),
    }
}

/// Whether the filter (if any) accepts this peeked language.
///
/// * `Some(lang)`: keep iff the dispatcher would route `lang` to one
///   of the labels in `allowed`.
/// * `Some(empty)`: language wasn't on the v_name — we can't decide
///   from peek alone, keep so the full decode + input-suffix
///   inference runs.
/// * `None`: peek failed, keep so the full decode surfaces the real
///   error.
pub(crate) fn peek_matches_filter(
    bytes: &[u8],
    encoding: UnitEncoding,
    has_class_input_hint: bool,
    allowed_labels: &std::collections::HashSet<String>,
) -> bool {
    match peek_unit_language(bytes, encoding) {
        Some(lang) if !lang.is_empty() => {
            let kind = crate::dispatch::choose(&lang, has_class_input_hint);
            allowed_labels.contains(kind.label())
        }
        _ => true, // unknown — defer to full decode
    }
}

// ---------------------------------------------------------------- proto

/// Hand-rolled `varint -> u64` decoder that returns `(value,
/// bytes_consumed)`. Returns `None` on malformed input (>10 bytes
/// without terminator) or end-of-buffer.
fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift = 0;
    for i in 0..10 {
        let b = *bytes.get(i)?;
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((val, i + 1));
        }
        shift += 7;
    }
    None
}

/// Skip one proto field given its wire type. Returns the new cursor.
fn skip_field(bytes: &[u8], cursor: usize, wire_type: u8) -> Option<usize> {
    match wire_type {
        0 => {
            // Varint.
            let (_, n) = read_varint(&bytes[cursor..])?;
            Some(cursor + n)
        }
        1 => {
            // 64-bit.
            if cursor + 8 > bytes.len() { return None; }
            Some(cursor + 8)
        }
        2 => {
            // Length-delimited.
            let (len, n) = read_varint(&bytes[cursor..])?;
            let len = len as usize;
            let end = cursor.checked_add(n)?.checked_add(len)?;
            if end > bytes.len() { return None; }
            Some(end)
        }
        5 => {
            // 32-bit.
            if cursor + 4 > bytes.len() { return None; }
            Some(cursor + 4)
        }
        // Groups (3, 4) are obsolete and Kythe protos don't use them.
        _ => None,
    }
}

/// Walk a length-delimited sub-message looking for one specific
/// (field_number, wire_type) inside it. If wire_type 2 (length-
/// delimited), return the sub-buffer; returns `None` if the field
/// isn't found before the end of `bytes`.
///
/// When `allow_truncated` is true and the LD field's declared length
/// runs past the end of `bytes`, we return the truncated tail anyway.
/// That's the right call when peeking a proto file we know is bigger
/// than our peek buffer — the field WAS present, we just can't see
/// all of it. The caller decides whether the truncated tail is enough
/// to make a decision (in our case yes: the language field sits very
/// near the start of the v_name sub-message).
fn find_field_ld(bytes: &[u8], target_field: u32, allow_truncated: bool) -> Option<&[u8]> {
    let mut cursor = 0;
    while cursor < bytes.len() {
        let (tag, n) = read_varint(&bytes[cursor..])?;
        cursor += n;
        let field_no = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;
        if field_no == target_field && wire_type == 2 {
            let (len, ln) = read_varint(&bytes[cursor..])?;
            cursor += ln;
            let len = len as usize;
            let end = cursor.checked_add(len)?;
            let clamp = if allow_truncated { bytes.len() } else { end };
            if end > bytes.len() && !allow_truncated { return None; }
            return Some(&bytes[cursor..end.min(clamp)]);
        }
        cursor = skip_field(bytes, cursor, wire_type)?;
    }
    None
}

/// Walk a length-delimited sub-message looking for a string field.
/// Returns `Some(String)` for matching, length-delimited UTF-8 bytes,
/// or `None` if not found. Malformed UTF-8 returns `Some(empty)` —
/// we treat that as "peek failed to read a clean language".
fn find_field_string(bytes: &[u8], target_field: u32) -> Option<String> {
    let raw = find_field_ld(bytes, target_field, false)?;
    Some(std::str::from_utf8(raw).unwrap_or("").to_string())
}

fn peek_proto_language(bytes: &[u8]) -> Option<String> {
    // IndexedCompilation.unit (field 1) is a sub-message. The unit's
    // declared length may run past our peek buffer; accept the
    // truncated tail because all we need from inside is the v_name
    // sub-sub-message, which sits at the very front (field 1).
    let cu = find_field_ld(bytes, 1, true)?;
    // CompilationUnit.v_name (field 1) is a sub-message — same
    // truncation reasoning.
    let vname = find_field_ld(cu, 1, true)?;
    // VName.language (field 5) is a string. By the time we reach
    // here we're inside the v_name sub-message, which is tiny
    // (signature + corpus + path + root + language, each a short
    // string) — disallow truncation so we don't return a half-read
    // language string.
    Some(find_field_string(vname, 5).unwrap_or_default())
}

// ---------------------------------------------------------------- json

/// Minimal serde shape that captures `unit.v_name.language` only.
/// Every other field in the JSON is ignored at the deserializer
/// level. Both snake_case (`v_name`) and camelCase (`vName`) field
/// names are accepted per the proto3-JSON spec; serde's
/// `#[serde(alias = "vName")]` handles the camelCase twin.
#[derive(Debug, Deserialize)]
struct PeekShape {
    #[serde(default)]
    unit: Option<PeekUnit>,
}

#[derive(Debug, Deserialize)]
struct PeekUnit {
    #[serde(default, alias = "vName")]
    v_name: Option<PeekVName>,
}

#[derive(Debug, Deserialize)]
struct PeekVName {
    #[serde(default)]
    language: Option<String>,
}

fn peek_json_language(bytes: &[u8]) -> Option<String> {
    // Fast path: AOSP extractors emit JSON with `unit.v_name` at the
    // very start and `language` as one of its first fields, so the
    // string `"language"` lives within the first few hundred bytes.
    // A bounded substring search avoids full serde_json descent over
    // the multi-MB `required_input` array that dominates the unit
    // body. The bound (4 KiB) is the largest reasonable v_name prefix
    // we've seen in practice; if `language` isn't there we fall back
    // to the slow serde walk.
    const FAST_PREFIX: usize = 4096;
    let head = &bytes[..bytes.len().min(FAST_PREFIX)];
    if let Some(lang) = scan_json_language_prefix(head) {
        return Some(lang);
    }
    // Slow path: structured deserialize, only invoked when the prefix
    // scan didn't find `language` (e.g. the field is below our 4 KiB
    // cap, or it sits inside a non-v_name object whose enclosing
    // context we can't tell from a substring).
    let shape: PeekShape = serde_json::from_slice(bytes).ok()?;
    Some(
        shape
            .unit
            .and_then(|u| u.v_name)
            .and_then(|v| v.language)
            .unwrap_or_default(),
    )
}

/// Locate `"language":"<value>"` in a JSON prefix and pull the value
/// out. Tolerates whitespace between the `:` and the opening quote.
/// Returns `None` when the field isn't present in `head`.
///
/// We can't tell from a substring whether `language` we matched is
/// the `VName.language` we want vs. a nested input's vname, but in
/// the AOSP-extractor wire format `unit.v_name.language` is always
/// the first `"language"` token to appear — earlier sibling fields
/// (`signature`, `corpus`) don't contain the substring "language".
/// The handful of v0.0.75 indexers that write a per-input vname put
/// `language` *after* the outer v_name's language.
fn scan_json_language_prefix(head: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"\"language\"";
    let pos = memmem(head, KEY)?;
    let mut cursor = pos + KEY.len();
    // Skip optional whitespace then expect `:`.
    while cursor < head.len() && head[cursor].is_ascii_whitespace() { cursor += 1; }
    if head.get(cursor) != Some(&b':') { return None; }
    cursor += 1;
    while cursor < head.len() && head[cursor].is_ascii_whitespace() { cursor += 1; }
    if head.get(cursor) != Some(&b'"') { return None; }
    cursor += 1;
    // Consume up to the closing quote. proto3 language strings are
    // simple ASCII tokens (`c++`, `java`, `kotlin`, `go`, `protobuf`,
    // `textproto`, `rust`, `objc`) so we don't need to handle JSON
    // escape sequences — a `\` inside would be a wire malformation,
    // and we cap at FAST_PREFIX so an unterminated string can't run
    // away.
    let start = cursor;
    while cursor < head.len() && head[cursor] != b'"' && head[cursor] != b'\\' {
        cursor += 1;
    }
    if cursor >= head.len() || head[cursor] != b'"' { return None; }
    Some(std::str::from_utf8(&head[start..cursor]).ok()?.to_string())
}

/// Tiny byte-substring search. Adequate for ~4 KiB prefixes; using
/// `memchr::memmem` would be marginally faster but adds a transitive
/// dependency the rest of `scry-kzip` doesn't need.
fn memmem(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() { return None; }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::analysis::{CompilationUnit, IndexedCompilation, VName};
    use protobuf::{Message, MessageField};

    fn sample(lang: &str) -> IndexedCompilation {
        let mut v = VName::new();
        v.language = lang.to_string();
        v.corpus = "test".to_string();
        let mut cu = CompilationUnit::new();
        cu.v_name = MessageField::some(v);
        let mut ic = IndexedCompilation::new();
        ic.unit = MessageField::some(cu);
        ic
    }

    #[test]
    fn proto_peek_returns_language() {
        let ic = sample("c++");
        let bytes = ic.write_to_bytes().unwrap();
        assert_eq!(peek_unit_language(&bytes, UnitEncoding::Proto).as_deref(), Some("c++"));
    }

    #[test]
    fn proto_peek_returns_empty_when_unset() {
        let ic = sample("");
        // v_name present but language is "" — proto3 omits the field
        // on the wire, so the peek falls through to find_field_string's
        // not-found path which returns "".
        let bytes = ic.write_to_bytes().unwrap();
        assert_eq!(peek_unit_language(&bytes, UnitEncoding::Proto).as_deref(), Some(""));
    }

    #[test]
    fn proto_peek_skips_intervening_fields() {
        let mut ic = sample("go");
        // Add some required_inputs so field 3 sits between field 1
        // (unit/v_name) on the CompilationUnit. The peek must skip
        // them cleanly.
        let mut fi_info = crate::proto::analysis::FileInfo::new();
        fi_info.path = "a/b.go".to_string();
        fi_info.digest = "deadbeef".to_string();
        let mut fi = crate::proto::analysis::compilation_unit::FileInput::new();
        fi.info = MessageField::some(fi_info);
        ic.unit.as_mut().unwrap().required_input.push(fi);
        let bytes = ic.write_to_bytes().unwrap();
        assert_eq!(peek_unit_language(&bytes, UnitEncoding::Proto).as_deref(), Some("go"));
    }

    #[test]
    fn json_peek_snake_case() {
        let json = r#"{"unit":{"v_name":{"language":"java"}}}"#;
        assert_eq!(
            peek_unit_language(json.as_bytes(), UnitEncoding::Json).as_deref(),
            Some("java"),
        );
    }

    #[test]
    fn json_peek_camel_case() {
        let json = r#"{"unit":{"vName":{"language":"kotlin"}}}"#;
        assert_eq!(
            peek_unit_language(json.as_bytes(), UnitEncoding::Json).as_deref(),
            Some("kotlin"),
        );
    }

    #[test]
    fn json_peek_ignores_extra_fields() {
        let json = r#"{
            "unit": {
                "v_name": {"language":"go", "corpus":"x"},
                "required_input": [{"info":{"path":"x.go"}}],
                "argument": ["go","build"]
            }
        }"#;
        assert_eq!(
            peek_unit_language(json.as_bytes(), UnitEncoding::Json).as_deref(),
            Some("go"),
        );
    }

    #[test]
    fn json_peek_empty_when_no_vname() {
        let json = r#"{"unit":{}}"#;
        assert_eq!(
            peek_unit_language(json.as_bytes(), UnitEncoding::Json).as_deref(),
            Some(""),
        );
    }

    #[test]
    fn json_peek_malformed_returns_none() {
        assert!(peek_unit_language(b"{not json", UnitEncoding::Json).is_none());
    }

    /// AOSP-shape JSON: language sits right after corpus inside the
    /// first v_name. The fast prefix scan should find it without
    /// touching serde_json.
    #[test]
    fn json_prefix_scan_handles_aosp_shape() {
        let raw = r##"{"unit":{"v_name":{"signature":"#abc","corpus":"x","language":"c++"},"required_input":[]}}"##;
        assert_eq!(
            scan_json_language_prefix(raw.as_bytes()),
            Some("c++".to_string()),
        );
    }

    /// Prefix scan tolerates whitespace + camelCase. Used to peek
    /// units whose extractor emits pretty-printed JSON.
    #[test]
    fn json_prefix_scan_tolerates_whitespace() {
        let raw = b"{\n  \"unit\": {\n    \"vName\": {\n      \"language\" : \"java\"\n    }\n  }\n}";
        assert_eq!(
            scan_json_language_prefix(raw),
            Some("java".to_string()),
        );
    }

    /// Prefix scan returns None when language is missing — the
    /// caller then falls back to the structured deserializer.
    #[test]
    fn json_prefix_scan_returns_none_when_absent() {
        let raw = br#"{"unit":{"v_name":{"signature":"x","corpus":"y"}}}"#;
        assert_eq!(scan_json_language_prefix(raw), None);
    }

    #[test]
    fn peek_matches_filter_keeps_matching() {
        let ic = sample("c++");
        let bytes = ic.write_to_bytes().unwrap();
        let mut allowed = std::collections::HashSet::new();
        allowed.insert("cxx".to_string());
        assert!(peek_matches_filter(&bytes, UnitEncoding::Proto, false, &allowed));
    }

    #[test]
    fn peek_matches_filter_rejects_other() {
        let ic = sample("go");
        let bytes = ic.write_to_bytes().unwrap();
        let mut allowed = std::collections::HashSet::new();
        allowed.insert("cxx".to_string());
        assert!(!peek_matches_filter(&bytes, UnitEncoding::Proto, false, &allowed));
    }

    #[test]
    fn peek_matches_filter_keeps_when_language_empty() {
        // No language → can't decide from peek alone, must defer.
        let ic = sample("");
        let bytes = ic.write_to_bytes().unwrap();
        let mut allowed = std::collections::HashSet::new();
        allowed.insert("cxx".to_string());
        assert!(peek_matches_filter(&bytes, UnitEncoding::Proto, false, &allowed));
    }
}
