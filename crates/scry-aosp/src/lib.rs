//! scry-aosp: AOSP-specific file format parsers.
//!
//! Covers Android.bp (Soong / Blueprint), AIDL, and OWNERS. Each parser is
//! deliberately small and tolerant — bad files yield best-effort partial
//! output rather than failing the whole indexing run.

#![forbid(unsafe_code)]

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

use scry_lang::{FnAdapter, FormatParser, RawRef, RawSymbol};
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

// ----- Parser trait adapters for FormatRegistry -----
fn p_soong(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { bp::parse(s) }
fn p_aidl(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { aidl::parse(s) }
fn p_owners(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { owners::parse(s) }
fn p_aconfig(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { aconfig::parse(s) }
fn p_initrc(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { initrc::parse(s) }
fn p_sepolicy(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { sepolicy::parse(s) }
fn p_manifest(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { manifest::parse(s) }
fn p_hidl(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { hidl::parse(s) }
fn p_bazel(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { bazel::parse(s) }
fn p_cmake(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { cmake::parse(s) }
fn p_gn(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { gn::parse(s) }
fn p_api_txt(_: FileKind, s: &[u8]) -> (Vec<RawSymbol>, Vec<RawRef>) { api_txt::parse(s) }

/// All AOSP custom-format parsers ready to register with a FormatRegistry.
pub fn aosp_parsers() -> Vec<Box<dyn FormatParser>> {
    vec![
        Box::new(FnAdapter { name: "aosp-soong", kinds: &[FileKind::Soong], f: p_soong }),
        Box::new(FnAdapter { name: "aosp-aidl", kinds: &[FileKind::Aidl], f: p_aidl }),
        Box::new(FnAdapter { name: "aosp-owners", kinds: &[FileKind::Owners], f: p_owners }),
        Box::new(FnAdapter { name: "aosp-aconfig", kinds: &[FileKind::Aconfig], f: p_aconfig }),
        Box::new(FnAdapter { name: "aosp-initrc", kinds: &[FileKind::InitRc], f: p_initrc }),
        Box::new(FnAdapter { name: "aosp-sepolicy", kinds: &[FileKind::Sepolicy], f: p_sepolicy }),
        Box::new(FnAdapter { name: "aosp-manifest", kinds: &[FileKind::Manifest], f: p_manifest }),
        Box::new(FnAdapter { name: "aosp-hidl", kinds: &[FileKind::Hidl], f: p_hidl }),
        Box::new(FnAdapter { name: "aosp-bazel", kinds: &[FileKind::Bazel, FileKind::Bzl], f: p_bazel }),
        Box::new(FnAdapter { name: "aosp-cmake", kinds: &[FileKind::CMake], f: p_cmake }),
        Box::new(FnAdapter { name: "aosp-gn", kinds: &[FileKind::Gn], f: p_gn }),
        Box::new(FnAdapter { name: "aosp-api-txt", kinds: &[FileKind::ApiTxt], f: p_api_txt }),
    ]
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
