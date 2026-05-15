//! GN (.gn / .gni) parser — Chromium-style build files used by some AOSP
//! externals (skia, webrtc, swiftshader, ANGLE).
//!
//! Grammar of interest:
//!   template_name("target_name") {
//!     sources = [ "a.cc", "b.cc" ]
//!     deps = [ ":other_target", "//foo:bar" ]
//!   }
//!   import("//build/config.gni")
//!
//! We emit one symbol per `template_name("target")` and Import refs for
//! sources/deps lists + `import()` calls.

use crate::bazel; // share string-list extraction
use crate::{make_ref, make_symbol};
use scry_lang::{RawRef, RawSymbol};
use scry_store::{RefKind, SymbolKind};

pub fn parse(source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let src = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut syms = Vec::new();
    let mut refs = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if c == b'\n' { line += 1; i += 1; continue; }
        if c.is_ascii_alphabetic() || c == b'_' {
            let kw_start = i;
            let kw_line = line;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            let kw = &src[kw_start..i];
            // Skip whitespace
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
            if i < bytes.len() && bytes[i] == b'(' {
                let close = match find_matching_paren(bytes, i) { Some(e) => e, None => break };
                let args = &src[i + 1..close];
                // template_name("target")  → extract target name from first string
                let strs = bazel::__priv_extract_all_strings(args);
                let target = strs.first().cloned();
                if kw == "import" {
                    if let Some(t) = target {
                        refs.push(make_ref(
                            t.clone(), RefKind::Import,
                            kw_line, 1, kw_start as u32, kw_start as u32 + t.len() as u32,
                            vec!["gn:import".to_string()],
                        ));
                    }
                    i = close + 1;
                    continue;
                }
                if let Some(t) = target.as_ref() {
                    syms.push(make_symbol(
                        t.clone(), SymbolKind::SoongModule,
                        kw_line, 1, kw_start as u32, kw_start as u32 + t.len() as u32,
                        vec![format!("gn:{kw}")],
                    ));
                }
                // After the (...), if a { block follows, parse its kv pairs.
                let mut j = close + 1;
                for b in &bytes[i..=close] { if *b == b'\n' { line += 1; } }
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') {
                    if bytes[j] == b'\n' { line += 1; }
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'{' {
                    let block_close = match find_matching_brace(bytes, j) { Some(e) => e, None => break };
                    let body = &src[j + 1..block_close];
                    for key in ["sources", "deps", "public_deps", "data_deps", "configs",
                                "include_dirs", "libs", "inputs", "outputs", "public"] {
                        for v in extract_kwarg_list(body, key) {
                            refs.push(make_ref(
                                v.clone(), RefKind::Import,
                                kw_line, 1, kw_start as u32, kw_start as u32 + v.len() as u32,
                                vec![
                                    format!("gn:{kw}"),
                                    target.clone().unwrap_or_else(|| "<anon>".into()),
                                    key.to_string(),
                                ],
                            ));
                        }
                    }
                    for b in &bytes[j..=block_close] { if *b == b'\n' { line += 1; } }
                    i = block_close + 1;
                    continue;
                }
                i = close + 1;
                continue;
            }
            continue;
        }
        i += 1;
    }
    (syms, refs)
}

fn find_matching_paren(bytes: &[u8], open_idx: usize) -> Option<usize> {
    bazel::__priv_find_matching(bytes, open_idx, b'(', b')')
}
fn find_matching_brace(bytes: &[u8], open_idx: usize) -> Option<usize> {
    bazel::__priv_find_matching(bytes, open_idx, b'{', b'}')
}

fn extract_kwarg_list(body: &str, key: &str) -> Vec<String> {
    // Find `key = [ ... ]`
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let s = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            let name = &body[s..i];
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') { j += 1; }
                if j < bytes.len() && bytes[j] == b'[' && name == key {
                    let close = match bazel::__priv_find_matching(bytes, j, b'[', b']') { Some(e) => e, None => return Vec::new() };
                    return bazel::__priv_extract_all_strings(&body[j + 1..close]);
                }
            }
        }
        i += 1;
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_gn() {
        let src = br#"
            import("//build/config.gni")

            static_library("foo") {
                sources = [
                    "a.cc",
                    "b.cc",
                ]
                deps = [
                    ":bar",
                    "//third_party:baz",
                ]
            }
        "#;
        let (syms, refs) = parse(src);
        assert!(syms.iter().any(|s| s.name == "foo"));
        let rnames: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(rnames.contains(&":bar"));
        assert!(rnames.contains(&"//third_party:baz"));
        assert!(rnames.contains(&"//build/config.gni"));
    }
}
