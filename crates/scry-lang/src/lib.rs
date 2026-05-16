//! scry-lang: per-language symbol extraction via tree-sitter.

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
        _ => return Ok(Vec::new()),
    };
    extract_with(spec, source)
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
            if let Some(n) = p.child_by_field_name("name") {
                // Don't push our own name as our own scope step — for a
                // function/class declaration where the captured @name IS
                // p's "name" field, the symbol belongs INSIDE p but
                // p doesn't enclose it scope-wise.
                if n.id() != node.id() {
                    if let Ok(s) = n.utf8_text(src) {
                        scope.push(s.to_string());
                    }
                }
            }
        }
        cur = p.parent();
    }
    scope.reverse();
    scope
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
            scope_node_kinds: &["class_declaration", "object_declaration", "function_declaration"],
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
/// Go / Python coverage out of the box.
pub fn tree_sitter_parsers() -> Vec<Box<dyn FormatParser>> {
    vec![
        Box::new(FnAdapter { name: "ts-java", kinds: &[FileKind::Java], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-kotlin", kinds: &[FileKind::Kotlin], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-c", kinds: &[FileKind::C], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-cpp", kinds: &[FileKind::Cpp, FileKind::HeaderCpp, FileKind::Header], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-rust", kinds: &[FileKind::Rust], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-go", kinds: &[FileKind::Go], f: ts_unified }),
        Box::new(FnAdapter { name: "ts-python", kinds: &[FileKind::Python], f: ts_unified }),
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

    /// Sanity: with timeout=0 we get unbounded parsing and a real tree.
    #[test]
    fn unbounded_parse_returns_tree() {
        let src = b"int main() { return 0; }";
        PARSER.with(|cell| {
            let mut parser = cell.borrow_mut();
            parser.set_language(cpp_spec().language).unwrap();
            let (outcome, _) = parse_with_explicit_timeout(&mut parser, src, 0);
            assert!(matches!(outcome, ParseOutcome::Tree(_)));
        });
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
