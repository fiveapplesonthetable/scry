//! scry-walker: gitignore-aware, per-profile parallel walker over source roots.
//!
//! Phase 0: walk roots, classify files into FileKind, return counts.
//! Later phases will hand each (path, kind) to language-specific parsers.

use anyhow::{anyhow, Result};
use ignore::{WalkBuilder, WalkState};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Profile {
    Aosp,
    Linux,
    Generic,
}

impl Profile {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "aosp" => Profile::Aosp,
            "linux" => Profile::Linux,
            "generic" => Profile::Generic,
            other => return Err(anyhow!("unknown profile: {other}")),
        })
    }

    pub fn auto_detect(root: &Path) -> Self {
        if root.join("build/soong").is_dir() && root.join("frameworks").is_dir() {
            Profile::Aosp
        } else if root.join("MAINTAINERS").is_file() && root.join("Kbuild").is_file() {
            Profile::Linux
        } else {
            Profile::Generic
        }
    }

    /// Directory basenames to skip entirely (no descent).
    pub fn skiplist(self) -> &'static [&'static str] {
        match self {
            Profile::Aosp => &[
                "out", "prebuilts", ".repo", ".git", "node_modules",
                ".gradle", ".idea",
            ],
            Profile::Linux => &[
                ".git", "Documentation/output", "tools/testing/selftests/bpf/tools",
            ],
            Profile::Generic => &[
                ".git", "node_modules", "target", "build", "dist", ".venv",
                "__pycache__", ".idea", ".gradle",
            ],
        }
    }
}

/// All file categories scry knows about. Drives every later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Ord, PartialOrd)]
pub enum FileKind {
    // ---- source languages ----
    C,
    Cpp,
    Header,        // .h (could be C or C++; ambiguous until parsed)
    HeaderCpp,     // .hpp / .hh / .hxx
    Java,
    Kotlin,
    Rust,
    Go,
    Python,
    Bash,
    Proto,
    Aidl,
    Assembly,      // .s / .S

    // ---- build files ----
    Soong,         // Android.bp / *.bp
    AndroidMk,     // Android.mk / *.mk
    Bazel,         // BUILD / BUILD.bazel
    Kconfig,       // Kconfig / Kconfig.*
    Makefile,      // Makefile / Kbuild / GNUmakefile
    FlagsFile,     // *.flags
    Jarjar,        // *.jarjar
    Toml,          // Cargo.toml etc.

    // ---- Android platform config ----
    Aconfig,       // *.aconfig
    InitRc,        // *.rc
    Sepolicy,      // *.te
    SepolicyOther, // *.policy
    Manifest,      // AndroidManifest.xml
    XmlOther,      // generic *.xml
    Json,
    Properties,
    Cfg,

    // ---- ownership / docs ----
    Owners,
    Markdown,
}

impl FileKind {
    /// Classify by filename + extension. Returns None for unrecognized files.
    pub fn classify(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;

        // Filename-based matches first (these win over extension).
        match name {
            "Android.bp" => return Some(FileKind::Soong),
            "Android.mk" => return Some(FileKind::AndroidMk),
            "BUILD" | "BUILD.bazel" => return Some(FileKind::Bazel),
            "Makefile" | "Kbuild" | "GNUmakefile" => return Some(FileKind::Makefile),
            "OWNERS" => return Some(FileKind::Owners),
            "AndroidManifest.xml" => return Some(FileKind::Manifest),
            _ => {}
        }
        if name == "Kconfig" || name.starts_with("Kconfig.") || name.starts_with("Kconfig-") {
            return Some(FileKind::Kconfig);
        }

        let ext = path.extension()?.to_str()?;
        // Note: .S vs .s preserved on case-sensitive fs (Linux assembly convention).
        let ext_l = ext.to_ascii_lowercase();
        Some(match ext_l.as_str() {
            "c" => FileKind::C,
            "cc" | "cpp" | "cxx" | "c++" => FileKind::Cpp,
            "h" => FileKind::Header,
            "hpp" | "hh" | "hxx" => FileKind::HeaderCpp,
            "java" => FileKind::Java,
            "kt" | "kts" => FileKind::Kotlin,
            "rs" => FileKind::Rust,
            "go" => FileKind::Go,
            "py" => FileKind::Python,
            "sh" | "bash" => FileKind::Bash,
            "proto" => FileKind::Proto,
            "aidl" => FileKind::Aidl,
            "s" => FileKind::Assembly,
            "bp" => FileKind::Soong,
            "mk" => FileKind::AndroidMk,
            "flags" => FileKind::FlagsFile,
            "jarjar" => FileKind::Jarjar,
            "toml" => FileKind::Toml,
            "aconfig" => FileKind::Aconfig,
            "rc" => FileKind::InitRc,
            "te" => FileKind::Sepolicy,
            "policy" => FileKind::SepolicyOther,
            "xml" => FileKind::XmlOther,
            "json" => FileKind::Json,
            "properties" => FileKind::Properties,
            "cfg" => FileKind::Cfg,
            "md" | "markdown" => FileKind::Markdown,
            _ => return None,
        })
    }

    pub fn is_source(self) -> bool {
        matches!(
            self,
            FileKind::C | FileKind::Cpp | FileKind::Header | FileKind::HeaderCpp |
            FileKind::Java | FileKind::Kotlin | FileKind::Rust | FileKind::Go |
            FileKind::Python | FileKind::Bash | FileKind::Proto | FileKind::Aidl |
            FileKind::Assembly
        )
    }

    pub fn is_build(self) -> bool {
        matches!(
            self,
            FileKind::Soong | FileKind::AndroidMk | FileKind::Bazel | FileKind::Kconfig |
            FileKind::Makefile | FileKind::FlagsFile | FileKind::Jarjar | FileKind::Toml
        )
    }

    pub fn is_android_config(self) -> bool {
        matches!(
            self,
            FileKind::Aconfig | FileKind::InitRc | FileKind::Sepolicy | FileKind::SepolicyOther |
            FileKind::Manifest
        )
    }
}

#[derive(Debug)]
pub struct WalkResult {
    pub root: PathBuf,
    pub profile: Profile,
    pub counts: HashMap<FileKind, u64>,
    pub total_files: u64,
    pub unknown_files: u64,
    pub total_bytes: u64,
    pub elapsed_ms: u128,
}

/// One classified file produced by `collect_files`. Phase-1+ pipelines consume
/// these directly as the work items for the parser pool.
#[derive(Debug, Clone)]
pub struct RawFile {
    pub path: PathBuf,
    pub relpath: PathBuf,
    pub kind: FileKind,
    pub size: u64,
}

#[derive(Debug)]
pub struct CollectedFiles {
    pub root: PathBuf,
    pub profile: Profile,
    pub files: Vec<RawFile>,
    pub total_bytes: u64,
    pub unknown_files: u64,
    pub elapsed_ms: u128,
}

/// Walk `root` and return every classified file. Files we can't classify
/// are counted (`unknown_files`) but dropped from the list.
pub fn collect_files(root: &Path, profile: Profile) -> Result<CollectedFiles> {
    let root = root.canonicalize()
        .map_err(|e| anyhow!("cannot resolve root {}: {e}", root.display()))?;
    let skiplist: std::collections::HashSet<String> =
        profile.skiplist().iter().map(|s| s.to_string()).collect();

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let files: Arc<Mutex<Vec<RawFile>>> = Arc::new(Mutex::new(Vec::with_capacity(1_000_000)));
    let unknown = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let mut builder = WalkBuilder::new(&root);
    builder
        .standard_filters(false)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .parents(false)
        .follow_links(false)
        .threads(threads);
    builder.filter_entry(move |e| {
        if let Some(name) = e.file_name().to_str() {
            !skiplist.contains(name)
        } else {
            true
        }
    });

    let start = Instant::now();
    let root_for_strip = root.clone();
    let files_c = files.clone();
    let unknown_c = unknown.clone();
    let bytes_c = bytes.clone();
    builder.build_parallel().run(|| {
        let files = files_c.clone();
        let unknown = unknown_c.clone();
        let bytes = bytes_c.clone();
        let root_for_strip = root_for_strip.clone();
        Box::new(move |dent| {
            if let Ok(e) = dent {
                if e.file_type().map_or(false, |t| t.is_file()) {
                    let p = e.path();
                    let md_ok = e.metadata().ok();
                    let size = md_ok.as_ref().map_or(0, |m| m.len());
                    bytes.fetch_add(size, Ordering::Relaxed);
                    match FileKind::classify(p) {
                        Some(kind) => {
                            let rel = p.strip_prefix(&root_for_strip)
                                .unwrap_or(p)
                                .to_path_buf();
                            let rf = RawFile {
                                path: p.to_path_buf(),
                                relpath: rel,
                                kind,
                                size,
                            };
                            files.lock().push(rf);
                        }
                        None => {
                            unknown.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            WalkState::Continue
        })
    });
    let elapsed_ms = start.elapsed().as_millis();
    let files = std::mem::take(&mut *files.lock());
    Ok(CollectedFiles {
        root,
        profile,
        files,
        total_bytes: bytes.load(Ordering::Relaxed),
        unknown_files: unknown.load(Ordering::Relaxed),
        elapsed_ms,
    })
}

/// Walk one source root with the given profile. Parallelized via the `ignore`
/// crate's WalkParallel (work-stealing across threads).
///
/// Skips:
///   - directories whose basename is in `profile.skiplist()`
///   - hidden directories starting with `.` that aren't explicitly allowed
///   - symlinks (we never follow them)
pub fn walk_root(root: &Path, profile: Profile) -> Result<WalkResult> {
    let root = root.canonicalize()
        .map_err(|e| anyhow!("cannot resolve root {}: {e}", root.display()))?;

    let skiplist: Vec<String> = profile.skiplist().iter().map(|s| s.to_string()).collect();

    let counts: Arc<Mutex<HashMap<FileKind, u64>>> = Arc::new(Mutex::new(HashMap::new()));
    let total = Arc::new(AtomicU64::new(0));
    let unknown = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);

    let mut builder = WalkBuilder::new(&root);
    // We disable git-aware ignores: AOSP isn't one big git repo, and many
    // .gitignore files inside the tree would exclude generated code we DO
    // want to see (e.g. tests). Hidden files like .gitignore itself stay.
    builder
        .standard_filters(false)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .parents(false)
        .follow_links(false)
        .threads(threads);

    // Hard skip by directory basename. Filter_entry runs on every entry
    // before descent — returning false prunes the subtree entirely.
    let skip_set: std::collections::HashSet<String> = skiplist.into_iter().collect();
    builder.filter_entry(move |e| {
        if let Some(name) = e.file_name().to_str() {
            !skip_set.contains(name)
        } else {
            true
        }
    });

    let start = Instant::now();
    let counts_c = counts.clone();
    let total_c = total.clone();
    let unknown_c = unknown.clone();
    let bytes_c = bytes.clone();
    builder.build_parallel().run(|| {
        let counts = counts_c.clone();
        let total = total_c.clone();
        let unknown = unknown_c.clone();
        let bytes = bytes_c.clone();
        Box::new(move |dent| {
            if let Ok(e) = dent {
                if e.file_type().map_or(false, |t| t.is_file()) {
                    total.fetch_add(1, Ordering::Relaxed);
                    if let Ok(md) = e.metadata() {
                        bytes.fetch_add(md.len(), Ordering::Relaxed);
                    }
                    match FileKind::classify(e.path()) {
                        Some(kind) => {
                            let mut m = counts.lock();
                            *m.entry(kind).or_insert(0) += 1;
                        }
                        None => {
                            unknown.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            WalkState::Continue
        })
    });
    let elapsed_ms = start.elapsed().as_millis();

    // Drain the Mutex instead of try_unwrap'ing the Arc — the ignore crate
    // may retain references to the per-worker closures inside its internal
    // thread pool, so the strong count isn't guaranteed to be 1 here.
    let counts = std::mem::take(&mut *counts.lock());

    Ok(WalkResult {
        root,
        profile,
        counts,
        total_files: total.load(Ordering::Relaxed),
        unknown_files: unknown.load(Ordering::Relaxed),
        total_bytes: bytes.load(Ordering::Relaxed),
        elapsed_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_basics() {
        assert_eq!(FileKind::classify(Path::new("foo.java")), Some(FileKind::Java));
        assert_eq!(FileKind::classify(Path::new("a/b/Android.bp")), Some(FileKind::Soong));
        assert_eq!(FileKind::classify(Path::new("a/b/Android.mk")), Some(FileKind::AndroidMk));
        assert_eq!(FileKind::classify(Path::new("Kconfig")), Some(FileKind::Kconfig));
        assert_eq!(FileKind::classify(Path::new("Kconfig.platforms")), Some(FileKind::Kconfig));
        assert_eq!(FileKind::classify(Path::new("AndroidManifest.xml")), Some(FileKind::Manifest));
        assert_eq!(FileKind::classify(Path::new("layout.xml")), Some(FileKind::XmlOther));
        assert_eq!(FileKind::classify(Path::new("OWNERS")), Some(FileKind::Owners));
        assert_eq!(FileKind::classify(Path::new("entry.S")), Some(FileKind::Assembly));
        assert_eq!(FileKind::classify(Path::new("BUILD")), Some(FileKind::Bazel));
        assert_eq!(FileKind::classify(Path::new("BUILD.bazel")), Some(FileKind::Bazel));
        assert_eq!(FileKind::classify(Path::new("Makefile")), Some(FileKind::Makefile));
        assert_eq!(FileKind::classify(Path::new("foo.aconfig")), Some(FileKind::Aconfig));
        assert_eq!(FileKind::classify(Path::new("init.rc")), Some(FileKind::InitRc));
        assert_eq!(FileKind::classify(Path::new("file.te")), Some(FileKind::Sepolicy));
        assert_eq!(FileKind::classify(Path::new("nonsense.zzz")), None);
    }

    #[test]
    fn profile_classification() {
        assert!(FileKind::Java.is_source());
        assert!(FileKind::Soong.is_build());
        assert!(FileKind::Aconfig.is_android_config());
        assert!(!FileKind::Aconfig.is_source());
    }
}
