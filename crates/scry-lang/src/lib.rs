//! scry-lang: per-language symbol extraction via tree-sitter.

use anyhow::Result;
use scry_store::SymbolKind;
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

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
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
        let tree = match parser.parse(source, None) {
            Some(t) => t,
            None => return Ok(Vec::new()),
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
            let scope_path = compute_scope(
                name_node,
                source,
                spec.scope_node_kinds,
                spec.package_node_kind,
            );
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
                if let Ok(s) = n.utf8_text(src) {
                    scope.push(s.to_string());
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
            Query::new(
                lang,
                r#"
                (class_declaration (type_identifier) @name) @def.class
                (object_declaration (type_identifier) @name) @def.class
                (function_declaration (simple_identifier) @name) @def.function
                (property_declaration (variable_declaration (simple_identifier) @name)) @def.field
                (type_alias (type_identifier) @name) @def.type
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
}
