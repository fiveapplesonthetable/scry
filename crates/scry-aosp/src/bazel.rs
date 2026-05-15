//! Bazel BUILD / Starlark (.bzl / .star) parser.
//!
//! Strategy: scan for top-level `func_name(...)` invocations, extract the
//! `name = "..."` kwarg as the rule's name (when present), and treat list
//! values inside `srcs=`, `deps=`, `data=`, `hdrs=` as Import refs.
//!
//! We also recognize `load(":foo.bzl", "macro_name")` as Import refs.

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

    // We use a very simple state machine: when we see IDENT(, record the
    // function name and capture key=value pairs (or list literals) until the
    // closing paren.
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut col = 1u32;

    let advance = |i: &mut usize, line: &mut u32, col: &mut u32, bytes: &[u8]| {
        if *i < bytes.len() {
            if bytes[*i] == b'\n' { *line += 1; *col = 1; } else { *col += 1; }
            *i += 1;
        }
    };
    let skip_str = |i: &mut usize, line: &mut u32, col: &mut u32, bytes: &[u8]| {
        let quote = bytes[*i];
        // Handle triple-quoted strings: """ or '''
        if *i + 2 < bytes.len() && bytes[*i + 1] == quote && bytes[*i + 2] == quote {
            *i += 3; *col += 3;
            while *i + 2 < bytes.len() && !(bytes[*i] == quote && bytes[*i + 1] == quote && bytes[*i + 2] == quote) {
                if bytes[*i] == b'\n' { *line += 1; *col = 1; } else { *col += 1; }
                *i += 1;
            }
            *i += 3; *col += 3;
            return;
        }
        advance(i, line, col, bytes);
        while *i < bytes.len() && bytes[*i] != quote {
            if bytes[*i] == b'\\' && *i + 1 < bytes.len() {
                advance(i, line, col, bytes);
            }
            advance(i, line, col, bytes);
        }
        if *i < bytes.len() { advance(i, line, col, bytes); }
    };

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' { advance(&mut i, &mut line, &mut col, bytes); }
            continue;
        }
        if c == b'"' || c == b'\'' {
            skip_str(&mut i, &mut line, &mut col, bytes);
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let fn_start = i;
            let fn_line = line;
            let fn_col = col;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                advance(&mut i, &mut line, &mut col, bytes);
            }
            let fn_name = &src[fn_start..i];
            // Skip whitespace + see if we hit a paren
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') { j += 1; }
            if j < bytes.len() && bytes[j] == b'(' {
                // Function call — parse args
                let args_end = match find_matching_paren(bytes, j) {
                    Some(e) => e,
                    None => { advance(&mut i, &mut line, &mut col, bytes); continue; }
                };
                let args_text = &src[j + 1..args_end];
                process_call(fn_name, args_text, fn_line, fn_col, fn_start as u32, &mut syms, &mut refs);
                // Skip past closing paren
                while i <= args_end {
                    advance(&mut i, &mut line, &mut col, bytes);
                }
                continue;
            }
            // not a call — already advanced past identifier
            continue;
        }
        advance(&mut i, &mut line, &mut col, bytes);
    }

    (syms, refs)
}

fn find_matching_paren(bytes: &[u8], open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open_idx;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'(' => depth += 1,
            b')' => { depth -= 1; if depth == 0 { return Some(i); } }
            b'"' | b'\'' => {
                let q = c;
                i += 1;
                // crude: skip until matching quote (no escape handling for our purposes)
                if i + 1 < bytes.len() && bytes[i] == q && bytes[i + 1] == q {
                    i += 2;
                    while i + 2 < bytes.len() && !(bytes[i] == q && bytes[i + 1] == q && bytes[i + 2] == q) {
                        i += 1;
                    }
                    i += 3;
                    continue;
                }
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() { i += 1; }
                    i += 1;
                }
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn process_call(
    fn_name: &str,
    args: &str,
    line: u32,
    col: u32,
    byte_start: u32,
    syms: &mut Vec<RawSymbol>,
    refs: &mut Vec<RawRef>,
) {
    // Extract name="..."
    let rule_name = extract_kwarg_string(args, "name");
    if let Some(name) = rule_name.clone() {
        syms.push(make_symbol(
            name.clone(), SymbolKind::SoongModule,  // reuse soong-module kind for Bazel rules
            line, col, byte_start, byte_start + name.len() as u32,
            vec![format!("bazel:{fn_name}")],
        ));
    }
    let rule_id = rule_name.unwrap_or_else(|| format!("<anon:{fn_name}>"));
    // Extract dep-like list kwargs: deps, srcs, hdrs, data, exports, tools.
    for k in ["deps", "srcs", "hdrs", "data", "exports", "tools", "exec_tools"] {
        for v in extract_kwarg_string_list(args, k) {
            if v.is_empty() { continue; }
            refs.push(make_ref(
                v.clone(), RefKind::Import,
                line, col, byte_start, byte_start + v.len() as u32,
                vec![format!("bazel:{fn_name}"), rule_id.clone(), k.to_string()],
            ));
        }
    }
    // `load("//foo:bar.bzl", "name1", "name2")` — first arg is the file,
    // subsequent are symbols.
    if fn_name == "load" {
        for v in extract_all_strings(args).into_iter().take(8) {
            refs.push(make_ref(
                v.clone(), RefKind::Import,
                line, col, byte_start, byte_start + v.len() as u32,
                vec!["bazel:load".to_string()],
            ));
        }
    }
}

fn extract_kwarg_string(args: &str, key: &str) -> Option<String> {
    // Look for `key = "..."`. Conservative scan.
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let s = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            let name = &args[s..i];
            // skip ws + =
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
            if j < bytes.len() && bytes[j] == b'=' && (j + 1 >= bytes.len() || bytes[j + 1] != b'=') {
                j += 1;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') { j += 1; }
                if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'') {
                    if name == key {
                        let q = bytes[j];
                        let str_start = j + 1;
                        let mut k = str_start;
                        while k < bytes.len() && bytes[k] != q { k += 1; }
                        return Some(args[str_start..k].to_string());
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn extract_kwarg_string_list(args: &str, key: &str) -> Vec<String> {
    let bytes = args.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let s = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            let name = &args[s..i];
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') { j += 1; }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n') { j += 1; }
                if j < bytes.len() && bytes[j] == b'[' && name == key {
                    let list_end = match find_matching_bracket(bytes, j) { Some(e) => e, None => return Vec::new() };
                    return extract_all_strings(&args[j + 1..list_end]);
                }
            }
        }
        i += 1;
    }
    Vec::new()
}

fn find_matching_bracket(bytes: &[u8], open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open_idx;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => { depth -= 1; if depth == 0 { return Some(i); } }
            b'"' | b'\'' => {
                let q = bytes[i]; i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() { i += 1; }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// Public helpers reused by sibling parsers (gn.rs).
pub(crate) fn __priv_extract_all_strings(s: &str) -> Vec<String> { extract_all_strings(s) }
pub(crate) fn __priv_find_matching(bytes: &[u8], open_idx: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open_idx;
    while i < bytes.len() {
        let c = bytes[i];
        if c == open { depth += 1; }
        else if c == close { depth -= 1; if depth == 0 { return Some(i); } }
        else if c == b'"' || c == b'\'' {
            let q = c; i += 1;
            while i < bytes.len() && bytes[i] != q {
                if bytes[i] == b'\\' && i + 1 < bytes.len() { i += 1; }
                i += 1;
            }
        }
        i += 1;
    }
    None
}

fn extract_all_strings(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            let q = c;
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != q {
                if bytes[i] == b'\\' && i + 1 < bytes.len() { i += 2; continue; }
                i += 1;
            }
            if i <= bytes.len() {
                out.push(s[start..i.min(bytes.len())].to_string());
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_bazel() {
        let src = br#"
            load("@rules_cc//cc:defs.bzl", "cc_library")
            cc_library(
                name = "foo",
                srcs = ["a.cc", "b.cc"],
                hdrs = ["foo.h"],
                deps = [
                    "//bar:libbar",
                    "@com_google_absl//absl/strings",
                ],
            )
        "#;
        let (syms, refs) = parse(src);
        assert!(syms.iter().any(|s| s.name == "foo"));
        let rnames: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(rnames.contains(&"//bar:libbar"));
        assert!(rnames.iter().any(|n| n.contains("absl/strings")));
        assert!(rnames.contains(&"a.cc"));
    }
}
