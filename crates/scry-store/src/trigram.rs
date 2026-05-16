//! Trigram extraction + posting list machinery for "100× rg" grep.
//!
//! The pitch: for each file in the corpus, record the set of unique 3-byte
//! sequences (trigrams) that appear in it. At grep time, decompose the
//! literal pattern into its constituent trigrams, intersect their posting
//! lists, and only open the candidate files. For literal queries that match
//! few files, this is 100-1000× faster than scanning everything.
//!
//! Standard technique: Google Code Search ("Regular Expression Matching with
//! a Trigram Index", Russ Cox 2012), livegrep, Hound. Not research.
//!
//! ## On-disk format (planned, mirrors names.fst + name_postings.bin)
//!
//! `trigrams.fst`:        FST mapping 3-byte key → u64 offset into postings
//! `trigram_postings.bin`: per-trigram delta-encoded varint file_id list
//!
//! ## Memory model
//! - Index build: per-file `HashSet<[u8; 3]>` (~3-5 KB per typical 5 KB
//!   source file). Per-batch sorted (trigram, file_id) side-files, k-way
//!   merge at finalize (same machinery as name FST construction).
//! - Query: mmap the FST + postings. Per trigram lookup is a single u64
//!   read + varint decode. Intersection is k-way merge over sorted file_ids.
//!
//! ## Status
//! Module scaffold + extraction + tests. Wiring into the pipeline + the
//! writer/reader/finalize paths follows in a subsequent commit so this can
//! be reviewed independently.

use std::collections::HashSet;

/// 3-byte sequence identifying a position in the corpus. Stored as a fixed
/// [u8; 3] (NOT a u32) so the FST key encoding is straightforward — fst
/// crate keys are byte slices, and a [u8; 3] borrows directly.
pub type Trigram = [u8; 3];

/// Extract the set of unique trigrams from a byte slice. Skips bytes
/// containing 0x00 (typical of binary blobs that slipped past the walker's
/// classification) to avoid polluting the index with garbage.
///
/// Returns sorted Vec<Trigram> (sorted enables stable per-file order when
/// flushed to a chunk file and matches the FST's expected sort order during
/// k-way merge).
pub fn extract_sorted(bytes: &[u8]) -> Vec<Trigram> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    let mut set: HashSet<Trigram> = HashSet::with_capacity(bytes.len() / 4);
    let mut i = 0;
    let last = bytes.len() - 3;
    while i <= last {
        let t: Trigram = [bytes[i], bytes[i + 1], bytes[i + 2]];
        // Skip trigrams containing NUL — likely binary noise.
        if t[0] != 0 && t[1] != 0 && t[2] != 0 {
            set.insert(t);
        }
        i += 1;
    }
    let mut v: Vec<Trigram> = set.into_iter().collect();
    v.sort_unstable();
    v
}

/// Trigrammify a literal query pattern. Returns the trigrams that the
/// pattern's matching files MUST contain. For patterns shorter than 3
/// bytes, returns empty (query falls back to full scan).
///
/// For a pattern of length N >= 3, this returns the N-2 trigrams of
/// consecutive byte triples. Any file lacking ALL of these trigrams cannot
/// possibly match — that's the trim invariant.
pub fn trigrams_of_query(needle: &[u8]) -> Vec<Trigram> {
    if needle.len() < 3 {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(needle.len() - 2);
    for w in needle.windows(3) {
        v.push([w[0], w[1], w[2]]);
    }
    // Dedup adjacent (common for patterns like "aaa" / "TODO"). Full sort
    // not needed — the query path consumes these as a set, not in order.
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_basic() {
        let v = extract_sorted(b"abcabc");
        assert_eq!(v, vec![[b'a', b'b', b'c'], [b'b', b'c', b'a'], [b'c', b'a', b'b']]);
    }

    #[test]
    fn extract_short() {
        assert!(extract_sorted(b"ab").is_empty());
        assert!(extract_sorted(b"").is_empty());
    }

    #[test]
    fn extract_dedup() {
        // "aaaa" produces only ["aaa"] (one unique trigram).
        let v = extract_sorted(b"aaaa");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], [b'a', b'a', b'a']);
    }

    #[test]
    fn extract_skips_nul() {
        // NUL bytes are usually a sign of binary data; skip those trigrams.
        let v = extract_sorted(b"a\0b\0c\0d");
        assert!(v.iter().all(|t| t[0] != 0 && t[1] != 0 && t[2] != 0));
    }

    #[test]
    fn query_trigrams_basic() {
        let v = trigrams_of_query(b"hello");
        // "hel", "ell", "llo"
        assert_eq!(v.len(), 3);
        assert!(v.contains(&[b'h', b'e', b'l']));
        assert!(v.contains(&[b'e', b'l', b'l']));
        assert!(v.contains(&[b'l', b'l', b'o']));
    }

    #[test]
    fn query_short_falls_back() {
        // < 3 bytes: empty → caller does full scan
        assert!(trigrams_of_query(b"ab").is_empty());
        assert!(trigrams_of_query(b"").is_empty());
    }

    #[test]
    fn query_dedup() {
        // "aaaa" → "aaa" trigram appears in all 2 windows → deduped to 1
        let v = trigrams_of_query(b"aaaa");
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn intersection_principle() {
        // If a file has trigrams {abc, bcd, cde}, and the query's required
        // trigrams are {abc, bcd}, the file IS a candidate (superset).
        // If query trigrams are {abc, xyz}, file is NOT a candidate (missing xyz).
        let file = extract_sorted(b"abcde");
        let q1 = trigrams_of_query(b"abcd");
        let q2 = trigrams_of_query(b"abcxyz");

        let in_file: HashSet<Trigram> = file.iter().copied().collect();
        let q1_set: HashSet<Trigram> = q1.iter().copied().collect();
        let q2_set: HashSet<Trigram> = q2.iter().copied().collect();

        assert!(q1_set.is_subset(&in_file), "abcd's trigrams should all be in abcde");
        assert!(!q2_set.is_subset(&in_file), "abcxyz's trigrams should NOT all be in abcde");
    }
}
