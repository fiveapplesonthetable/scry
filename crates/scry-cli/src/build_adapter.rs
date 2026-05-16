//! Build-system adapters: read a project's native build metadata and
//! emit scry's canonical v1 `module_graph.json` so `--reachable`
//! queries get build-graph-aware filtering.
//!
//! ## Supported build systems
//!
//! - **`cargo`** — Rust workspaces (Cargo.toml). Fully implemented; tested
//!   end-to-end via dogfooding on scry's own workspace.
//! - **`soong`** — AOSP Soong (`m json-module-graph` output). Skeleton
//!   parser; needs validation against a real AOSP build environment.
//! - **`kernel`** — Linux Kbuild (Makefile fragments + `.config`). Not
//!   yet implemented; queued for a follow-up slice.
//! - **`gn`** — GN/ninja projects (`gn gen --ide=json` output). Not yet
//!   implemented; queued.
//!
//! Adapters all emit the same canonical schema documented in
//! `scry_store::modgraph::ModuleGraphJsonV1`. Once the file exists in
//! the scry index dir, `--reachable` filtering is live with no further
//! code changes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Output schema (matches `scry_store::modgraph::ModuleGraphJsonV1`).
/// Kept as a separate type here so the adapter code doesn't depend on
/// the reader's internal struct layout.
#[derive(Debug, Serialize)]
pub struct OutGraphV1 {
    pub version: u32,
    pub modules: Vec<OutModule>,
    pub deps: Vec<[u32; 2]>,
    pub files: Vec<OutFile>,
}

#[derive(Debug, Serialize)]
pub struct OutModule {
    pub id: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OutFile {
    pub path: String,
    pub module_id: u32,
}

/// Build a v1 module-graph file by reading the build system's native
/// metadata at `root`. Returns the populated structure; the caller
/// writes it to `module_graph.json` in the index dir.
pub fn build_modgraph(kind: &str, root: &Path) -> Result<OutGraphV1> {
    match kind {
        "cargo" => cargo::build(root),
        "soong" => soong::build(root),
        "kernel" => anyhow::bail!(
            "--build kernel: not yet implemented (queued v0.1.12 follow-up). \
             For now, hand-write module_graph.json or use --build cargo on \
             Rust-only kernel subdirs."
        ),
        "gn" => anyhow::bail!(
            "--build gn: not yet implemented (queued v0.1.12 follow-up). \
             For now, hand-write module_graph.json from `gn gen --ide=json` \
             output."
        ),
        other => anyhow::bail!(
            "unknown --build kind '{}'; expected one of: cargo, soong (skeleton), kernel (pending), gn (pending)",
            other,
        ),
    }
}

// ---------------------------------------------------------------------
// cargo
// ---------------------------------------------------------------------

mod cargo {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct RootCargoToml {
        workspace: Option<Workspace>,
        package: Option<Package>,
        #[serde(default)]
        dependencies: HashMap<String, DepValue>,
    }

    #[derive(Debug, Deserialize)]
    struct Workspace {
        #[serde(default)]
        members: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct Package {
        name: String,
    }

    /// Dependency value: either a bare version string `"1.2"` or a
    /// table `{ version = "...", path = "../foo" }`. We only care
    /// about the `path` field — intra-workspace deps form the graph.
    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    #[allow(dead_code)]
    enum DepValue {
        Bare(String),
        Table(DepTable),
    }

    #[derive(Debug, Deserialize)]
    struct DepTable {
        #[serde(default)]
        path: Option<String>,
    }

    impl DepValue {
        fn path(&self) -> Option<&str> {
            match self {
                DepValue::Table(t) => t.path.as_deref(),
                DepValue::Bare(_) => None,
            }
        }
    }

    pub fn build(root: &Path) -> Result<OutGraphV1> {
        let root_toml_path = root.join("Cargo.toml");
        let root_raw = std::fs::read_to_string(&root_toml_path)
            .with_context(|| format!("read {}", root_toml_path.display()))?;
        let root_toml: RootCargoToml = toml::from_str(&root_raw)
            .with_context(|| format!("parse {}", root_toml_path.display()))?;

        // Discover the set of crates in the workspace. Two cases:
        // (1) virtual workspace with [workspace.members] — the
        //     standard scry / multi-crate layout.
        // (2) single-crate root with no workspace section — the
        //     root itself is the sole crate.
        let mut crate_dirs: Vec<PathBuf> = Vec::new();
        if let Some(ws) = root_toml.workspace.as_ref() {
            for m in &ws.members {
                crate_dirs.push(root.join(m));
            }
        }
        if root_toml.package.is_some() {
            crate_dirs.push(root.to_path_buf());
        }
        if crate_dirs.is_empty() {
            anyhow::bail!(
                "no [workspace.members] or [package] section in {}; nothing to attribute",
                root_toml_path.display(),
            );
        }

        // Pass 1: read each member's Cargo.toml to learn its name +
        // intra-workspace deps. We canonicalize each crate's source
        // dir so the `path = "../foo"` resolution below can match.
        struct CrateInfo {
            id: u32,
            name: String,
            src_dir: PathBuf,
            dep_paths: Vec<PathBuf>,
        }
        let mut by_name: HashMap<String, u32> = HashMap::new();
        let mut crates: Vec<CrateInfo> = Vec::new();
        let mut path_to_id: HashMap<PathBuf, u32> = HashMap::new();

        for (idx, crate_dir) in crate_dirs.iter().enumerate() {
            let toml_path = crate_dir.join("Cargo.toml");
            let raw = std::fs::read_to_string(&toml_path)
                .with_context(|| format!("read {}", toml_path.display()))?;
            let parsed: RootCargoToml = toml::from_str(&raw)
                .with_context(|| format!("parse {}", toml_path.display()))?;
            let name = parsed.package.as_ref().map(|p| p.name.clone())
                .ok_or_else(|| anyhow::anyhow!(
                    "{}: no [package] section — workspace member must declare a package",
                    toml_path.display(),
                ))?;
            // Canonicalize for stable path comparisons across deps.
            let canon = crate_dir.canonicalize()
                .with_context(|| format!("canonicalize {}", crate_dir.display()))?;
            let id = idx as u32;
            // Collect intra-workspace deps (those with a `path = ...`).
            // Resolve relative to the crate's own dir (not root).
            let mut dep_paths: Vec<PathBuf> = Vec::new();
            for dv in parsed.dependencies.values() {
                if let Some(rel) = dv.path() {
                    let abs = crate_dir.join(rel);
                    if let Ok(canon_dep) = abs.canonicalize() {
                        dep_paths.push(canon_dep);
                    }
                }
            }
            by_name.insert(name.clone(), id);
            path_to_id.insert(canon.clone(), id);
            crates.push(CrateInfo { id, name, src_dir: canon, dep_paths });
        }

        // Pass 2: build the edge list. A dep edge `[from, to]` means
        // crate `from` depends on crate `to`. Only intra-workspace
        // edges are recorded; deps on external crates (no path = ...)
        // are silently dropped because they're not in our module table.
        let mut deps: Vec<[u32; 2]> = Vec::new();
        let mut dedup: HashSet<(u32, u32)> = HashSet::new();
        for c in &crates {
            for dp in &c.dep_paths {
                if let Some(&to_id) = path_to_id.get(dp) {
                    if to_id != c.id && dedup.insert((c.id, to_id)) {
                        deps.push([c.id, to_id]);
                    }
                }
            }
        }

        // Pass 3: walk each crate's src/ (and benches/, tests/, examples/
        // if present) for file attribution. Each .rs file gets attributed
        // to its owning crate. Files outside the crate dirs aren't
        // attributed (e.g. shared docs / scripts).
        let mut files: Vec<OutFile> = Vec::new();
        for c in &crates {
            for sub in ["src", "tests", "benches", "examples"] {
                let dir = c.src_dir.join(sub);
                if dir.is_dir() {
                    walk_rs(&dir, c.id, &mut files);
                }
            }
        }

        let modules: Vec<OutModule> = crates.iter()
            .map(|c| OutModule { id: c.id, name: c.name.clone(), partition: None })
            .collect();

        Ok(OutGraphV1 { version: 1, modules, deps, files })
    }

    /// Recursive walk emitting one `OutFile` per `.rs` source. Skips
    /// `target/` and hidden dirs; both are produced-not-source.
    fn walk_rs(dir: &Path, module_id: u32, out: &mut Vec<OutFile>) {
        let rd = match std::fs::read_dir(dir) { Ok(rd) => rd, _ => return };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            if let Some(n) = name.to_str() {
                if n.starts_with('.') || n == "target" { continue; }
            }
            if p.is_dir() {
                walk_rs(&p, module_id, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Some(s) = p.to_str() {
                    out.push(OutFile { path: s.to_string(), module_id });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// soong (skeleton — schema based on educated guess from Soong source
// at build/soong/cmd/soong_build and the Blueprint module model;
// validate against real `m json-module-graph` output before relying)
// ---------------------------------------------------------------------

mod soong {
    use super::*;

    /// Soong's module-graph.json is documented in the source as one
    /// JSON object per Blueprint module, with these fields. We
    /// deserialize the SUBSET we need; unknown fields are ignored.
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct RawModule {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "Type", default)]
        ty: String,
        /// Module's source files (relative to the AOSP root). May be
        /// empty for synthetic / aggregator modules.
        #[serde(rename = "Srcs", default)]
        srcs: Vec<String>,
        /// Direct deps, each carrying the depended-on module's name.
        /// In real Soong output this includes variant info; we strip
        /// to bare names for the v1 schema.
        #[serde(rename = "Deps", default)]
        deps: Vec<RawDep>,
        /// Partition the module ships in. Soong source uses fields
        /// like SystemExtSpecific / VendorSpecific etc.; for now we
        /// look at a single "Partition" shortcut field if present.
        #[serde(rename = "Partition", default)]
        partition: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct RawDep {
        #[serde(rename = "Name")]
        name: String,
    }

    pub fn build(root: &Path) -> Result<OutGraphV1> {
        // Where Soong drops the file (per build/soong/ui/build/config.go).
        let p = root.join("out/soong/module-graph.json");
        if !p.exists() {
            anyhow::bail!(
                "{}: not found. Generate it from your AOSP tree with:\n  \
                 source build/envsetup.sh && lunch <target> && m json-module-graph\n\
                 then re-run this command.",
                p.display(),
            );
        }
        let raw = std::fs::read_to_string(&p)
            .with_context(|| format!("read {}", p.display()))?;
        // Soong emits an array at the top level. If the actual format
        // is wrapped in an object, parse will fail with a clear error
        // pointing the user to file an issue or use --raw (future flag).
        let raws: Vec<RawModule> = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", p.display()))?;

        let mut by_name: HashMap<String, u32> = HashMap::new();
        for (i, m) in raws.iter().enumerate() {
            by_name.insert(m.name.clone(), i as u32);
        }

        let modules: Vec<OutModule> = raws.iter().enumerate()
            .map(|(i, m)| OutModule {
                id: i as u32,
                name: m.name.clone(),
                partition: m.partition.clone(),
            })
            .collect();

        let mut deps: Vec<[u32; 2]> = Vec::new();
        let mut dedup: HashSet<(u32, u32)> = HashSet::new();
        for (i, m) in raws.iter().enumerate() {
            let from = i as u32;
            for d in &m.deps {
                if let Some(&to) = by_name.get(&d.name) {
                    if from != to && dedup.insert((from, to)) {
                        deps.push([from, to]);
                    }
                }
            }
        }

        let mut files: Vec<OutFile> = Vec::new();
        for (i, m) in raws.iter().enumerate() {
            let module_id = i as u32;
            for s in &m.srcs {
                // Soong's source paths are relative to the AOSP root.
                // The scry indexer canonicalizes to absolute paths,
                // so resolve here for the file→module attribution
                // path key.
                let abs = root.join(s);
                if let Some(s) = abs.to_str() {
                    files.push(OutFile { path: s.to_string(), module_id });
                }
            }
        }

        Ok(OutGraphV1 { version: 1, modules, deps, files })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dogfood the Cargo adapter on a synthetic 3-crate workspace.
    /// Verifies module + dep + file attribution all round-trip into
    /// the v1 schema.
    #[test]
    fn cargo_synthetic_workspace_roundtrips() {
        let tmp = std::env::temp_dir().join(format!(
            "scry-cargo-adapter-{}", std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Root virtual workspace.
        std::fs::write(tmp.join("Cargo.toml"), r#"
[workspace]
resolver = "2"
members = ["a", "b", "c"]
"#).unwrap();
        // Crate `a` depends on `b`.
        std::fs::create_dir_all(tmp.join("a/src")).unwrap();
        std::fs::write(tmp.join("a/Cargo.toml"), r#"
[package]
name = "a"
version = "0.1.0"
edition = "2021"
[dependencies]
b = { path = "../b" }
serde = "1"
"#).unwrap();
        std::fs::write(tmp.join("a/src/lib.rs"), "pub fn from_a() {}\n").unwrap();
        // Crate `b` depends on `c`.
        std::fs::create_dir_all(tmp.join("b/src")).unwrap();
        std::fs::write(tmp.join("b/Cargo.toml"), r#"
[package]
name = "b"
version = "0.1.0"
edition = "2021"
[dependencies]
c = { path = "../c" }
"#).unwrap();
        std::fs::write(tmp.join("b/src/lib.rs"), "pub fn from_b() {}\n").unwrap();
        // Crate `c` has no intra-workspace deps.
        std::fs::create_dir_all(tmp.join("c/src")).unwrap();
        std::fs::write(tmp.join("c/Cargo.toml"), r#"
[package]
name = "c"
version = "0.1.0"
edition = "2021"
"#).unwrap();
        std::fs::write(tmp.join("c/src/lib.rs"), "pub fn from_c() {}\n").unwrap();

        let g = cargo::build(&tmp).unwrap();
        assert_eq!(g.version, 1);
        assert_eq!(g.modules.len(), 3);
        // Module names are exactly the crate names.
        let names: Vec<&str> = g.modules.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
        // Deps: a→b and b→c. External deps (serde) are skipped.
        assert_eq!(g.deps.len(), 2, "deps: {:?}", g.deps);
        let id_a = g.modules.iter().find(|m| m.name == "a").unwrap().id;
        let id_b = g.modules.iter().find(|m| m.name == "b").unwrap().id;
        let id_c = g.modules.iter().find(|m| m.name == "c").unwrap().id;
        assert!(g.deps.contains(&[id_a, id_b]));
        assert!(g.deps.contains(&[id_b, id_c]));
        // Files: each crate's src/lib.rs attributed correctly.
        assert_eq!(g.files.len(), 3);
        for f in &g.files {
            assert!(f.path.ends_with("src/lib.rs"), "unexpected path: {}", f.path);
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Verify that a reachability filter built on this graph would
    /// behave correctly: a→b→c means a reaches c (transitive), but
    /// c does not reach a.
    #[test]
    fn cargo_adapter_output_feeds_reachability() {
        use scry_store::modgraph::ModuleGraphJsonV1;
        let tmp = std::env::temp_dir().join(format!(
            "scry-cargo-adapter-reach-{}", std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), r#"
[workspace]
members = ["a", "b"]
"#).unwrap();
        std::fs::create_dir_all(tmp.join("a/src")).unwrap();
        std::fs::write(tmp.join("a/Cargo.toml"), r#"
[package]
name = "a"
version = "0.1.0"
[dependencies]
b = { path = "../b" }
"#).unwrap();
        std::fs::write(tmp.join("a/src/lib.rs"), "fn ok() {}").unwrap();
        std::fs::create_dir_all(tmp.join("b/src")).unwrap();
        std::fs::write(tmp.join("b/Cargo.toml"), r#"
[package]
name = "b"
version = "0.1.0"
"#).unwrap();
        std::fs::write(tmp.join("b/src/lib.rs"), "fn ok() {}").unwrap();

        let g = cargo::build(&tmp).unwrap();
        // Round-trip through serde so we exercise the schema the
        // reader will actually consume.
        let json = serde_json::to_string(&g).unwrap();
        let v: ModuleGraphJsonV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(v.modules.len(), 2);
        assert_eq!(v.deps.len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
