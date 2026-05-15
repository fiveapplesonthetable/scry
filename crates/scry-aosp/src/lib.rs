//! scry-aosp: AOSP-specific file format parsers.
//!
//! Covers Android.bp (Soong / Blueprint), AIDL, and OWNERS. Each parser is
//! deliberately small and tolerant — bad files yield best-effort partial
//! output rather than failing the whole indexing run.

pub mod bp;
pub mod aidl;
pub mod owners;
pub mod aconfig;
pub mod initrc;
pub mod sepolicy;
pub mod manifest;
pub mod hidl;
pub mod bazel;
pub mod cmake;
pub mod gn;
pub mod api_txt;

use scry_lang::RawSymbol;
use scry_lang::RawRef;
use scry_store::{RefKind, SymbolKind};
use scry_walker::FileKind;

/// Top-level dispatch: given a (kind, source) pair, return any symbols and
/// refs extracted by the AOSP-specific parsers. Tree-sitter source langs
/// are handled by scry-lang::extract / extract_refs instead.
pub fn extract(kind: FileKind, source: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) {
    match kind {
        FileKind::Soong => bp::parse(source),
        FileKind::Aidl => aidl::parse(source),
        FileKind::Owners => owners::parse(source),
        FileKind::Aconfig => aconfig::parse(source),
        FileKind::InitRc => initrc::parse(source),
        FileKind::Sepolicy => sepolicy::parse(source),
        FileKind::Manifest => manifest::parse(source),
        FileKind::Hidl => hidl::parse(source),
        FileKind::Bazel | FileKind::Bzl => bazel::parse(source),
        FileKind::CMake => cmake::parse(source),
        FileKind::Gn => gn::parse(source),
        FileKind::ApiTxt => api_txt::parse(source),
        _ => (Vec::new(), Vec::new()),
    }
}

/// Helper used by every sub-parser when it wants to push a refless symbol.
pub(crate) fn make_symbol(
    name: String,
    kind: SymbolKind,
    line: u32,
    col: u32,
    byte_start: u32,
    byte_end: u32,
    scope_path: Vec<String>,
) -> RawSymbol {
    RawSymbol { name, kind, byte_start, byte_end, line, col, scope_path }
}

pub(crate) fn make_ref(
    name: String,
    kind: RefKind,
    line: u32,
    col: u32,
    byte_start: u32,
    byte_end: u32,
    scope_path: Vec<String>,
) -> RawRef {
    RawRef { name, kind, byte_start, byte_end, line, col, scope_path }
}
