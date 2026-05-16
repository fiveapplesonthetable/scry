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
        mut resolve_file_id: impl FnMut(&str) -> Option<u32>,
    ) -> Self {
        let modules = v.modules;
        let n_modules = modules.len();
        let stride = n_modules.div_ceil(64);
        // Allocate the reachability bitmap. Start with the
        // reflexive closure (every module reaches itself) and the
        // direct edges; we'll compute the transitive closure below.
        let mut reach = vec![0u64; n_modules * stride.max(1)];
        for m in &modules {
            set_bit(&mut reach, stride, m.id as usize, m.id as usize);
        }
        for [from, to] in &v.deps {
            let (from, to) = (*from as usize, *to as usize);
            if from == to || from >= n_modules || to >= n_modules {
                continue;
            }
            set_bit(&mut reach, stride, from, to);
        }
        // Transitive closure via repeated dep-frontier propagation.
        // For each module `from`, OR in the reach-set of every
        // module currently reachable. Repeat until stable. This is
        // Warshall's algorithm in bitmap form (O(n³/64), but n
        // is in the thousands and the per-row OR is wide-SIMD-
        // friendly, so it's plenty fast for AOSP-scale graphs).
        let mut changed = true;
        while changed {
            changed = false;
            for from in 0..n_modules {
                // Snapshot the current row (avoid borrow conflict
                // while ORing in other rows).
                let snapshot: Vec<u64> = reach[from * stride..(from + 1) * stride].to_vec();
                for (w, &word) in snapshot.iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                        let to = w * 64 + bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        if to >= n_modules || to == from {
                            continue;
                        }
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

        // Build the file→module table.
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
}
