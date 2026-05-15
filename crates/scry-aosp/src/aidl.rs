//! AIDL parser. Tiny: enough to capture `interface IFoo { void bar(...); }`,
//! `parcelable Foo;`, and `enum E { A; B; }`. Annotations and bodies of
//! methods are skipped.
//!
//! Note that AIDL's primary use to scry is *cross-language linkage*:
//! when later phases want "find all Java + Cpp + Rust callers of
//! IBinder.transact", we ground the resolution in the .aidl source's
//! interface symbol id.

use crate::{make_ref, make_symbol};
use scry_lang::{RawRef, RawSymbol};
use scry_store::{RefKind, SymbolKind};

pub fn parse(source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let src = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let mut p = P::new(src);
    let mut syms = Vec::new();
    let mut refs = Vec::new();
    p.parse_file(&mut syms, &mut refs);
    (syms, refs)
}

struct P<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
    package: Option<String>,
}

impl<'a> P<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            src: s, bytes: s.as_bytes(),
            pos: 0, line: 1, col: 1,
            package: None,
        }
    }
    fn at_end(&self) -> bool { self.pos >= self.bytes.len() }
    fn peek(&self) -> u8 { if self.at_end() { 0 } else { self.bytes[self.pos] } }
    fn peek_at(&self, k: usize) -> u8 {
        if self.pos + k >= self.bytes.len() { 0 } else { self.bytes[self.pos + k] }
    }
    fn advance(&mut self) {
        if self.at_end() { return; }
        let c = self.bytes[self.pos]; self.pos += 1;
        if c == b'\n' { self.line += 1; self.col = 1; } else { self.col += 1; }
    }

    fn ws(&mut self) {
        loop {
            while !self.at_end() && self.peek().is_ascii_whitespace() { self.advance(); }
            if self.peek() == b'/' && self.peek_at(1) == b'/' {
                while !self.at_end() && self.peek() != b'\n' { self.advance(); }
                continue;
            }
            if self.peek() == b'/' && self.peek_at(1) == b'*' {
                self.advance(); self.advance();
                while !self.at_end() {
                    if self.peek() == b'*' && self.peek_at(1) == b'/' {
                        self.advance(); self.advance(); break;
                    }
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
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' { self.advance(); }
            else { break; }
        }
        if self.pos > start { Some(&self.src[start..self.pos]) } else { None }
    }

    fn skip_annotation(&mut self) {
        // @Name or @Name(...)
        if self.peek() != b'@' { return; }
        self.advance();
        let _ = self.parse_ident();
        self.ws();
        if self.peek() == b'(' {
            let mut depth = 0i32;
            self.advance(); depth = 1;
            while !self.at_end() && depth > 0 {
                let c = self.peek();
                if c == b'(' { self.advance(); depth += 1; }
                else if c == b')' { self.advance(); depth -= 1; }
                else { self.advance(); }
            }
        }
    }

    fn parse_file(&mut self, syms: &mut Vec<RawSymbol>, refs: &mut Vec<RawRef>) {
        loop {
            self.ws();
            if self.at_end() { return; }
            // skip annotations
            while self.peek() == b'@' { self.skip_annotation(); self.ws(); }
            if self.at_end() { return; }
            let start_line = self.line;
            let start_col = self.col;
            let start_byte = self.pos as u32;
            let kw = match self.parse_ident() {
                Some(s) => s,
                None => { self.advance(); continue; }
            };
            match kw {
                "package" => {
                    self.ws();
                    if let Some(name) = self.parse_ident() {
                        self.package = Some(name.to_string());
                    }
                    self.skip_to_semi();
                }
                "import" => {
                    self.ws();
                    if let Some(name) = self.parse_ident() {
                        let bytes = name.len() as u32;
                        refs.push(make_ref(
                            name.to_string(), RefKind::Import,
                            start_line, start_col, start_byte, start_byte + bytes,
                            vec!["aidl".to_string()],
                        ));
                    }
                    self.skip_to_semi();
                }
                "interface" => {
                    self.ws();
                    let (name, nline, ncol, nbyte) = self.consume_name();
                    let scope: Vec<String> = self.package.iter().cloned().collect();
                    if let Some(n) = name {
                        syms.push(make_symbol(
                            n.clone(), SymbolKind::AidlInterface,
                            nline, ncol, nbyte, nbyte + n.len() as u32, scope.clone(),
                        ));
                        self.parse_interface_body(&n, &scope, syms);
                    } else {
                        self.skip_to_close_brace();
                    }
                }
                "parcelable" => {
                    self.ws();
                    let (name, nline, ncol, nbyte) = self.consume_name();
                    if let Some(n) = name {
                        let scope: Vec<String> = self.package.iter().cloned().collect();
                        syms.push(make_symbol(
                            n.clone(), SymbolKind::AidlParcelable,
                            nline, ncol, nbyte, nbyte + n.len() as u32, scope,
                        ));
                    }
                    // parcelable can be forward-declared (`parcelable Foo;`) or
                    // structured (`parcelable Foo { ... }`). Handle both.
                    self.ws();
                    if self.peek() == b'{' { self.skip_to_close_brace(); }
                    else { self.skip_to_semi(); }
                }
                "enum" | "union" => {
                    self.ws();
                    let (name, nline, ncol, nbyte) = self.consume_name();
                    if let Some(n) = name {
                        let scope: Vec<String> = self.package.iter().cloned().collect();
                        let kind = if kw == "enum" { SymbolKind::Enum } else { SymbolKind::Union };
                        syms.push(make_symbol(
                            n, kind, nline, ncol, nbyte, nbyte, scope,
                        ));
                    }
                    self.ws();
                    if self.peek() == b'{' { self.skip_to_close_brace(); }
                    else { self.skip_to_semi(); }
                }
                _ => {
                    // unknown — skip line
                    self.skip_to_semi();
                }
            }
        }
    }

    fn consume_name(&mut self) -> (Option<String>, u32, u32, u32) {
        self.ws();
        let line = self.line; let col = self.col; let byte = self.pos as u32;
        let name = self.parse_ident().map(|s| s.to_string());
        (name, line, col, byte)
    }

    fn parse_interface_body(
        &mut self,
        interface: &str,
        pkg_scope: &[String],
        syms: &mut Vec<RawSymbol>,
    ) {
        self.ws();
        if self.peek() != b'{' { return; }
        self.advance();
        loop {
            self.ws();
            if self.at_end() || self.peek() == b'}' { self.advance(); return; }
            // skip annotations
            while self.peek() == b'@' { self.skip_annotation(); self.ws(); }
            // Each member is "type name(...)" or "const T name = ...;" or
            // a nested type. We just scan for the function-like name.
            // Strategy: skim until we find an identifier followed by '('
            let mline = self.line; let mcol = self.col;
            let mbyte = self.pos as u32;
            let mut last_ident: Option<String> = None;
            let mut last_ident_line = mline;
            let mut last_ident_col = mcol;
            let mut last_ident_byte = mbyte;
            loop {
                self.ws();
                if self.at_end() || self.peek() == b'}' { return; }
                let c = self.peek();
                if c == b';' { self.advance(); break; }
                if c == b'(' {
                    if let Some(n) = last_ident.take() {
                        let mut scope = pkg_scope.to_vec();
                        scope.push(interface.to_string());
                        syms.push(make_symbol(
                            n.clone(), SymbolKind::AidlMethod,
                            last_ident_line, last_ident_col, last_ident_byte,
                            last_ident_byte + n.len() as u32, scope,
                        ));
                    }
                    self.skip_balanced(b'(', b')');
                    continue;
                }
                if c == b'{' { self.skip_to_close_brace(); continue; }
                if c == b'=' {
                    // const declaration body — just skip to ;
                    while !self.at_end() && self.peek() != b';' { self.advance(); }
                    continue;
                }
                if c.is_ascii_alphabetic() || c == b'_' {
                    let line = self.line; let col = self.col;
                    let byte = self.pos as u32;
                    if let Some(id) = self.parse_ident() {
                        last_ident = Some(id.to_string());
                        last_ident_line = line;
                        last_ident_col = col;
                        last_ident_byte = byte;
                    }
                } else {
                    self.advance();
                }
            }
        }
    }

    fn skip_to_semi(&mut self) {
        while !self.at_end() && self.peek() != b';' { self.advance(); }
        if self.peek() == b';' { self.advance(); }
    }
    fn skip_to_close_brace(&mut self) {
        self.skip_balanced(b'{', b'}');
    }
    fn skip_balanced(&mut self, open: u8, close: u8) {
        // Assumes peek() == open OR we'll find one soon.
        while !self.at_end() && self.peek() != open { self.advance(); }
        if self.peek() != open { return; }
        let mut depth: i32;
        self.advance(); depth = 1;
        while !self.at_end() && depth > 0 {
            let c = self.peek();
            if c == open { self.advance(); depth += 1; }
            else if c == close { self.advance(); depth -= 1; }
            else { self.advance(); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_interface() {
        let src = br#"
            package android.os;
            interface IBinder {
                void transact(in int code, in Parcel data);
                String getInterfaceDescriptor();
            }
        "#;
        let (syms, _refs) = parse(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"IBinder"), "names: {:?}", names);
        assert!(names.contains(&"transact"), "names: {:?}", names);
        assert!(names.contains(&"getInterfaceDescriptor"), "names: {:?}", names);
    }

    #[test]
    fn parcelable_forward_decl() {
        let src = br#"
            package android.os;
            parcelable Parcel;
        "#;
        let (syms, _refs) = parse(src);
        assert!(syms.iter().any(|s| s.name == "Parcel"));
    }
}
