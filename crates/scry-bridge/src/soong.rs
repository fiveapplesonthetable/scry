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
        let per_shard: Vec<ShardData> = ninjas
            .par_iter()
            .map(|p| extract_jvm_rules(p))
            .collect::<Result<Vec<_>>>()?;

        // Merge the top-level variable tables across shards. Soong
        // sometimes splits a module's variable defs and its build
        // rules across different shards (large modules in particular).
        // Without a unified table, an expansion in shard N that
        // references `${m.<mod>_<variant>.javacFlags}` defined in
        // shard M ≠ N silently degrades to the literal "${...}" text.
        let mut all_vars: HashMap<String, String> = HashMap::new();
        for shard in &per_shard {
            for (k, v) in &shard.vars {
                all_vars.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        let mut javac_rules: Vec<JavacRule> = per_shard.iter()
            .flat_map(|s| s.javac_rules.iter().cloned()).collect();
        let mut kotlinc_rules: Vec<KotlincRule> = per_shard.into_iter()
            .flat_map(|s| s.kotlin_rules.into_iter()).collect();

        // Soong sometimes emits the same `<rule>/<mod>.jar` across
        // multiple shards (variant duplication). Keep the first;
        // the bindings are identical when this happens.
        javac_rules.sort_by(|a, b| a.output.cmp(&b.output));
        javac_rules.dedup_by(|a, b| a.output == b.output);
        kotlinc_rules.sort_by(|a, b| a.output.cmp(&b.output));
        kotlinc_rules.dedup_by(|a, b| a.output == b.output);

        // Expand `${var}` / `$var` references in every binding using
        // the unified variable table. Without this step, AOSP modules
        // that put their actual javac flags behind a
        // `${m.<module>_<variant>.javacFlags}` indirection (libcore,
        // ART, anything using `--patch-module=java.base=…`) would lose
        // the indirection and javac would reject the compilation with
        // "package exists in another module: java.base".
        for rule in &mut javac_rules {
            for v in rule.bindings.values_mut() {
                *v = expand_ninja_vars(v, &all_vars);
            }
        }
        for rule in &mut kotlinc_rules {
            for v in rule.bindings.values_mut() {
                *v = expand_ninja_vars(v, &all_vars);
            }
        }

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
        let extra_args = self.bindings.get("javacFlags")
            .map(|s| forward_javac_flags(s, source_root))
            .unwrap_or_default();
        Ok(Compilation {
            module,
            language,
            source_root: source_root.to_path_buf(),
            sources,
            classpath,
            bootclasspath,
            defines: Vec::new(),
            java_version: self.bindings.get("javaVersion").cloned(),
            extra_args,
        })
    }
}

/// Filter Soong's `javacFlags` for forwarding to javac, with paths
/// rewritten to absolute. Drops flags that conflict with what we
/// already synthesize on the javac command line:
///
///   - `-source N`, `-target N`, `--release=N` — we set these
///     ourselves from `compilation.java_version`. Letting Soong's
///     copy through would either duplicate or contradict ours.
///   - `-Werror`, `-Werror:*` — turns warnings into fatals, which
///     defeats the partial-output recovery the SemanticDB plugin
///     relies on.
///   - `-d <dir>` — javac output dir, ours.
///   - `-processorpath …`, `-Xplugin:…` — registered by us.
///   - `-classpath …`, `-cp …`, `-bootclasspath …`, `--system=…`,
///     `--module-path=…` — surfaced by us through the typed
///     `classpath` / `bootclasspath` Compilation slots.
///
/// Everything else passes through verbatim, with any colon-separated
/// path lists in `--patch-module=NAME=path1:path2:…` and the
/// `--add-modules=` / `--add-exports=` family rewritten to be
/// absolute under `source_root`. Without the absolute rewrite,
/// libcore's `--patch-module=java.base=.:out/soong:…` resolves
/// relative to the scry process cwd and javac can't find the
/// patched module dirs.
pub(crate) fn forward_javac_flags(binding: &str, source_root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Use shell-aware tokenization so a single shell-quoted arg like
    // `'-Xplugin:ErrorProne -Xep:X:ERROR …'` parses as ONE token,
    // not N space-split tokens. AOSP's javacFlags wraps the entire
    // Error Prone configuration in single quotes — without this,
    // the first token leaks through (because the leading `'` made
    // our `-Xplugin:` prefix check miss) AND every subsequent
    // `-Xep:…` flag also leaked through and javac rejected them
    // all with "invalid flag".
    let parsed: Vec<String> = shell_words::split(binding)
        .unwrap_or_else(|_| binding.split_whitespace().map(str::to_string).collect());
    let mut tokens = parsed.iter().map(String::as_str).peekable();
    while let Some(tok) = tokens.next() {
        // Two-token flags whose value we either drop or already have.
        if matches!(tok, "-source" | "-target" | "-d" | "-processorpath"
                       | "-classpath" | "-cp" | "-bootclasspath") {
            tokens.next();
            continue;
        }
        // `-Werror` (with or without `:tag` suffix).
        if tok == "-Werror" || tok.starts_with("-Werror:") {
            continue;
        }
        // `-Xplugin:semanticdb…` collides with our own plugin
        // registration. Soong doesn't normally emit Xplugin in
        // javacFlags, but be defensive.
        if tok.starts_with("-Xplugin:") {
            continue;
        }
        if let Some(eq) = tok.find('=') {
            let (k, v) = (&tok[..eq], &tok[eq + 1..]);
            // Drop the modern single-knob version selector — we set it.
            if k == "--release" { continue; }
            // We surface classpath / module-path through typed slots.
            if matches!(k, "--class-path" | "--module-path"
                          | "--upgrade-module-path"
                          | "--system" | "--source-path") {
                continue;
            }
            // `--patch-module=NAME=PATHS` is two-level: the outer `=`
            // splits flag/value, the inner separates module from
            // colon-separated patch sources. Rewrite the inner path
            // list to absolute so javac resolves it from anywhere.
            if k == "--patch-module" {
                if let Some(inner_eq) = v.find('=') {
                    let module = &v[..inner_eq];
                    let paths = &v[inner_eq + 1..];
                    let rewritten = rewrite_path_list_to_absolute(paths, source_root);
                    out.push(format!("--patch-module={module}={rewritten}"));
                    continue;
                }
                // Malformed — pass verbatim.
                out.push(tok.to_string());
                continue;
            }
            // Generic path-bearing `--FLAG=value` (e.g.
            // `--processor-module-path=`). Rewrite per the same
            // whitelist used for classpath/bootclasspath bindings.
            if is_path_bearing_eq_flag(k) {
                out.push(format!("{k}={}",
                    rewrite_path_list_to_absolute(v, source_root)));
                continue;
            }
            // Other `--FLAG=value` — pass verbatim (`--add-modules=`,
            // `--add-exports=`, `--limit-modules=`, `-Xlint=…`, etc.).
            out.push(tok.to_string());
            continue;
        }
        // Bare token — pass through (e.g. `-Xlint:all`,
        // `-XDcompilePolicy=…`, `-XDsuppressNotes`).
        out.push(tok.to_string());
    }
    out
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
                .map(|s| forward_kotlinc_flags(s, source_root))
                .unwrap_or_default(),
        })
    }
}

/// Filter Soong's `kotlincFlags` for forwarding to kotlinc, with
/// any path values rewritten to absolute. Symmetric to
/// [`forward_javac_flags`]: drops flags that collide with what
/// kotlin_indexer synthesizes (`-d`, `-no-stdlib`, `-jvm-target`,
/// `-Xplugin:semanticdb*`, `-P plugin:semanticdb-kotlinc:*`,
/// `-classpath`/`-cp`), keeps everything else. Most importantly
/// keeps `-Xmultiplatform` / `-Xexpect-actual-classes` which AOSP
/// modules like kotlinx-coroutines need or kotlinc rejects every
/// `expect`/`actual` declaration.
pub(crate) fn forward_kotlinc_flags(binding: &str, source_root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Shell-aware tokenization for the same reason as
    // [`forward_javac_flags`]: kotlincFlags occasionally inlines
    // quoted multi-word args.
    let parsed: Vec<String> = shell_words::split(binding)
        .unwrap_or_else(|_| binding.split_whitespace().map(str::to_string).collect());
    let mut tokens = parsed.iter().map(String::as_str).peekable();
    while let Some(tok) = tokens.next() {
        // Two-token flags whose value we either drop or already have.
        if matches!(tok, "-d" | "-jvm-target" | "-classpath" | "-cp"
                       | "-language-version" | "-api-version") {
            tokens.next();
            continue;
        }
        // `-no-stdlib`, `-no-jdk`, `-no-reflect` — we set our own.
        if matches!(tok, "-no-stdlib" | "-no-jdk" | "-no-reflect") {
            continue;
        }
        // Our own plugin registration must not be duplicated.
        if tok.starts_with("-Xplugin=") && tok.contains("semanticdb-kotlinc") {
            continue;
        }
        // -P plugin:semanticdb-kotlinc:* — same.
        if tok == "-P" {
            if let Some(next) = tokens.peek() {
                if next.starts_with("plugin:semanticdb-kotlinc:") {
                    tokens.next();
                    continue;
                }
            }
            out.push(tok.to_string());
            continue;
        }
        // -Xfriend-paths=PATH:PATH — colon-separated path list; rewrite.
        if let Some(eq) = tok.find('=') {
            let (k, v) = (&tok[..eq], &tok[eq + 1..]);
            if matches!(k, "-Xfriend-paths" | "-Xklib"
                          | "-Xcommon-sources"
                          | "-Xplugin"
                          | "-Xklib-relative-path-base") {
                out.push(format!("{k}={}",
                    rewrite_path_list_to_absolute(v, source_root)));
                continue;
            }
            // Other `--FLAG=value` — pass verbatim
            // (`-Xexpect-actual-classes`, `-Xmultiplatform`, etc.).
            out.push(tok.to_string());
            continue;
        }
        // Bare token — pass through (covers `-Xmultiplatform`,
        // `-Xjvm-default=all`, `-Xskip-prerelease-check`, etc.).
        out.push(tok.to_string());
    }
    out
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

/// Output of one shard's parse: the JVM build rules + every
/// top-level `<name> = <value>` variable definition. Variables get
/// merged across shards by [`Soong::extract_compilations`] before
/// the bindings are expanded — Soong sometimes defines a module's
/// variable in one shard and references it from a build rule in
/// another.
struct ShardData {
    javac_rules: Vec<JavacRule>,
    kotlin_rules: Vec<KotlincRule>,
    vars: HashMap<String, String>,
}

/// Stream one ninja shard and extract every JVM compilation rule's
/// bindings — both `javac/<mod>.jar` (Java) and `kotlin/<mod>.jar`
/// (Kotlin) — plus every top-level variable definition. The variables
/// are needed so that bindings like
/// `javacFlags = ${m.core-libart_android_common.javacFlags}` can be
/// resolved to the actual flag string (which includes the
/// `--patch-module=java.base=…` that libcore and friends need).
fn extract_jvm_rules(path: &Path) -> Result<ShardData> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read ninja shard {}", path.display()))?;
    let mut javac_out: Vec<JavacRule> = Vec::new();
    let mut kotlin_out: Vec<KotlincRule> = Vec::new();
    let mut vars: HashMap<String, String> = HashMap::new();
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
            // Non-indented, non-`build` line ends the current rule
            // block. Capture top-level variable definitions
            // (`<name> = <value>`) and skip everything else (`rule`,
            // `pool`, `default`, `subninja`, `include`, comments).
            flush(current.take(), &mut javac_out, &mut kotlin_out);
            if is_top_level_var_def(raw_line) {
                if let Some((k, v)) = parse_binding(raw_line) {
                    vars.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    flush(current.take(), &mut javac_out, &mut kotlin_out);
    Ok(ShardData { javac_rules: javac_out, kotlin_rules: kotlin_out, vars })
}

/// True when a non-indented line is a `<name> = <value>` variable
/// definition (the only top-level thing we care about). Filters out
/// ninja directives (`rule`, `pool`, `default`, `subninja`,
/// `include`), comments, and the `build` header (already handled
/// upstream).
fn is_top_level_var_def(line: &str) -> bool {
    if line.starts_with('#') || !line.contains('=') {
        return false;
    }
    const DIRECTIVES: &[&str] = &[
        "build ", "rule ", "pool ", "default ", "subninja ", "include ", "phony ",
    ];
    !DIRECTIVES.iter().any(|d| line.starts_with(d))
}

/// Expand ninja `${name}` and `$name` variable references in `value`
/// against `vars`. Resolves transitively (a var whose value itself
/// references another var) up to a small fixed-point bound so a
/// pathological cycle can't hang us. Unknown refs are kept verbatim
/// — better to surface them in javac's error than to silently drop.
fn expand_ninja_vars(value: &str, vars: &HashMap<String, String>) -> String {
    let mut cur = value.to_string();
    // 8 passes covers every Soong indirection depth seen in practice
    // (`m.<mod>.javacFlags` → `g.java.config.<…>` → constant) with
    // headroom; the loop exits early when no substitution fires.
    for _ in 0..8 {
        let next = expand_once(&cur, vars);
        if next == cur { return cur; }
        cur = next;
    }
    cur
}

fn expand_once(value: &str, vars: &HashMap<String, String>) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'$' {
            // Push the longest run of non-`$` bytes in one go.
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' { i += 1; }
            out.push_str(&value[start..i]);
            continue;
        }
        // `b == '$'`. Look at what follows.
        let next = bytes.get(i + 1).copied();
        match next {
            Some(b'{') => {
                // `${name}` form. Scan to matching `}`.
                if let Some(end) = value[i + 2..].find('}') {
                    let name = &value[i + 2 .. i + 2 + end];
                    if let Some(replacement) = vars.get(name) {
                        out.push_str(replacement);
                    } else {
                        out.push_str(&value[i .. i + 2 + end + 1]);
                    }
                    i += 2 + end + 1;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            Some(b'$') => {
                // Escaped `$$` → literal `$`.
                out.push('$');
                i += 2;
            }
            Some(c) if is_ninja_ident_byte(c) => {
                // `$name` bare form. Scan while identifier chars.
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && is_ninja_ident_byte(bytes[end]) {
                    end += 1;
                }
                let name = &value[start..end];
                if let Some(replacement) = vars.get(name) {
                    out.push_str(replacement);
                } else {
                    out.push_str(&value[i..end]);
                }
                i = end;
            }
            Some(_) | None => {
                // `$` followed by something we don't understand (a
                // space, end-of-string). Pass it through unchanged.
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

/// Bytes that ninja accepts in a variable name. Mirrors the
/// upstream lexer: letters, digits, underscore, dot, dash.
fn is_ninja_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'
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
            // `--FLAG=VALUE` form. Only rewrite the value for the
            // small whitelist of javac flags whose value is a path or
            // path list. For anything else (e.g. `--release=21`),
            // pass the token through unchanged — without the
            // whitelist, "21" gets path-joined into "<root>/21" and
            // javac rejects it.
            let (k, v) = (&tok[..eq], &tok[eq + 1..]);
            if is_path_bearing_eq_flag(k) {
                out.push(format!("{k}={}", rewrite_path_list_to_absolute(v, source_root)));
            } else {
                out.push(tok.to_string());
            }
        } else {
            // Bare token (e.g. `--enable-preview` or a bare path value
            // we already consumed via the previous flag branch — but
            // be defensive).
            out.push(tok.to_string());
        }
    }
    out
}

/// True for the small set of javac `--FLAG=VALUE` forms whose VALUE
/// is a path or a colon-separated path list. Everything else (e.g.
/// `--release=21`, `--enable-preview`) carries non-path data and
/// must round-trip verbatim.
fn is_path_bearing_eq_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--system"
        | "--module-path"
        | "--upgrade-module-path"
        | "--patch-module"
        | "--module-source-path"
        | "--processor-path"
        | "--processor-module-path"
        | "--source-path"
        | "--class-path"
    )
}

/// Rewrite each colon-separated entry in `list` to an absolute path
/// under `source_root` when it's relative. Absolute paths and empty
/// entries pass through unchanged. The javac magic value `none`
/// (used by `--system=none` to mean "no system modules") is also
/// passed through — without this, libcore/core-all silently becomes
/// `--system=<source_root>/none` and javac fails with "illegal
/// argument for --system".
fn rewrite_path_list_to_absolute(list: &str, source_root: &Path) -> String {
    list.split(':')
        .map(|s| {
            if s.is_empty()
                || s == "none"
                || Path::new(s).is_absolute()
            {
                s.to_string()
            } else {
                source_root.join(s).display().to_string()
            }
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
    fn tokenize_preserves_javac_magic_values() {
        // libcore/core-all is compiled with `--system=none` (no system
        // modules); the literal "none" is a javac magic value and must
        // not be turned into `<source_root>/none`.
        let bcp = tokenize_javac_arg_line("--system=none", Path::new("/aosp"));
        assert_eq!(bcp, vec!["--system=none".to_string()]);
        // Same for bare version numbers.
        let rel = tokenize_javac_arg_line("--release=21", Path::new("/aosp"));
        assert_eq!(rel, vec!["--release=21".to_string()]);
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
        let tmp = crate::scry_tmp_dir().join(format!(
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
        let shard = extract_jvm_rules(&ninja).unwrap();
        assert_eq!(shard.javac_rules.len(), 1, "javac-header should be skipped");
        assert_eq!(shard.kotlin_rules.len(), 1, "kotlin_headers should be skipped");
        let j = &shard.javac_rules[0];
        assert!(j.output.ends_with("/javac/core-libart.jar"));
        assert_eq!(j.bindings["classpath"], "-classpath a.jar:b.jar");
        assert_eq!(j.bindings["bootClasspath"], "-bootclasspath boot.jar");
        let k = &shard.kotlin_rules[0];
        assert!(k.output.ends_with("/kotlin/mod.jar"));
        assert_eq!(k.bindings["kotlinJvmTarget"], "21");
        assert_eq!(k.bindings["kotlincFlags"], "-Xfriend-paths=foo");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn extract_jvm_rules_collects_top_level_vars() {
        let tmp = crate::scry_tmp_dir().join(format!(
            "scry-bridge-soong-vars-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&tmp);
        let ninja = tmp.join("build.vars.ninja");
        std::fs::write(&ninja, "\
m.core-libart_android_common.javacFlags = -Xlint:-dep-ann --patch-module=java.base=.

build out/soong/.intermediates/libcore/core-libart/android_common/javac/core-libart.jar: javac in.java
    javacFlags = ${m.core-libart_android_common.javacFlags}
").unwrap();
        let shard = extract_jvm_rules(&ninja).unwrap();
        assert_eq!(
            shard.vars.get("m.core-libart_android_common.javacFlags").map(String::as_str),
            Some("-Xlint:-dep-ann --patch-module=java.base=."),
        );
        // The raw binding still holds the literal `${…}` reference;
        // expansion happens in extract_compilations, not here.
        assert_eq!(
            shard.javac_rules[0].bindings["javacFlags"],
            "${m.core-libart_android_common.javacFlags}",
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn expand_ninja_vars_handles_braced_bare_and_escape() {
        let mut vars = HashMap::new();
        vars.insert("a".to_string(), "ALPHA".to_string());
        vars.insert("m.foo.javacFlags".to_string(),
                    "--patch-module=java.base=.".to_string());
        // Braced.
        assert_eq!(
            expand_ninja_vars("x ${m.foo.javacFlags} y", &vars),
            "x --patch-module=java.base=. y",
        );
        // Bare (identifier stops at space).
        assert_eq!(expand_ninja_vars("$a end", &vars), "ALPHA end");
        // Escaped $$ stays literal.
        assert_eq!(expand_ninja_vars("price$$5", &vars), "price$5");
        // Unknown ref passes through unchanged (so the operator sees
        // it in javac stderr instead of getting a silent drop).
        assert_eq!(expand_ninja_vars("${unknown}", &vars), "${unknown}");
    }

    #[test]
    fn forward_javac_flags_keeps_patch_module_with_absolute_paths() {
        let src = "-Xlint:-dep-ann --patch-module=java.base=.:out/soong:abs/already/handled.jar";
        let out = forward_javac_flags(src, Path::new("/aosp"));
        // -Xlint passes verbatim; --patch-module path list is
        // rewritten to absolute under /aosp; "abs/already/handled.jar"
        // would also be rewritten since it's relative.
        assert_eq!(out, vec![
            "-Xlint:-dep-ann".to_string(),
            "--patch-module=java.base=/aosp/.:/aosp/out/soong:/aosp/abs/already/handled.jar"
                .to_string(),
        ]);
    }

    #[test]
    fn forward_javac_flags_drops_synthesized_and_dangerous_flags() {
        // -Werror, -source/-target/--release, -classpath/-cp,
        // -bootclasspath, --system=, -d, -processorpath all get
        // dropped (we synthesize the right versions elsewhere).
        let src = "-Werror -source 11 -target 11 --release=21 \
                   -classpath x.jar -cp y.jar -bootclasspath boot.jar \
                   --system=none -d /tmp/out -processorpath proc.jar \
                   -Xlint:all";
        let out = forward_javac_flags(src, Path::new("/aosp"));
        assert_eq!(out, vec!["-Xlint:all".to_string()]);
    }

    #[test]
    fn forward_kotlinc_flags_keeps_multiplatform_and_friend_paths() {
        let src = "-Xmultiplatform -Xexpect-actual-classes \
                   -Xfriend-paths=foo/a.jar:bar/b.jar \
                   -Xjvm-default=all -no-stdlib -d /tmp/out \
                   -jvm-target 21 -classpath some/jar.jar";
        let out = forward_kotlinc_flags(src, Path::new("/aosp"));
        // -no-stdlib, -d <dir>, -jvm-target <ver>, -classpath <jar> dropped.
        // Others pass through verbatim; -Xfriend-paths has its
        // colon-separated entries rewritten absolute.
        assert_eq!(out, vec![
            "-Xmultiplatform".to_string(),
            "-Xexpect-actual-classes".to_string(),
            "-Xfriend-paths=/aosp/foo/a.jar:/aosp/bar/b.jar".to_string(),
            "-Xjvm-default=all".to_string(),
        ]);
    }

    #[test]
    fn forward_javac_flags_handles_shell_quoted_errorprone_blob() {
        // AOSP wraps the entire Error Prone configuration in single
        // quotes as ONE shell-quoted arg. Without proper shell-aware
        // tokenization, the leading `'-Xplugin:` failed our prefix
        // check and every inner `-Xep:…` flag also leaked, causing
        // javac to bail with "invalid flag" before producing any
        // semanticdb output.
        let src = "-Xlint:-dep-ann \
                   '-Xplugin:ErrorProne -Xep:JdkObsolete:ERROR \
                    -XepExcludedPaths:.*/gen/.*'";
        let out = forward_javac_flags(src, Path::new("/aosp"));
        assert_eq!(out, vec!["-Xlint:-dep-ann".to_string()]);
    }

    #[test]
    fn forward_kotlinc_flags_drops_duplicate_semanticdb_plugin() {
        // A duplicate `-Xplugin=…semanticdb-kotlinc…` from Soong
        // must not collide with our own plugin registration; same
        // for any inherited `-P plugin:semanticdb-kotlinc:…` pairs.
        let src = "-Xplugin=/some/path/semanticdb-kotlinc-1.0.jar \
                   -P plugin:semanticdb-kotlinc:sourceroot=/old \
                   -Xmultiplatform";
        let out = forward_kotlinc_flags(src, Path::new("/aosp"));
        assert_eq!(out, vec!["-Xmultiplatform".to_string()]);
    }

    #[test]
    fn forward_javac_flags_preserves_module_system_flags() {
        // --add-modules / --add-exports / --add-reads / --limit-modules
        // must round-trip — they're how Soong opens cross-module
        // visibility for libcore consumers.
        let src = "--add-exports=java.base/java.lang=ALL-UNNAMED \
                   --add-reads=java.base=ALL-UNNAMED \
                   --add-modules=java.base \
                   --limit-modules=java.base";
        let out = forward_javac_flags(src, Path::new("/aosp"));
        assert_eq!(out, vec![
            "--add-exports=java.base/java.lang=ALL-UNNAMED".to_string(),
            "--add-reads=java.base=ALL-UNNAMED".to_string(),
            "--add-modules=java.base".to_string(),
            "--limit-modules=java.base".to_string(),
        ]);
    }

    #[test]
    fn expand_ninja_vars_resolves_nested_indirection() {
        let mut vars = HashMap::new();
        vars.insert("outer".to_string(), "${inner}-suffix".to_string());
        vars.insert("inner".to_string(), "VAL".to_string());
        assert_eq!(expand_ninja_vars("${outer}", &vars), "VAL-suffix");
    }
}
