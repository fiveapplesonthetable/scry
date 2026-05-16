//! Embedding-based semantic retrieval (ROADMAP #1, foundation).
//!
//! `scry ask "how do I parse TOML"` answers queries that scry's
//! lexical paths (def / grep / fuzzy) can't: questions where the
//! agent doesn't know what identifier to search for. The pipeline:
//!
//!   build-embeddings:
//!     each indexed file →
//!       chunk into overlapping 100-line windows →
//!       embed each chunk to a dense `dim`-float vector →
//!       L2-normalize → write to embeddings.bin (one row per chunk).
//!
//!   ask:
//!     query → embed to one row → cosine similarity against every
//!     embedding (brute force — O(N) but small constants thanks to
//!     SIMD-friendly contiguous mmap layout) → top-K.
//!
//! ## Embedding model — deterministic feature hashing
//!
//! The default model is a deterministic hash-based bag-of-tokens
//! embedding: tokenize on non-alphanumeric, lowercase, hash each
//! token into a fixed dimension, accumulate counts, L2-normalize.
//! This is the "hashing trick" (Weinberger et al., 2009) — well-known
//! to produce competitive retrieval at a fraction of the cost of
//! a transformer. It captures vocabulary overlap (the dominant
//! signal for code search) and is identical across machines without
//! requiring a model download.
//!
//! A future commit can swap in a real transformer embedding behind
//! a feature flag without changing the wire shape — the sidecar
//! format stays the same; the model just produces denser vectors.
//!
//! ## On-disk format
//!
//! `chunks.bin`     bincode-encoded `Vec<ChunkEntry>`. Sorted by
//!                  (file_id, start_line). One entry per chunk.
//!
//! `embeddings.bin` Header (8 bytes):
//!                    u32 LE dim
//!                    u32 LE chunk_count
//!                  Body: `chunk_count` rows × `dim` × f32 LE.
//!                  Stored as f32 (not f16) because (a) most query
//!                  hosts have plenty of disk, (b) f32 cosine is
//!                  bit-exact across runs, which matters for
//!                  reproducibility of ranked output.

use serde::{Deserialize, Serialize};

/// One indexed chunk: a window of source lines from a single file.
/// chunk_idx is implicit (the position in the chunks.bin Vec).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkEntry {
    pub file_id: u32,
    pub start_line: u32,
    pub end_line: u32,
}

/// Default embedding dimension. 64 is enough for the hashing trick
/// to discriminate code chunks while keeping the embeddings.bin file
/// under 1 GB on the full AOSP+Linux corpus.
pub const DEFAULT_DIM: usize = 64;

/// Default chunk window in lines. Standard RAG sizing for code.
pub const DEFAULT_CHUNK_LINES: usize = 100;
/// Overlap between consecutive chunks. Captures definitions that
/// straddle chunk boundaries (the canonical example: a long Java
/// class with multi-line methods).
pub const DEFAULT_CHUNK_OVERLAP: usize = 20;

/// Chunk a source string into overlapping line windows.
/// Returns `Vec<(start_line, end_line, body)>` where line numbers are
/// 1-based and end_line is inclusive (matches the rest of scry's
/// `SymbolRecord` convention). Each body slice borrows from `source`.
pub fn chunk_lines(source: &str, chunk_lines: usize, overlap: usize) -> Vec<(u32, u32, &str)> {
    if source.is_empty() || chunk_lines == 0 {
        return Vec::new();
    }
    // Build a Vec of line-start byte offsets so we can slice in O(1)
    // per chunk rather than scanning each window.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let total_lines = line_starts.len();
    let stride = chunk_lines.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < total_lines {
        let end = (start + chunk_lines).min(total_lines);
        let byte_start = line_starts[start];
        let byte_end = if end < total_lines {
            line_starts[end]
        } else {
            source.len()
        };
        let body = &source[byte_start..byte_end];
        out.push(((start + 1) as u32, end as u32, body));
        if end == total_lines { break; }
        start += stride;
    }
    out
}

/// Compute the hashing-trick embedding for a single text chunk.
/// Tokenization: split on non-alphanumeric ASCII, lowercase. Hash
/// each token into a dim-sized bucket; accumulate +1.0; L2-normalize.
/// Deterministic across machines — uses FNV-1a, no global state.
pub fn embed_text(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    if dim == 0 { return v; }
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if !c.is_ascii_alphanumeric() && c != b'_' {
            if start < i {
                let h = fnv1a_lower(&bytes[start..i]) as usize;
                v[h % dim] += 1.0;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        let h = fnv1a_lower(&bytes[start..]) as usize;
        v[h % dim] += 1.0;
    }
    l2_normalize_inplace(&mut v);
    v
}

/// L2-normalize a vector in place. After this, `dot(v, v) == 1.0`
/// (modulo floating point), so cosine similarity reduces to a plain
/// dot product. Pure helper, public for the query path that does
/// the same normalization on the query vector.
pub fn l2_normalize_inplace(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity between two L2-normalized vectors. Since both
/// inputs are unit-norm, this is just a dot product. Public so the
/// reader can compute query-vs-chunk similarity using the same
/// kernel as the writer's normalization.
pub fn cosine_unit(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = 0.0f32;
    for i in 0..n {
        acc += a[i] * b[i];
    }
    acc
}

/// FNV-1a hash over the lowercased view of `bytes`. Deterministic,
/// no allocations, ~3 GB/s. Chosen over more sophisticated hashes
/// because the hashing trick's quality is bounded by `dim`, not by
/// hash quality — any decent hash works equally well.
fn fnv1a_lower(bytes: &[u8]) -> u32 {
    const FNV_OFFSET: u32 = 2_166_136_261;
    const FNV_PRIME: u32 = 16_777_619;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        let lower = if b.is_ascii_uppercase() { b + 32 } else { b };
        h ^= lower as u32;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_lines_simple() {
        let src = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        // 10 lines, chunk size 4, overlap 1 -> stride 3 -> windows:
        //   [1..4], [4..7], [7..10], [10..10] (last is empty? no — stop)
        let chunks = chunk_lines(src, 4, 1);
        assert!(!chunks.is_empty());
        // First chunk starts at line 1.
        assert_eq!(chunks[0].0, 1);
        // Sliding stride is 3, not 4 — chunks overlap by 1 line.
        if chunks.len() >= 2 {
            assert_eq!(chunks[1].0, 4);
        }
    }

    #[test]
    fn chunk_lines_handles_empty() {
        assert!(chunk_lines("", 10, 2).is_empty());
        assert!(chunk_lines("foo\n", 0, 0).is_empty());
    }

    #[test]
    fn chunk_lines_short_input_one_chunk() {
        let src = "only one line";
        let chunks = chunk_lines(src, 100, 20);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, 1);
    }

    #[test]
    fn embed_text_deterministic() {
        let a = embed_text("hello world", 16);
        let b = embed_text("hello world", 16);
        assert_eq!(a, b, "embedding must be deterministic for the same input");
    }

    #[test]
    fn embed_text_case_insensitive() {
        let lower = embed_text("Hello World", 16);
        let mixed = embed_text("HELLO world", 16);
        assert_eq!(lower, mixed, "tokenization should fold case");
    }

    #[test]
    fn embed_text_unit_norm() {
        let v = embed_text("the quick brown fox jumps over the lazy dog", 32);
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5 || n == 0.0,
                "expected unit norm, got {n}");
    }

    /// Cosine similarity captures the obvious intuition: same text
    /// has similarity 1.0; sharing a token boosts similarity; entirely
    /// disjoint tokens stay low.
    #[test]
    fn cosine_orders_similar_above_disjoint() {
        let q = embed_text("parse toml configuration", 64);
        let same = embed_text("parse toml configuration", 64);
        let related = embed_text("parse toml file contents", 64);
        let unrelated = embed_text("xyz qrs wvu", 64);
        assert!((cosine_unit(&q, &same) - 1.0).abs() < 1e-4);
        let sim_related = cosine_unit(&q, &related);
        let sim_unrelated = cosine_unit(&q, &unrelated);
        assert!(sim_related > sim_unrelated,
                "related ({sim_related}) should outscore unrelated ({sim_unrelated})");
    }

    /// Empty text → all-zero vector → cosine with anything is 0.
    /// Important so we don't crash on empty file chunks.
    #[test]
    fn empty_text_yields_zero_vector() {
        let v = embed_text("", 16);
        assert!(v.iter().all(|&x| x == 0.0));
        let other = embed_text("anything", 16);
        assert_eq!(cosine_unit(&v, &other), 0.0);
    }
}
