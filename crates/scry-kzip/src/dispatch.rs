//! Pick the right Kythe indexer for a given `KzipUnit`.
//!
//! Routing is per `CompilationUnit.v_name.language`, with one fork
//! for Kotlin (source-level Kotlin needs Google-internal tooling not
//! in the public Kythe v0.0.75 release; we fall through to the JVM
//! bytecode indexer if the unit ships `.class` / `.jar` inputs).

use crate::walker::KzipUnit;

/// Which Kythe indexer (if any) handles this unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexerKind {
    /// `cxx_indexer` — C / C++ / ObjC, writes USR-style symbol identity.
    Cxx,
    /// `java_indexer.jar` — source-level Java.
    JavaSource,
    /// `jvm_indexer.jar` — JVM bytecode (`.class` / `.jar`), used both
    /// for Java post-compile inputs and for Kotlin (we don't have a
    /// source-level Kotlin indexer in the public release).
    JvmBytecode,
    /// `go_indexer`.
    Go,
    /// `proto_indexer`.
    Proto,
    /// `textproto_indexer`.
    TextProto,
    /// No indexer in the public release; we skip and log.
    Skip(&'static str),
}

impl IndexerKind {
    /// Stable short label used for log lines + filenames under
    /// `kythe-logs/`. Two `IndexerKind` values are *not* expected to
    /// share a label.
    pub fn label(self) -> &'static str {
        match self {
            IndexerKind::Cxx          => "cxx",
            IndexerKind::JavaSource   => "java",
            IndexerKind::JvmBytecode  => "jvm",
            IndexerKind::Go           => "go",
            IndexerKind::Proto        => "proto",
            IndexerKind::TextProto    => "textproto",
            IndexerKind::Skip(_)      => "skip",
        }
    }

    /// True if this routing means "we will spawn a real Kythe binary
    /// for this CU". `Skip(_)` returns false.
    pub fn is_runnable(self) -> bool {
        !matches!(self, IndexerKind::Skip(_))
    }
}

/// Routing logic. Matches Kythe's language strings (the same labels
/// the schema uses: "c++", "java", "kotlin", "go", "protobuf",
/// "textproto", "rust"). Unrecognised languages fall through to
/// `Skip` with an explanation.
pub fn choose(language: &str, has_class_or_jar_input: bool) -> IndexerKind {
    match language {
        "c++" | "c" | "objc" => IndexerKind::Cxx,
        "java" => IndexerKind::JavaSource,
        "kotlin" => {
            if has_class_or_jar_input {
                IndexerKind::JvmBytecode
            } else {
                IndexerKind::Skip(
                    "kotlin source-only indexer not in public Kythe v0.0.75",
                )
            }
        }
        "go" => IndexerKind::Go,
        "protobuf" | "proto" => IndexerKind::Proto,
        "textproto" => IndexerKind::TextProto,
        "rust" => IndexerKind::Skip(
            "rust indexer not in public Kythe v0.0.75",
        ),
        "" => IndexerKind::Skip("no language label on CU"),
        _ => IndexerKind::Skip("unknown language label"),
    }
}

/// Convenience: `choose` against a `KzipUnit`'s fields.
pub fn choose_for(unit: &KzipUnit) -> IndexerKind {
    choose(&unit.language, unit.has_class_or_jar_input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cxx_languages_route_to_cxx() {
        assert_eq!(choose("c++", false), IndexerKind::Cxx);
        assert_eq!(choose("c", false),   IndexerKind::Cxx);
        assert_eq!(choose("objc", false), IndexerKind::Cxx);
    }

    #[test]
    fn kotlin_with_bytecode_routes_to_jvm() {
        assert_eq!(choose("kotlin", true), IndexerKind::JvmBytecode);
    }

    #[test]
    fn kotlin_without_bytecode_skips_with_message() {
        match choose("kotlin", false) {
            IndexerKind::Skip(msg) => assert!(msg.contains("kotlin")),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn rust_skips_with_message() {
        match choose("rust", false) {
            IndexerKind::Skip(msg) => assert!(msg.contains("rust")),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn unknown_language_skips() {
        match choose("haskell", false) {
            IndexerKind::Skip(_) => (),
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn proto_variants_route_correctly() {
        assert_eq!(choose("protobuf", false), IndexerKind::Proto);
        assert_eq!(choose("proto", false),    IndexerKind::Proto);
        assert_eq!(choose("textproto", false), IndexerKind::TextProto);
    }
}
