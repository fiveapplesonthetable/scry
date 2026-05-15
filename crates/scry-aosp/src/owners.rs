//! OWNERS file parser.
//!
//! Format (per gerrit's owners plugin, which AOSP uses):
//!
//! ```text
//! set noparent
//! include path/to/other/OWNERS
//! email@google.com
//! per-file PATTERN = email@google.com
//! *           # anyone
//! # comments start with #
//! ```
//!
//! We emit one OwnersEmail symbol per email/wildcard, scoped to the file.
//! `include` directives are emitted as Import refs (so `scry ref ANCESTOR_OWNERS`
//! finds files that include it).

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
    let mut byte_offset: u32 = 0;
    for (i, raw_line) in src.lines().enumerate() {
        let line_no = (i + 1) as u32;
        let line_len = raw_line.len() as u32;
        // strip comment
        let body = match raw_line.find('#') {
            Some(p) => &raw_line[..p],
            None => raw_line,
        };
        let trimmed = body.trim();
        let leading = (raw_line.len() - body.trim_start().len()) as u32;

        if trimmed.is_empty() { byte_offset += line_len + 1; continue; }

        if trimmed.eq_ignore_ascii_case("set noparent") || trimmed.eq_ignore_ascii_case("noparent") {
            byte_offset += line_len + 1; continue;
        }
        if let Some(rest) = trimmed.strip_prefix("include ") {
            let path = rest.trim();
            refs.push(make_ref(
                path.to_string(), RefKind::Import,
                line_no, leading.saturating_add(1),
                byte_offset + leading, byte_offset + leading + path.len() as u32,
                vec!["OWNERS".to_string()],
            ));
            byte_offset += line_len + 1; continue;
        }
        if let Some(rest) = trimmed.strip_prefix("per-file ") {
            // per-file PATTERN = email or per-file PATTERN=*
            if let Some(eq) = rest.find('=') {
                let pattern = rest[..eq].trim();
                let value = rest[eq + 1..].trim();
                // Emit each value as an OwnersEmail symbol, scoped to the per-file pattern
                for v in value.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    syms.push(make_symbol(
                        v.to_string(), SymbolKind::OwnersEmail,
                        line_no, leading.saturating_add(1),
                        byte_offset + leading, byte_offset + leading + v.len() as u32,
                        vec!["per-file".to_string(), pattern.to_string()],
                    ));
                }
            }
            byte_offset += line_len + 1; continue;
        }
        // Plain email or '*'
        syms.push(make_symbol(
            trimmed.to_string(), SymbolKind::OwnersEmail,
            line_no, leading.saturating_add(1),
            byte_offset + leading, byte_offset + leading + trimmed.len() as u32,
            Vec::new(),
        ));
        byte_offset += line_len + 1;
    }
    (syms, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_owners() {
        let src = br#"# owners
set noparent
alice@google.com
bob@google.com
include /platform/frameworks/base/OWNERS
per-file *.java = carol@google.com
* # anyone
"#;
        let (syms, refs) = parse(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alice@google.com"));
        assert!(names.contains(&"bob@google.com"));
        assert!(names.contains(&"carol@google.com"));
        assert!(names.contains(&"*"));
        assert!(refs.iter().any(|r| r.name == "/platform/frameworks/base/OWNERS"));
    }
}
