//! Android.bp / Blueprint parser.
//!
//! Blueprint is a Go-like DSL with a small grammar:
//!   file        := decl*
//!   decl        := IDENT '{' kv* '}'                       // module decl
//!                | IDENT '=' value | IDENT '+=' value      // assignment
//!   kv          := IDENT ':' value (',')?
//!   value       := STRING | INT | BOOL | IDENT
//!                | '[' (value ',')* ']'
//!                | '{' kv* '}'
//!                | value '+' value
//!
//! We only need to extract module name, deps, srcs, cflags, visibility,
//! defaults — enough to answer `scry mod`, `scry module-of`, `scry cflag`.
//!
//! On parse error we emit whatever symbols we collected before the error and
//! stop. Soong's own parser is the source of truth; this is best-effort.

use crate::{make_ref, make_symbol};
use scry_lang::{RawRef, RawSymbol};
use scry_store::{RefKind, SymbolKind};

pub fn parse(source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let src = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut p = Parser::new(src);
    let mut syms = Vec::new();
    let mut refs = Vec::new();
    p.parse_file(&mut syms, &mut refs);
    (syms, refs)
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, bytes: src.as_bytes(), pos: 0, line: 1, col: 1 }
    }

    fn at_end(&self) -> bool { self.pos >= self.bytes.len() }
    fn peek(&self) -> u8 { if self.at_end() { 0 } else { self.bytes[self.pos] } }
    fn peek_at(&self, k: usize) -> u8 {
        if self.pos + k >= self.bytes.len() { 0 } else { self.bytes[self.pos + k] }
    }
    fn advance(&mut self) {
        if self.at_end() { return; }
        let c = self.bytes[self.pos];
        self.pos += 1;
        if c == b'\n' { self.line += 1; self.col = 1; }
        else { self.col += 1; }
    }
    fn pos_marker(&self) -> (u32, u32, u32) { (self.pos as u32, self.line, self.col) }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while !self.at_end() && (self.peek() == b' ' || self.peek() == b'\t' ||
                                     self.peek() == b'\n' || self.peek() == b'\r') {
                self.advance();
            }
            // line comment
            if self.peek() == b'/' && self.peek_at(1) == b'/' {
                while !self.at_end() && self.peek() != b'\n' {
                    self.advance();
                }
                continue;
            }
            // block comment
            if self.peek() == b'/' && self.peek_at(1) == b'*' {
                self.advance(); self.advance();
                while !self.at_end() {
                    if self.peek() == b'*' && self.peek_at(1) == b'/' {
                        self.advance(); self.advance();
                        break;
                    }
                    self.advance();
                }
                continue;
            }
            // Hash comment (rare, but Blueprint accepts # in some places)
            if self.peek() == b'#' {
                while !self.at_end() && self.peek() != b'\n' {
                    self.advance();
                }
                continue;
            }
            break;
        }
    }

    fn parse_ident(&mut self) -> Option<&'a str> {
        let start = self.pos;
        let c = self.peek();
        if !(c.is_ascii_alphabetic() || c == b'_') { return None; }
        while !self.at_end() {
            let c = self.peek();
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos > start {
            Some(&self.src[start..self.pos])
        } else {
            None
        }
    }

    /// Parse a (possibly + concatenated) value. Strings are unescaped only
    /// minimally (we just take the inner bytes).
    fn parse_value(&mut self, name_field: &str) -> Vec<String> {
        // Returns a flat list of strings that the field accumulates.
        // For non-list fields we yield exactly one element (or zero on error).
        self.skip_ws_and_comments();
        let mut out: Vec<String> = Vec::new();
        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                b'"' => {
                    if let Some(s) = self.parse_string() {
                        out.push(s);
                    } else {
                        return out;
                    }
                }
                b'[' => {
                    self.advance();
                    loop {
                        self.skip_ws_and_comments();
                        if self.peek() == b']' { self.advance(); break; }
                        if self.at_end() { return out; }
                        // Each element is itself a "value" but we flatten.
                        let inner = self.parse_value(name_field);
                        out.extend(inner);
                        self.skip_ws_and_comments();
                        if self.peek() == b',' { self.advance(); }
                    }
                }
                b'{' => {
                    // nested map — for our extraction we ignore the structure
                    // but consume it so we stay synchronized.
                    self.skip_object();
                }
                b't' | b'f' => {
                    if self.src[self.pos..].starts_with("true") {
                        out.push("true".to_string()); self.pos += 4; self.col += 4;
                    } else if self.src[self.pos..].starts_with("false") {
                        out.push("false".to_string()); self.pos += 5; self.col += 5;
                    } else if let Some(id) = self.parse_ident() {
                        out.push(id.to_string());
                    } else { return out; }
                }
                c if c.is_ascii_digit() || c == b'-' => {
                    // number
                    let start = self.pos;
                    if c == b'-' { self.advance(); }
                    while !self.at_end() && self.peek().is_ascii_digit() {
                        self.advance();
                    }
                    out.push(self.src[start..self.pos].to_string());
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    if let Some(id) = self.parse_ident() {
                        out.push(id.to_string());
                    } else {
                        return out;
                    }
                }
                _ => return out,
            }
            // Concat with `+` continues
            self.skip_ws_and_comments();
            if self.peek() == b'+' && self.peek_at(1) != b'=' {
                self.advance();
                continue;
            }
            return out;
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if self.peek() != b'"' { return None; }
        self.advance();
        let mut s = String::new();
        while !self.at_end() {
            let c = self.peek();
            if c == b'"' { self.advance(); return Some(s); }
            if c == b'\\' && self.peek_at(1) != 0 {
                self.advance();
                let e = self.peek();
                self.advance();
                s.push(match e {
                    b'n' => '\n', b't' => '\t', b'r' => '\r',
                    b'\\' => '\\', b'"' => '"', other => other as char,
                });
                continue;
            }
            if c == b'\n' { return Some(s); } // unterminated
            s.push(c as char);
            self.advance();
        }
        Some(s)
    }

    fn skip_object(&mut self) {
        // Assumes peek() == '{', consumes balanced braces.
        if self.peek() != b'{' { return; }
        let mut depth: i32;
        self.advance(); depth = 1;
        while !self.at_end() && depth > 0 {
            match self.peek() {
                b'{' => { self.advance(); depth += 1; }
                b'}' => { self.advance(); depth -= 1; }
                b'"' => { let _ = self.parse_string(); }
                b'/' if self.peek_at(1) == b'/' || self.peek_at(1) == b'*' =>
                    self.skip_ws_and_comments(),
                _ => self.advance(),
            }
        }
    }

    fn parse_file(&mut self, syms: &mut Vec<RawSymbol>, refs: &mut Vec<RawRef>) {
        loop {
            self.skip_ws_and_comments();
            if self.at_end() { return; }
            let (mark_byte, mark_line, mark_col) = self.pos_marker();
            let ident = match self.parse_ident() {
                Some(s) => s.to_string(),
                None => { self.advance(); continue; }
            };
            self.skip_ws_and_comments();
            match self.peek() {
                b'{' => {
                    // module declaration: IDENT { ... }
                    self.parse_module(&ident, mark_byte, mark_line, mark_col, syms, refs);
                }
                b'=' => {
                    self.advance();
                    let _ = self.parse_value("");
                    // record as a Variable symbol so users can find them
                    syms.push(make_symbol(
                        ident, SymbolKind::Variable, mark_line, mark_col,
                        mark_byte, self.pos as u32, Vec::new(),
                    ));
                }
                b'+' if self.peek_at(1) == b'=' => {
                    self.advance(); self.advance();
                    let _ = self.parse_value("");
                }
                _ => {
                    // unknown; skip token
                }
            }
        }
    }

    fn parse_module(
        &mut self,
        module_type: &str,
        type_byte: u32,
        type_line: u32,
        type_col: u32,
        syms: &mut Vec<RawSymbol>,
        refs: &mut Vec<RawRef>,
    ) {
        // peek() == '{'
        if self.peek() != b'{' { return; }
        self.advance();
        // Collect fields. We're interested in `name`, `srcs`, `deps`,
        // `static_libs`, `shared_libs`, `header_libs`, `whole_static_libs`,
        // `defaults`, `cflags`, `cppflags`, `ldflags`, `visibility`,
        // `apex_available`, `required`.
        let mut module_name: Option<String> = None;
        let mut module_name_line: u32 = type_line;
        let mut module_name_col: u32 = type_col;
        let mut module_name_byte: u32 = type_byte;
        let mut field_buf: Vec<(String, Vec<String>, u32, u32, u32)> = Vec::new();
        loop {
            self.skip_ws_and_comments();
            if self.at_end() || self.peek() == b'}' { self.advance(); break; }
            let (fb, fl, fc) = self.pos_marker();
            let field_name = match self.parse_ident() {
                Some(s) => s.to_string(),
                None => { self.advance(); continue; }
            };
            self.skip_ws_and_comments();
            if self.peek() != b':' {
                // Could be syntax error; skip to next comma or close
                continue;
            }
            self.advance();
            let values = self.parse_value(&field_name);
            if field_name == "name" {
                if let Some(v) = values.first() {
                    module_name = Some(v.clone());
                    module_name_byte = fb;
                    module_name_line = fl;
                    module_name_col = fc;
                }
            } else {
                field_buf.push((field_name, values, fb, fl, fc));
            }
            self.skip_ws_and_comments();
            if self.peek() == b',' { self.advance(); }
        }

        let module_id = module_name.unwrap_or_else(|| format!("<anon:{module_type}>"));

        // Module symbol
        syms.push(make_symbol(
            module_id.clone(),
            SymbolKind::SoongModule,
            module_name_line,
            module_name_col,
            module_name_byte,
            module_name_byte + module_id.len() as u32,
            vec![module_type.to_string()],
        ));

        // Field-level refs: each *_libs / defaults / required entry becomes a
        // ref to a module name; each cflag becomes a ref to a flag name.
        for (field, values, fb, fl, fc) in field_buf {
            let kind = match field.as_str() {
                "static_libs" | "shared_libs" | "header_libs" | "whole_static_libs"
                | "host_required" | "target_required" | "required" | "defaults"
                | "libs" | "runtime_libs" | "data" | "lint_baseline"
                | "srcs" | "exclude_srcs" | "generated_sources" | "java_resources"
                | "out" | "tools" | "tool_files" => {
                    Some(RefKind::Import)
                }
                "cflags" | "cppflags" | "ldflags" | "asflags" | "tidy_flags"
                | "ldlibs" | "rtti" | "stl" | "sanitize" => {
                    Some(RefKind::TypeUse)
                }
                _ => None,
            };
            if let Some(rk) = kind {
                for v in &values {
                    if v.is_empty() { continue; }
                    refs.push(make_ref(
                        v.clone(), rk, fl, fc, fb, fb + v.len() as u32,
                        vec![module_type.to_string(), module_id.clone(), field.clone()],
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scry_store::SymbolKind;

    #[test]
    fn simple_module() {
        let bp = br#"
            cc_library_static {
                name: "libfoo",
                srcs: ["foo.cpp", "bar.cpp"],
                shared_libs: ["liblog", "libbase"],
                cflags: ["-Wall", "-Wno-error"],
            }
        "#;
        let (syms, refs) = parse(bp);
        let s = syms.iter().find(|s| s.name == "libfoo").expect("module sym");
        assert_eq!(s.kind, SymbolKind::SoongModule);
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"liblog"));
        assert!(names.contains(&"libbase"));
        assert!(names.contains(&"-Wall"));
    }

    #[test]
    fn anonymous_assignment() {
        let bp = br#"
            my_default_flags = ["-Wfoo", "-Wbar"]
            cc_library {
                name: "libthing",
                cflags: my_default_flags,
            }
        "#;
        let (syms, _refs) = parse(bp);
        assert!(syms.iter().any(|s| s.name == "my_default_flags"));
        assert!(syms.iter().any(|s| s.name == "libthing"));
    }
}
