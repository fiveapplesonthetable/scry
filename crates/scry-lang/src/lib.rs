//! scry-lang: per-language symbol extraction via tree-sitter.

#![forbid(unsafe_code)]

use anyhow::Result;
use scry_store::{RefKind, SymbolKind};
use scry_walker::FileKind;
use std::cell::RefCell;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor};

#[derive(Debug, Clone)]
pub struct RawSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line: u32,
    pub col: u32,
    pub scope_path: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RawRef {
    pub name: String,
    pub kind: RefKind,
    pub byte_start: u32,
    pub byte_end: u32,
    pub line: u32,
    pub col: u32,
    pub scope_path: Vec<String>,
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
    /// Filename of the file currently being parsed on this worker thread.
    /// Set by `with_current_file` (called from the indexing pipeline) so
    /// extract_with / extract_refs_with can emit USEFUL diagnostic logs
    /// when tree-sitter aborts a parse — "tree-sitter timed out" with no
    /// path is useless for investigation, which is the whole point of
    /// surfacing it.
    static CURRENT_FILE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Scope a string as the "current file" while `f` runs on this thread.
/// Used so the tree-sitter parse layer can name the file in failure logs
/// without changing the FormatParser trait signature.
pub fn with_current_file<R>(name: String, f: impl FnOnce() -> R) -> R {
    CURRENT_FILE.with(|c| *c.borrow_mut() = Some(name));
    let r = f();
    CURRENT_FILE.with(|c| *c.borrow_mut() = None);
    r
}

fn current_file_name() -> String {
    CURRENT_FILE.with(|c| c.borrow().clone()).unwrap_or_else(|| "<unknown>".into())
}

/// Per-file tree-sitter parse timeout (microseconds). Set via env var
/// SCRY_PARSE_TIMEOUT_MS at process start; 0 = unlimited. Read once and
/// cached so the per-parse setup is a single load.
///
/// Why this exists: tree-sitter grammars can transiently allocate gigabytes
/// on adversarial inputs (e.g. the ctags Kotlin parser test fixtures). A
/// timeout lets us cap that — when it fires, parse() returns None and we
/// record a parse failure for that file instead of OOM-killing the whole
/// indexer.
fn parse_timeout_micros() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("SCRY_PARSE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|ms| ms.saturating_mul(1000))
            .unwrap_or(0)
    })
}

enum ParseOutcome {
    Tree(tree_sitter::Tree),
    TimedOut,
    Failed,
}

/// Parse `source` with a wall-clock budget enforced via a progress
/// callback. Returns the tree (or a failure reason) plus elapsed millis.
///
/// We use `parse_with_options` + `progress_callback` rather than the
/// deprecated `set_timeout_micros`. The latter relies on tree-sitter
/// internally checking the clock at certain decision points; for some
/// adversarial inputs it never does, and the parse runs forever (we
/// observed >1h stalls on real AOSP Java files). The progress callback
/// is invoked at every parser step, so the abort is guaranteed to be
/// observed within microseconds of the deadline.
fn parse_with_timeout(parser: &mut Parser, source: &[u8]) -> (ParseOutcome, u128) {
    parse_with_explicit_timeout(parser, source, parse_timeout_micros())
}

fn parse_with_explicit_timeout(
    parser: &mut Parser,
    source: &[u8],
    timeout_micros: u64,
) -> (ParseOutcome, u128) {
    let start = std::time::Instant::now();
    let len = source.len();
    let mut read = |i: usize, _: tree_sitter::Point| -> &[u8] {
        if i < len { &source[i..] } else { &[] }
    };

    if timeout_micros == 0 {
        let tree = parser.parse_with_options(&mut read, None, None);
        let elapsed = start.elapsed().as_millis();
        return match tree {
            Some(t) => (ParseOutcome::Tree(t), elapsed),
            None => (ParseOutcome::Failed, elapsed),
        };
    }

    let timeout = std::time::Duration::from_micros(timeout_micros);
    let mut timed_out = false;
    let tree = {
        let mut cb = |_: &tree_sitter::ParseState| -> bool {
            if start.elapsed() > timeout {
                timed_out = true;
                true
            } else {
                false
            }
        };
        let opts = tree_sitter::ParseOptions::new().progress_callback(&mut cb);
        parser.parse_with_options(&mut read, None, Some(opts))
    };
    let elapsed = start.elapsed().as_millis();
    match (tree, timed_out) {
        (Some(t), _) => (ParseOutcome::Tree(t), elapsed),
        (None, true) => (ParseOutcome::TimedOut, elapsed),
        (None, false) => (ParseOutcome::Failed, elapsed),
    }
}

pub fn extract(kind: FileKind, source: &[u8]) -> Result<Vec<RawSymbol>> {
    use FileKind::*;
    let spec = match kind {
        C => c_spec(),
        Cpp | HeaderCpp => cpp_spec(),
        Header => cpp_spec(),
        Java => java_spec(),
        Kotlin => kotlin_spec(),
        Rust => rust_spec(),
        Go => go_spec(),
        Python => python_spec(),
        Bash => bash_spec(),
        TypeScript => typescript_spec(),
        Proto => proto_spec(),
        Html => html_spec(),
        Css => css_spec(),
        Scss => scss_spec(),
        Markdown => markdown_spec(),
        _ => return Ok(Vec::new()),
    };
    let mut syms = extract_with(spec, source)?;
    if kind == Kotlin {
        inject_anonymous_companions(source, &mut syms);
    }
    if kind == Java {
        inject_jni_shadows(source, &mut syms);
    }
    Ok(syms)
}

/// JNI shadow injector. Walks the Java AST and for every method
/// declaration carrying the `native` modifier, emits a synthetic
/// `Function` symbol named after the JNI mangling rule the JVM uses
/// when `RegisterNatives` isn't called explicitly:
///
/// `Java_<package_with_dots_to_underscores>_<class>_<method>`
///
/// Identifiers with literal underscores get `_1` in place of `_`
/// (JNI's standard escape); `$` becomes `_00024`. Non-ASCII isn't
/// emitted (AOSP names are all ASCII; an inner class containing
/// non-ASCII would simply not produce a shadow rather than emit
/// an incorrect one).
///
/// The shadow gets [`SymbolKind::JniBinding`], its own location at
/// the Java method's site (so navigation lands on the declaration),
/// and the same scope_path as the method. A C/C++ side that defines
/// `Java_foo_Bar_baz(JNIEnv*, jobject, ...)` will collide on name —
/// both records remain in the index; the Java shadow surfaces when
/// the C++ side is missing or shared across many .cpp files.
fn inject_jni_shadows(source: &[u8], syms: &mut Vec<RawSymbol>) {
    PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        if parser.set_language(java_spec().language).is_err() { return; }
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return,
        };
        let mut collected = Vec::new();
        visit_native_methods(tree.root_node(), source, &mut collected);
        syms.extend(collected);
    });
}

fn visit_native_methods(node: tree_sitter::Node, source: &[u8], out: &mut Vec<RawSymbol>) {
    if node.kind() == "method_declaration" && method_has_native_modifier(node) {
        let method_name = match node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok()) {
            Some(s) => s.to_string(),
            None => return,
        };
        // Java's package_declaration is a SIBLING of class_declaration at
        // the program root, so compute_scope (parent walk) never sees it.
        // Find the package text by walking up to the file root then
        // scanning siblings for `package_declaration`.
        let scope = compute_scope(
            node, source,
            java_spec().scope_node_kinds,
            java_spec().package_node_kind,
        );
        if scope.is_empty() { return; }  // need at least an enclosing class
        let class = scope[scope.len() - 1].clone();
        let pkg = java_package_text(node, source).unwrap_or_default();
        if pkg.is_empty() { return; }
        let mangled = jni_mangle(&pkg, &class, &method_name);
        let start = node.start_position();
        let mut full_scope = vec![pkg];
        full_scope.extend(scope);
        out.push(RawSymbol {
            name: mangled,
            kind: SymbolKind::JniBinding,
            line: (start.row + 1) as u32,
            col: (start.column + 1) as u32,
            byte_start: node.start_byte() as u32,
            byte_end:   node.end_byte()   as u32,
            scope_path: full_scope,
        });
    }
    let mut w = node.walk();
    for c in node.named_children(&mut w) {
        visit_native_methods(c, source, out);
    }
}

/// Find the package text for a Java AST node by walking up to the
/// `program` root and scanning its children for a `package_declaration`.
/// Returns the verbatim text of the package's identifier (e.g.
/// "android.os" or "com.foo"), or None if the file has no package.
fn java_package_text(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cur = node;
    while let Some(p) = cur.parent() { cur = p; }
    let root = cur;
    let mut w = root.walk();
    for c in root.named_children(&mut w) {
        if c.kind() != "package_declaration" { continue; }
        // The package identifier is the only named child (a scoped or
        // bare identifier). Try the "name" field first, fall back to
        // the first named child.
        let ident = match c.child_by_field_name("name") {
            Some(n) => n,
            None => {
                let mut iw = c.walk();
                let first = c.named_children(&mut iw).next();
                match first {
                    Some(n) => n,
                    None => continue,
                }
            }
        };
        return ident.utf8_text(source).ok().map(String::from);
    }
    None
}

fn method_has_native_modifier(method: tree_sitter::Node) -> bool {
    let mut w = method.walk();
    for c in method.named_children(&mut w) {
        if c.kind() != "modifiers" { continue; }
        // tree-sitter-java exposes `native` as a named child of
        // `modifiers` in current versions. Older grammar versions
        // model it as an anonymous keyword token. Walk ALL children
        // (named + anonymous via `child(i)`) so we catch either shape.
        for i in 0..c.child_count() {
            if let Some(kw) = c.child(i) {
                if kw.kind() == "native" { return true; }
            }
        }
    }
    false
}

/// JNI mangling for the default (non-RegisterNatives) lookup path.
/// Spec: <https://docs.oracle.com/en/java/javase/21/docs/specs/jni/design.html#resolving-native-method-names>.
/// We implement the ASCII-only subset that covers ~all AOSP names:
///   `.`  in package → `_`
///   `_`  in any identifier → `_1`
///   `$`  in inner class    → `_00024`
/// Non-ASCII characters are passed through unchanged; agents wanting
/// strict spec compliance can refine later.
pub fn jni_mangle(pkg: &str, class: &str, method: &str) -> String {
    let mut out = String::with_capacity(8 + pkg.len() + class.len() + method.len());
    out.push_str("Java_");
    push_jni_escaped(&mut out, pkg, /*pkg=*/ true);
    out.push('_');
    push_jni_escaped(&mut out, class, /*pkg=*/ false);
    out.push('_');
    push_jni_escaped(&mut out, method, /*pkg=*/ false);
    out
}

fn push_jni_escaped(out: &mut String, s: &str, is_pkg: bool) {
    for c in s.chars() {
        match c {
            '.' if is_pkg => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            '$' => out.push_str("_00024"),
            _ => out.push(c),
        }
    }
}

/// Anonymous Kotlin `companion object { ... }` lacks an identifier
/// child, so the symbol query can't capture it as a Class. The members'
/// scope is already fixed up by [`scope_label_for`] (which synthesizes
/// "Companion"), but agents asking `def Companion` or browsing an
/// outline expect a Class entry too. This walker injects one synthetic
/// Class symbol named "Companion" per anonymous companion_object, at
/// the companion's start position.
fn inject_anonymous_companions(source: &[u8], syms: &mut Vec<RawSymbol>) {
    PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        if parser.set_language(kotlin_spec().language).is_err() { return; }
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return,
        };
        visit_companions(tree.root_node(), source, syms);
    });
}

fn visit_companions(node: tree_sitter::Node, source: &[u8], syms: &mut Vec<RawSymbol>) {
    if node.kind() == "companion_object" {
        let mut w = node.walk();
        let has_name = node.named_children(&mut w).any(|c| c.kind() == "identifier");
        if !has_name {
            let start = node.start_position();
            let scope = compute_scope(
                node,
                source,
                kotlin_spec().scope_node_kinds,
                kotlin_spec().package_node_kind,
            );
            syms.push(RawSymbol {
                name: "Companion".to_string(),
                kind: SymbolKind::Class,
                line: (start.row + 1) as u32,
                col: (start.column + 1) as u32,
                byte_start: node.start_byte() as u32,
                byte_end:   node.end_byte()   as u32,
                scope_path: scope,
            });
        }
    }
    let mut w = node.walk();
    for c in node.named_children(&mut w) {
        visit_companions(c, source, syms);
    }
}

pub fn extract_refs(kind: FileKind, source: &[u8]) -> Result<Vec<RawRef>> {
    use FileKind::*;
    let (lang_spec, refs) = match kind {
        C => (c_spec(), c_refs_spec()),
        Cpp | HeaderCpp | Header => (cpp_spec(), cpp_refs_spec()),
        Java => (java_spec(), java_refs_spec()),
        Kotlin => (kotlin_spec(), kotlin_refs_spec()),
        Rust => (rust_spec(), rust_refs_spec()),
        Go => (go_spec(), go_refs_spec()),
        Python => (python_spec(), python_refs_spec()),
        _ => return Ok(Vec::new()),
    };
    extract_refs_with(lang_spec, refs, source)
}

struct RefSpec {
    query: &'static Query,
    capture_kinds: &'static [(&'static str, RefKind)],
}

fn extract_refs_with(
    lang: &'static LangSpec,
    refs: &'static RefSpec,
    source: &[u8],
) -> Result<Vec<RawRef>> {
    PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        parser.set_language(lang.language)?;
        let tree = match parse_with_timeout(&mut parser, source) {
            (ParseOutcome::Tree(t), _) => t,
            (ParseOutcome::TimedOut, elapsed_ms) => {
                eprintln!(
                    "[ts-TIMEOUT] {} ({} bytes) — progress callback aborted after {} ms (refs query)",
                    current_file_name(), source.len(), elapsed_ms,
                );
                return Ok(Vec::new());
            },
            (ParseOutcome::Failed, elapsed_ms) => {
                eprintln!(
                    "[ts-ABORT] {} ({} bytes) — tree-sitter parse returned None after {} ms (refs query)",
                    current_file_name(), source.len(), elapsed_ms,
                );
                return Ok(Vec::new());
            },
        };
        let mut out: Vec<RawRef> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(refs.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_name = &refs.query.capture_names()[cap.index as usize];
                if let Some((_, k)) = refs.capture_kinds.iter().find(|(n, _)| n == cap_name) {
                    let node = cap.node;
                    let name = match node.utf8_text(source) {
                        Ok(s) if !s.is_empty() => s.to_string(),
                        _ => continue,
                    };
                    let pos = node.start_position();
                    out.push(RawRef {
                        name,
                        kind: *k,
                        byte_start: node.start_byte() as u32,
                        byte_end: node.end_byte() as u32,
                        line: (pos.row as u32).saturating_add(1),
                        col: (pos.column as u32).saturating_add(1),
                        scope_path: compute_scope(
                            node,
                            source,
                            lang.scope_node_kinds,
                            lang.package_node_kind,
                        ),
                    });
                }
            }
        }
        Ok(out)
    })
}


struct LangSpec {
    language: &'static Language,
    query: &'static Query,
    capture_kinds: &'static [(&'static str, SymbolKind)],
    name_capture: &'static str,
    scope_node_kinds: &'static [&'static str],
    package_node_kind: Option<&'static str>,
}

fn extract_with(spec: &'static LangSpec, source: &[u8]) -> Result<Vec<RawSymbol>> {
    PARSER.with(|cell| {
        let mut parser = cell.borrow_mut();
        parser.set_language(spec.language)?;
        let tree = match parse_with_timeout(&mut parser, source) {
            (ParseOutcome::Tree(t), _) => t,
            (ParseOutcome::TimedOut, elapsed_ms) => {
                eprintln!(
                    "[ts-TIMEOUT] {} ({} bytes) — progress callback aborted after {} ms (symbols query)",
                    current_file_name(), source.len(), elapsed_ms,
                );
                return Ok(Vec::new());
            },
            (ParseOutcome::Failed, elapsed_ms) => {
                eprintln!(
                    "[ts-ABORT] {} ({} bytes) — tree-sitter parse returned None after {} ms (symbols query)",
                    current_file_name(), source.len(), elapsed_ms,
                );
                return Ok(Vec::new());
            },
        };
        let mut out: Vec<RawSymbol> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(spec.query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            let mut name_node: Option<tree_sitter::Node> = None;
            let mut kind: Option<SymbolKind> = None;
            for cap in m.captures {
                let cap_name = &spec.query.capture_names()[cap.index as usize];
                if *cap_name == spec.name_capture {
                    name_node = Some(cap.node);
                } else if let Some((_, k)) = spec
                    .capture_kinds
                    .iter()
                    .find(|(n, _)| *n == *cap_name)
                {
                    kind = Some(*k);
                }
            }
            let (Some(name_node), Some(kind)) = (name_node, kind) else { continue };
            // C++ out-of-line method definitions (`Foo::bar() { ... }`)
            // capture the whole qualified_identifier as @name. Drill in
            // to extract the bare name + harvest the qualifiers into
            // scope_path so a search for `def bar` finds it AND its
            // scope reads `[Foo]` instead of the symbol being literally
            // named `Foo::bar`.
            let (name_node, qualified_prefix): (tree_sitter::Node, Vec<String>) =
                if name_node.kind() == "qualified_identifier" {
                    drill_qualified_identifier(name_node, source)
                } else {
                    (name_node, Vec::new())
                };
            let name = match name_node.utf8_text(source) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            let mut scope_path = compute_scope(
                name_node,
                source,
                spec.scope_node_kinds,
                spec.package_node_kind,
            );
            // Prepend any qualifiers we drilled out of the
            // qualified_identifier (e.g. ["BBinder"] for BBinder::transact)
            // AFTER the ancestor-derived scope (the enclosing namespace),
            // so a method on android::BBinder reads as
            // ["android", "BBinder"] → "android::BBinder::transact".
            if !qualified_prefix.is_empty() {
                scope_path.extend(qualified_prefix);
            }
            // Receiver-typed declaration (Kotlin extension fn / property):
            // - extension function name's parent IS function_declaration
            // - extension property name's parent is variable_declaration,
            //   grandparent is property_declaration. The receiver
            //   user_type lives on the property_declaration directly.
            // Prepend its identifier to scope_path so `def shouted` shows
            // scope "String", and outline groups extensions by receiver.
            let decl_node = if let Some(parent) = name_node.parent() {
                match parent.kind() {
                    "function_declaration" => Some((parent, name_node)),
                    "variable_declaration" => parent.parent()
                        .filter(|gp| gp.kind() == "property_declaration")
                        .map(|gp| (gp, parent)),
                    _ => None,
                }
            } else { None };
            if let Some((decl, boundary)) = decl_node {
                if let Some(recv) = kotlin_receiver_for_decl(decl, boundary, source) {
                    scope_path.insert(0, recv);
                }
            }
            let start = name_node.start_position();
            out.push(RawSymbol {
                name,
                kind,
                byte_start: name_node.start_byte() as u32,
                byte_end: name_node.end_byte() as u32,
                line: (start.row as u32).saturating_add(1),
                col: (start.column as u32).saturating_add(1),
                scope_path,
            });
        }
        Ok(out)
    })
}

/// Kotlin extension function / property receiver lookup.
///
/// For a function or property declaration that defines an extension
/// (`fun String.shouted()` or `val String.firstChar`), the AST has a
/// `user_type` node positioned BEFORE the identifier-name field. Return
/// the receiver identifier text if present, or None when this is a
/// plain function/property.
///
/// We can't express "any user_type before the name field" in
/// tree-sitter query syntax cleanly (return-type user_type would also
/// match), so this runs as a post-process on each function_decl /
/// property_decl match.
/// Split a C++ `qualified_identifier` AST node like `android::BBinder::transact`
/// into the bare-name node ("transact") and the scope prefix
/// (["android", "BBinder"]). Handles nested qualified_identifier when
/// the C++ grammar wraps deeper qualifications. Returns the input node
/// itself + an empty prefix if the node has no named children (defensive).
fn drill_qualified_identifier<'a>(
    qid: tree_sitter::Node<'a>,
    src: &[u8],
) -> (tree_sitter::Node<'a>, Vec<String>) {
    let mut cursor = qid.walk();
    let children: Vec<tree_sitter::Node> = qid.named_children(&mut cursor).collect();
    if children.is_empty() {
        return (qid, Vec::new());
    }
    let last = *children.last().unwrap();
    let mut prefix: Vec<String> = Vec::new();
    for p in &children[..children.len() - 1] {
        if let Ok(s) = p.utf8_text(src) {
            // A nested qualified_identifier in the prefix position
            // serializes as "A::B" etc. — splitting still gives the
            // canonical scope segments without us having to recurse
            // here.
            for part in s.split("::") {
                if !part.is_empty() {
                    prefix.push(part.to_string());
                }
            }
        }
    }
    // If the last child is itself a qualified_identifier (rare; happens
    // with template-instantiated names), recurse so we always return an
    // atomic identifier for `name`.
    if last.kind() == "qualified_identifier" {
        let (inner_name, inner_prefix) = drill_qualified_identifier(last, src);
        prefix.extend(inner_prefix);
        return (inner_name, prefix);
    }
    (last, prefix)
}

fn kotlin_receiver_for_decl(
    decl: tree_sitter::Node,
    name_node: tree_sitter::Node,
    src: &[u8],
) -> Option<String> {
    let mut cursor = decl.walk();
    let name_start = name_node.start_byte();
    let mut found_receiver: Option<String> = None;
    for child in decl.named_children(&mut cursor) {
        if child.start_byte() >= name_start { break; }
        if child.kind() == "user_type" {
            // Pull the first identifier text inside the receiver's user_type.
            let mut cw = child.walk();
            for grand in child.named_children(&mut cw) {
                if grand.kind() == "identifier" {
                    if let Ok(s) = grand.utf8_text(src) {
                        found_receiver = Some(s.to_string());
                        break;
                    }
                }
            }
            break;
        }
    }
    found_receiver
}

fn compute_scope(
    node: tree_sitter::Node,
    src: &[u8],
    scope_kinds: &[&str],
    package_kind: Option<&str>,
) -> Vec<String> {
    let mut scope: Vec<String> = Vec::new();
    let mut cur = node.parent();
    while let Some(p) = cur {
        let k = p.kind();
        if scope_kinds.contains(&k) || Some(k) == package_kind {
            if let Some(label) = scope_label_for(p, node, src) {
                scope.push(label);
            }
        }
        cur = p.parent();
    }
    scope.reverse();
    scope
}

/// Resolve a scope-bearing parent to the string that should appear on
/// the scope_path. Most grammars expose a "name" field on the scope
/// node and we lift its text verbatim. Two special cases:
///
/// 1. Kotlin `companion_object` carries the name as a plain `identifier`
///    named child (no "name" field). When the identifier is absent
///    (anonymous companion — by far the common case), synthesize the
///    name "Companion" so members get scoped consistently with the
///    JVM convention (`Holder.Companion.foo()`).
///
/// 2. If the matched child IS the queried `node` itself, return None —
///    a declaration is not its own scope.
fn scope_label_for(
    parent: tree_sitter::Node,
    node: tree_sitter::Node,
    src: &[u8],
) -> Option<String> {
    if let Some(n) = parent.child_by_field_name("name") {
        if n.id() == node.id() { return None; }
        return n.utf8_text(src).ok().map(String::from);
    }
    if parent.kind() == "companion_object" {
        // Named companion: `companion object Builder { ... }` exposes the
        // name as the first `identifier` named child.
        let mut w = parent.walk();
        for c in parent.named_children(&mut w) {
            if c.kind() == "identifier" && c.id() != node.id() {
                return c.utf8_text(src).ok().map(String::from);
            }
        }
        // Anonymous companion: synthesize the JVM-default `Companion`.
        return Some("Companion".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

fn java_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_java::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (class_declaration name: (identifier) @name) @def.class
                (interface_declaration name: (identifier) @name) @def.interface
                (enum_declaration name: (identifier) @name) @def.enum
                (annotation_type_declaration name: (identifier) @name) @def.annotation
                (record_declaration name: (identifier) @name) @def.class
                (method_declaration name: (identifier) @name) @def.method
                (constructor_declaration name: (identifier) @name) @def.ctor
                (field_declaration declarator: (variable_declarator name: (identifier) @name)) @def.field
                "#,
            )
            .expect("java query")
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.class", SymbolKind::Class),
                ("def.interface", SymbolKind::Interface),
                ("def.enum", SymbolKind::Enum),
                ("def.annotation", SymbolKind::Annotation),
                ("def.method", SymbolKind::Method),
                ("def.ctor", SymbolKind::Constructor),
                ("def.field", SymbolKind::Field),
            ],
            name_capture: "name",
            scope_node_kinds: &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "annotation_type_declaration",
                "record_declaration",
            ],
            package_node_kind: Some("package_declaration"),
        }
    })
}

// ---------------------------------------------------------------------------
// Kotlin
// ---------------------------------------------------------------------------

fn kotlin_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_kotlin_ng::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            // tree-sitter-kotlin-NG uses `identifier` for class/function/property
            // names (NOT `type_identifier` / `simple_identifier` — those are
            // the older tree-sitter-kotlin grammar). For extension functions
            // and properties, the receiver is wrapped in a `user_type` node,
            // so a direct `(function_declaration (identifier) @name)` query
            // captures the function NAME identifier (the receiver's
            // identifier is nested inside user_type, not a direct child).
            Query::new(
                lang,
                r#"
                (class_declaration (identifier) @name) @def.class
                (object_declaration (identifier) @name) @def.class
                (companion_object (identifier) @name) @def.class
                (function_declaration (identifier) @name) @def.function
                (property_declaration (variable_declaration (identifier) @name)) @def.field
                (type_alias (identifier) @name) @def.type
                (enum_entry (identifier) @name) @def.variant
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(source_file) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.class", SymbolKind::Class),
                ("def.function", SymbolKind::Function),
                ("def.field", SymbolKind::Field),
                ("def.type", SymbolKind::Type),
                ("def.variant", SymbolKind::EnumVariant),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &[
                "class_declaration",
                "object_declaration",
                "companion_object",
                "function_declaration",
            ],
            package_node_kind: Some("package_header"),
        }
    })
}

// ---------------------------------------------------------------------------
// C
// ---------------------------------------------------------------------------

fn c_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_c::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (function_definition
                  declarator: (function_declarator
                    declarator: (identifier) @name)) @def.function
                (struct_specifier name: (type_identifier) @name) @def.struct
                (union_specifier name: (type_identifier) @name) @def.union
                (enum_specifier name: (type_identifier) @name) @def.enum
                (type_definition declarator: (type_identifier) @name) @def.type
                (preproc_def name: (identifier) @name) @def.macro
                (preproc_function_def name: (identifier) @name) @def.macro
                "#,
            )
            .expect("c query")
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.function", SymbolKind::Function),
                ("def.struct", SymbolKind::Struct),
                ("def.union", SymbolKind::Union),
                ("def.enum", SymbolKind::Enum),
                ("def.type", SymbolKind::Type),
                ("def.macro", SymbolKind::Macro),
            ],
            name_capture: "name",
            scope_node_kinds: &["struct_specifier", "union_specifier", "enum_specifier"],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// C++
// ---------------------------------------------------------------------------

fn cpp_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_cpp::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (function_definition
                  declarator: (function_declarator
                    declarator: (identifier) @name)) @def.function
                (function_definition
                  declarator: (function_declarator
                    declarator: (field_identifier) @name)) @def.method
                (function_definition
                  declarator: (function_declarator
                    declarator: (qualified_identifier) @name)) @def.method
                (class_specifier name: (type_identifier) @name) @def.class
                (struct_specifier name: (type_identifier) @name) @def.struct
                (union_specifier name: (type_identifier) @name) @def.union
                (enum_specifier name: (type_identifier) @name) @def.enum
                (namespace_definition name: (namespace_identifier) @name) @def.namespace
                (type_definition declarator: (type_identifier) @name) @def.type
                (alias_declaration name: (type_identifier) @name) @def.type
                (preproc_def name: (identifier) @name) @def.macro
                (preproc_function_def name: (identifier) @name) @def.macro
                "#,
            )
            .expect("cpp query")
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.function", SymbolKind::Function),
                ("def.method", SymbolKind::Method),
                ("def.class", SymbolKind::Class),
                ("def.struct", SymbolKind::Struct),
                ("def.union", SymbolKind::Union),
                ("def.enum", SymbolKind::Enum),
                ("def.namespace", SymbolKind::Namespace),
                ("def.type", SymbolKind::Type),
                ("def.macro", SymbolKind::Macro),
            ],
            name_capture: "name",
            scope_node_kinds: &[
                "class_specifier",
                "struct_specifier",
                "union_specifier",
                "enum_specifier",
                "namespace_definition",
            ],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn rust_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_rust::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (function_item name: (identifier) @name) @def.function
                (struct_item name: (type_identifier) @name) @def.struct
                (enum_item name: (type_identifier) @name) @def.enum
                (trait_item name: (type_identifier) @name) @def.trait
                (impl_item type: (type_identifier) @name) @def.impl
                (mod_item name: (identifier) @name) @def.module
                (type_item name: (type_identifier) @name) @def.type
                (const_item name: (identifier) @name) @def.const
                (static_item name: (identifier) @name) @def.const
                (macro_definition name: (identifier) @name) @def.macro
                "#,
            )
            .expect("rust query")
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.function", SymbolKind::Function),
                ("def.struct", SymbolKind::Struct),
                ("def.enum", SymbolKind::Enum),
                ("def.trait", SymbolKind::Trait),
                ("def.impl", SymbolKind::Other),
                ("def.module", SymbolKind::Module),
                ("def.type", SymbolKind::Type),
                ("def.const", SymbolKind::Constant),
                ("def.macro", SymbolKind::Macro),
            ],
            name_capture: "name",
            scope_node_kinds: &["mod_item", "impl_item", "trait_item", "struct_item", "enum_item"],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

fn go_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_go::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (function_declaration name: (identifier) @name) @def.function
                (method_declaration name: (field_identifier) @name) @def.method
                (type_declaration (type_spec name: (type_identifier) @name)) @def.type
                (const_declaration (const_spec name: (identifier) @name)) @def.const
                (var_declaration (var_spec name: (identifier) @name)) @def.var
                "#,
            )
            .expect("go query")
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.function", SymbolKind::Function),
                ("def.method", SymbolKind::Method),
                ("def.type", SymbolKind::Type),
                ("def.const", SymbolKind::Constant),
                ("def.var", SymbolKind::Variable),
            ],
            name_capture: "name",
            scope_node_kinds: &[],
            package_node_kind: Some("package_clause"),
        }
    })
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn python_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_python::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (function_definition name: (identifier) @name) @def.function
                (class_definition name: (identifier) @name) @def.class
                "#,
            )
            .expect("python query")
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.function", SymbolKind::Function),
                ("def.class", SymbolKind::Class),
            ],
            name_capture: "name",
            scope_node_kinds: &["class_definition", "function_definition"],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// Bash — AOSP build scripts (envsetup.sh, soong_ui.bash, etc.).
//
// Coverage is deliberately tight: function definitions + variable
// assignments at command-level. The high-value targets are the
// `lunch`, `mm`, `mmm`, `croot`, `cgrep` family of build commands
// users actually invoke; these are all just bash `name() { ... }`
// declarations in envsetup.sh.
// ---------------------------------------------------------------------------

fn bash_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_bash::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (function_definition name: (word) @name) @def.function
                (variable_assignment name: (variable_name) @name) @def.var
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(program) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.function", SymbolKind::Function),
                ("def.var", SymbolKind::Variable),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            // No nesting scopes: bash function definitions inside other
            // functions are exceedingly rare in AOSP scripts and would
            // just produce noise. A flat name-list is the right shape.
            scope_node_kinds: &[],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// TypeScript / TSX — perfetto trace_viewer UI, general TS coverage.
//
// tree-sitter-typescript ships two grammars: plain TS and TSX. We use
// the TS grammar (handles .ts well); .tsx files also parse acceptably
// with TS in practice, just without JSX-specific node kinds — fine for
// scry's identifier-capture purpose.
// ---------------------------------------------------------------------------

fn typescript_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (class_declaration name: (type_identifier) @name) @def.class
                (interface_declaration name: (type_identifier) @name) @def.interface
                (function_declaration name: (identifier) @name) @def.function
                (method_definition name: (property_identifier) @name) @def.method
                (enum_declaration name: (identifier) @name) @def.enum
                (type_alias_declaration name: (type_identifier) @name) @def.type
                (variable_declarator name: (identifier) @name) @def.var
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(program) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.class", SymbolKind::Class),
                ("def.interface", SymbolKind::Interface),
                ("def.function", SymbolKind::Function),
                ("def.method", SymbolKind::Method),
                ("def.enum", SymbolKind::Enum),
                ("def.type", SymbolKind::Type),
                ("def.var", SymbolKind::Variable),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &[
                "class_declaration",
                "interface_declaration",
                "function_declaration",
                "method_definition",
                "enum_declaration",
            ],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// Proto — proto2 + proto3 IDL files (perfetto, gRPC, AOSP trace_processor).
//
// tree-sitter-proto handles both proto2 and proto3 syntax in one grammar.
// Captures messages, enums, services, and rpc methods as their own kinds
// (ProtoMessage / ProtoEnum / ProtoService) so agents can filter
// `--kind proto.msg`.
// ---------------------------------------------------------------------------

fn proto_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_proto::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (message (message_name (identifier) @name)) @def.message
                (enum (enum_name (identifier) @name)) @def.enum
                (service (service_name (identifier) @name)) @def.service
                (rpc (rpc_name (identifier) @name)) @def.method
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(source_file) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.message", SymbolKind::ProtoMessage),
                ("def.enum", SymbolKind::ProtoEnum),
                ("def.service", SymbolKind::ProtoService),
                ("def.method", SymbolKind::Method),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &["message", "service", "enum"],
            package_node_kind: Some("package"),
        }
    })
}

// ---------------------------------------------------------------------------
// HTML — perfetto UI templates + general HTML.
//
// Captures elements with `id` and `name` attributes as XmlId symbols
// (reusing the existing AOSP XmlId kind — same intent: "thing JS code
// will reference by name"). Tag names aren't captured (too noisy on a
// large UI). Targeting the cross-reference value: `getElementById("x")`
// in TS should resolve to id="x" in an .html template.
// ---------------------------------------------------------------------------

fn html_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_html::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (attribute
                  (quoted_attribute_value (attribute_value) @name)) @def.xmlid
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(document) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.xmlid", SymbolKind::XmlId),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &[],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// CSS — class selectors, ID selectors, and @keyframes / @font-face names.
//
// The high-value captures for a code-search tool are the things JS / TS
// code references back into: `.my-class` → DOM toggleClass, `#main` →
// querySelector. Pseudo-classes and descendant combinators aren't
// indexed (no agent question shaped like "where is :hover defined").
// ---------------------------------------------------------------------------

fn css_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_css::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (class_selector (class_name) @name) @def.class
                (id_selector (id_name) @name) @def.xmlid
                (keyframes_statement (keyframes_name) @name) @def.type
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(stylesheet) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.class", SymbolKind::Class),
                ("def.xmlid", SymbolKind::XmlId),
                ("def.type", SymbolKind::Type),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &[],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// SCSS — perfetto UI primary stylesheet format. Adds @mixin and
// @function captures on top of the CSS surface; the rest reuses
// css-shape patterns (tree-sitter-scss is a superset of CSS).
// ---------------------------------------------------------------------------

fn scss_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        // tree-sitter-scss exposes `language()` (a function) rather than
        // the newer LANGUAGE const — the grammar predates the convention.
        let lang = LANG.get_or_init(tree_sitter_scss::language);
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (class_selector (class_name) @name) @def.class
                (id_selector (id_name) @name) @def.xmlid
                (mixin_statement (identifier) @name) @def.function
                (function_statement (identifier) @name) @def.function
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(stylesheet) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.class", SymbolKind::Class),
                ("def.xmlid", SymbolKind::XmlId),
                ("def.function", SymbolKind::Function),
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &[],
            package_node_kind: None,
        }
    })
}

// ---------------------------------------------------------------------------
// Markdown — heading-level navigation for the docs-heavy case
// (scry's own docs/, perfetto's docs/, AOSP design notes).
//
// Captures all six heading levels as Module-kind symbols named after
// the heading text. Cheap; agents can `scry def "Verification checklist"`
// to find that section of DEVELOPMENT.md without grepping.
// ---------------------------------------------------------------------------

fn markdown_spec() -> &'static LangSpec {
    static SPEC: OnceLock<LangSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static LANG: OnceLock<Language> = OnceLock::new();
        static QUERY: OnceLock<Query> = OnceLock::new();
        let lang = LANG.get_or_init(|| tree_sitter_md::LANGUAGE.into());
        let q = QUERY.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (atx_heading (inline) @name) @def.module
                (setext_heading (paragraph) @name) @def.module
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(document) @def.module").unwrap())
        });
        LangSpec {
            language: lang,
            query: q,
            capture_kinds: &[
                ("def.module", SymbolKind::Module),
            ],
            name_capture: "name",
            scope_node_kinds: &[],
            package_node_kind: None,
        }
    })
}

// ===========================================================================
// REF QUERIES — one per language. Phase 2 focuses on call sites + ctors +
// inheritance edges, which gives us callers/callees/impls cheaply. Generic
// "type identifier used anywhere" refs are deferred until we can filter out
// definition sites cleanly.
// ===========================================================================

fn java_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = java_spec().language;
        let q = Q.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (method_invocation name: (identifier) @ref.call)
                (object_creation_expression type: (type_identifier) @ref.ctor)
                (superclass (type_identifier) @ref.inherit)
                (super_interfaces (type_list (type_identifier) @ref.inherit))
                (extends_interfaces (type_list (type_identifier) @ref.inherit))
                (import_declaration (scoped_identifier name: (identifier) @ref.import))
                (import_declaration (identifier) @ref.import)
                "#,
            )
            .expect("java refs")
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.ctor", RefKind::Ctor),
                ("ref.inherit", RefKind::InheritFrom),
                ("ref.import", RefKind::Import),
            ],
        }
    })
}

fn kotlin_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = kotlin_spec().language;
        let q = Q.get_or_init(|| {
            // tree-sitter-kotlin-NG: direct calls are `call_expression (identifier)`;
            // method calls are `call_expression (navigation_expression ...)`;
            // imports use the `import` node containing `qualified_identifier`.
            // The receiver identifier in a navigation IS a real reference too
            // (could be a class name like `Bar.process`), so we capture
            // navigation identifiers as FieldAccess — best-effort kind label.
            Query::new(
                lang,
                r#"
                (call_expression (identifier) @ref.call)
                (navigation_expression (identifier) @ref.field)
                (import (qualified_identifier) @ref.import)
                (user_type (identifier) @ref.type)
                "#,
            )
            .unwrap_or_else(|_| Query::new(lang, "(source_file) @noop").unwrap())
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.field", RefKind::FieldAccess),
                ("ref.import", RefKind::Import),
                ("ref.type", RefKind::TypeUse),
            ],
        }
    })
}

fn c_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = c_spec().language;
        let q = Q.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (call_expression function: (identifier) @ref.call)
                (preproc_include path: (string_literal) @ref.import)
                "#,
            )
            .expect("c refs")
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.import", RefKind::Import),
            ],
        }
    })
}

fn cpp_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = cpp_spec().language;
        let q = Q.get_or_init(|| {
            // `using namespace X;` and `using namespace X::Y;` both bind
            // the namespace identifier as a child of using_declaration.
            // Capture the namespace-ish identifier — the resolver only
            // cares about the leaf name (or last :: segment), which we
            // record as the ref's name.
            Query::new(
                lang,
                r#"
                (call_expression function: (identifier) @ref.call)
                (call_expression function: (field_expression field: (field_identifier) @ref.call))
                (call_expression function: (qualified_identifier name: (identifier) @ref.call))
                (call_expression function: (template_function name: (identifier) @ref.call))
                (new_expression type: (type_identifier) @ref.ctor)
                (base_class_clause (type_identifier) @ref.inherit)
                (preproc_include path: (string_literal) @ref.import)
                (using_declaration (qualified_identifier) @ref.using-ns)
                (using_declaration (identifier) @ref.using-ns)
                "#,
            )
            .expect("cpp refs")
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.ctor", RefKind::Ctor),
                ("ref.inherit", RefKind::InheritFrom),
                ("ref.import", RefKind::Import),
                ("ref.using-ns", RefKind::UsingNamespace),
            ],
        }
    })
}

fn rust_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = rust_spec().language;
        let q = Q.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (call_expression function: (identifier) @ref.call)
                (call_expression function: (scoped_identifier name: (identifier) @ref.call))
                (call_expression function: (field_expression field: (field_identifier) @ref.call))
                (macro_invocation macro: (identifier) @ref.call)
                (use_declaration (scoped_identifier name: (identifier) @ref.import))
                (impl_item trait: (type_identifier) @ref.inherit)
                "#,
            )
            .expect("rust refs")
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.inherit", RefKind::InheritFrom),
                ("ref.import", RefKind::Import),
            ],
        }
    })
}

fn go_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = go_spec().language;
        let q = Q.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (call_expression function: (identifier) @ref.call)
                (call_expression function: (selector_expression field: (field_identifier) @ref.call))
                (import_spec path: (interpreted_string_literal) @ref.import)
                "#,
            )
            .expect("go refs")
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.import", RefKind::Import),
            ],
        }
    })
}

fn python_refs_spec() -> &'static RefSpec {
    static SPEC: OnceLock<RefSpec> = OnceLock::new();
    SPEC.get_or_init(|| {
        static Q: OnceLock<Query> = OnceLock::new();
        let lang = python_spec().language;
        let q = Q.get_or_init(|| {
            Query::new(
                lang,
                r#"
                (call function: (identifier) @ref.call)
                (call function: (attribute attribute: (identifier) @ref.call))
                (import_statement name: (dotted_name) @ref.import)
                (import_from_statement module_name: (dotted_name) @ref.import)
                "#,
            )
            .expect("python refs")
        });
        RefSpec {
            query: q,
            capture_kinds: &[
                ("ref.call", RefKind::Call),
                ("ref.import", RefKind::Import),
            ],
        }
    })
}

// ===========================================================================
// FORMAT REGISTRY — open/closed extension point. Adding a new format is
// "implement FormatParser, register it". Tree-sitter source parsers and
// the AOSP custom parsers both live behind this trait.
// ===========================================================================

pub trait FormatParser: Send + Sync {
    fn kinds(&self) -> &'static [FileKind];
    fn parse(&self, kind: FileKind, source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>);
    fn name(&self) -> &'static str;
}

pub struct FormatRegistry {
    parsers: Vec<Box<dyn FormatParser>>,
    by_kind: std::collections::HashMap<FileKind, usize>,
}

impl FormatRegistry {
    pub fn new() -> Self {
        Self {
            parsers: Vec::new(),
            by_kind: std::collections::HashMap::new(),
        }
    }
    pub fn register(&mut self, p: Box<dyn FormatParser>) {
        let id = self.parsers.len();
        for k in p.kinds() {
            self.by_kind.insert(*k, id);
        }
        self.parsers.push(p);
    }
    pub fn parse(&self, kind: FileKind, source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
        match self.by_kind.get(&kind) {
            Some(&id) => self.parsers[id].parse(kind, source),
            None => (Vec::new(), Vec::new()),
        }
    }
    pub fn supports(&self, kind: FileKind) -> bool {
        self.by_kind.contains_key(&kind)
    }
    pub fn list(&self) -> Vec<(&'static str, &'static [FileKind])> {
        self.parsers.iter().map(|p| (p.name(), p.kinds())).collect()
    }
}

impl Default for FormatRegistry {
    fn default() -> Self { Self::new() }
}

/// Free-function adapter so most parsers can be one-line registrations.
pub struct FnAdapter {
    pub name: &'static str,
    pub kinds: &'static [FileKind],
    pub f: fn(FileKind, &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>),
}
impl FormatParser for FnAdapter {
    fn kinds(&self) -> &'static [FileKind] { self.kinds }
    fn name(&self) -> &'static str { self.name }
    fn parse(&self, kind: FileKind, source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
        (self.f)(kind, source)
    }
}

fn ts_unified(kind: FileKind, src: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    let s = extract(kind, src).unwrap_or_default();
    let r = extract_refs(kind, src).unwrap_or_default();
    (s, r)
}

/// Built-in tree-sitter source-language parsers. Call this from your
/// FormatRegistry setup to get C / C++ / Header / Java / Kotlin / Rust /
/// Go / Python / Bash / TypeScript / Proto / HTML / CSS / SCSS / Markdown
/// coverage out of the box.
pub fn tree_sitter_parsers() -> Vec<Box<dyn FormatParser>> {
    vec![
        Box::new(FnAdapter { name: "ts-java", kinds: &[FileKind::Java], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-kotlin", kinds: &[FileKind::Kotlin], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-c", kinds: &[FileKind::C], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-cpp", kinds: &[FileKind::Cpp, FileKind::HeaderCpp, FileKind::Header], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-rust", kinds: &[FileKind::Rust], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-go", kinds: &[FileKind::Go], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-python", kinds: &[FileKind::Python], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-bash", kinds: &[FileKind::Bash], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-typescript", kinds: &[FileKind::TypeScript], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-proto", kinds: &[FileKind::Proto], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-html", kinds: &[FileKind::Html], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-css", kinds: &[FileKind::Css], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-scss", kinds: &[FileKind::Scss], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-markdown", kinds: &[FileKind::Markdown], f: ts_unified }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_minimal() {
        let src = br#"
            package com.android.foo;
            public class Foo {
                public void bar() {}
                public int baz;
            }
        "#;
        let syms = extract(FileKind::Java, src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "names: {:?}", names);
        assert!(names.contains(&"bar"));
        assert!(names.contains(&"baz"));
    }

    #[test]
    fn cpp_minimal() {
        let src = br#"
            namespace foo {
            class Bar {
            public:
                void baz();
                int qux;
            };
            void Bar::baz() {}
            int top_level() { return 0; }
            }
        "#;
        let syms = extract(FileKind::Cpp, src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Bar"), "names: {:?}", names);
        assert!(names.contains(&"top_level"), "names: {:?}", names);
    }

    #[test]
    fn rust_minimal() {
        let src = br#"
            mod foo {
                pub struct Bar;
                impl Bar {
                    pub fn baz() {}
                }
                pub fn quux() {}
            }
        "#;
        let syms = extract(FileKind::Rust, src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"baz"));
        assert!(names.contains(&"quux"));
        assert!(names.contains(&"foo"));
    }

    #[test]
    fn go_minimal() {
        let src = br#"
            package foo

            type Bar struct {
                X int
            }

            func (b *Bar) Baz() int { return b.X }

            func TopLevel() int { return 0 }

            const Pi = 3
            var Counter = 0
        "#;
        let syms = extract(FileKind::Go, src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Bar"), "type Bar; names: {:?}", names);
        assert!(names.contains(&"Baz"), "method Baz; names: {:?}", names);
        assert!(names.contains(&"TopLevel"), "func TopLevel; names: {:?}", names);
        assert!(names.contains(&"Pi"), "const Pi; names: {:?}", names);
        assert!(names.contains(&"Counter"), "var Counter; names: {:?}", names);
    }

    #[test]
    fn python_minimal() {
        let src = br#"
class Foo:
    def bar(self):
        return 1

    def baz(self):
        return 2

def top_level():
    return 0
"#;
        let syms = extract(FileKind::Python, src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "class Foo; names: {:?}", names);
        assert!(names.contains(&"bar"), "method bar; names: {:?}", names);
        assert!(names.contains(&"baz"), "method baz; names: {:?}", names);
        assert!(names.contains(&"top_level"), "func top_level; names: {:?}", names);

        // Methods should carry their class as scope; top_level should not.
        let by_name: std::collections::HashMap<_, _> = syms.iter()
            .map(|s| (s.name.clone(), s.scope_path.clone()))
            .collect();
        assert_eq!(by_name.get("bar"), Some(&vec!["Foo".to_string()]),
                   "Foo.bar scope = [Foo]; got {:?}", by_name.get("bar"));
        assert_eq!(by_name.get("top_level"), Some(&vec![]),
                   "top_level scope = empty; got {:?}", by_name.get("top_level"));
    }

    /// The progress callback path must actually abort runaway parses.
    /// We feed the cpp grammar a synthetic input large enough to take
    /// far more than 1 microsecond and assert it returns TimedOut well
    /// before the (artificial) timeout's natural duration.
    ///
    /// Why this test exists: the previous implementation used
    /// set_timeout_micros, which silently failed to abort some real
    /// AOSP Java files in production (parses ran >1h with a 60s
    /// budget). The migration to parse_with_options must keep that
    /// regression from recurring.
    #[test]
    fn progress_callback_aborts_runaway_parse() {
        // Generate a large C++-ish source the grammar will plow through.
        let mut src = String::with_capacity(2_000_000);
        for i in 0..50_000 {
            src.push_str(&format!(
                "template <class T{0}> struct S{0} {{ T{0} a, b, c, d, e; }};\n",
                i
            ));
        }
        let bytes = src.as_bytes();

        // A 1-micro budget: any non-trivial parse will exceed it on the
        // very first progress callback invocation.
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(cpp_spec().language).unwrap();
            let start = std::time::Instant::now();
            let (outcome, elapsed) = parse_with_explicit_timeout(&mut parser, bytes, 1);
            let wall = start.elapsed().as_millis();
            assert!(
                matches!(outcome, ParseOutcome::TimedOut),
                "expected TimedOut, got something else after {} ms wall / {} ms reported",
                wall, elapsed,
            );
            // Sanity: abort should happen quickly. A 1-micro budget on a
            // 2MB synthetic input that the unbounded grammar would parse
            // in ~hundreds of ms must abort well under that.
            assert!(
                wall < 100,
                "abort took {} ms wall — progress callback is not firing fast enough",
                wall,
            );
        });
    }

    /// timeout=0 takes the no-progress-callback path. Verify it is
    /// functionally equivalent to a vanilla `parse_with_options(_, None)`
    /// call: same node kind, same node count, same source range — so a
    /// future refactor that subtly drops options on the timeout=0 branch
    /// would break this assertion before it broke real parsing. Catches
    /// the "we accidentally pass a stale progress callback even when the
    /// timeout is zero" class of bug.
    #[test]
    fn unbounded_parse_returns_tree() {
        let src = b"int main() { int x = 1; int y = 2; return x + y; }";

        // Reference: vanilla parse_with_options(_, None).
        let mut reference_parser = Parser::new();
        reference_parser.set_language(cpp_spec().language).unwrap();
        let len = src.len();
        let mut read = |i: usize, _: tree_sitter::Point| -> &[u8] {
            if i < len { &src[i..] } else { &[] }
        };
        let reference_tree = reference_parser
            .parse_with_options(&mut read, None, None)
            .expect("reference parse must succeed");
        let ref_root = reference_tree.root_node();
        let ref_kind = ref_root.kind();
        let ref_byte_range = ref_root.byte_range();
        let ref_node_count = walk_node_count(&ref_root);

        // System-under-test: parse_with_explicit_timeout with timeout=0.
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(cpp_spec().language).unwrap();
            let (outcome, _) = parse_with_explicit_timeout(&mut parser, src, 0);
            let tree = match outcome {
                ParseOutcome::Tree(t) => t,
                ParseOutcome::TimedOut => panic!("timeout=0 must never report TimedOut"),
                ParseOutcome::Failed => panic!("timeout=0 on valid C++ must not fail"),
            };
            let root = tree.root_node();
            assert_eq!(root.kind(), ref_kind, "root kind diverged from reference");
            assert_eq!(root.byte_range(), ref_byte_range, "root range diverged");
            assert_eq!(walk_node_count(&root), ref_node_count,
                       "node count diverged from reference parse");
            // A real syntax tree never has its root as a parse error.
            assert!(!root.has_error(), "root reports has_error on a valid program");
        });
    }

    fn walk_node_count(node: &tree_sitter::Node) -> usize {
        let mut n = 1;
        for i in 0..node.child_count() {
            if let Some(c) = node.child(i) {
                n += walk_node_count(&c);
            }
        }
        n
    }

    /// Kotlin extension function gets its receiver type prepended to the
    /// scope_path so `def shouted` shows scope ["String"], and outline of
    /// a file groups extensions by their receiver. Plain Kotlin functions
    /// keep their normal scope (empty here).
    #[test]
    fn kotlin_extension_receiver_scoping() {
        let src = br#"
            package x
            fun String.shouted(): String = uppercase() + "!"
            val String.firstChar: Char get() = this[0]
            fun plain() {}
            class Holder {
                fun method() {}
            }
        "#;
        let syms = extract(FileKind::Kotlin, src).unwrap();
        let by_name: std::collections::HashMap<_, _> = syms.iter()
            .map(|s| (s.name.clone(), s.scope_path.clone()))
            .collect();
        // Extension function -> scope[0] = "String"
        assert_eq!(by_name.get("shouted"), Some(&vec!["String".to_string()]),
                   "shouted should be scoped to String, got {:?}", by_name.get("shouted"));
        // Extension property -> scope[0] = "String"
        assert_eq!(by_name.get("firstChar"), Some(&vec!["String".to_string()]),
                   "firstChar should be scoped to String, got {:?}", by_name.get("firstChar"));
        // Plain function -> empty scope
        assert_eq!(by_name.get("plain"), Some(&vec![]),
                   "plain should have empty scope, got {:?}", by_name.get("plain"));
        // Method on a class -> scope = ["Holder"] (normal class scope, not receiver)
        assert_eq!(by_name.get("method"), Some(&vec!["Holder".to_string()]),
                   "Holder.method scope, got {:?}", by_name.get("method"));
    }

    /// C++ out-of-line method definitions (`Foo::bar() {}` outside the
    /// class body) used to capture `Foo::bar` as the SYMBOL NAME — so
    /// `scry def bar` returned nothing for any C++ method defined out
    /// of line, which is how Binder.cpp / SurfaceFlinger.cpp / most of
    /// real C++ AOSP code is written.
    ///
    /// After the drill_qualified_identifier change: bare name is `bar`,
    /// scope_path includes the qualifier (and any enclosing namespaces).
    #[test]
    fn cpp_qualified_method_def_is_bare_name() {
        let src = br#"
            namespace android {
            class BBinder {
            public:
                int transact(int code);
            };
            int BBinder::transact(int code) { return 0; }
            }
        "#;
        let syms = extract(FileKind::Cpp, src).unwrap();
        let by_name: std::collections::HashMap<_, Vec<_>> = syms.iter()
            .fold(std::collections::HashMap::new(), |mut acc, s| {
                acc.entry(s.name.clone()).or_insert_with(Vec::new).push(s.scope_path.clone());
                acc
            });
        // Expect `transact` to appear twice — once for the in-class
        // declaration (scope ["android", "BBinder"] from compute_scope's
        // ancestor walk) and once for the out-of-line definition
        // (scope ["android", "BBinder"] from the namespace ancestor +
        // the drilled-out qualifier).
        // The out-of-line `int BBinder::transact(int code) { ... }` is a
        // function_definition; the in-class `int transact(int code);` is
        // just a function_declaration, so only the out-of-line def
        // matches the cpp query — that's the one we used to lose entirely
        // and the one this fix recovers.
        let transacts = by_name.get("transact")
            .unwrap_or_else(|| panic!("expected `transact` symbol (the out-of-line def), got names: {:?}",
                                       by_name.keys().collect::<Vec<_>>()));
        assert_eq!(transacts.len(), 1,
                   "expected exactly one out-of-line transact def, got {} with scopes {:?}",
                   transacts.len(), transacts);
        assert_eq!(transacts[0], vec!["android".to_string(), "BBinder".to_string()],
                   "out-of-line def scope should be ['android','BBinder'], got {:?}",
                   transacts[0]);
        // The qualified form must NOT appear as a symbol name (regression —
        // it used to be the ONLY way to find this symbol).
        assert!(!by_name.contains_key("BBinder::transact"),
                "qualified form leaked into the bare name field");
        assert!(!by_name.contains_key("android::BBinder::transact"),
                "fully-qualified form leaked into the bare name field");
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture to see C++ AST"]
    fn dump_cpp_qualified_ast() {
        let src = br#"
namespace android {
class BBinder {
public:
    int transact(int code);
};
}
namespace android {
int BBinder::transact(int code) { return 0; }
}
"#;
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(cpp_spec().language).unwrap();
            let tree = parser.parse(src.as_ref(), None).unwrap();
            fn walk(n: tree_sitter::Node, src: &[u8], depth: usize) {
                let text = n.utf8_text(src).unwrap_or("?");
                let snippet = if text.len() < 60 { text.replace('\n', "\\n") } else { format!("<{}b>", text.len()) };
                println!("{:indent$}{} = {:?}", "", n.kind(), snippet, indent = depth*2);
                let mut w = n.walk();
                for c in n.named_children(&mut w) { walk(c, src, depth + 1); }
            }
            walk(tree.root_node(), src.as_ref(), 0);
        });
    }

    /// Kotlin companion-object coverage: anonymous + named.
    ///
    /// Anonymous `companion object { ... }` injects a synthetic
    /// Class symbol named "Companion" scoped to the enclosing class,
    /// and members get scope = [Outer, "Companion"]. Named
    /// `companion object Builder { ... }` is captured by the query
    /// directly as a Class, and members scope to the explicit name.
    #[test]
    fn kotlin_companion_object_coverage() {
        let src = br#"
            package x
            class H {
                companion object {
                    fun anon() {}
                    const val MAX = 100
                }
            }
            class H2 {
                companion object Builder {
                    fun named() {}
                }
            }
        "#;
        let syms = extract(FileKind::Kotlin, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}/{:?}", s.kind, s.name, s.scope_path);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        assert!(
            syms.iter().any(|s| s.name == "Companion"
                && s.kind == SymbolKind::Class
                && s.scope_path == vec!["H".to_string()]),
            "anonymous companion must surface as Companion class under H. all:\n  {}",
            all.join("\n  "),
        );
        assert!(
            syms.iter().any(|s| s.name == "anon"
                && s.kind == SymbolKind::Function
                && s.scope_path == vec!["H".to_string(), "Companion".to_string()]),
            "anonymous companion fun `anon` must scope to [H, Companion]. all:\n  {}",
            all.join("\n  "),
        );
        assert!(
            syms.iter().any(|s| s.name == "MAX"
                && s.scope_path == vec!["H".to_string(), "Companion".to_string()]),
            "anonymous companion const MAX must scope to [H, Companion]. all:\n  {}",
            all.join("\n  "),
        );

        assert!(
            syms.iter().any(|s| s.name == "Builder"
                && s.kind == SymbolKind::Class
                && s.scope_path == vec!["H2".to_string()]),
            "named companion `Builder` must surface as Class under H2. all:\n  {}",
            all.join("\n  "),
        );
        assert!(
            syms.iter().any(|s| s.name == "named"
                && s.kind == SymbolKind::Function
                && s.scope_path == vec!["H2".to_string(), "Builder".to_string()]),
            "named companion fun must scope to [H2, Builder]. all:\n  {}",
            all.join("\n  "),
        );
    }

    /// Bash extraction — the AOSP build script case. envsetup.sh
    /// defines `lunch`, `mm`, `mmm`, `croot`, `cgrep` etc. as plain
    /// `name() { ... }` declarations; we should pick those up.
    #[test]
    fn bash_envsetup_functions() {
        let src = br#"
            #!/bin/bash
            BUILD_TOP=$(pwd)
            export TARGET_PRODUCT=aosp_arm64

            function lunch() {
                local _lunch_combo=$1
                echo "lunched $_lunch_combo"
            }

            mm() {
                m -j "$@"
            }

            mmm() {
                m -j "$@"
            }
        "#;
        let syms = extract(FileKind::Bash, src).unwrap();
        let names: std::collections::HashSet<&str> =
            syms.iter().map(|s| s.name.as_str()).collect();
        for must_have in &["lunch", "mm", "mmm"] {
            assert!(names.contains(must_have),
                    "bash extraction missing function `{must_have}`; got: {names:?}");
        }
        // The variable assignments at top level should be captured too.
        for var in &["BUILD_TOP", "TARGET_PRODUCT"] {
            assert!(names.contains(var),
                    "bash extraction missing variable `{var}`; got: {names:?}");
        }
    }

    /// TypeScript extraction — class / interface / function / method /
    /// enum / type-alias / top-level var captures. The perfetto UI is
    /// ~5500 .ts files; this is the gold-standard test that the
    /// trace_viewer side gets covered the same way as C++/Java.
    #[test]
    fn typescript_minimal_extraction() {
        let src = br#"
            export interface Foo {
                bar(): string;
            }
            export class FooImpl implements Foo {
                bar(): string { return "x"; }
                static create(): FooImpl { return new FooImpl(); }
            }
            export function topLevel(x: number): number { return x + 1; }
            export type Id = string;
            export enum Direction { Up, Down }
            export const MAX_RETRIES: number = 5;
        "#;
        let syms = extract(FileKind::TypeScript, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}", s.kind, s.name);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        assert!(syms.iter().any(|s| s.kind == SymbolKind::Interface && s.name == "Foo"),
                "missing interface Foo. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Class && s.name == "FooImpl"),
                "missing class FooImpl. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Method && s.name == "bar"),
                "missing method bar. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Method && s.name == "create"),
                "missing static method create. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Function && s.name == "topLevel"),
                "missing fn topLevel. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Type && s.name == "Id"),
                "missing type Id. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Enum && s.name == "Direction"),
                "missing enum Direction. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Variable && s.name == "MAX_RETRIES"),
                "missing var MAX_RETRIES. all:\n  {}", all.join("\n  "));
    }

    /// Proto extraction — proto3-style message / enum / service / rpc.
    /// perfetto's trace.proto family + AOSP's framework protos.
    #[test]
    fn proto_message_enum_service_rpc() {
        let src = br#"
            syntax = "proto3";
            package perfetto.protos;

            message TracePacket {
                string name = 1;
                uint64 timestamp = 2;
            }

            enum Verbosity {
                INFO = 0;
                WARN = 1;
                ERROR = 2;
            }

            service Recorder {
                rpc StartTrace(StartRequest) returns (StartResponse);
                rpc StopTrace(StopRequest) returns (StopResponse);
            }
        "#;
        let syms = extract(FileKind::Proto, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}", s.kind, s.name);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        assert!(syms.iter().any(|s| s.kind == SymbolKind::ProtoMessage && s.name == "TracePacket"),
                "missing message TracePacket. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::ProtoEnum && s.name == "Verbosity"),
                "missing enum Verbosity. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::ProtoService && s.name == "Recorder"),
                "missing service Recorder. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Method && s.name == "StartTrace"),
                "missing rpc StartTrace. all:\n  {}", all.join("\n  "));
    }

    /// proto2 syntax — distinct from proto3 in field rules (required /
    /// optional). The grammar's the same; we just need the high-level
    /// captures to keep firing.
    #[test]
    fn proto2_messages_capture() {
        let src = br#"
            syntax = "proto2";
            package legacy;
            message ConfigV1 {
                required string name = 1;
                optional int32 count = 2;
            }
        "#;
        let syms = extract(FileKind::Proto, src).unwrap();
        assert!(syms.iter().any(|s| s.kind == SymbolKind::ProtoMessage && s.name == "ConfigV1"),
                "proto2 message not captured. got: {:?}",
                syms.iter().map(|s| &s.name).collect::<Vec<_>>());
    }

    /// CSS extraction — class selectors, ID selectors, @keyframes
    /// names. The perfetto UI bundles its own; same patterns appear in
    /// any web frontend.
    #[test]
    fn css_class_id_keyframes() {
        let src = br#"
            .main-panel { background: white; }
            .nested .child { color: red; }
            #app-root { padding: 1em; }
            @keyframes fadeIn {
                from { opacity: 0; }
                to   { opacity: 1; }
            }
        "#;
        let syms = extract(FileKind::Css, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}", s.kind, s.name);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        for cls in &["main-panel", "nested", "child"] {
            assert!(syms.iter().any(|s| s.kind == SymbolKind::Class && s.name == *cls),
                    "css class `{cls}` missing. all:\n  {}", all.join("\n  "));
        }
        assert!(syms.iter().any(|s| s.kind == SymbolKind::XmlId && s.name == "app-root"),
                "css id `app-root` missing. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Type && s.name == "fadeIn"),
                "@keyframes name missing. all:\n  {}", all.join("\n  "));
    }

    /// SCSS extraction — perfetto UI's primary stylesheet format.
    /// Adds @mixin / @function captures on top of CSS-shape patterns.
    #[test]
    fn scss_class_mixin_function() {
        let src = br#"
            .panel { color: black; }
            #root { padding: 0; }

            @mixin reset {
                margin: 0;
                padding: 0;
            }

            @function pow($base, $exp) {
                @return $base * $exp;
            }
        "#;
        let syms = extract(FileKind::Scss, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}", s.kind, s.name);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        assert!(syms.iter().any(|s| s.kind == SymbolKind::Class && s.name == "panel"),
                "scss class `panel` missing. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::XmlId && s.name == "root"),
                "scss id `root` missing. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Function && s.name == "reset"),
                "scss @mixin `reset` missing. all:\n  {}", all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::Function && s.name == "pow"),
                "scss @function `pow` missing. all:\n  {}", all.join("\n  "));
    }

    /// HTML extraction — id/name attribute values. JS code referencing
    /// `getElementById("x")` should be able to find id="x" in the
    /// .html that defines it. Conservative: capture nothing else
    /// (tag names are noise on a real UI tree).
    #[test]
    fn html_attribute_value_capture() {
        let src = br#"
            <html>
              <body>
                <div id="app-root">
                  <button name="submit-btn">Go</button>
                  <input id="search-input" />
                </div>
              </body>
            </html>
        "#;
        let syms = extract(FileKind::Html, src).unwrap();
        let names: std::collections::HashSet<&str> =
            syms.iter().map(|s| s.name.as_str()).collect();
        for must_have in &["app-root", "submit-btn", "search-input"] {
            assert!(names.contains(must_have),
                    "html attribute value `{must_have}` missing; got: {names:?}");
        }
    }

    /// Markdown extraction — atx + setext headings as Module-kind
    /// symbols. Lets `scry def "Verification checklist"` jump to that
    /// section of DEVELOPMENT.md.
    #[test]
    fn markdown_headings_capture() {
        let src = br#"# Top Title

Some intro.

## Section One

Body.

### Subsection

Body.

Setext Heading
==============

Body.
"#;
        let syms = extract(FileKind::Markdown, src).unwrap();
        let names: std::collections::HashSet<String> =
            syms.iter().map(|s| s.name.trim().to_string()).collect();
        for must_have in &["Top Title", "Section One", "Subsection", "Setext Heading"] {
            assert!(names.contains(*must_have),
                    "markdown heading `{must_have}` missing; got: {names:?}");
        }
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture"]
    fn dump_scss_ast() {
        let src = b".panel { color: black; }\n#root { padding: 0; }\n@mixin reset { margin: 0; }\n@function pow($a) { @return $a; }\n";
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(&tree_sitter_scss::language()).unwrap();
            let tree = parser.parse(src.as_ref(), None).unwrap();
            fn walk(n: tree_sitter::Node, src: &[u8], depth: usize) {
                let text = n.utf8_text(src).unwrap_or("?");
                let snip = if text.len() < 40 { text.replace('\n', "\\n") } else { format!("<{}b>", text.len()) };
                println!("{:indent$}{} = {:?}", "", n.kind(), snip, indent = depth*2);
                for i in 0..n.child_count() { if let Some(c) = n.child(i) { walk(c, src, depth + 1); } }
            }
            walk(tree.root_node(), src.as_ref(), 0);
        });
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture"]
    fn dump_html_ast() {
        let src = b"<div id=\"foo\"><button name=\"bar\">x</button></div>\n";
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(&tree_sitter_html::LANGUAGE.into()).unwrap();
            let tree = parser.parse(src.as_ref(), None).unwrap();
            fn walk(n: tree_sitter::Node, src: &[u8], depth: usize) {
                let text = n.utf8_text(src).unwrap_or("?");
                let snip = if text.len() < 40 { text.replace('\n', "\\n") } else { format!("<{}b>", text.len()) };
                println!("{:indent$}{} = {:?}", "", n.kind(), snip, indent = depth*2);
                for i in 0..n.child_count() { if let Some(c) = n.child(i) { walk(c, src, depth + 1); } }
            }
            walk(tree.root_node(), src.as_ref(), 0);
        });
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture"]
    fn dump_java_native_ast() {
        let src = b"package x;\nclass C {\n  public static native String foo(long ptr);\n}\n";
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(java_spec().language).unwrap();
            let tree = parser.parse(src.as_ref(), None).unwrap();
            fn walk(n: tree_sitter::Node, src: &[u8], depth: usize) {
                let text = n.utf8_text(src).unwrap_or("?");
                let snippet = if text.len() < 50 { text.replace('\n', "\\n") } else { format!("<{}b>", text.len()) };
                println!("{:indent$}{} = {:?}", "", n.kind(), snippet, indent = depth*2);
                for i in 0..n.child_count() {
                    if let Some(c) = n.child(i) { walk(c, src, depth + 1); }
                }
            }
            walk(tree.root_node(), src.as_ref(), 0);
        });
    }

    /// JNI shadow: a Java `native` method produces a synthetic
    /// `JniBinding` symbol whose name is the standard JNI mangling.
    /// Lets `scry def Java_android_os_Parcel_nativeWriteString` find
    /// the Java declaration even when the C/C++ implementation isn't
    /// in the indexed tree or is shared across modules.
    #[test]
    fn java_native_method_emits_jni_shadow() {
        let src = br#"
            package android.os;
            public final class Parcel {
                public static native String nativeReadString(long ptr);
                public static native void nativeWriteString(long ptr, String value);
                public void notNative() {}
                private int normal_field;
            }
        "#;
        let syms = extract(FileKind::Java, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}", s.kind, s.name);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        assert!(syms.iter().any(|s| s.kind == SymbolKind::JniBinding
                                 && s.name == "Java_android_os_Parcel_nativeReadString"),
                "missing JniBinding for nativeReadString. all:\n  {}",
                all.join("\n  "));
        assert!(syms.iter().any(|s| s.kind == SymbolKind::JniBinding
                                 && s.name == "Java_android_os_Parcel_nativeWriteString"),
                "missing JniBinding for nativeWriteString. all:\n  {}",
                all.join("\n  "));
        // Non-native methods MUST NOT produce a JniBinding.
        assert!(!syms.iter().any(|s| s.kind == SymbolKind::JniBinding
                                  && s.name.contains("notNative")),
                "non-native method `notNative` should not have a JniBinding");
    }

    /// JNI mangling escapes underscores in identifiers ("_" → "_1")
    /// and dots in the package ("." → "_") per the JNI spec.
    #[test]
    fn jni_mangle_escapes_underscores_and_dots() {
        // Package dot → underscore; method underscore → "_1".
        assert_eq!(
            jni_mangle("com.weird_pkg", "MyClass", "my_method"),
            "Java_com_weird_1pkg_MyClass_my_1method",
        );
        // Plain ASCII path stays simple.
        assert_eq!(
            jni_mangle("android.os", "Parcel", "nativeRead"),
            "Java_android_os_Parcel_nativeRead",
        );
    }

    /// Sealed-class hierarchies and inline reified fns are already
    /// covered by the standard class_declaration / function_declaration
    /// captures. Pin that behavior so a future query refactor doesn't
    /// silently drop them.
    #[test]
    fn kotlin_sealed_and_inline_reified_coverage() {
        let src = br#"
            package x
            sealed class Result {
                class Ok : Result()
                class Err(val msg: String) : Result()
                object Pending : Result()
            }
            inline fun <reified T> printType() { println(T::class.simpleName) }
        "#;
        let syms = extract(FileKind::Kotlin, src).unwrap();
        let pretty = |s: &RawSymbol| format!("{:?}/{}/{:?}", s.kind, s.name, s.scope_path);
        let all: Vec<String> = syms.iter().map(pretty).collect();

        assert!(syms.iter().any(|s| s.name == "Result" && s.kind == SymbolKind::Class
                                 && s.scope_path.is_empty()),
                "sealed parent `Result` must be top-level Class. all:\n  {}",
                all.join("\n  "));
        for sub in &["Ok", "Err", "Pending"] {
            assert!(syms.iter().any(|s| s.name == *sub && s.scope_path == vec!["Result".to_string()]),
                    "sealed subtype `{sub}` must scope under [Result]. all:\n  {}",
                    all.join("\n  "));
        }

        assert!(syms.iter().any(|s| s.name == "printType"
                                 && s.kind == SymbolKind::Function
                                 && s.scope_path.is_empty()),
                "inline reified fn `printType` must be top-level Function. all:\n  {}",
                all.join("\n  "));
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture"]
    fn dump_kotlin_modern_features() {
        // Probe what extract() currently does for companion / sealed / inline reified.
        for (label, src) in &[
            ("companion-anonymous", b"package x\nclass H { companion object { fun create() = H() } }\n" as &[u8]),
            ("companion-named",     b"package x\nclass H { companion object Builder { fun build() = H() } }\n" as &[u8]),
            ("sealed-with-children", b"package x\nsealed class R { class Ok : R(); class Err : R(); object Pending : R() }\n" as &[u8]),
            ("inline-reified",       b"package x\ninline fun <reified T> printType() { println(T::class.simpleName) }\n" as &[u8]),
        ] {
            println!("--- {label} ---");
            let syms = extract(FileKind::Kotlin, src).unwrap();
            for s in &syms {
                println!("  {:?} name={:?} scope={:?}", s.kind, s.name, s.scope_path);
            }
        }
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture to see companion AST"]
    fn dump_kotlin_companion_ast() {
        let src = b"package x\nclass H {\n  companion object {\n    fun anon() {}\n  }\n}\nclass H2 {\n  companion object Builder {\n    fun named() {}\n  }\n}\n";
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(kotlin_spec().language).unwrap();
            let tree = parser.parse(src.as_ref(), None).unwrap();
            fn walk(n: tree_sitter::Node, src: &[u8], depth: usize) {
                let text = n.utf8_text(src).unwrap_or("?");
                let snippet = if text.len() < 50 { text.replace('\n', "\\n") } else { format!("<{}b>", text.len()) };
                let by_name_field = n.parent()
                    .and_then(|p| {
                        let mut w = p.walk();
                        let mut fname = None;
                        for (i, c) in p.named_children(&mut w).enumerate() {
                            if c.id() == n.id() {
                                fname = p.field_name_for_named_child(i as u32).map(String::from);
                                break;
                            }
                        }
                        fname
                    })
                    .map(|s| format!(" [field={s}]"))
                    .unwrap_or_default();
                println!("{:indent$}{}{} = {:?}", "", n.kind(), by_name_field, snippet, indent = depth*2);
                let mut w = n.walk();
                for c in n.named_children(&mut w) { walk(c, src, depth + 1); }
            }
            walk(tree.root_node(), src.as_ref(), 0);
        });
    }

    #[test]
    #[ignore = "exploratory; run with -- --ignored --nocapture to see Kotlin AST"]
    fn dump_kotlin_ext_ast() {
        let src = b"package x\nfun String.shouted(): String = uppercase() + \"!\"\nval String.firstChar: Char get() = this[0]\n";
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(kotlin_spec().language).unwrap();
            let tree = parser.parse(src.as_ref(), None).unwrap();
            fn walk(n: tree_sitter::Node, src: &[u8], depth: usize) {
                let text = n.utf8_text(src).unwrap_or("?");
                let snippet = if text.len() < 40 { text.replace('\n', "\\n") } else { format!("<{}b>", text.len()) };
                let field = n.parent()
                    .and_then(|p| {
                        let mut w = p.walk();
                        let mut field_name = None;
                        for (i, c) in p.named_children(&mut w).enumerate() {
                            if c.id() == n.id() {
                                field_name = p.field_name_for_named_child(i as u32).map(String::from);
                                break;
                            }
                        }
                        field_name
                    })
                    .map(|s| format!(" [field={s}]"))
                    .unwrap_or_default();
                println!("{:indent$}{}{} = {:?}", "", n.kind(), field, snippet, indent = depth*2);
                let mut w = n.walk();
                for c in n.named_children(&mut w) { walk(c, src, depth + 1); }
            }
            walk(tree.root_node(), src.as_ref(), 0);
        });
    }
}

#[cfg(test)]
mod scope_regression_tests {
    //! Pin the contract that a symbol's own enclosing declaration is
    //! NOT pushed onto its scope_path. Pre-704d917, every top-level
    //! Java class had scope_path = [ClassName] and FQN doubled to
    //! `Foo::Foo`. The compute_scope check `n.id() != node.id()`
    //! prevents the recursion; these tests catch any regression that
    //! breaks that identity check (e.g. a tree-sitter upgrade that
    //! changes node-id semantics).
    use super::*;

    #[test]
    fn java_top_level_class_has_empty_scope() {
        let src = br#"
            package com.android.foo;
            public class Foo {
                public void bar() {}
            }
        "#;
        let syms = extract(FileKind::Java, src).unwrap();
        let foo = syms.iter().find(|s| s.name == "Foo" && s.kind == SymbolKind::Class)
            .expect("Foo class extracted");
        assert!(foo.scope_path.is_empty(),
                "top-level class Foo must have empty scope; got {:?}", foo.scope_path);
        let bar = syms.iter().find(|s| s.name == "bar" && s.kind == SymbolKind::Method)
            .expect("bar method extracted");
        assert_eq!(bar.scope_path, vec!["Foo".to_string()],
                "method bar must be scoped under [Foo]; got {:?}", bar.scope_path);
    }

    #[test]
    fn java_nested_class_has_outer_scope() {
        let src = br#"
            public class Outer {
                public static class Inner {
                    public void deep() {}
                }
            }
        "#;
        let syms = extract(FileKind::Java, src).unwrap();
        let inner = syms.iter().find(|s| s.name == "Inner" && s.kind == SymbolKind::Class)
            .expect("Inner extracted");
        assert_eq!(inner.scope_path, vec!["Outer".to_string()],
                "Inner must be scoped under [Outer], NOT [Outer, Inner]; got {:?}",
                inner.scope_path);
        let deep = syms.iter().find(|s| s.name == "deep")
            .expect("deep extracted");
        assert_eq!(deep.scope_path, vec!["Outer".to_string(), "Inner".to_string()],
                "deep must be scoped under [Outer, Inner]; got {:?}",
                deep.scope_path);
    }

    #[test]
    fn cpp_top_level_class_has_empty_scope() {
        let src = br#"
            class Widget {
            public:
                void poke();
            };
        "#;
        let syms = extract(FileKind::Cpp, src).unwrap();
        let widget = syms.iter().find(|s| s.name == "Widget" && s.kind == SymbolKind::Class)
            .expect("Widget extracted");
        assert!(widget.scope_path.is_empty(),
                "top-level class Widget must have empty scope; got {:?}", widget.scope_path);
    }
}
