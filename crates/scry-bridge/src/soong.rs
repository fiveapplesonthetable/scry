//! Soong (AOSP) → [`Compilation`] bridge.
//!
//! AOSP's Soong build system emits a graph of `build*.ninja` files
//! under `out/soong/`. Each Java module shows up as one `build`
//! statement whose output is `…/javac/<module>.jar` and whose
//! bindings include a `classpath = -classpath J1:J2:…` line and a
//! sibling `<module>.jar.rsp` file holding the space-separated
//! source list.
//!
//! This module extracts those compilations without re-running
//! Soong. Three reasons we parse the ninja directly instead of
//! shelling out to AOSP's bundled `ninja -t commands`:
//!
//!   1. AOSP forks ninja to add a `highmem_pool` pool directive
//!      that stock ninja rejects, so a generic ninja install on the
//!      analysis host can't replay these files.
//!   2. We only need a tiny subset of the build graph — Java javac
//!      rules. Loading the full ~150 MiB of ninja state just to ask
//!      for a few thousand commands is wasteful.
//!   3. Streaming the file in pure Rust gives us deterministic
//!      memory + lets us parallelise extraction trivially across
//!      the ~10 `build.aosp_<target>.<idx>.ninja` shards Soong
//!      emits.
//!
//! The parser is intentionally narrow: it only recognises
//! `build <output>: <rule> <inputs>` headers followed by
//! `<indent><key> = <value>` bindings, and only retains rules
//! whose output path matches `<…>/javac/<module>.jar`. Everything
//! else streams past at memchr speed.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{BuildSystem, Compilation, Language};

/// Soong build-system bridge.
///
/// `build_dir` for a Soong tree is `<AOSP>/out/soong/`. The bridge
/// auto-discovers `build*.ninja` shards inside that directory and
/// streams them in parallel.
#[derive(Debug, Clone)]
pub struct Soong {
    /// Absolute path to the AOSP source root (the parent of
    /// `out/soong/`). Compilation `source_root` defaults to this so
    /// indexer output paths line up with scry's indexed files.
    pub source_root: PathBuf,
}

impl Soong {
    /// Build a Soong bridge for the given AOSP source root.
    pub fn new(source_root: impl Into<PathBuf>) -> Self {
        Self { source_root: source_root.into() }
    }
}

impl BuildSystem for Soong {
    fn extract_compilations(&self, build_dir: &Path) -> Result<Vec<Compilation>> {
        let ninjas = discover_ninja_shards(build_dir)
            .with_context(|| format!("discover ninja shards in {}",
                                     build_dir.display()))?;
        if ninjas.is_empty() {
            return Ok(Vec::new());
        }
        // Each shard contributes independently; merge at the end.
        // Parallelism here is mostly I/O-bound but each shard does
        // string-parsing work too — measurable speedup at AOSP scale
        // (~10 shards, ~150 MiB total, ~5 s wall single-threaded).
        let per_shard: Vec<(Vec<JavacRule>, Vec<KotlincRule>)> = ninjas
            .par_iter()
            .map(|p| extract_jvm_rules(p))
            .collect::<Result<Vec<_>>>()?;
        let mut javac_rules: Vec<JavacRule> = per_shard.iter()
            .flat_map(|(j, _)| j.iter().cloned()).collect();
        let mut kotlinc_rules: Vec<KotlincRule> = per_shard.into_iter()
            .flat_map(|(_, k)| k.into_iter()).collect();

        // Soong sometimes emits the same `<rule>/<mod>.jar` across
        // multiple shards (variant duplication). Keep the first;
        // the bindings are identical when this happens.
        javac_rules.sort_by(|a, b| a.output.cmp(&b.output));
        javac_rules.dedup_by(|a, b| a.output == b.output);
        kotlinc_rules.sort_by(|a, b| a.output.cmp(&b.output));
        kotlinc_rules.dedup_by(|a, b| a.output == b.output);

        // Hydrate each rule's source list from its sibling
        // `<output>.rsp` file (which Soong always writes alongside)
        // and (for kotlinc) the classpath.rsp file the binding
        // pointed at. Sequential here — each file is small and the
        // I/O parallelism we already paid for above dominates.
        let mut compilations: Vec<Compilation> = javac_rules
            .into_par_iter()
            .filter_map(|r| match r.into_compilation(&self.source_root) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("[scry-bridge] soong: skip javac rule: {e:#}");
                    None
                }
            })
            .collect();
        compilations.par_extend(kotlinc_rules.into_par_iter().filter_map(|r| {
            match r.into_compilation(&self.source_root) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("[scry-bridge] soong: skip kotlinc rule: {e:#}");
                    None
                }
            }
        }));
        Ok(compilations)
    }
}

/// A javac rule extracted from a single ninja shard. Intermediate
/// representation — once we hydrate the source list from the
/// sibling `.rsp` file it becomes a [`Compilation`].
#[derive(Debug, Clone)]
struct JavacRule {
    /// Output jar path, relative to the build dir (e.g.
    /// `out/soong/.intermediates/libcore/core-libart/android_common/javac/core-libart.jar`).
    output: String,
    /// Bindings on the rule. We keep them as a flat map so callers
    /// can pull out exactly the ones they care about (`classpath`,
    /// `bootClasspath`, `javacFlags`, etc.) without paying for parses
    /// we don't use.
    bindings: HashMap<String, String>,
}

/// A kotlinc rule extracted from a single ninja shard. Same shape
/// as [`JavacRule`] but the bindings differ: kotlinc's `classpath`
/// is a path to a sibling `classpath.rsp` file (one space-separated
/// jar list inside) rather than an inline `-classpath J1:J2`
/// string. The source `.jar.rsp` may contain a mix of `.kt` and
/// `.java` files since kotlinc compiles both.
#[derive(Debug, Clone)]
struct KotlincRule {
    /// Output jar path, relative to the build dir (e.g.
    /// `out/soong/.intermediates/<mod>/<variant>/kotlin/<mod>.jar`).
    output: String,
    bindings: HashMap<String, String>,
}

impl JavacRule {
    /// Turn the parsed rule into a [`Compilation`] by reading the
    /// sibling `.rsp` file for sources and slicing the classpath
    /// binding into individual jar paths.
    fn into_compilation(self, source_root: &Path) -> Result<Compilation> {
        let rsp_path = source_root.join(format!("{}.rsp", self.output));
        let rsp_contents = std::fs::read_to_string(&rsp_path)
            .with_context(|| format!("read sources rsp {}", rsp_path.display()))?;
        let sources: Vec<String> = rsp_contents
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if sources.is_empty() {
            anyhow::bail!("rsp file {} contained no sources", rsp_path.display());
        }

        let classpath = tokenize_javac_arg_line(
            self.bindings.get("classpath").map(String::as_str).unwrap_or(""),
            source_root,
        );
        let bootclasspath = tokenize_javac_arg_line(
            self.bindings.get("bootClasspath").map(String::as_str).unwrap_or(""),
            source_root,
        );

        // Heuristic: any `.kt` source pins the module as Kotlin (the
        // upstream rule actually invokes kotlinc *and* javac, but the
        // identity-bearing pass is the one that processes the kt
        // files). Fall through to Java for pure-Java modules.
        let language = if sources.iter().any(|s| s.ends_with(".kt")) {
            Language::Kotlin
        } else {
            Language::Java
        };
        let module = module_name_from_output(&self.output);
        Ok(Compilation {
            module,
            language,
            source_root: source_root.to_path_buf(),
            sources,
            classpath,
            bootclasspath,
            defines: Vec::new(),
            java_version: self.bindings.get("javaVersion").cloned(),
            extra_args: self.bindings.get("javacFlags")
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }
}

impl KotlincRule {
    /// Convert the parsed kotlinc rule to a [`Compilation`]. The
    /// classpath in Soong's kotlinc rule is indirect: the binding
    /// names a `.../kotlinc/classpath.rsp` file whose body is the
    /// space-separated jar list. We resolve it.
    fn into_compilation(self, source_root: &Path) -> Result<Compilation> {
        let rsp_path = source_root.join(format!("{}.rsp", self.output));
        let rsp_contents = std::fs::read_to_string(&rsp_path)
            .with_context(|| format!("read sources rsp {}", rsp_path.display()))?;
        let sources: Vec<String> = rsp_contents
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if sources.is_empty() {
            anyhow::bail!("rsp file {} contained no sources", rsp_path.display());
        }

        // Classpath binding: a path to the kotlinc classpath.rsp file.
        let classpath = if let Some(cp_rsp_rel) = self.bindings.get("classpath") {
            let cp_rsp = source_root.join(cp_rsp_rel.trim());
            match std::fs::read_to_string(&cp_rsp) {
                Ok(body) => {
                    let jars: Vec<String> = body
                        .split_whitespace()
                        .filter(|s| !s.is_empty())
                        .map(|s| source_root.join(s).display().to_string())
                        .collect();
                    if jars.is_empty() {
                        Vec::new()
                    } else {
                        // kotlinc accepts `-classpath J1:J2:...`
                        vec!["-classpath".to_string(), jars.join(":")]
                    }
                }
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        // kotlinc target version — bind from Soong's `kotlinJvmTarget`.
        let java_version = self.bindings.get("kotlinJvmTarget").cloned();

        let language = Language::Kotlin;
        let module = module_name_from_kotlin_output(&self.output);
        Ok(Compilation {
            module,
            language,
            source_root: source_root.to_path_buf(),
            sources,
            classpath,
            bootclasspath: Vec::new(),
            defines: Vec::new(),
            java_version,
            extra_args: self.bindings.get("kotlincFlags")
                .map(|s| s.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        })
    }
}

/// Discover all `build*.ninja` shards inside the given build dir.
/// Soong emits ten or so per target with names like
/// `build.aosp_arm64.0.ninja`. We deliberately skip the `.ninja.d`
/// and `.ninja.globs*` siblings.
fn discover_ninja_shards(build_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(build_dir)
        .with_context(|| format!("readdir {}", build_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name_s.starts_with("build") && name_s.ends_with(".ninja")
            && !name_s.contains(".ninja.")
        {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Classify a build-rule output path as a JVM compilation we care
/// about. Yields the Kind so the caller can route it to the right
/// rule type without re-matching the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JvmRuleKind {
    Javac,
    Kotlinc,
}

fn classify_jvm_target(target: &str) -> Option<JvmRuleKind> {
    if !target.ends_with(".jar") {
        return None;
    }
    if target.contains("/javac/") && !target.contains("javac-header") {
        return Some(JvmRuleKind::Javac);
    }
    if target.contains("/kotlin/") && !target.contains("kotlin_headers")
        && !target.contains("kotlin-jar-snapshot")
    {
        return Some(JvmRuleKind::Kotlinc);
    }
    None
}

/// Stream one ninja shard and extract every JVM compilation rule's
/// bindings — both `javac/<mod>.jar` (Java) and `kotlin/<mod>.jar`
/// (Kotlin). Same line-oriented parser as before; the only addition
/// is a per-rule classifier that routes to the right output bucket.
fn extract_jvm_rules(path: &Path) -> Result<(Vec<JavacRule>, Vec<KotlincRule>)> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read ninja shard {}", path.display()))?;
    let mut javac_out: Vec<JavacRule> = Vec::new();
    let mut kotlin_out: Vec<KotlincRule> = Vec::new();
    let mut current: Option<(String, HashMap<String, String>, Option<JvmRuleKind>)> = None;
    let flush = |cur: Option<(String, HashMap<String, String>, Option<JvmRuleKind>)>,
                 jout: &mut Vec<JavacRule>,
                 kout: &mut Vec<KotlincRule>| {
        if let Some((output, bindings, kind)) = cur {
            match kind {
                Some(JvmRuleKind::Javac) => jout.push(JavacRule { output, bindings }),
                Some(JvmRuleKind::Kotlinc) => kout.push(KotlincRule { output, bindings }),
                None => {}
            }
        }
    };

    let joined = contents.replace("$\n", "");
    for raw_line in joined.lines() {
        if raw_line.is_empty() {
            flush(current.take(), &mut javac_out, &mut kotlin_out);
            continue;
        }
        if raw_line.starts_with("build ") {
            flush(current.take(), &mut javac_out, &mut kotlin_out);
            if let Some(output) = parse_build_header_output(raw_line) {
                let kind = classify_jvm_target(&output);
                current = Some((output, HashMap::new(), kind));
            }
        } else if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
            if let Some((_, bindings, _)) = current.as_mut() {
                if let Some((k, v)) = parse_binding(raw_line) {
                    bindings.insert(k.to_string(), v.to_string());
                }
            }
        } else {
            flush(current.take(), &mut javac_out, &mut kotlin_out);
        }
    }
    flush(current.take(), &mut javac_out, &mut kotlin_out);
    Ok((javac_out, kotlin_out))
}


/// Parse the first output path off a `build out1 out2: rule ...`
/// header. Soong's javac rules emit only one output, so we return
/// the first whitespace-delimited token after `build`. Returns None
/// for malformed headers; the caller skips them silently.
fn parse_build_header_output(line: &str) -> Option<String> {
    // Strip the `build ` prefix and the `: <rule> ...` suffix.
    let rest = line.strip_prefix("build ")?;
    let colon = rest.find(':')?;
    let outputs = &rest[..colon];
    // Take the first output (Soong javac rules only have one).
    let first = outputs.split_whitespace().next()?;
    Some(first.to_string())
}

/// Parse an indented `<key> = <value>` binding line. Whitespace
/// around `=` is permissive; values may contain anything (we don't
/// interpret them here).
fn parse_binding(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let eq = trimmed.find('=')?;
    let key = trimmed[..eq].trim();
    let value = trimmed[eq + 1..].trim();
    if key.is_empty() { return None; }
    Some((key, value))
}

/// Tokenize a Soong classpath / bootClasspath binding into a stream
/// of javac-ready arguments, rewriting any embedded relative paths
/// into absolute paths under `source_root`.
///
/// Examples (input → output tokens):
///   - `-classpath a.jar:b.jar`           → `["-classpath", "<root>/a.jar:<root>/b.jar"]`
///   - `--system=foo/system`              → `["--system=<root>/foo/system"]`
///   - `-bootclasspath boot.jar`          → `["-bootclasspath", "<root>/boot.jar"]`
///   - ``                                  → `[]`
///
/// We keep the original argument shape (one token per arg, two
/// tokens for flag+value pairs) because javac is strict about
/// `--release` / `--system=` / `-bootclasspath` mutual exclusion;
/// merging them all into one bag would lose enough structure that
/// we'd have to re-detect the form anyway.
fn tokenize_javac_arg_line(binding: &str, source_root: &Path) -> Vec<String> {
    let trimmed = binding.trim();
    if trimmed.is_empty() { return Vec::new(); }
    let mut out: Vec<String> = Vec::new();
    let mut tokens = trimmed.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        if tok == "-classpath" || tok == "-cp" || tok == "-bootclasspath" {
            // Flag + colon-separated path-list value.
            out.push(tok.to_string());
            if let Some(value) = tokens.next() {
                out.push(rewrite_path_list_to_absolute(value, source_root));
            }
        } else if let Some(eq) = tok.find('=') {
            // `--system=PATH` or `--module-path=...` etc. Rewrite the
            // value portion to absolute when it looks like a path.
            let (k, v) = (&tok[..eq], &tok[eq + 1..]);
            out.push(format!("{k}={}", rewrite_path_list_to_absolute(v, source_root)));
        } else {
            // Bare token (e.g. `--enable-preview` or a bare path value
            // we already consumed via the previous flag branch — but
            // be defensive).
            out.push(tok.to_string());
        }
    }
    out
}

/// Rewrite each colon-separated entry in `list` to an absolute path
/// under `source_root` when it's relative. Absolute paths and tokens
/// without colons are returned unchanged.
fn rewrite_path_list_to_absolute(list: &str, source_root: &Path) -> String {
    list.split(':')
        .map(|s| if s.is_empty() || Path::new(s).is_absolute() {
            s.to_string()
        } else {
            source_root.join(s).display().to_string()
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// Derive a stable module name from the javac rule's output path.
/// `out/soong/.intermediates/libcore/core-libart/android_common/javac/core-libart.jar`
/// becomes `libcore/core-libart`. Used for grouping + diagnostics.
fn module_name_from_output(output: &str) -> String {
    module_name_for_marker(output, "/javac/")
}

/// Same as [`module_name_from_output`] but for kotlinc outputs that
/// use `/kotlin/` instead of `/javac/`. Kept symmetric so the two
/// rule types produce comparable module slugs (a Kotlin module's
/// kotlinc compilation gets the same slug a hypothetical javac
/// compilation of the same module would).
fn module_name_from_kotlin_output(output: &str) -> String {
    module_name_for_marker(output, "/kotlin/")
}

fn module_name_for_marker(output: &str, marker: &str) -> String {
    let stripped = output
        .strip_prefix("out/soong/.intermediates/")
        .unwrap_or(output);
    if let Some(idx) = stripped.rfind(marker) {
        let before = &stripped[..idx];
        if let Some(slash) = before.rfind('/') {
            return before[..slash].to_string();
        }
        return before.to_string();
    }
    stripped.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_build_header_extracts_first_output() {
        let line = "build out/soong/.intermediates/x/y/z/javac/y.jar: javac in.java";
        assert_eq!(
            parse_build_header_output(line).as_deref(),
            Some("out/soong/.intermediates/x/y/z/javac/y.jar"),
        );
    }

    #[test]
    fn parse_binding_handles_spacing() {
        let line = "    classpath = -classpath a.jar:b.jar";
        let (k, v) = parse_binding(line).unwrap();
        assert_eq!(k, "classpath");
        assert_eq!(v, "-classpath a.jar:b.jar");
    }

    #[test]
    fn classify_jvm_target_routes_javac_kotlin_skips_headers() {
        assert_eq!(classify_jvm_target("x/javac/foo.jar"), Some(JvmRuleKind::Javac));
        assert_eq!(classify_jvm_target("x/kotlin/foo.jar"), Some(JvmRuleKind::Kotlinc));
        assert!(classify_jvm_target("x/javac-header/foo.jar").is_none());
        assert!(classify_jvm_target("x/kotlin_headers/foo.jar").is_none());
        assert!(classify_jvm_target("x/kotlin-jar-snapshot/foo.jar").is_none());
        assert!(classify_jvm_target("x/turbine/foo.jar").is_none());
        assert!(classify_jvm_target("x/javac/foo.txt").is_none());
    }

    #[test]
    fn tokenize_classpath_flag_plus_jars() {
        let cp = tokenize_javac_arg_line(
            "-classpath a.jar:b.jar:c.jar", Path::new("/root"));
        assert_eq!(cp, vec![
            "-classpath".to_string(),
            "/root/a.jar:/root/b.jar:/root/c.jar".to_string(),
        ]);
    }

    #[test]
    fn tokenize_classpath_handles_empty_binding() {
        assert!(tokenize_javac_arg_line("", Path::new("/root")).is_empty());
    }

    #[test]
    fn tokenize_system_form_keeps_flag_intact() {
        // AOSP libcore/art uses `--system=PATH` instead of `-bootclasspath JARS`.
        let bcp = tokenize_javac_arg_line(
            "--system=foo/bar/system-modules", Path::new("/aosp"));
        assert_eq!(bcp, vec![
            "--system=/aosp/foo/bar/system-modules".to_string(),
        ]);
    }

    #[test]
    fn tokenize_preserves_absolute_paths() {
        let cp = tokenize_javac_arg_line(
            "-classpath /abs/a.jar:b.jar", Path::new("/root"));
        assert_eq!(cp[1], "/abs/a.jar:/root/b.jar");
    }

    #[test]
    fn module_name_strips_intermediates_prefix_and_variant() {
        assert_eq!(
            module_name_from_output(
                "out/soong/.intermediates/libcore/core-libart/android_common/javac/core-libart.jar"
            ),
            "libcore/core-libart",
        );
    }

    #[test]
    fn extract_jvm_rules_splits_javac_and_kotlinc_skips_headers() {
        let tmp = std::env::temp_dir().join(format!(
            "scry-bridge-soong-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&tmp);
        let ninja = tmp.join("build.test.ninja");
        std::fs::write(&ninja, "\
rule javac
    command = javac
rule kotlinc
    command = kotlinc

build out/soong/.intermediates/libcore/core-libart/android_common/javac/core-libart.jar: javac in.java
    classpath = -classpath a.jar:b.jar
    bootClasspath = -bootclasspath boot.jar
    javacFlags = -Xlint:all

build out/soong/.intermediates/libcore/core-libart/android_common/javac-header/core-libart.jar: turbine in.java
    classpath = -classpath should-not-appear.jar

build out/soong/.intermediates/myapp/mod/android_common/kotlin/mod.jar: kotlinc in.kt
    classpath = out/soong/.intermediates/myapp/mod/android_common/kotlinc/classpath.rsp
    kotlinJvmTarget = 21
    kotlincFlags = -Xfriend-paths=foo

build out/soong/.intermediates/myapp/mod/android_common/kotlin_headers/mod.jar: kotlinc-header in.kt
    classpath = out/should-not-appear/classpath.rsp

").unwrap();
        let (javac, kotlinc) = extract_jvm_rules(&ninja).unwrap();
        assert_eq!(javac.len(), 1, "javac-header should be skipped");
        assert_eq!(kotlinc.len(), 1, "kotlin_headers should be skipped");
        let j = &javac[0];
        assert!(j.output.ends_with("/javac/core-libart.jar"));
        assert_eq!(j.bindings["classpath"], "-classpath a.jar:b.jar");
        assert_eq!(j.bindings["bootClasspath"], "-bootclasspath boot.jar");
        let k = &kotlinc[0];
        assert!(k.output.ends_with("/kotlin/mod.jar"));
        assert_eq!(k.bindings["kotlinJvmTarget"], "21");
        assert_eq!(k.bindings["kotlincFlags"], "-Xfriend-paths=foo");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
