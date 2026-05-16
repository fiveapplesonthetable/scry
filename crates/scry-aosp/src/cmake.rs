//! CMake parser.
//!
//! Extracts the target names from `add_library(NAME ...)`,
//! `add_executable(NAME ...)`, and `add_custom_target(NAME ...)`, and emits
//! Import refs for `target_link_libraries(target dep1 dep2 ...)`,
//! `find_package(NAME ...)`, `include(NAME)`, `add_subdirectory(PATH)`.
//!
//! Strategy: scan for top-level COMMAND(ARGS) invocations. Each command is
//! an identifier followed by `(`. Args are tokenized at whitespace, with
//! string literals preserved.

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
            let cmd_start = i;
            let cmd_line = line;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') { i += 1; }
            let cmd = src[cmd_start..i].to_string();
            // skip ws
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') { i += 1; }
            if i < bytes.len() && bytes[i] == b'(' {
                let close = match find_matching_paren(bytes, i) { Some(e) => e, None => break };
                let args_str = &src[i + 1..close];
                let args = tokenize_cmake_args(args_str);
                process_cmake_call(&cmd, &args, cmd_line, &mut syms, &mut refs, cmd_start as u32);
                // Advance line counter past args
                for b in &bytes[i + 1..=close] { if *b == b'\n' { line += 1; } }
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
    let mut depth = 0i32;
    let mut i = open_idx;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => { depth -= 1; if depth == 0 { return Some(i); } }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() { i += 1; }
                    i += 1;
                }
            }
            b'#' => { while i < bytes.len() && bytes[i] != b'\n' { i += 1; } }
            _ => {}
        }
        i += 1;
    }
    None
}

fn tokenize_cmake_args(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() { i += 1; continue; }
        if c == b'#' { while i < bytes.len() && bytes[i] != b'\n' { i += 1; } continue; }
        if c == b'"' {
            i += 1;
            let s_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() { i += 1; }
                i += 1;
            }
            tokens.push(s[s_start..i.min(bytes.len())].to_string());
            if i < bytes.len() { i += 1; }
            continue;
        }
        let start = i;
        // Stop also at `#` — CMake treats # as comment-start ANYWHERE, not
        // just at token boundaries. Without this, `good_target# trailing
        // comment` parses as the single token `good_target#`, which is
        // wrong (it should be the identifier `good_target` followed by a
        // comment-to-newline).
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b')'
            && bytes[i] != b'#'
        {
            i += 1;
        }
        if i == start {
            // No progress: bytes[i] is `)` (the only single-byte case that
            // hits neither the whitespace nor the identifier loops). Without
            // an explicit advance we infinite-loop pushing empty tokens, which
            // is what blew up on ctags' cmake-comments test fixture: a `)`
            // inside a `# comment` is invisible to find_matching_paren's
            // comment-aware scan but VISIBLE to tokenize_cmake_args here
            // because we entered tokenize with the args slice already past
            // the comment context. Defensive skip.
            i += 1;
        } else {
            tokens.push(s[start..i].to_string());
        }
    }
    tokens
}

fn process_cmake_call(
    cmd: &str,
    args: &[String],
    line: u32,
    syms: &mut Vec<RawSymbol>,
    refs: &mut Vec<RawRef>,
    byte_start: u32,
) {
    let cmd_lower = cmd.to_ascii_lowercase();
    let target = args.first().cloned();
    match cmd_lower.as_str() {
        "add_library" | "add_executable" | "add_custom_target" | "add_test" => {
            if let Some(t) = target.as_ref() {
                syms.push(make_symbol(
                    t.clone(), SymbolKind::SoongModule,
                    line, 1, byte_start, byte_start + t.len() as u32,
                    vec![format!("cmake:{cmd_lower}")],
                ));
            }
        }
        "target_link_libraries" | "target_include_directories" | "target_sources" => {
            let owner = target.unwrap_or_else(|| "<unknown>".into());
            for a in args.iter().skip(1) {
                // Skip PRIVATE/PUBLIC/INTERFACE keywords
                let s = a.as_str();
                if matches!(s, "PRIVATE" | "PUBLIC" | "INTERFACE" | "SYSTEM" | "BEFORE") { continue; }
                refs.push(make_ref(
                    s.to_string(), RefKind::Import,
                    line, 1, byte_start, byte_start + s.len() as u32,
                    vec![format!("cmake:{cmd_lower}"), owner.clone()],
                ));
            }
        }
        "find_package" | "find_library" | "find_path" => {
            if let Some(t) = target {
                refs.push(make_ref(
                    t.clone(), RefKind::Import,
                    line, 1, byte_start, byte_start + t.len() as u32,
                    vec![format!("cmake:{cmd_lower}")],
                ));
            }
        }
        "include" | "add_subdirectory" => {
            if let Some(t) = target {
                refs.push(make_ref(
                    t.clone(), RefKind::Import,
                    line, 1, byte_start, byte_start + t.len() as u32,
                    vec![format!("cmake:{cmd_lower}")],
                ));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: tokenize_cmake_args used to infinite-loop on `)` inside
    /// a comment that was passed through find_matching_paren. This fixture
    /// is verbatim from ctags/Units/parser-cmake.r/cmake-comments.d/input.cmake
    /// — the exact 605-byte file that hung the indexer for >2 minutes before
    /// being cgroup-OOM-killed. Test must complete in well under 1 second.
    #[test]
    fn cmake_comments_with_unbalanced_paren() {
        let src = br#"# this is a test of comments set(DO_NOT_TAG "foo")

#[[
multi-linecomments
option(DO_NOT_TAG "foo" OFF)
] not the end
]]set(tag_this)

add_custom_target(# comment set(NO_TAG "foo")
    # anothe rline comment
    good_target# this is legal comment placement set(NO_TAG foo)
    ALL)

add_library(another_good_target# <-- target
    SHARED # set(NO_TAG bar)
    gmock-all.cc
    )
"#;
        let t = std::time::Instant::now();
        let (syms, _refs) = parse(src);
        assert!(t.elapsed().as_millis() < 1000, "parse should be sub-second");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"good_target"), "got: {:?}", names);
        assert!(names.contains(&"another_good_target"), "got: {:?}", names);
    }

    #[test]
    fn basic_cmake() {
        let src = br#"
            cmake_minimum_required(VERSION 3.10)
            project(my_thing)

            add_library(foo SHARED a.cc b.cc)
            target_link_libraries(foo PRIVATE bar baz)
            add_executable(prog main.cc)
            target_link_libraries(prog foo)
            find_package(Threads REQUIRED)
            add_subdirectory(subdir)
        "#;
        let (syms, refs) = parse(src);
        let snames: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(snames.contains(&"foo"));
        assert!(snames.contains(&"prog"));
        let rnames: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(rnames.contains(&"bar"));
        assert!(rnames.contains(&"baz"));
        assert!(rnames.contains(&"Threads"));
        assert!(rnames.contains(&"subdir"));
    }
}
