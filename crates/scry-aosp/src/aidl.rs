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
            self.advance();
            let mut depth: i32 = 1;
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
                        // Cross-language shadow symbols. AIDL `interface
                        // IFoo` produces a fixed family of generated
                        // bindings (`IFoo.Stub`, `IFoo.Stub.Proxy` in
                        // Java; `BpIFoo` / `BnIFoo` in C++; `IFoo` in
                        // Rust). Emitting them as symbols at the .aidl
                        // location means `scry def IFoo.Stub` lands on
                        // the right file instead of returning nothing.
                        // The lang of these shadow records is left as
                        // the source kind (Aidl); callers identify them
                        // by SymbolKind::AidlShadow.
                        emit_aidl_shadows(&n, &scope, nline, ncol, nbyte, syms);
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
        let name = self.parse_ident().map(ToString::to_string);
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

/// Emit synthetic shadow symbols for the cross-language bindings the
/// AIDL toolchain generates from `interface IFoo`. All shadows live at
/// the AIDL source location (line/col/byte offset of the interface
/// name) and carry `SymbolKind::AidlShadow`, so a `scry def IFoo.Stub`
/// query lands on the .aidl file even though `IFoo.Stub` only exists
/// as generated Java code.
///
/// The set of shadows is fixed by the AIDL toolchain conventions:
///
///   Java:  IFoo.Stub        — abstract Binder server stub
///          IFoo.Stub.Proxy  — generated client proxy
///   C++:   BpIFoo           — proxy ("Bp" = Binder Proxy)
///          BnIFoo           — server stub ("Bn" = Binder Native)
///   Rust:  IFoo             — same name as the AIDL interface
///          IFooAsyncServer  — the async-server variant emitted since
///                             AIDL Rust async support landed
///
/// Six shadows per interface; cheap to store and gives us the cross-
/// language pivot scry's design lives on. The names are pinned in the
/// `aidl_shadow_names` test so a future drift in the toolchain (e.g.
/// "BpIFoo" → "Bp_IFoo") is loud at test time.
pub(crate) fn emit_aidl_shadows(
    iface: &str,
    pkg_scope: &[String],
    line: u32,
    col: u32,
    byte: u32,
    syms: &mut Vec<RawSymbol>,
) {
    for name in aidl_shadow_names(iface) {
        syms.push(make_symbol(
            name.clone(),
            SymbolKind::AidlShadow,
            line, col, byte, byte + name.len() as u32,
            pkg_scope.to_vec(),
        ));
    }
}

/// Frozen-version detection. Returns true for any path that lives under
/// AOSP's `aidl_api/` convention — that's where every frozen API surface
/// for an AIDL package lands (`aidl_api/<pkg>/<N>/<file>.aidl` for
/// version N, plus `aidl_api/<pkg>/current/<file>.aidl` for the working
/// copy that becomes the next frozen snapshot). The development source
/// lives under plain `aidl/` and is not flagged.
///
/// Detection is path-substring; the indexer calls it after parse and
/// flips every `AidlInterface` symbol on a frozen file to `AidlFrozen`
/// (via [`apply_frozen_post`]). This lets agents ask "what is the V3
/// surface of IFoo" with `--kind aidl.frozen` while the live development
/// source stays under `--kind aidl.iface`.
pub fn is_frozen_path(path: &str) -> bool {
    path.contains("/aidl_api/") || path.starts_with("aidl_api/")
}

/// Post-process: flip every AidlInterface in `syms` to AidlFrozen.
/// Called by the indexer when [`is_frozen_path`] matched the source
/// file. AidlMethod / AidlParcelable / AidlShadow are intentionally
/// left untouched — only the top-level interface symbol changes kind,
/// matching how Gerrit's "what frozen interfaces exist" view groups by.
pub fn apply_frozen_post(syms: &mut [RawSymbol]) {
    for s in syms.iter_mut() {
        if s.kind == SymbolKind::AidlInterface {
            s.kind = SymbolKind::AidlFrozen;
        }
    }
}

/// The fixed set of generated-binding names for an AIDL interface,
/// in a stable order. Extracted into a free function so tests can
/// pin the exact list without exercising the parser.
pub(crate) fn aidl_shadow_names(iface: &str) -> Vec<String> {
    vec![
        format!("{iface}.Stub"),
        format!("{iface}.Stub.Proxy"),
        format!("Bp{iface}"),
        format!("Bn{iface}"),
        // Rust binding has the same bare name as the AIDL interface;
        // we still emit it as a shadow so `scry def IFoo --kind aidl.shadow`
        // also lists the Rust target side-by-side.
        iface.to_string(),
        format!("{iface}AsyncServer"),
    ]
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

    /// Shadow-name set is fixed by the AIDL toolchain; pin every name
    /// so a future drift in the toolchain (or in our shadow scheme) is
    /// loud at test time. The names listed here are the same ones
    /// `scry def IFoo.Stub` and friends will match against.
    #[test]
    fn aidl_shadow_names_complete() {
        let names = aidl_shadow_names("IBinder");
        assert_eq!(names, vec![
            "IBinder.Stub",
            "IBinder.Stub.Proxy",
            "BpIBinder",
            "BnIBinder",
            "IBinder",
            "IBinderAsyncServer",
        ]);
    }

    /// End-to-end: parse a small AIDL file and assert that the shadow
    /// symbols got emitted at the same line/col as the interface
    /// declaration, with `SymbolKind::AidlShadow` so callers can filter.
    #[test]
    fn interface_emits_shadow_symbols() {
        let src = br#"
            package android.os;
            interface IBinder {
                void transact(in int code, in Parcel data);
            }
        "#;
        let (syms, _) = parse(src);
        let iface = syms.iter().find(|s| s.name == "IBinder" && s.kind == SymbolKind::AidlInterface)
            .expect("IBinder interface symbol must exist");
        let shadows: Vec<&RawSymbol> = syms.iter()
            .filter(|s| s.kind == SymbolKind::AidlShadow).collect();
        assert_eq!(shadows.len(), 6, "expected exactly 6 shadow symbols, got {}", shadows.len());
        // All shadows share the interface's location (we emit them at
        // the same line/col so navigation lands the user on the AIDL).
        for s in &shadows {
            assert_eq!(s.line, iface.line);
            assert_eq!(s.col, iface.col);
        }
        // The fixed set must be present.
        let names: std::collections::HashSet<&str> = shadows.iter()
            .map(|s| s.name.as_str()).collect();
        for must_have in &["IBinder.Stub", "IBinder.Stub.Proxy", "BpIBinder", "BnIBinder"] {
            assert!(names.contains(must_have), "missing shadow {must_have} in {names:?}");
        }
    }

    /// is_frozen_path pins the AOSP `aidl_api/` convention.
    #[test]
    fn frozen_path_detection() {
        // Real frozen versions under aidl_api/<pkg>/<N>/
        assert!(is_frozen_path("hardware/interfaces/foo/aidl/aidl_api/android.hardware.foo/3/android/hardware/foo/IFoo.aidl"));
        assert!(is_frozen_path("aidl_api/android.os/2/android/os/IBinder.aidl"));
        // The current/ working copy that becomes the next freeze.
        assert!(is_frozen_path("hardware/interfaces/foo/aidl/aidl_api/android.hardware.foo/current/android/hardware/foo/IFoo.aidl"));
        // Live development source — not frozen.
        assert!(!is_frozen_path("hardware/interfaces/foo/aidl/android/hardware/foo/IFoo.aidl"));
        assert!(!is_frozen_path("frameworks/base/core/java/android/os/IBinder.aidl"));
        // Adjacent-but-not-matching paths should not false-positive.
        assert!(!is_frozen_path("hardware/notaidl_api/foo.aidl"));
    }

    /// apply_frozen_post promotes AidlInterface → AidlFrozen without
    /// touching methods, parcelables, or shadows.
    #[test]
    fn frozen_post_processing_promotes_only_interface() {
        let src = br#"
            package android.os;
            interface IFoo {
                void doThing();
            }
            parcelable Bar;
        "#;
        let (mut syms, _) = parse(src);
        // Sanity: parser emits AidlInterface, AidlMethod, AidlParcelable, AidlShadow.
        assert!(syms.iter().any(|s| s.kind == SymbolKind::AidlInterface));
        let pre_methods = syms.iter().filter(|s| s.kind == SymbolKind::AidlMethod).count();
        let pre_parcels = syms.iter().filter(|s| s.kind == SymbolKind::AidlParcelable).count();
        let pre_shadows = syms.iter().filter(|s| s.kind == SymbolKind::AidlShadow).count();
        let pre_ifaces  = syms.iter().filter(|s| s.kind == SymbolKind::AidlInterface).count();

        apply_frozen_post(&mut syms);

        // Interface gets promoted; everything else is untouched.
        assert_eq!(syms.iter().filter(|s| s.kind == SymbolKind::AidlInterface).count(), 0,
                   "AidlInterface must be promoted to AidlFrozen");
        assert_eq!(syms.iter().filter(|s| s.kind == SymbolKind::AidlFrozen).count(), pre_ifaces,
                   "every prior AidlInterface should now be AidlFrozen");
        assert_eq!(syms.iter().filter(|s| s.kind == SymbolKind::AidlMethod).count(), pre_methods);
        assert_eq!(syms.iter().filter(|s| s.kind == SymbolKind::AidlParcelable).count(), pre_parcels);
        assert_eq!(syms.iter().filter(|s| s.kind == SymbolKind::AidlShadow).count(), pre_shadows);
    }
}
