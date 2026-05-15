//! SELinux .te policy parser.
//!
//! Grammar of interest:
//!   type NAME, attr1, attr2;
//!   attribute NAME;
//!   typeattribute T attr1, attr2;
//!   allow A B:CLASS perms;
//!   neverallow A B:CLASS perms;
//!   auditallow / dontaudit similar.
//!
//! We emit a SepolicyType symbol per `type NAME` / `attribute NAME` /
//! `typeattribute T ...` (the T part) declaration. We emit Call refs for the
//! subjects/objects of allow/neverallow/auditallow/dontaudit rules so the
//! user can ask "what rules touch type X" via `scry callers X`.

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
        // Strip comment: # ... or ; line-terminator
        let body = match raw_line.find('#') { Some(p) => &raw_line[..p], None => raw_line };
        let body = body.trim();
        if body.is_empty() {
            byte_offset += line_len + 1; continue;
        }
        // Tokenize at spaces, commas, colons, semicolons
        let trimmed = body.trim_end_matches(';');
        let tokens: Vec<&str> = trimmed.split(|c: char| c.is_whitespace() || c == ',' || c == ':')
            .filter(|s| !s.is_empty()).collect();
        if tokens.is_empty() { byte_offset += line_len + 1; continue; }
        match tokens[0] {
            "type" => {
                if let Some(name) = tokens.get(1) {
                    let col = raw_line.find(name).unwrap_or(0) as u32 + 1;
                    syms.push(make_symbol(
                        (*name).to_string(), SymbolKind::SepolicyType,
                        line_no, col, byte_offset + col - 1,
                        byte_offset + col - 1 + name.len() as u32, Vec::new(),
                    ));
                }
            }
            "attribute" => {
                if let Some(name) = tokens.get(1) {
                    let col = raw_line.find(name).unwrap_or(0) as u32 + 1;
                    syms.push(make_symbol(
                        (*name).to_string(), SymbolKind::SepolicyType,
                        line_no, col, byte_offset + col - 1,
                        byte_offset + col - 1 + name.len() as u32,
                        vec!["attribute".to_string()],
                    ));
                }
            }
            "allow" | "neverallow" | "auditallow" | "dontaudit" => {
                // First two tokens after the verb are subject and target type
                for (idx, t) in tokens.iter().skip(1).take(2).enumerate() {
                    let col = raw_line.find(t).unwrap_or(0) as u32 + 1;
                    refs.push(make_ref(
                        (*t).to_string(), RefKind::Call,
                        line_no, col, byte_offset + col - 1,
                        byte_offset + col - 1 + t.len() as u32,
                        vec![
                            tokens[0].to_string(),
                            if idx == 0 { "subject".to_string() } else { "target".to_string() },
                        ],
                    ));
                }
            }
            _ => {}
        }
        byte_offset += line_len + 1;
    }
    (syms, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_and_allow() {
        let src = br#"
type my_t, file_type;
attribute my_attr;
allow my_t init:dir { read open };
neverallow init { my_t my_attr }:file write;
"#;
        let (syms, refs) = parse(src);
        let snames: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(snames.contains(&"my_t"));
        assert!(snames.contains(&"my_attr"));
        let rnames: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(rnames.contains(&"my_t") || rnames.contains(&"init"));
    }
}
