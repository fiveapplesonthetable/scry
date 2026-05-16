//! Build-system-agnostic module graph + precomputed reachability bitmap.
//!
//! This is the foundation for `--precise` queries in v0.1.12. A
//! `ModuleGraph` represents the build system's notion of "module that
//! owns this file" and "which modules depend on which". Filters like
//! "find callers of bindService in modules that can actually reach
//! the framework module" are an O(1) bitmap intersection on top of
//! this structure.
//!
//! ## Build-system-agnostic by design
//!
//! Adapters convert Soong `module-graph.json` / GN `--ide=json` /
//! Linux `Makefile`+`Kconfig` into this canonical representation.
//! The core data model and reachability algorithm don't care which
//! build system produced the graph — they just see `(modules,
//! dep-edges, file → module)`.
//!
//! ## Canonical schema
//!
//! On-disk JSON (the v1 format scry's adapters produce):
//!
//! ```json
//! {
//!   "version": 1,
//!   "modules": [
//!     {"id": 0, "name": "framework-minus-apex", "partition": "system"},
//!     {"id": 1, "name": "libbinder",            "partition": "system"},
//!     ...
//!   ],
//!   "deps": [
//!     [0, 1], [0, 2], [3, 0], ...
//!   ],
//!   "files": [
//!     {"path": "frameworks/base/.../FooManager.java", "module_id": 0},
//!     ...
//!   ]
//! }
//! ```
//!
//! The packed binary sidecar (`module_graph.bin`) stores the same
//! information plus a precomputed transitive-closure bitmap; the
//! sidecar is what the query path reads (mmap'd, O(1) lookup).

use serde::Deserialize;
use std::collections::HashMap;

/// One module in the build graph. The name is what the build system
/// calls it (Soong module name, GN target name, kernel subdir, etc.);
/// scry uses it only for display and as a key. The id is dense and
/// stable across a single index (referenced by file→module mappings
/// and by the reachability bitmap).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Module {
    pub id: u32,
    pub name: String,
    /// Optional build-system-specific partition string. For Soong:
    /// `"system" / "vendor" / "product" / "system_ext" / "odm"`.
    /// For Linux: kernel subsystem ("drivers/net", "fs/btrfs"). For
    /// GN: empty. Used only for display + filtering at query time.
    #[serde(default)]
    pub partition: Option<String>,
}

/// A canonical, build-system-agnostic module dependency graph plus
/// the file→module attribution layer. Construct via [`Self::from_json`]
/// (for adapter output) or [`Self::new`] (for tests / fixtures).
///
/// Reachability is precomputed once at construction time and stored
/// as a packed bitmap: `is_reachable(from, to)` is O(1).
#[derive(Debug, Clone)]
pub struct ModuleGraph {
    pub modules: Vec<Module>,
    /// `file_module[file_id]` = Some(module_id) if the file is owned
    /// by a known module; None otherwise (e.g. generated code outside
    /// any build target, third-party code not in the compdb).
    pub file_module: Vec<Option<u32>>,
    /// Reachability bitmap. `reach[from * stride + (to / 64)]` has
    /// bit `to % 64` set iff module `from` transitively depends on
    /// module `to` (including reflexively — every module reaches
    /// itself). `stride = (n_modules + 63) / 64`.
    reach: Vec<u64>,
    stride: usize,
    name_to_id: HashMap<String, u32>,
}

/// Raw JSON form read from a v1 module-graph file. Adapters emit this
/// shape; we deserialize via serde then compact into [`ModuleGraph`].
#[derive(Debug, Deserialize)]
pub struct ModuleGraphJsonV1 {
    pub version: u32,
    pub modules: Vec<Module>,
    /// Edges as `[from_id, to_id]` pairs. Multiple entries with the
    /// same pair are deduplicated; self-loops are ignored.
    pub deps: Vec<[u32; 2]>,
    pub files: Vec<FileAttr>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FileAttr {
    pub path: String,
    pub module_id: u32,
}

impl ModuleGraph {
    /// Build a graph from the parsed v1 JSON form. `total_files` is
    /// the file-id space of the scry index this graph attaches to;
    /// `file_module[i]` defaults to `None` for ids not mentioned in
    /// the input. `resolve_file_id` maps an input file path (as the
    /// adapter recorded it) to the scry index's u32 file_id; if the
    /// adapter's path doesn't resolve, the attribution is dropped.
    pub fn from_json_v1(
        v: ModuleGraphJsonV1,
        total_files: usize,
        resolve_file_id: impl FnMut(&str) -> Option<u32>,
    ) -> Self {
        Self::from_json_v1_with_cache(v, total_files, resolve_file_id, None)
    }

    /// Same as [`from_json_v1`] but consults an on-disk reachability
    /// bitmap cache. The cache stores the full Warshall closure
    /// (the ~1GB hot loop) keyed by a hash bound to the input. On
    /// hit we skip Warshall entirely and proceed straight to file
    /// attribution; on miss we compute as usual AND write the cache
    /// for the next reader. The Warshall closure is the only slow
    /// part of [`from_json_v1`] — everything else is sub-second
    /// even on AOSP-scale graphs.
    pub fn from_json_v1_with_cache(
        v: ModuleGraphJsonV1,
        total_files: usize,
        mut resolve_file_id: impl FnMut(&str) -> Option<u32>,
        cache: Option<ReachCache<'_>>,
    ) -> Self {
        let modules = v.modules;
        let n_modules = modules.len();
        let stride = n_modules.div_ceil(64);

        let reach = match cache.as_ref().and_then(|c| c.try_load(n_modules, stride)) {
            Some(loaded) => loaded,
            None => {
                let reach = compute_reach_bitmap(&modules, &v.deps, stride);
                if let Some(c) = cache.as_ref() {
                    if let Err(e) = c.write(&reach, n_modules, stride) {
                        eprintln!(
                            "[modgraph] failed to write reach cache to {}: {e} \
                             (queries still work; next open will re-Warshall)",
                            c.path.display(),
                        );
                    }
                }
                reach
            }
        };

        let mut file_module = vec![None; total_files];
        for fa in &v.files {
            if let Some(fid) = resolve_file_id(&fa.path) {
                if (fid as usize) < total_files && (fa.module_id as usize) < n_modules {
                    file_module[fid as usize] = Some(fa.module_id);
                }
            }
        }

        let name_to_id: HashMap<String, u32> = modules
            .iter()
            .map(|m| (m.name.clone(), m.id))
            .collect();

        ModuleGraph { modules, file_module, reach, stride, name_to_id }
    }

    /// Test-fixture constructor. Skips JSON parsing; useful for unit
    /// tests of the reachability + filter paths.
    pub fn new(
        modules: Vec<Module>,
        deps: &[(u32, u32)],
        file_attr: Vec<Option<u32>>,
    ) -> Self {
        let json = ModuleGraphJsonV1 {
            version: 1,
            modules,
            deps: deps.iter().map(|&(a, b)| [a, b]).collect(),
            files: file_attr
                .iter()
                .enumerate()
                .filter_map(|(i, m)| m.map(|mid| FileAttr {
                    path: format!("#test#{i}"),
                    module_id: mid,
                }))
                .collect(),
        };
        let n = file_attr.len();
        let mut fa_iter = file_attr.into_iter().enumerate();
        Self::from_json_v1(json, n, move |path| {
            // Test path format: "#test#<id>" lets us map back.
            if let Some(suffix) = path.strip_prefix("#test#") {
                if let Ok(idx) = suffix.parse::<u32>() {
                    // Walk forward through the iter once per call —
                    // tests use small graphs, so O(n) per resolve is fine.
                    for (i, _) in fa_iter.by_ref() {
                        if i == idx as usize { return Some(idx); }
                    }
                }
            }
            None
        })
    }

    /// Number of modules. Stable across the lifetime of this graph.
    pub fn n_modules(&self) -> usize { self.modules.len() }

    /// Resolve a build-system module name to its dense id.
    pub fn module_id(&self, name: &str) -> Option<u32> {
        self.name_to_id.get(name).copied()
    }

    /// Owning module of an indexed file. `None` if the file is not
    /// attributed to any module (e.g. third-party code that the
    /// adapter didn't see).
    pub fn module_of_file(&self, file_id: u32) -> Option<u32> {
        self.file_module.get(file_id as usize).and_then(|m| *m)
    }

    /// Is module `from` reachable to module `to` through the
    /// transitive dependency graph? Reflexive — a module reaches
    /// itself. O(1) bitmap lookup.
    pub fn is_reachable(&self, from: u32, to: u32) -> bool {
        let (f, t) = (from as usize, to as usize);
        if f >= self.modules.len() || t >= self.modules.len() {
            return false;
        }
        let word = self.reach[f * self.stride + (t / 64)];
        (word >> (t % 64)) & 1 == 1
    }

    /// Convenience: is a caller-file reachable to a callee-file's
    /// owning module? Used by the `--precise` filter on `callers` /
    /// `ref` to drop cross-module name-matches that the build graph
    /// proves can't actually link. A file with no module attribution
    /// always passes (we can't prove unreachability without data).
    pub fn caller_can_reach_callee(
        &self,
        caller_file_id: u32,
        callee_file_id: u32,
    ) -> bool {
        match (self.module_of_file(caller_file_id), self.module_of_file(callee_file_id)) {
            (Some(c), Some(t)) => self.is_reachable(c, t),
            _ => true,
        }
    }
}

fn set_bit(reach: &mut [u64], stride: usize, from: usize, to: usize) {
    reach[from * stride + (to / 64)] |= 1u64 << (to % 64);
}

/// Compute the reachability bitmap via bitmap-Warshall. Extracted
/// from the constructor so the cache path can skip it. O(n³/64)
/// but the per-row OR is SIMD-friendly; ~30s on AOSP's 91k modules,
/// which is exactly why we cache it on disk.
fn compute_reach_bitmap(
    modules: &[Module],
    deps: &[[u32; 2]],
    stride: usize,
) -> Vec<u64> {
    let n_modules = modules.len();
    let mut reach = vec![0u64; n_modules * stride.max(1)];
    for m in modules {
        set_bit(&mut reach, stride, m.id as usize, m.id as usize);
    }
    for [from, to] in deps {
        let (from, to) = (*from as usize, *to as usize);
        if from == to || from >= n_modules || to >= n_modules { continue; }
        set_bit(&mut reach, stride, from, to);
    }
    let mut changed = true;
    while changed {
        changed = false;
        for from in 0..n_modules {
            let snapshot: Vec<u64> = reach[from * stride..(from + 1) * stride].to_vec();
            for (w, &word) in snapshot.iter().enumerate() {
                let mut bits = word;
                while bits != 0 {
                    let to = w * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if to >= n_modules || to == from { continue; }
                    for k in 0..stride {
                        let new = reach[from * stride + k] | reach[to * stride + k];
                        if new != reach[from * stride + k] {
                            reach[from * stride + k] = new;
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    reach
}

/// On-disk cache for the reachability bitmap. The bitmap dominates
/// `ModuleGraph` construction cost on AOSP-scale graphs (Warshall
/// is ~30s; everything else is sub-second), so caching it shaves
/// the same 30s off every cold `--reachable` query.
///
/// File layout (little-endian):
///   bytes  0..  9   magic = b"scryREAC1"
///   bytes  9.. 13   format version (u32)
///   bytes 13.. 21   n_modules (u64)
///   bytes 21.. 29   stride (u64)
///   bytes 29.. 61   binding hash (32 bytes) — bound to the
///                   producing module_graph.json's contents
///   bytes 61..      raw u64 bitmap (n_modules * stride * 8 bytes)
///
/// On load we verify magic + version + binding hash + dimensions.
/// Any mismatch is silent: load returns None and the caller falls
/// back to a full Warshall compute (and writes a fresh cache).
pub struct ReachCache<'a> {
    pub path: &'a std::path::Path,
    /// Binding hash from the input. Typically `blake3(module_graph.json)`;
    /// any 32-byte value works as long as it changes when the graph
    /// changes. Stored in the cache header and compared on load.
    pub binding_hash: [u8; 32],
}

const REACH_CACHE_MAGIC: &[u8; 9] = b"scryREAC1";
const REACH_CACHE_VERSION: u32 = 1;
const REACH_CACHE_HEADER_LEN: usize = 9 + 4 + 8 + 8 + 32;

impl ReachCache<'_> {
    /// Attempt to load the bitmap. Returns None on any mismatch
    /// (missing file, wrong magic / version / hash / dims). The
    /// caller treats all these the same: recompute Warshall.
    fn try_load(&self, n_modules: usize, stride: usize) -> Option<Vec<u64>> {
        let bytes = std::fs::read(self.path).ok()?;
        if bytes.len() < REACH_CACHE_HEADER_LEN { return None; }
        if &bytes[..9] != REACH_CACHE_MAGIC { return None; }
        let version = u32::from_le_bytes(bytes[9..13].try_into().ok()?);
        if version != REACH_CACHE_VERSION { return None; }
        let cached_n = u64::from_le_bytes(bytes[13..21].try_into().ok()?) as usize;
        let cached_stride = u64::from_le_bytes(bytes[21..29].try_into().ok()?) as usize;
        if cached_n != n_modules || cached_stride != stride { return None; }
        if bytes[29..61] != self.binding_hash { return None; }
        let payload_bytes = n_modules.checked_mul(stride)?.checked_mul(8)?;
        let payload = bytes.get(REACH_CACHE_HEADER_LEN..REACH_CACHE_HEADER_LEN + payload_bytes)?;
        // Decode the raw u64 LE words.
        let mut reach = Vec::with_capacity(n_modules * stride);
        for chunk in payload.chunks_exact(8) {
            reach.push(u64::from_le_bytes(chunk.try_into().ok()?));
        }
        Some(reach)
    }

    /// Atomically write the cache. Uses tmp + rename so a partial
    /// write doesn't leave a corrupt file behind.
    fn write(&self, reach: &[u64], n_modules: usize, stride: usize) -> std::io::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("bin.tmp");
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let f = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::new(f);
        w.write_all(REACH_CACHE_MAGIC)?;
        w.write_all(&REACH_CACHE_VERSION.to_le_bytes())?;
        w.write_all(&(n_modules as u64).to_le_bytes())?;
        w.write_all(&(stride as u64).to_le_bytes())?;
        w.write_all(&self.binding_hash)?;
        for &word in reach {
            w.write_all(&word.to_le_bytes())?;
        }
        w.flush()?;
        drop(w);
        std::fs::rename(&tmp, self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u32, name: &str) -> Module {
        Module { id, name: name.to_string(), partition: None }
    }

    #[test]
    fn reachability_is_reflexive() {
        let g = ModuleGraph::new(vec![m(0, "a"), m(1, "b")], &[], vec![None, None]);
        assert!(g.is_reachable(0, 0));
        assert!(g.is_reachable(1, 1));
    }

    #[test]
    fn reachability_handles_direct_and_transitive_edges() {
        // a → b → c → d
        let g = ModuleGraph::new(
            vec![m(0, "a"), m(1, "b"), m(2, "c"), m(3, "d")],
            &[(0, 1), (1, 2), (2, 3)],
            vec![None, None, None, None],
        );
        assert!(g.is_reachable(0, 1));   // direct
        assert!(g.is_reachable(0, 2));   // transitive
        assert!(g.is_reachable(0, 3));   // doubly transitive
        // No reverse edges.
        assert!(!g.is_reachable(1, 0));
        assert!(!g.is_reachable(2, 0));
        assert!(!g.is_reachable(3, 0));
    }

    #[test]
    fn reachability_handles_cycle() {
        // a → b → c → a (cycle); plus a separate d unrelated.
        let g = ModuleGraph::new(
            vec![m(0, "a"), m(1, "b"), m(2, "c"), m(3, "d")],
            &[(0, 1), (1, 2), (2, 0)],
            vec![None, None, None, None],
        );
        // Every node in the cycle reaches every other in the cycle.
        for from in 0..3 {
            for to in 0..3 {
                assert!(g.is_reachable(from, to),
                        "{from} should reach {to} in cycle");
            }
        }
        // d is isolated.
        assert!(!g.is_reachable(0, 3));
        assert!(!g.is_reachable(3, 0));
        assert!(g.is_reachable(3, 3));
    }

    #[test]
    fn caller_can_reach_callee_uses_file_attribution() {
        // framework → libbinder; file 0 in framework, file 1 in libbinder,
        // file 2 in unrelated "vendor".
        let g = ModuleGraph::new(
            vec![m(0, "framework"), m(1, "libbinder"), m(2, "vendor")],
            &[(0, 1)],
            vec![Some(0), Some(1), Some(2)],
        );
        // framework caller → libbinder callee: yes (direct dep)
        assert!(g.caller_can_reach_callee(0, 1));
        // libbinder caller → framework callee: no (no reverse edge)
        assert!(!g.caller_can_reach_callee(1, 0));
        // vendor caller → libbinder callee: no
        assert!(!g.caller_can_reach_callee(2, 1));
        // framework → vendor: no
        assert!(!g.caller_can_reach_callee(0, 2));
        // Self-reach: yes
        assert!(g.caller_can_reach_callee(0, 0));
    }

    #[test]
    fn unattributed_files_pass_through() {
        // No file attribution at all → every query passes (we can't
        // prove unreachability without data).
        let g = ModuleGraph::new(
            vec![m(0, "a"), m(1, "b")],
            &[],
            vec![None, None, None],
        );
        assert!(g.caller_can_reach_callee(0, 1));
        assert!(g.caller_can_reach_callee(1, 0));
        // Even cross-file with no attribution.
        assert!(g.caller_can_reach_callee(2, 2));
    }

    #[test]
    fn module_id_lookup_by_name() {
        let g = ModuleGraph::new(
            vec![m(0, "framework"), m(1, "libbinder")],
            &[],
            vec![],
        );
        assert_eq!(g.module_id("framework"), Some(0));
        assert_eq!(g.module_id("libbinder"), Some(1));
        assert_eq!(g.module_id("nonexistent"), None);
    }

    #[test]
    fn json_v1_roundtrips() {
        let json = r#"{
            "version": 1,
            "modules": [
                {"id": 0, "name": "fw", "partition": "system"},
                {"id": 1, "name": "lib", "partition": "system"}
            ],
            "deps": [[0, 1]],
            "files": [
                {"path": "fw/Foo.java", "module_id": 0},
                {"path": "lib/Bar.cpp", "module_id": 1}
            ]
        }"#;
        let v: ModuleGraphJsonV1 = serde_json::from_str(json).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(v.modules.len(), 2);
        assert_eq!(v.deps.len(), 1);
        assert_eq!(v.files.len(), 2);
        // Map paths to a synthetic file_id by their index.
        let g = ModuleGraph::from_json_v1(v, 2, |p| match p {
            "fw/Foo.java" => Some(0),
            "lib/Bar.cpp" => Some(1),
            _ => None,
        });
        assert_eq!(g.n_modules(), 2);
        assert_eq!(g.module_of_file(0), Some(0));
        assert_eq!(g.module_of_file(1), Some(1));
        assert!(g.is_reachable(0, 1));
        assert!(!g.is_reachable(1, 0));
        assert_eq!(g.modules[0].partition.as_deref(), Some("system"));
    }

    /// Build a 3-module graph A → B → C, write its reach bitmap
    /// to disk via ReachCache, round-trip-load it, and assert
    /// the bitmap matches.
    #[test]
    fn reach_cache_roundtrip_loads_same_bitmap() {
        let tmp_dir = std::env::temp_dir().join(format!("scry-reachcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cache_path = tmp_dir.join("reach.bin");
        let json = ModuleGraphJsonV1 {
            version: 1,
            modules: vec![m(0, "A"), m(1, "B"), m(2, "C")],
            deps: vec![[0, 1], [1, 2]],
            files: vec![],
        };
        let binding_hash = *blake3::hash(b"v1").as_bytes();

        // First build: cache miss → compute Warshall, write cache.
        let cache = ReachCache { path: &cache_path, binding_hash };
        let g1 = ModuleGraph::from_json_v1_with_cache(json, 0, |_| None, Some(cache));
        assert!(g1.is_reachable(0, 2), "A reaches C transitively");
        assert!(cache_path.exists(), "first build must write the cache");

        // Second build: same hash → cache hit. Use a JSON with the
        // SAME shape (deps don't matter once cache hits) but flip
        // the deps to confirm the loader actually used the cached
        // bitmap (if it recomputed it would lose A → C).
        let json2 = ModuleGraphJsonV1 {
            version: 1,
            modules: vec![m(0, "A"), m(1, "B"), m(2, "C")],
            deps: vec![],       // recompute would give NO transitive reach
            files: vec![],
        };
        let cache2 = ReachCache { path: &cache_path, binding_hash };
        let g2 = ModuleGraph::from_json_v1_with_cache(json2, 0, |_| None, Some(cache2));
        assert!(g2.is_reachable(0, 2),
            "cache hit should preserve the A→C edge from g1");

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    // Local clone helper for tests (kept inside the test module
    // so it doesn't add an impl after the test module — clippy
    // bans that as "items after a test module").
    fn clone_json(j: &ModuleGraphJsonV1) -> ModuleGraphJsonV1 {
        ModuleGraphJsonV1 {
            version: j.version,
            modules: j.modules.clone(),
            deps: j.deps.clone(),
            files: j.files.clone(),
        }
    }

    /// A cache built for a different binding hash must be ignored.
    /// Confirms automatic invalidation when module_graph.json
    /// content changes.
    #[test]
    fn reach_cache_wrong_binding_hash_is_ignored() {
        let tmp_dir = std::env::temp_dir().join(format!("scry-reachhash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let cache_path = tmp_dir.join("reach.bin");
        let json = ModuleGraphJsonV1 {
            version: 1,
            modules: vec![m(0, "A"), m(1, "B"), m(2, "C")],
            deps: vec![[0, 1], [1, 2]],
            files: vec![],
        };
        let cache1 = ReachCache { path: &cache_path, binding_hash: *blake3::hash(b"v1").as_bytes() };
        let _g1 = ModuleGraph::from_json_v1_with_cache(clone_json(&json), 0, |_| None, Some(cache1));

        // Open with a DIFFERENT hash + empty deps: should recompute,
        // and the recomputed bitmap reflects empty deps (no A→C).
        let json_empty = ModuleGraphJsonV1 { deps: vec![], ..json };
        let cache2 = ReachCache { path: &cache_path, binding_hash: *blake3::hash(b"v2").as_bytes() };
        let g2 = ModuleGraph::from_json_v1_with_cache(json_empty, 0, |_| None, Some(cache2));
        assert!(!g2.is_reachable(0, 2),
            "wrong binding hash should force recompute; with empty deps A doesn't reach C");

        std::fs::remove_dir_all(&tmp_dir).ok();
    }
}
