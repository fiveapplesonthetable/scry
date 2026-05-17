//! Build-system adapters: read a project's native build metadata and
//! emit scry's canonical v1 `module_graph.json` so `--reachable`
//! queries get build-graph-aware filtering.
//!
//! ## Supported build systems
//!
//! - **`cargo`** — Rust workspaces (Cargo.toml). One module per
//!   workspace member; dep edges follow `[dependencies]`.
//! - **`soong`** — AOSP Soong (`m json-module-graph` output). One
//!   module per Soong module; dep edges follow the union of
//!   `Static_libs`, `Shared_libs`, etc.
//! - **`kernel`** — Linux Kbuild. One module per top-level subsystem
//!   directory (drivers/, fs/, kernel/, …); permissive reachability
//!   (every subsystem reaches every other, since static-kernel
//!   non-static symbols are callable across the linkage domain).
//! - **`gn`** — GN/ninja projects (`gn gen --ide=json` output). One
//!   module per GN target; dep edges follow the `deps` arrays.
//! - **`bazel`** — Bazel workspaces (`bazel query --output=streamed_proto`
//!   or text equivalent). One module per `*_library` target.
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
pub(crate) fn build_modgraph(kind: &str, root: &Path) -> Result<OutGraphV1> {
    match kind {
        "cargo" => cargo::build(root),
        "soong" => soong::build(root),
        "kernel" => kernel::build(root),
        "gn" => gn::build(root),
        "bazel" => bazel::build(root),
        other => anyhow::bail!(
            "unknown --build kind '{}'; expected one of: cargo, soong, kernel, gn, bazel",
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
// kernel (Linux Kbuild)
//
// Scope: each top-level subdir under the kernel source root
// (drivers/, fs/, kernel/, mm/, net/, ipc/, security/, sound/, …) is a
// module. Reachability is permissive (all-to-all within the kernel
// linkage domain) because real EXPORT_SYMBOL-aware reachability needs
// per-file symbol-export parsing.
//
// Even with permissive reachability, this delivers value: every
// indexed file gets a meaningful module name (matching `drivers/net`,
// `fs/btrfs`, `arch/x86`, …) that scry can surface in `--reachable`
// diagnostics and that downstream tooling can use.
// ---------------------------------------------------------------------

mod kernel {
    use super::*;

    /// Top-level kernel source subdirs that the build system treats as
    /// subsystem boundaries. We attribute every file under one of these
    /// roots to a module named after the root. Anything outside this
    /// list (e.g. `Documentation/`, `tools/`, `samples/`) is still
    /// indexed but doesn't get a module attribution, which means the
    /// `--reachable` filter passes it through unchanged.
    const TOPLEVEL_SUBSYSTEMS: &[&str] = &[
        "arch", "block", "certs", "crypto", "drivers", "fs", "include",
        "init", "io_uring", "ipc", "kernel", "lib", "mm", "net",
        "rust", "samples", "scripts", "security", "sound", "tools",
        "usr", "virt",
    ];

    pub fn build(root: &Path) -> Result<OutGraphV1> {
        let canon = root.canonicalize()
            .with_context(|| format!("canonicalize kernel root {}", root.display()))?;
        // Sanity check — kernel source root must have a `Makefile`
        // and a `Kconfig` at top level. Catches the common mistake of
        // pointing at a subsystem dir.
        if !canon.join("Makefile").exists() || !canon.join("Kconfig").exists() {
            anyhow::bail!(
                "{}: doesn't look like a Linux kernel source root \
                 (expected Makefile + Kconfig at top level)",
                canon.display(),
            );
        }
        // Modules: one per top-level subsystem that actually exists
        // in this checkout. Some configs (tiny kernels, vendored
        // forks) may omit subsystems we list as canonical.
        let mut modules: Vec<OutModule> = Vec::new();
        let mut subsys_to_id: HashMap<String, u32> = HashMap::new();
        for sub in TOPLEVEL_SUBSYSTEMS {
            if canon.join(sub).is_dir() {
                let id = modules.len() as u32;
                modules.push(OutModule {
                    id,
                    name: (*sub).to_string(),
                    partition: Some("kernel".to_string()),
                });
                subsys_to_id.insert((*sub).to_string(), id);
            }
        }
        if modules.is_empty() {
            anyhow::bail!(
                "{}: no recognized kernel subsystems found",
                canon.display(),
            );
        }
        // Edges: permissive — every subsystem reaches every other.
        // In a static kernel build, any subsystem's non-static
        // symbols are callable from any other. This produces a fully
        // connected graph minus self-loops; `--reachable` becomes a
        // useful filter only when external (non-kernel) callers exist
        // in the same index.
        let mut deps: Vec<[u32; 2]> = Vec::new();
        for i in 0..modules.len() {
            for j in 0..modules.len() {
                if i != j {
                    deps.push([i as u32, j as u32]);
                }
            }
        }
        // File attribution: walk every subsystem dir, attributing
        // .c/.h/.S/.rs files. The walk is shallow-recursive (no
        // build-output exclusion needed — the kernel's `make
        // mrproper` produces no .o files in source dirs).
        let mut files: Vec<OutFile> = Vec::new();
        for (sub, id) in &subsys_to_id {
            walk_kernel_sources(&canon.join(sub), *id, &mut files);
        }
        Ok(OutGraphV1 { version: 1, modules, deps, files })
    }

    /// Walk a kernel subsystem subtree, emitting one OutFile per
    /// source file (.c/.h/.S/.rs). Excludes generated dirs that
    /// might appear in unclean trees.
    fn walk_kernel_sources(dir: &Path, module_id: u32, out: &mut Vec<OutFile>) {
        let rd = match std::fs::read_dir(dir) { Ok(rd) => rd, _ => return };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            if let Some(n) = name.to_str() {
                // Skip hidden + build-output dirs that occasionally
                // leak into a "clean" tree.
                if n.starts_with('.') || n == "build" || n == "obj" {
                    continue;
                }
            }
            if p.is_dir() {
                walk_kernel_sources(&p, module_id, out);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if matches!(ext, "c" | "h" | "S" | "rs") {
                    if let Some(s) = p.to_str() {
                        out.push(OutFile { path: s.to_string(), module_id });
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_non_kernel_root() {
            let tmp = scry_store::scry_tmp_dir().join(format!(
                "scry-kernel-bad-{}", std::process::id(),
            ));
            std::fs::create_dir_all(&tmp).ok();
            let err = build(&tmp).unwrap_err();
            assert!(err.to_string().contains("doesn't look like a Linux kernel"),
                    "expected kernel-root rejection, got: {err}");
            std::fs::remove_dir_all(&tmp).ok();
        }

        #[test]
        fn synthetic_kernel_tree_produces_subsystem_modules() {
            let tmp = scry_store::scry_tmp_dir().join(format!(
                "scry-kernel-fake-{}", std::process::id(),
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).unwrap();
            // Minimum kernel-root markers + two subsystems.
            std::fs::write(tmp.join("Makefile"), "# fake\n").unwrap();
            std::fs::write(tmp.join("Kconfig"),  "# fake\n").unwrap();
            std::fs::create_dir_all(tmp.join("drivers/net")).unwrap();
            std::fs::create_dir_all(tmp.join("fs/btrfs")).unwrap();
            std::fs::write(tmp.join("drivers/net/dummy.c"), "int foo(void) { return 0; }\n").unwrap();
            std::fs::write(tmp.join("drivers/net/dummy.h"), "int foo(void);\n").unwrap();
            std::fs::write(tmp.join("fs/btrfs/btrfs.c"), "int bar(void) { return 0; }\n").unwrap();
            std::fs::write(tmp.join("fs/btrfs/btrfs.rs"), "fn baz() {}\n").unwrap();

            let g = build(&tmp).unwrap();
            // Two subsystems present + the synthetic Makefile shouldn't count.
            let names: Vec<&str> = g.modules.iter().map(|m| m.name.as_str()).collect();
            assert!(names.contains(&"drivers"), "missing drivers: {names:?}");
            assert!(names.contains(&"fs"), "missing fs: {names:?}");
            assert_eq!(g.modules.len(), 2);
            // Files: 2 in drivers (dummy.c, dummy.h), 2 in fs (btrfs.c, btrfs.rs).
            assert_eq!(g.files.len(), 4, "got: {:?}", g.files);
            // Reachability: dense (each pair reachable both ways).
            assert_eq!(g.deps.len(), 2); // 2 modules → 2 directed edges
            // Every module is marked "kernel" partition.
            for m in &g.modules {
                assert_eq!(m.partition.as_deref(), Some("kernel"));
            }
            std::fs::remove_dir_all(&tmp).ok();
        }
    }
}

// ---------------------------------------------------------------------
// soong (validated against real AOSP cached output)
//
// Reads `out/soong/module-info-<lunch_target>.json` which Soong emits
// as part of every build. Schema discovered empirically against real
// AOSP output: an array of single-key objects mapping module name to
// {path, class, shared_libs, static_libs, dependencies, ...}. This is
// the same data `module-graph.json` would contain, but cached after
// every build instead of needing an explicit `m json-module-graph`.
//
// File attribution: each module's `path` field names its source dir.
// For file → module mapping, we use longest-prefix-match against the
// path list. A file in `frameworks/av/camera/ndk/foo.cpp` belongs to
// the module whose `path` matches that prefix most specifically.
//
// Dep edges: union of `shared_libs`, `static_libs`, `dependencies`.
// External deps (dep name not in our module table) are silently
// dropped; intra-graph deps form the reachability bitmap.
// ---------------------------------------------------------------------

mod soong {
    use super::*;

    /// One entry in module-info-*.json. The outer file is an array of
    /// these single-key objects; we re-shape into Vec<(name, info)>.
    #[derive(Debug, Deserialize, Default)]
    struct ModInfo {
        #[serde(default)]
        path: Vec<String>,
        #[serde(default)]
        class: Vec<String>,
        #[serde(default)]
        shared_libs: Vec<String>,
        #[serde(default)]
        static_libs: Vec<String>,
        #[serde(default)]
        dependencies: Vec<String>,
    }

    pub fn build(root: &Path) -> Result<OutGraphV1> {
        // Find a cached module-info JSON. Soong writes one per lunch
        // target as `out/soong/module-info-<target>.json`.
        let soong_out = root.join("out/soong");
        if !soong_out.is_dir() {
            anyhow::bail!(
                "{}: no out/soong directory found — has Soong ever run \
                 in this tree? Try `source build/envsetup.sh && lunch \
                 <target> && m nothing` first.",
                root.display(),
            );
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&soong_out) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                if let Some(n) = name.to_str() {
                    if n.starts_with("module-info-") && n.ends_with(".json") {
                        candidates.push(entry.path());
                    }
                }
            }
        }
        if candidates.is_empty() {
            anyhow::bail!(
                "{}: no module-info-<target>.json found. Run `m nothing` \
                 (or any soong-only build) to generate one.",
                soong_out.display(),
            );
        }
        // Pick the most recently modified — usually the lunch target
        // the user most recently built. If they have multiple targets
        // and want a specific one, they can rename / symlink as needed.
        candidates.sort_by_key(|p| {
            std::fs::metadata(p).and_then(|m| m.modified()).ok()
        });
        let chosen = candidates.last().unwrap().clone();
        eprintln!("[soong] reading module-info from {}", chosen.display());
        let raw = std::fs::read_to_string(&chosen)
            .with_context(|| format!("read {}", chosen.display()))?;
        // Schema: an array of {NAME: {fields}} objects. Re-shape into
        // a flat Vec<(String, ModInfo)>.
        let parsed: Vec<HashMap<String, ModInfo>> = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", chosen.display()))?;
        let mut mods: Vec<(String, ModInfo)> = Vec::with_capacity(parsed.len());
        for entry in parsed {
            for (k, v) in entry {
                mods.push((k, v));
            }
        }

        // Build module table. Dedup by name (some modules appear in
        // multiple variants; we keep the first and ignore the rest
        // for the v1 schema since variant differentiation needs the
        // clang USR pass).
        let mut by_name: HashMap<String, u32> = HashMap::new();
        let mut modules: Vec<OutModule> = Vec::new();
        let mut compact_mods: Vec<(String, ModInfo)> = Vec::new();
        for (name, info) in mods {
            if by_name.contains_key(&name) { continue; }
            let id = modules.len() as u32;
            let partition = info.class.first().cloned();
            by_name.insert(name.clone(), id);
            modules.push(OutModule { id, name: name.clone(), partition });
            compact_mods.push((name, info));
        }

        // Dep edges: union of shared_libs + static_libs + dependencies.
        // Drop deps whose target isn't in our module table (external,
        // synthetic, or variant-tagged with a suffix we don't decode).
        let mut deps: Vec<[u32; 2]> = Vec::new();
        let mut dedup: HashSet<(u32, u32)> = HashSet::new();
        for (i, (_, info)) in compact_mods.iter().enumerate() {
            let from = i as u32;
            for dep_name in info.shared_libs.iter()
                .chain(info.static_libs.iter())
                .chain(info.dependencies.iter())
            {
                if let Some(&to) = by_name.get(dep_name) {
                    if from != to && dedup.insert((from, to)) {
                        deps.push([from, to]);
                    }
                }
            }
        }

        // File attribution: build a single dir→module_id map where the
        // longest module path wins (sort length-desc, then or_insert).
        // Then walk only the *non-overlapping* roots once, climbing
        // each file's parent chain to find its owning module.
        //
        // Why this matters: AOSP has ~120k Soong modules and many
        // share path prefixes (e.g. `frameworks/base` plus dozens of
        // `frameworks/base/services/*`). Walking each module's path
        // independently re-walks the same subtrees N times — fatal
        // at AOSP scale. Single walk + parent-climb is O(F·depth).
        let mut module_paths: Vec<(String, u32)> = Vec::new();
        for (i, (_, info)) in compact_mods.iter().enumerate() {
            for p in &info.path {
                if !p.is_empty() {
                    module_paths.push((p.clone(), i as u32));
                }
            }
        }
        module_paths.sort_by_key(|x| std::cmp::Reverse(x.0.len()));
        let mut dir_to_module: HashMap<String, u32> =
            HashMap::with_capacity(module_paths.len());
        for (p, id) in &module_paths {
            dir_to_module.entry(p.clone()).or_insert(*id);
        }

        // Compute non-overlapping walk roots: sort paths ASC, then
        // drop any whose ancestor is already in the walk set.
        let mut sorted_asc: Vec<&String> = dir_to_module.keys().collect();
        sorted_asc.sort();
        let mut walk_roots: Vec<String> = Vec::new();
        for p in sorted_asc {
            let covered = walk_roots.last().is_some_and(|last| {
                let with_sep = format!("{last}/");
                p == last || p.starts_with(&with_sep)
            });
            if !covered {
                walk_roots.push(p.clone());
            }
        }
        eprintln!(
            "[soong] {} modules, {} module-path entries → {} non-overlapping walk roots",
            modules.len(), module_paths.len(), walk_roots.len(),
        );

        let mut files: Vec<OutFile> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let t_walk = std::time::Instant::now();
        for (idx, rel_root) in walk_roots.iter().enumerate() {
            let abs_root = root.join(rel_root);
            if !abs_root.is_dir() { continue; }
            walk_and_attribute(
                &abs_root, root, &dir_to_module,
                &mut files, &mut seen_files,
            );
            if idx % 1000 == 999 {
                eprintln!(
                    "[soong] walk progress: {}/{} roots, {} files attributed ({}s)",
                    idx + 1, walk_roots.len(), files.len(),
                    t_walk.elapsed().as_secs(),
                );
            }
        }
        eprintln!(
            "[soong] walk done: {} files attributed in {}s",
            files.len(), t_walk.elapsed().as_secs(),
        );

        Ok(OutGraphV1 { version: 1, modules, deps, files })
    }

    /// Walk a subtree once. For each source file, find the longest
    /// module-path prefix that owns it by climbing its parent chain
    /// against `dir_to_module`.
    fn walk_and_attribute(
        dir: &Path,
        root: &Path,
        dir_to_module: &HashMap<String, u32>,
        out: &mut Vec<OutFile>,
        seen: &mut HashSet<String>,
    ) {
        let rd = match std::fs::read_dir(dir) { Ok(rd) => rd, _ => return };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            if let Some(n) = name.to_str() {
                if n.starts_with('.') { continue; }
            }
            // Skip well-known non-source dirs to keep the walk bounded.
            // Top-level prebuilts/ + out/ should already be excluded by
            // walk-root selection, but defend against nested cases.
            let file_type = match entry.file_type() { Ok(t) => t, _ => continue };
            if file_type.is_symlink() { continue; }
            if file_type.is_dir() {
                walk_and_attribute(&p, root, dir_to_module, out, seen);
                continue;
            }
            let Some(ext) = p.extension().and_then(|e| e.to_str()) else { continue };
            if !matches!(ext,
                "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" |
                "java" | "kt" | "rs" | "aidl" | "proto" | "hal"
            ) { continue; }
            let Ok(rel) = p.strip_prefix(root) else { continue };
            let mut climb = rel.parent();
            while let Some(d) = climb {
                if let Some(s) = d.to_str() {
                    if let Some(&id) = dir_to_module.get(s) {
                        if let Some(ps) = p.to_str() {
                            if seen.insert(ps.to_string()) {
                                out.push(OutFile {
                                    path: ps.to_string(),
                                    module_id: id,
                                });
                            }
                        }
                        break;
                    }
                }
                climb = d.parent();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_module_info_array_shape() {
            let tmp = scry_store::scry_tmp_dir().join(format!(
                "scry-soong-fake-{}", std::process::id(),
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(tmp.join("out/soong")).unwrap();
            std::fs::create_dir_all(tmp.join("frameworks/foo")).unwrap();
            std::fs::create_dir_all(tmp.join("frameworks/bar")).unwrap();
            std::fs::write(tmp.join("frameworks/foo/a.cpp"), "//\n").unwrap();
            std::fs::write(tmp.join("frameworks/bar/b.cpp"), "//\n").unwrap();
            std::fs::write(tmp.join("out/soong/module-info-tinytest.json"), r#"[
                {"foo": {
                    "path": ["frameworks/foo"],
                    "class": ["SHARED_LIBRARIES"],
                    "shared_libs": ["bar"],
                    "static_libs": [],
                    "dependencies": ["bar"]
                }},
                {"bar": {
                    "path": ["frameworks/bar"],
                    "class": ["STATIC_LIBRARIES"],
                    "shared_libs": [],
                    "static_libs": [],
                    "dependencies": []
                }}
            ]"#).unwrap();
            let g = build(&tmp).unwrap();
            assert_eq!(g.modules.len(), 2);
            let names: Vec<&str> = g.modules.iter().map(|m| m.name.as_str()).collect();
            assert!(names.contains(&"foo"));
            assert!(names.contains(&"bar"));
            // foo → bar (via shared_libs OR dependencies; dedup'd to one edge).
            assert_eq!(g.deps.len(), 1, "deps: {:?}", g.deps);
            // 2 source files attributed.
            assert_eq!(g.files.len(), 2);
            std::fs::remove_dir_all(&tmp).ok();
        }
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
        let tmp = scry_store::scry_tmp_dir().join(format!(
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
        let tmp = scry_store::scry_tmp_dir().join(format!(
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

// ---------------------------------------------------------------------
// gn (Chromium / perfetto / V8 / ANGLE …)
//
// Reads the `project.json` that `gn gen --ide=json out/` produces.
// Schema (well documented at gn.googlesource.com/gn):
//   { "targets": { "//foo:bar": { "type": "...", "deps": [...],
//                                  "sources": [...] }, ... } }
// Targets named `//foo:bar` map to module name `foo:bar`. Deps are
// other `//…` labels — intra-graph deps form our edges; external
// labels are silently dropped (they don't appear in our module
// table).
// ---------------------------------------------------------------------

mod gn {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct GnProject {
        targets: HashMap<String, GnTarget>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct GnTarget {
        #[serde(rename = "type", default)]
        ty: Option<String>,
        #[serde(default)]
        deps: Vec<String>,
        #[serde(default)]
        sources: Vec<String>,
    }

    pub fn build(root: &Path) -> Result<OutGraphV1> {
        // GN writes project.json to whichever out dir was used at
        // `gn gen --ide=json`. Try a handful of common names; the
        // user can also point us at a specific one via a symlink.
        let candidates = [
            "out/Default/project.json",
            "out/Release/project.json",
            "out/Debug/project.json",
            "out/project.json",
            "project.json",
        ];
        let mut chosen: Option<PathBuf> = None;
        for c in &candidates {
            let p = root.join(c);
            if p.is_file() { chosen = Some(p); break; }
        }
        let p = chosen.ok_or_else(|| anyhow::anyhow!(
            "no GN project.json found under {}; ran `gn gen --ide=json out/Default` yet?",
            root.display(),
        ))?;
        let raw = std::fs::read_to_string(&p)
            .with_context(|| format!("read {}", p.display()))?;
        let parsed: GnProject = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", p.display()))?;

        // Normalize target label "//path/to:target" → "path/to:target".
        fn norm_label(s: &str) -> String {
            s.strip_prefix("//").unwrap_or(s).to_string()
        }

        let mut by_label: HashMap<String, u32> = HashMap::new();
        let mut modules: Vec<OutModule> = Vec::new();
        let mut compact: Vec<(String, GnTarget)> = Vec::new();
        for (label, tgt) in parsed.targets {
            let n = norm_label(&label);
            if by_label.contains_key(&n) { continue; }
            let id = modules.len() as u32;
            let partition = tgt.ty.clone();
            by_label.insert(n.clone(), id);
            modules.push(OutModule { id, name: n.clone(), partition });
            compact.push((n, tgt));
        }

        let mut deps: Vec<[u32; 2]> = Vec::new();
        let mut dedup: HashSet<(u32, u32)> = HashSet::new();
        for (i, (_, tgt)) in compact.iter().enumerate() {
            let from = i as u32;
            for dep in &tgt.deps {
                let dep_n = norm_label(dep);
                if let Some(&to) = by_label.get(&dep_n) {
                    if from != to && dedup.insert((from, to)) {
                        deps.push([from, to]);
                    }
                }
            }
        }

        // Files: sources entries are usually `//path/to/foo.cc` —
        // resolve against the root.
        let mut files: Vec<OutFile> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for (i, (_, tgt)) in compact.iter().enumerate() {
            for src in &tgt.sources {
                let rel = src.strip_prefix("//").unwrap_or(src);
                let abs = root.join(rel);
                if let Some(s) = abs.to_str() {
                    if seen.insert(s.to_string()) {
                        files.push(OutFile { path: s.to_string(), module_id: i as u32 });
                    }
                }
            }
        }

        Ok(OutGraphV1 { version: 1, modules, deps, files })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_gn_project_json() {
            let tmp = scry_store::scry_tmp_dir().join(format!(
                "scry-gn-fake-{}", std::process::id(),
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(tmp.join("out/Default")).unwrap();
            std::fs::create_dir_all(tmp.join("src/foo")).unwrap();
            std::fs::create_dir_all(tmp.join("src/bar")).unwrap();
            std::fs::write(tmp.join("src/foo/a.cc"), "//\n").unwrap();
            std::fs::write(tmp.join("src/bar/b.cc"), "//\n").unwrap();
            std::fs::write(tmp.join("out/Default/project.json"), r#"{
                "targets": {
                    "//src/foo:foo": {
                        "type": "static_library",
                        "deps": ["//src/bar:bar"],
                        "sources": ["//src/foo/a.cc"]
                    },
                    "//src/bar:bar": {
                        "type": "static_library",
                        "deps": [],
                        "sources": ["//src/bar/b.cc"]
                    }
                }
            }"#).unwrap();
            let g = build(&tmp).unwrap();
            assert_eq!(g.modules.len(), 2);
            assert_eq!(g.deps.len(), 1);
            assert_eq!(g.files.len(), 2);
            std::fs::remove_dir_all(&tmp).ok();
        }
    }
}

// ---------------------------------------------------------------------
// bazel
//
// Reads the output of `bazel query --output=jsonproto 'kind(rule, //...)'`
// (or its streamed cousin). Schema follows build.proto's Target type:
//
//   [ {"type": "RULE",
//      "rule": { "name": "//foo:bar",
//                "rule_class": "cc_library",
//                "attribute": [
//                  {"name": "deps", "string_list_value": ["//baz:lib", …]},
//                  {"name": "srcs", "string_list_value": ["//foo/a.cc", …]}
//                ] } },
//      ...
//   ]
//
// Or it may be wrapped in `{"results": [...]}`. We try both shapes.
// User runs `bazel query ... > bazel-query.json` and points us at it,
// OR we look for `<root>/bazel-query.json` automatically.
// ---------------------------------------------------------------------

mod bazel {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct TargetWrap {
        #[serde(rename = "type", default)]
        ty: Option<String>,
        #[serde(default)]
        rule: Option<RawRule>,
    }

    #[derive(Debug, Deserialize)]
    struct RawRule {
        name: String,
        #[serde(rename = "ruleClass", default)]
        rule_class: Option<String>,
        #[serde(default)]
        attribute: Vec<RawAttr>,
    }

    #[derive(Debug, Deserialize)]
    struct RawAttr {
        name: String,
        #[serde(default, rename = "stringListValue")]
        string_list_value: Vec<String>,
    }

    /// Top-level shape: either an array, or `{ "results": [...] }`.
    /// We hand-disambiguate by trying both.
    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    enum BazelOutput {
        Wrapped { results: Vec<TargetWrap> },
        Flat(Vec<TargetWrap>),
    }

    pub fn build(root: &Path) -> Result<OutGraphV1> {
        let candidates = ["bazel-query.json", "bazel-targets.json"];
        let mut chosen: Option<PathBuf> = None;
        for c in &candidates {
            let p = root.join(c);
            if p.is_file() { chosen = Some(p); break; }
        }
        let p = chosen.ok_or_else(|| anyhow::anyhow!(
            "no bazel-query.json found under {}; run:\n  \
             bazel query 'kind(rule, //...)' --output=jsonproto > {}/bazel-query.json\n\
             and try again.",
            root.display(), root.display(),
        ))?;
        let raw = std::fs::read_to_string(&p)
            .with_context(|| format!("read {}", p.display()))?;
        let parsed: BazelOutput = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", p.display()))?;
        let targets = match parsed {
            BazelOutput::Wrapped { results } => results,
            BazelOutput::Flat(v) => v,
        };

        // Normalize bazel label `//path/to:target` → `path/to:target`.
        fn norm_label(s: &str) -> String {
            s.strip_prefix("//").unwrap_or(s).to_string()
        }

        let mut by_label: HashMap<String, u32> = HashMap::new();
        let mut modules: Vec<OutModule> = Vec::new();
        let mut compact: Vec<(String, RawRule)> = Vec::new();
        for tw in targets {
            if tw.ty.as_deref() != Some("RULE") { continue; }
            let rule = match tw.rule { Some(r) => r, None => continue };
            let n = norm_label(&rule.name);
            if by_label.contains_key(&n) { continue; }
            let id = modules.len() as u32;
            let partition = rule.rule_class.clone();
            by_label.insert(n.clone(), id);
            modules.push(OutModule { id, name: n.clone(), partition });
            compact.push((n, rule));
        }

        let mut deps: Vec<[u32; 2]> = Vec::new();
        let mut dedup: HashSet<(u32, u32)> = HashSet::new();
        let mut files: Vec<OutFile> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        for (i, (_, rule)) in compact.iter().enumerate() {
            let from = i as u32;
            for attr in &rule.attribute {
                match attr.name.as_str() {
                    "deps" => {
                        for d in &attr.string_list_value {
                            let dn = norm_label(d);
                            if let Some(&to) = by_label.get(&dn) {
                                if from != to && dedup.insert((from, to)) {
                                    deps.push([from, to]);
                                }
                            }
                        }
                    }
                    "srcs" => {
                        for s in &attr.string_list_value {
                            let rel = s.strip_prefix("//")
                                .map(|s| s.replace(':', "/"))
                                .unwrap_or_else(|| s.clone());
                            let abs = root.join(&rel);
                            if let Some(s) = abs.to_str() {
                                if seen_files.insert(s.to_string()) {
                                    files.push(OutFile { path: s.to_string(), module_id: from });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(OutGraphV1 { version: 1, modules, deps, files })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_bazel_jsonproto_shape() {
            let tmp = scry_store::scry_tmp_dir().join(format!(
                "scry-bazel-fake-{}", std::process::id(),
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(tmp.join("src/foo")).unwrap();
            std::fs::create_dir_all(tmp.join("src/bar")).unwrap();
            std::fs::write(tmp.join("src/foo/a.cc"), "//\n").unwrap();
            std::fs::write(tmp.join("src/bar/b.cc"), "//\n").unwrap();
            std::fs::write(tmp.join("bazel-query.json"), r#"[
                {"type": "RULE", "rule": {
                    "name": "//src/foo:foo",
                    "ruleClass": "cc_library",
                    "attribute": [
                        {"name": "deps", "stringListValue": ["//src/bar:bar"]},
                        {"name": "srcs", "stringListValue": ["//src/foo:a.cc"]}
                    ]
                }},
                {"type": "RULE", "rule": {
                    "name": "//src/bar:bar",
                    "ruleClass": "cc_library",
                    "attribute": [
                        {"name": "srcs", "stringListValue": ["//src/bar:b.cc"]}
                    ]
                }}
            ]"#).unwrap();
            let g = build(&tmp).unwrap();
            assert_eq!(g.modules.len(), 2);
            assert_eq!(g.deps.len(), 1, "deps: {:?}", g.deps);
            assert_eq!(g.files.len(), 2);
            std::fs::remove_dir_all(&tmp).ok();
        }
    }
}
