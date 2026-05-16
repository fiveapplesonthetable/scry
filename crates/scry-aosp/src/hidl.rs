//! HIDL (.hal) parser. HIDL is the legacy AOSP HAL IPC syntax: very similar
//! to AIDL but with `oneway`, `generates(...)`, versioned packages, and
//! C++-flavored types. We extract interfaces, methods, enums, structs, unions
//! and emit InheritFrom refs for `extends` clauses + Import refs for `import`.

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
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'@' {
                self.advance();
            } else { break; }
        }
        if self.pos > start { Some(&self.src[start..self.pos]) } else { None }
    }
    fn skip_annotation(&mut self) {
        if self.peek() != b'@' { return; }
        self.advance();
        let _ = self.parse_ident();
        self.ws();
        if self.peek() == b'(' {
            let mut depth: i32; self.advance(); depth = 1;
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
            while self.peek() == b'@' { self.skip_annotation(); self.ws(); }
            if self.at_end() { return; }
            let kw_byte = self.pos as u32;
            let kw_line = self.line;
            let kw_col = self.col;
            let kw = match self.parse_ident() { Some(s) => s, None => { self.advance(); continue; }};
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
                        refs.push(make_ref(
                            name.to_string(), RefKind::Import,
                            kw_line, kw_col, kw_byte, kw_byte + name.len() as u32,
                            vec!["hidl".to_string()],
                        ));
                    }
                    self.skip_to_semi();
                }
                "interface" => {
                    self.ws();
                    let (name, nl, nc, nb) = self.consume_name();
                    let scope: Vec<String> = self.package.iter().cloned().collect();
                    if let Some(n) = name {
                        syms.push(make_symbol(
                            n.clone(), SymbolKind::AidlInterface,
                            nl, nc, nb, nb + n.len() as u32, scope.clone(),
                        ));
                        // HIDL toolchain emits its own family of bindings:
                        // Bp{I}, Bn{I}, Bs{I} (passthrough wrapper).
                        // Shadow them as `HidlShadow` so `scry def BpIFoo`
                        // lands on the .hal source.
                        emit_hidl_shadows(&n, &scope, nl, nc, nb, syms);
                        // Optional `extends Base`
                        self.ws();
                        if self.peek_word("extends") {
                            self.advance_word();
                            self.ws();
                            let (base, bl, bc, bb) = self.consume_name();
                            if let Some(b) = base {
                                refs.push(make_ref(
                                    b.clone(), RefKind::InheritFrom,
                                    bl, bc, bb, bb + b.len() as u32, scope.clone(),
                                ));
                            }
                        }
                        self.parse_interface_body(&n, &scope, syms);
                    } else {
                        self.skip_to_close_brace();
                    }
                }
                "enum" => {
                    self.ws();
                    let (name, nl, nc, nb) = self.consume_name();
                    if let Some(n) = name {
                        let scope: Vec<String> = self.package.iter().cloned().collect();
                        syms.push(make_symbol(
                            n, SymbolKind::Enum, nl, nc, nb, nb, scope,
                        ));
                    }
                    self.ws();
                    if self.peek() == b':' { self.advance(); self.ws(); let _ = self.parse_ident(); }
                    self.ws();
                    if self.peek() == b'{' { self.skip_to_close_brace(); } else { self.skip_to_semi(); }
                }
                "struct" | "union" => {
                    self.ws();
                    let (name, nl, nc, nb) = self.consume_name();
                    let scope: Vec<String> = self.package.iter().cloned().collect();
                    let k = if kw == "struct" { SymbolKind::Struct } else { SymbolKind::Union };
                    if let Some(n) = name {
                        syms.push(make_symbol(n, k, nl, nc, nb, nb, scope));
                    }
                    self.ws();
                    if self.peek() == b'{' { self.skip_to_close_brace(); } else { self.skip_to_semi(); }
                }
                "typedef" => {
                    self.ws();
                    // typedef Old New;
                    let _ = self.parse_ident();
                    self.ws();
                    if let Some(new_name) = self.parse_ident() {
                        let scope: Vec<String> = self.package.iter().cloned().collect();
                        syms.push(make_symbol(
                            new_name.to_string(), SymbolKind::Type,
                            kw_line, kw_col, kw_byte, kw_byte + new_name.len() as u32, scope,
                        ));
                    }
                    self.skip_to_semi();
                }
                _ => { self.skip_to_semi(); }
            }
        }
    }
    fn peek_word(&self, w: &str) -> bool {
        let rest = &self.src[self.pos..];
        rest.starts_with(w) && {
            let after = rest.as_bytes().get(w.len()).copied().unwrap_or(b' ');
            !(after.is_ascii_alphanumeric() || after == b'_')
        }
    }
    fn advance_word(&mut self) {
        while !self.at_end() {
            let c = self.peek();
            if c.is_ascii_alphanumeric() || c == b'_' { self.advance(); } else { break; }
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
            while self.peek() == b'@' { self.skip_annotation(); self.ws(); }

            // Nested type declarations inside an interface body
            for kw in ["enum", "struct", "union", "typedef"] {
                self.ws();
                if self.peek_word(kw) {
                    let line = self.line; let col = self.col; let byte = self.pos as u32;
                    self.advance_word();
                    self.ws();
                    let (name, nl, nc, nb) = self.consume_name();
                    if let Some(n) = name {
                        let mut scope = pkg_scope.to_vec();
                        scope.push(interface.to_string());
                        let kind = match kw {
                            "enum" => SymbolKind::Enum,
                            "struct" => SymbolKind::Struct,
                            "union" => SymbolKind::Union,
                            _ => SymbolKind::Type,
                        };
                        syms.push(make_symbol(
                            n, kind, nl, nc, nb, nb, scope,
                        ));
                    }
                    let _ = line; let _ = col; let _ = byte;
                    self.ws();
                    // optional ': BaseType' for enums
                    if self.peek() == b':' {
                        self.advance(); self.ws(); let _ = self.parse_ident();
                    }
                    self.ws();
                    if self.peek() == b'{' { self.skip_to_close_brace(); }
                    self.ws();
                    if self.peek() == b';' { self.advance(); }
                    continue;
                }
            }

            // skip `oneway`
            if self.peek_word("oneway") { self.advance_word(); self.ws(); }
            // method-name spotting: same heuristic as AIDL — find IDENT '('
            let mut last_ident: Option<(String, u32, u32, u32)> = None;
            loop {
                self.ws();
                if self.at_end() || self.peek() == b'}' { return; }
                let c = self.peek();
                if c == b';' { self.advance(); break; }
                if c == b'(' {
                    if let Some((n, line, col, byte)) = last_ident.take() {
                        let mut scope = pkg_scope.to_vec();
                        scope.push(interface.to_string());
                        syms.push(make_symbol(
                            n.clone(), SymbolKind::AidlMethod,
                            line, col, byte, byte + n.len() as u32, scope,
                        ));
                    }
                    self.skip_balanced(b'(', b')');
                    // optional `generates ( ... )`
                    self.ws();
                    if self.peek_word("generates") {
                        self.advance_word();
                        self.ws();
                        if self.peek() == b'(' { self.skip_balanced(b'(', b')'); }
                    }
                    continue;
                }
                if c == b'{' { self.skip_to_close_brace(); continue; }
                if c == b'=' {
                    while !self.at_end() && self.peek() != b';' { self.advance(); }
                    continue;
                }
                if c.is_ascii_alphabetic() || c == b'_' {
                    let line = self.line; let col = self.col; let byte = self.pos as u32;
                    if let Some(id) = self.parse_ident() {
                        last_ident = Some((id.to_string(), line, col, byte));
                    }
                } else { self.advance(); }
            }
        }
    }
    fn skip_to_semi(&mut self) {
        while !self.at_end() && self.peek() != b';' { self.advance(); }
        if self.peek() == b';' { self.advance(); }
    }
    fn skip_to_close_brace(&mut self) { self.skip_balanced(b'{', b'}'); }
    fn skip_balanced(&mut self, open: u8, close: u8) {
        while !self.at_end() && self.peek() != open { self.advance(); }
        if self.peek() != open { return; }
        let mut depth: i32; self.advance(); depth = 1;
        while !self.at_end() && depth > 0 {
            let c = self.peek();
            if c == open { self.advance(); depth += 1; }
            else if c == close { self.advance(); depth -= 1; }
            else { self.advance(); }
        }
    }
}

/// Emit synthetic shadow symbols for the C++ bindings the HIDL
/// toolchain generates from `interface IFoo`. All shadows live at
/// the .hal source location and carry `SymbolKind::HidlShadow`, so
/// `scry def BpIFoo` finds the HIDL declaration even though `BpIFoo`
/// only exists as generated C++.
///
/// HIDL prefix scheme (fixed by the toolchain):
///   Bp{I}  — proxy   (client-side)
///   Bn{I}  — native  (server-side stub)
///   Bs{I}  — passthrough wrapper (same-process / direct call)
pub(crate) fn emit_hidl_shadows(
    iface: &str,
    pkg_scope: &[String],
    line: u32,
    col: u32,
    byte: u32,
    syms: &mut Vec<RawSymbol>,
) {
    for name in hidl_shadow_names(iface) {
        syms.push(make_symbol(
            name.clone(),
            SymbolKind::HidlShadow,
            line, col, byte, byte + name.len() as u32,
            pkg_scope.to_vec(),
        ));
    }
}

/// The fixed set of HIDL shadow names for an interface, in stable
/// order. Pinned by a test so a toolchain rename is loud.
pub(crate) fn hidl_shadow_names(iface: &str) -> Vec<String> {
    vec![
        format!("Bp{iface}"),
        format!("Bn{iface}"),
        format!("Bs{iface}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_hidl() {
        let src = br#"
            package android.hardware.foo@1.0;
            import android.hardware.foo@1.0::IBase;
            interface IFoo extends IBase {
                doSomething(int32_t x) generates (string y);
                enum Status : uint8_t { OK = 0, FAIL = 1, };
                struct Bar { int32_t x; };
                oneway notify();
            }
        "#;
        let (syms, refs) = parse(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"IFoo"));
        assert!(names.contains(&"doSomething"));
        assert!(names.contains(&"notify"));
        assert!(names.contains(&"Status"));
        assert!(names.contains(&"Bar"));
        let rnames: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(rnames.iter().any(|n| n.contains("IBase")));
    }

    /// HIDL shadow names are fixed by the toolchain: Bp / Bn / Bs.
    /// Pin the set so a toolchain rename or a refactor of our shadow
    /// scheme can't silently break the cross-language pivot.
    #[test]
    fn hidl_shadow_names_pinned() {
        assert_eq!(hidl_shadow_names("IFoo"), vec!["BpIFoo", "BnIFoo", "BsIFoo"]);
    }

    /// End-to-end: parse a small HIDL file, assert the shadows landed
    /// with `SymbolKind::HidlShadow` at the same location as the
    /// interface declaration.
    #[test]
    fn interface_emits_hidl_shadows() {
        let src = br#"
            package android.hardware.foo@1.0;
            interface IFoo {
                doSomething(int32_t x);
            }
        "#;
        let (syms, _) = parse(src);
        let iface = syms.iter().find(|s| s.name == "IFoo" && s.kind == SymbolKind::AidlInterface)
            .expect("IFoo interface symbol must exist");
        let shadows: Vec<&RawSymbol> = syms.iter()
            .filter(|s| s.kind == SymbolKind::HidlShadow).collect();
        assert_eq!(shadows.len(), 3, "expected exactly 3 HIDL shadows, got {}", shadows.len());
        for s in &shadows {
            assert_eq!(s.line, iface.line, "shadow {} not at iface line", s.name);
        }
        let names: std::collections::HashSet<&str> = shadows.iter()
            .map(|s| s.name.as_str()).collect();
        for must_have in &["BpIFoo", "BnIFoo", "BsIFoo"] {
            assert!(names.contains(must_have), "missing {must_have} in {names:?}");
        }
    }
}
