//! Decode the length-delimited `kythe.proto.Entry` stream a Kythe
//! indexer writes to stdout, then collapse it into per-anchor
//! `(file_path, byte_offset, byte_end, target_vname, role)` records
//! ready for the precision sidecar emitter.
//!
//! ## Wire format
//!
//! Indexers write a stream of `Entry` messages, each preceded by
//! its proto-varint length. Each Entry is either:
//!
//! * **Node fact** — `(source=vname, fact_name, fact_value)`, no
//!   edge. Common fact_names we care about:
//!     - `/kythe/node/kind` — `"anchor" | "function" | "record" | ...`
//!     - `/kythe/loc/start` — anchor start byte offset (as ASCII int)
//!     - `/kythe/loc/end`   — anchor end   byte offset (as ASCII int)
//! * **Edge** — `(source=vname, edge_kind, target=vname)`, fact_name
//!   typically empty. Common edge_kinds we care about:
//!     - `/kythe/edge/defines/binding` — anchor binds a decl
//!     - `/kythe/edge/defines`         — non-binding decl
//!     - `/kythe/edge/ref`             — anchor refs a symbol
//!     - `/kythe/edge/ref/call`        — anchor calls a symbol
//!     - `/kythe/edge/childof`         — anchor lives inside `file`
//!       (target is the file vname; we don't need this if the anchor
//!       vname already carries the path field, which it does for
//!       every indexer in v0.0.75).
//!
//! ## Strategy
//!
//! Stream-process. Keep a single `HashMap<VName, AnchorAccum>` and
//! flush an anchor as soon as it has start + end + an out-edge —
//! O(in-flight anchors) memory, not O(graph). The decoder yields
//! `DecodedRecord`s; the caller (emit.rs) translates the VName
//! into the precision sidecar's `(abs_path, symbol_string)` shape.

use crate::proto::storage::Entry;
use anyhow::{Context, Result};
use protobuf::Message;
use std::collections::HashMap;
use std::io::Read;

/// Subset of `VName` we hash on. We re-allocate `String`s here for
/// ownership, but VNames are small (5 short strings) and the
/// accumulator's lifetime is one indexer-run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VName {
    pub signature: String,
    pub corpus: String,
    pub root: String,
    pub path: String,
    pub language: String,
}

impl VName {
    pub fn from_proto(p: &crate::proto::storage::VName) -> Self {
        Self {
            signature: p.signature.clone(),
            corpus: p.corpus.clone(),
            root: p.root.clone(),
            path: p.path.clone(),
            language: p.language.clone(),
        }
    }

    /// True if this VName carries no usable information.
    pub fn is_empty(&self) -> bool {
        self.signature.is_empty()
            && self.corpus.is_empty()
            && self.root.is_empty()
            && self.path.is_empty()
            && self.language.is_empty()
    }

    /// Canonical SCIP-flavoured symbol string. Stable across runs as
    /// long as upstream `language` / `corpus` / `signature` are stable
    /// — Kythe's own convention.
    pub fn to_symbol_string(&self) -> String {
        // `<lang> <corpus>#<root>#<path>#<signature>` keeps every
        // field in the symbol so two VNames that differ only in path
        // or root never collide.
        format!(
            "kythe:{}:{}#{}#{}#{}",
            self.language, self.corpus, self.root, self.path, self.signature,
        )
    }
}

/// Role of a decoded anchor. Matches scry's precision-packed `kind`
/// byte (0 = decl, 1 = ref, 2 = call) so the emitter can pass it
/// through unmodified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Decl = 0,
    Ref  = 1,
    Call = 2,
}

/// One flushed anchor: a `[start, end)` span in `file_path` that
/// binds, refs, or calls `target_symbol`.
#[derive(Debug, Clone)]
pub struct DecodedRecord {
    pub file_path: String,
    pub start: u32,
    pub end: u32,
    pub target_symbol: String,
    pub role: Role,
}

/// Per-anchor accumulator. Each Kythe anchor needs four facts
/// (`/kythe/node/kind = "anchor"`, `/kythe/loc/start`, `/kythe/loc/end`)
/// plus at least one out-edge before we can emit. We flush as soon
/// as the first edge arrives if start+end are known, or when start +
/// end land after the first edge.
#[derive(Debug, Default)]
struct AnchorAccum {
    is_anchor: bool,
    start: Option<u32>,
    end: Option<u32>,
    /// Pending edges that haven't been emitted yet because start/end
    /// weren't known when they came in. Most indexers emit anchor
    /// facts before the edges, so this stays tiny.
    pending: Vec<(VName, Role)>,
}

/// Stream-decode Entry protos out of `reader`. Calls `sink` once
/// per flushed `DecodedRecord`. Returns the total Entry count read,
/// useful for the per-language summary.
pub fn decode_stream<R, F>(mut reader: R, mut sink: F) -> Result<u64>
where
    R: Read,
    F: FnMut(DecodedRecord),
{
    // Slurp the whole stream into memory. Indexer output for a single
    // CU is bounded (~hundreds of KB on a busy C++ TU); the kzip
    // driver runs one indexer per CU so we never hold the whole
    // multi-GB AOSP entry stream at once.
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).context("read indexer stdout")?;
    let mut cursor = 0usize;
    let mut anchors: HashMap<VName, AnchorAccum> = HashMap::new();
    let mut total: u64 = 0;
    while cursor < buf.len() {
        let (len, n) = read_varint(&buf[cursor..])
            .context("decode delimited Entry length prefix")?;
        cursor += n;
        if cursor + (len as usize) > buf.len() {
            return Err(anyhow::anyhow!(
                "truncated entry stream at byte {cursor}: want {len}, have {}",
                buf.len() - cursor,
            ));
        }
        let entry = Entry::parse_from_bytes(&buf[cursor..cursor + len as usize])
            .with_context(|| format!("decode Entry at byte {cursor}"))?;
        cursor += len as usize;
        total += 1;
        process_entry(&entry, &mut anchors, &mut sink);
    }
    // Drop the accumulator. Any anchors that didn't accumulate
    // start + end + at least one edge are incomplete — Kythe
    // routinely emits these for non-binding decls.
    drop(anchors);
    Ok(total)
}

fn process_entry<F: FnMut(DecodedRecord)>(
    entry: &Entry,
    anchors: &mut HashMap<VName, AnchorAccum>,
    sink: &mut F,
) {
    let Some(source) = entry.source.as_ref() else { return };
    let source_vn = VName::from_proto(source);
    if entry.edge_kind.is_empty() {
        // Node fact.
        match entry.fact_name.as_str() {
            "/kythe/node/kind" => {
                if entry.fact_value.as_slice() == b"anchor" {
                    let a = anchors.entry(source_vn.clone()).or_default();
                    a.is_anchor = true;
                    flush_ready(&source_vn, a, sink);
                }
            }
            "/kythe/loc/start" => {
                if let Some(v) = parse_ascii_u32(&entry.fact_value) {
                    let a = anchors.entry(source_vn.clone()).or_default();
                    a.start = Some(v);
                    flush_ready(&source_vn, a, sink);
                }
            }
            "/kythe/loc/end" => {
                if let Some(v) = parse_ascii_u32(&entry.fact_value) {
                    let a = anchors.entry(source_vn.clone()).or_default();
                    a.end = Some(v);
                    flush_ready(&source_vn, a, sink);
                }
            }
            _ => {} // facts we don't need
        }
    } else {
        // Edge. We only care about anchors-to-targets.
        let Some(target) = entry.target.as_ref() else { return };
        let target_vn = VName::from_proto(target);
        if target_vn.is_empty() { return; }
        let role = match entry.edge_kind.as_str() {
            // defines/binding is the canonical decl edge. defines (no
            // /binding) covers non-binding decls (e.g. forward decls).
            s if s == "/kythe/edge/defines/binding"
                || s.starts_with("/kythe/edge/defines/binding.")
                || s == "/kythe/edge/defines"
                || s.starts_with("/kythe/edge/defines.") => Role::Decl,
            s if s == "/kythe/edge/ref/call"
                || s.starts_with("/kythe/edge/ref/call.") => Role::Call,
            s if s == "/kythe/edge/ref"
                || s.starts_with("/kythe/edge/ref.")
                || s == "/kythe/edge/ref/writes"
                || s.starts_with("/kythe/edge/ref/writes.")
                || s == "/kythe/edge/ref/imports"
                || s.starts_with("/kythe/edge/ref/imports.") => Role::Ref,
            _ => return,
        };
        let a = anchors.entry(source_vn.clone()).or_default();
        if let Some(rec) = make_record(&source_vn, a, &target_vn, role) {
            sink(rec);
        } else {
            a.pending.push((target_vn, role));
        }
    }
}

/// Try to emit a single record for `(source, target, role)`. Returns
/// `None` if the anchor's facts (start, end, is_anchor) aren't all in
/// yet, or the source VName doesn't carry a path.
fn make_record(
    source: &VName,
    a: &AnchorAccum,
    target: &VName,
    role: Role,
) -> Option<DecodedRecord> {
    if !a.is_anchor { return None; }
    let start = a.start?;
    let end = a.end?;
    if source.path.is_empty() { return None; }
    Some(DecodedRecord {
        file_path: source.path.clone(),
        start, end,
        target_symbol: target.to_symbol_string(),
        role,
    })
}

/// When start/end/is_anchor have all just landed, flush any edges
/// that had been queued because they arrived before the facts.
fn flush_ready<F: FnMut(DecodedRecord)>(
    source: &VName,
    a: &mut AnchorAccum,
    sink: &mut F,
) {
    if !a.is_anchor || a.start.is_none() || a.end.is_none() {
        return;
    }
    if a.pending.is_empty() { return; }
    let pending = std::mem::take(&mut a.pending);
    for (target, role) in pending {
        if let Some(r) = make_record(source, a, &target, role) {
            sink(r);
        }
    }
}

/// Read a proto-style base-128 varint. Returns (value, bytes_consumed).
fn read_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i >= 10 {
            return Err(anyhow::anyhow!("varint > 10 bytes"));
        }
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((val, i + 1));
        }
        shift += 7;
    }
    Err(anyhow::anyhow!("truncated varint"))
}

/// Kythe encodes `/kythe/loc/start` and `/kythe/loc/end` fact_values
/// as ASCII decimal byte offsets. e.g. fact_value = b"42" for offset 42.
fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(bytes).ok()?;
    s.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::storage::{Entry, VName as PVName};
    use protobuf::Message;

    /// Helper: build a delimited Entry stream from a list of entries.
    fn write_stream(entries: &[Entry]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in entries {
            let bytes = e.write_to_bytes().unwrap();
            // varint length
            let mut len = bytes.len() as u64;
            loop {
                let mut b = (len & 0x7F) as u8;
                len >>= 7;
                if len != 0 { b |= 0x80; out.push(b); } else { out.push(b); break; }
            }
            out.extend_from_slice(&bytes);
        }
        out
    }

    fn vn(sig: &str, path: &str) -> PVName {
        let mut v = PVName::new();
        v.signature = sig.to_string();
        v.path = path.to_string();
        v.corpus = "test".to_string();
        v.language = "java".to_string();
        v
    }

    fn fact(source: PVName, name: &str, value: &[u8]) -> Entry {
        let mut e = Entry::new();
        e.source = protobuf::MessageField::some(source);
        e.fact_name = name.to_string();
        e.fact_value = value.to_vec();
        e
    }

    fn edge(source: PVName, target: PVName, kind: &str) -> Entry {
        let mut e = Entry::new();
        e.source = protobuf::MessageField::some(source);
        e.target = protobuf::MessageField::some(target);
        e.edge_kind = kind.to_string();
        // Kythe edge entries usually carry fact_name = "/" with no
        // fact_value — `process_entry`'s edge branch ignores facts.
        e.fact_name = "/".to_string();
        e
    }

    /// Anchor facts BEFORE the edge → flushed at edge-time.
    #[test]
    fn flush_when_facts_precede_edge() {
        let anchor = vn("a1", "src/Foo.java");
        let target = vn("Foo#", "src/Foo.java");
        let stream = write_stream(&[
            fact(anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(anchor.clone(), "/kythe/loc/start", b"100"),
            fact(anchor.clone(), "/kythe/loc/end",   b"103"),
            edge(anchor.clone(), target.clone(), "/kythe/edge/defines/binding"),
        ]);
        let mut out = Vec::new();
        let n = decode_stream(&stream[..], |r| out.push(r)).unwrap();
        assert_eq!(n, 4);
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r.file_path, "src/Foo.java");
        assert_eq!(r.start, 100);
        assert_eq!(r.end, 103);
        assert_eq!(r.role, Role::Decl);
        assert!(r.target_symbol.contains("Foo#"));
    }

    /// Anchor with only a ref edge (no defines) routes to Ref role.
    #[test]
    fn ref_edge_routes_to_ref_role() {
        let anchor = vn("a2", "src/Foo.java");
        let target = vn("Bar#", "src/Bar.java");
        let stream = write_stream(&[
            fact(anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(anchor.clone(), "/kythe/loc/start", b"42"),
            fact(anchor.clone(), "/kythe/loc/end",   b"45"),
            edge(anchor.clone(), target.clone(), "/kythe/edge/ref"),
        ]);
        let mut out = Vec::new();
        decode_stream(&stream[..], |r| out.push(r)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Ref);
    }

    /// `/kythe/edge/ref/call` routes to Call role.
    #[test]
    fn ref_call_routes_to_call_role() {
        let anchor = vn("a3", "src/Foo.java");
        let target = vn("bar()#", "src/Bar.java");
        let stream = write_stream(&[
            fact(anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(anchor.clone(), "/kythe/loc/start", b"7"),
            fact(anchor.clone(), "/kythe/loc/end",   b"10"),
            edge(anchor.clone(), target.clone(), "/kythe/edge/ref/call"),
        ]);
        let mut out = Vec::new();
        decode_stream(&stream[..], |r| out.push(r)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::Call);
    }

    /// Edge BEFORE the anchor facts → flushed once start+end+is_anchor land.
    #[test]
    fn flush_when_edge_precedes_facts() {
        let anchor = vn("a5", "src/Foo.java");
        let target = vn("Baz#", "src/Baz.java");
        let stream = write_stream(&[
            // Edge first — pending until facts land.
            edge(anchor.clone(), target.clone(), "/kythe/edge/ref"),
            fact(anchor.clone(), "/kythe/loc/end",   b"55"),
            fact(anchor.clone(), "/kythe/loc/start", b"50"),
            fact(anchor.clone(), "/kythe/node/kind", b"anchor"),
        ]);
        let mut out = Vec::new();
        decode_stream(&stream[..], |r| out.push(r)).unwrap();
        assert_eq!(out.len(), 1, "pending edge should have flushed once facts arrived");
        let r = &out[0];
        assert_eq!(r.role, Role::Ref);
        assert_eq!(r.start, 50);
        assert_eq!(r.end, 55);
        assert!(r.target_symbol.contains("Baz#"));
    }

    /// Anchor without an edge → no record emitted.
    #[test]
    fn anchor_without_edge_drops_silently() {
        let anchor = vn("a4", "src/Foo.java");
        let stream = write_stream(&[
            fact(anchor.clone(), "/kythe/node/kind", b"anchor"),
            fact(anchor.clone(), "/kythe/loc/start", b"0"),
            fact(anchor.clone(), "/kythe/loc/end",   b"3"),
        ]);
        let mut out = Vec::new();
        decode_stream(&stream[..], |r| out.push(r)).unwrap();
        assert!(out.is_empty());
    }

    /// Empty input → 0 entries, no records.
    #[test]
    fn empty_input_decodes_cleanly() {
        let mut out = Vec::new();
        let n = decode_stream(&[][..], |r| out.push(r)).unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    /// Truncated stream surfaces as an error.
    #[test]
    fn truncated_stream_errors() {
        // varint says "100 byte entry follows", buf has only 5 bytes
        let mut buf = vec![100u8];
        buf.extend_from_slice(b"trunc");
        let err = decode_stream(&buf[..], |_| {}).unwrap_err();
        assert!(format!("{err}").contains("truncated"));
    }

    #[test]
    fn varint_roundtrip_small_and_large() {
        // 1 byte
        assert_eq!(read_varint(&[0x05]).unwrap(), (5, 1));
        // 2 bytes
        assert_eq!(read_varint(&[0xAC, 0x02]).unwrap(), (300, 2));
        // 5 bytes (max u32)
        assert_eq!(read_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).unwrap(),
                   (u32::MAX as u64, 5));
    }
}
