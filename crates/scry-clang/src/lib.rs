//! Path B precision: drive libclang per-TU from a
//! `compile_commands.json` and emit the `clang_usrs.bin` sidecar
//! consumed by the `scry-store::clang_usrs` reader.
//!
//! Folded into the main `scry` binary as a subcommand so users get
//! one CLI surface. libclang itself is loaded dynamically at runtime
//! via `clang-sys`'s runtime feature, so users without libclang see
//! a clean error message — but they don't need an extra binary.
//!
//! Per-thread libclang loading (via `thread_local!`) is required:
//! clang-sys's runtime mode resolves symbols through thread-local
//! storage, and rayon workers each need their own load. Without
//! this, parallel parses panic on the second worker.

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use scry_store::clang_usrs::{UsrRecord, UsrSidecar};
use serde::Deserialize;
use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Deserialize)]
struct CompileCommand {
    directory: String,
    file: String,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default)]
    command: Option<String>,
}

thread_local! {
    static LIBCLANG_LOADED: std::cell::OnceCell<()> = const { std::cell::OnceCell::new() };
}

/// Each thread that calls libclang must load it once. clang-sys's
/// runtime mode resolves symbols via thread-local lookups; main's
/// load doesn't propagate to rayon workers.
fn ensure_libclang_loaded() -> Result<()> {
    LIBCLANG_LOADED.with(|c| {
        if c.get().is_some() {
            return Ok(());
        }
        clang_sys::load().map_err(|e| {
            anyhow!(
                "failed to load libclang ({e}). Install libclang-dev \
                 (Debian/Ubuntu) or clang-devel (Fedora/RHEL) and retry."
            )
        })?;
        let _ = c.set(());
        Ok(())
    })
}

/// Build the `clang_usrs.bin` sidecar in `index_dir`. Reads
/// `compile_commands` (compile_commands.json), filters by optional
/// `root` prefix, parses each TU through libclang in parallel,
/// interns USRs into a flat table.
pub fn build_clang_usrs(
    compile_commands: &Path,
    index_dir: &Path,
    root: Option<&Path>,
    workers: usize,
    max_file_bytes: u64,
) -> Result<()> {
    ensure_libclang_loaded()?;

    let raw = std::fs::read_to_string(compile_commands)
        .with_context(|| format!("read {}", compile_commands.display()))?;
    let cmds: Vec<CompileCommand> = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", compile_commands.display()))?;
    let total = cmds.len();
    let filtered: Vec<CompileCommand> = cmds
        .into_iter()
        .filter(|c| match root {
            None => true,
            Some(root) => {
                let f = PathBuf::from(&c.file);
                let abs = if f.is_absolute() {
                    f
                } else {
                    PathBuf::from(&c.directory).join(&f)
                };
                abs.starts_with(root)
            }
        })
        .collect();
    eprintln!(
        "[clang-index] {total} TUs in compile_commands.json, {} after root filter",
        filtered.len(),
    );

    // Best-effort: a process-wide rayon pool may already be sized.
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build_global()
        .ok();

    let sidecar = Arc::new(Mutex::new(UsrSidecar {
        version: 1,
        usr_table: Vec::new(),
        records: Vec::new(),
    }));
    let interner: Arc<Mutex<std::collections::HashMap<String, u32>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    let t = Instant::now();
    let parsed = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicUsize::new(0);
    let skipped_missing = std::sync::atomic::AtomicUsize::new(0);
    let n = filtered.len();

    filtered.par_iter().for_each(|cmd| {
        match parse_one(cmd, max_file_bytes, &sidecar, &interner) {
            Ok(ParseOutcome::Parsed) => {
                let done = parsed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if done % 50 == 0 || done == n {
                    eprintln!(
                        "[clang-index] progress: {done}/{n} TUs parsed, \
                         {} failed, {} skipped (missing source) ({}s)",
                        failed.load(std::sync::atomic::Ordering::Relaxed),
                        skipped_missing.load(std::sync::atomic::Ordering::Relaxed),
                        t.elapsed().as_secs(),
                    );
                }
            }
            Ok(ParseOutcome::SkippedMissingSource) => {
                // GN / CMake emit compile_commands.json for every graph
                // target, including ones whose generated source files
                // haven't been written by the build yet. Count those
                // separately from real parse failures.
                skipped_missing.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!("[clang-index] {} failed: {}", cmd.file, e);
            }
        }
    });

    let sidecar = Arc::try_unwrap(sidecar)
        .map_err(|_| anyhow!("sidecar still has outstanding refs"))?
        .into_inner();
    let out = index_dir.join("clang_usrs.bin");
    let buf = bincode::serialize(&sidecar).context("serialize sidecar")?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = out.with_extension("bin.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &out)
        .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))?;
    eprintln!(
        "[clang-index] done: {} USRs, {} records, {} bytes \
         ({} parsed, {} failed, {} skipped) → {} ({}s)",
        sidecar.usr_table.len(),
        sidecar.records.len(),
        buf.len(),
        parsed.load(std::sync::atomic::Ordering::Relaxed),
        failed.load(std::sync::atomic::Ordering::Relaxed),
        skipped_missing.load(std::sync::atomic::Ordering::Relaxed),
        out.display(),
        t.elapsed().as_secs(),
    );
    Ok(())
}

/// What happened when we tried to parse one compile command. Splits
/// the success case in two so callers can distinguish "the build
/// didn't emit this source" (a GN/CMake idiosyncrasy — not our bug,
/// not noise) from "we parsed it cleanly".
enum ParseOutcome {
    Parsed,
    SkippedMissingSource,
}

fn parse_one(
    cmd: &CompileCommand,
    max_bytes: u64,
    sidecar: &Mutex<UsrSidecar>,
    interner: &Mutex<std::collections::HashMap<String, u32>>,
) -> Result<ParseOutcome> {
    ensure_libclang_loaded()?;
    let src = PathBuf::from(&cmd.file);
    let src_abs = if src.is_absolute() {
        src
    } else {
        PathBuf::from(&cmd.directory).join(&src)
    };
    let meta = match std::fs::metadata(&src_abs) {
        Ok(m) => m,
        // GN and CMake list every graph target in
        // compile_commands.json including ones whose generated source
        // hasn't been written yet ("build target was never invoked").
        // Treat missing source as a clean skip, not a parse failure,
        // so the failure count reflects real errors only.
        Err(_) => return Ok(ParseOutcome::SkippedMissingSource),
    };
    if max_bytes > 0 && meta.len() > max_bytes {
        return Ok(ParseOutcome::Parsed);
    }

    let raw_args: Vec<String> = if !cmd.arguments.is_empty() {
        cmd.arguments.clone()
    } else if let Some(c) = &cmd.command {
        shell_words::split(c).with_context(|| format!("split command for {}", cmd.file))?
    } else {
        Vec::new()
    };
    let abs_args = rewrite_relative_paths(&raw_args, Path::new(&cmd.directory));
    // Strip the source file in EVERY form it might appear in the
    // command: the absolute path stored in cmd.file, plus its
    // path relative to cmd.directory. Some build systems write
    // `init/main.c` (relative) into the cmdline even though they
    // report the absolute form in cmd.file (the kernel does this).
    let src_file_rel = Path::new(&cmd.file)
        .strip_prefix(&cmd.directory)
        .map(|p| p.to_string_lossy().into_owned());
    let mut src_forms: Vec<String> = vec![cmd.file.clone()];
    if let Ok(rel) = &src_file_rel { src_forms.push(rel.clone()); }
    let filtered_args = filter_args(&abs_args, &src_forms);

    let records = unsafe { parse_tu_unsafe(&src_abs, &filtered_args)? };
    if records.is_empty() {
        return Ok(ParseOutcome::Parsed);
    }

    let mut local_recs: Vec<UsrRecord> = Vec::with_capacity(records.len());
    {
        let mut intern_guard = interner.lock();
        let mut side_guard = sidecar.lock();
        for (abs_path, byte_offset, usr, kind) in records {
            let id = match intern_guard.get(&usr) {
                Some(&id) => id,
                None => {
                    let id = side_guard.usr_table.len() as u32;
                    side_guard.usr_table.push(usr.clone());
                    intern_guard.insert(usr, id);
                    id
                }
            };
            local_recs.push(UsrRecord {
                abs_path,
                byte_offset,
                usr_id: id,
                kind,
            });
        }
        side_guard.records.extend(local_recs);
    }
    Ok(ParseOutcome::Parsed)
}

/// Rewrite every relative path in `-I`, `-isystem`, `-iquote`,
/// `-include`, `-include-pch`, and `-L` flags to be absolute under
/// `directory` (the compdb entry's `directory` field).
///
/// Why: libclang resolves relative paths against the process's
/// current working directory. The kernel + Chromium-style projects
/// emit `-I./include` and `-include ./linux/compiler-version.h`
/// flags relative to the build directory. Without this rewrite,
/// libclang either fails to find headers (→ CXError_ASTReadError)
/// or reads the wrong files. Setting `chdir(directory)` per call
/// would race in the Rayon-parallel parse, so we rewrite per-arg
/// instead.
fn rewrite_relative_paths(raw: &[String], directory: &Path) -> Vec<String> {
    // Flags whose VALUE is a path. Stored once; we match each input
    // arg against the list as either `-FLAG VALUE` (two tokens) or
    // `-FLAGVALUE` (one token, e.g. `-I./include`).
    const PATH_FLAGS: &[&str] = &[
        "-I", "-isystem", "-iquote", "-idirafter",
        "-include", "-include-pch",
        "-L", "-F", "-iframework",
    ];
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let a = &raw[i];
        // Two-token form: `-I include/path`.
        if PATH_FLAGS.iter().any(|f| *f == a) {
            out.push(a.clone());
            if i + 1 < raw.len() {
                out.push(absolutise(&raw[i + 1], directory));
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // One-token form: `-Iinclude/path`. First matching prefix
        // wins; longer prefixes are checked first so `-isystemFOO`
        // doesn't get split as `-I` + `systemFOO`.
        if let Some(rewritten) = try_rewrite_glued_path_flag(a, PATH_FLAGS, directory) {
            out.push(rewritten);
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// If `arg` is a glued path flag (`-Iinclude` shape), return the
/// rewritten form with the value absolutised. Otherwise return
/// `None`. Longest-prefix-first so `-isystem./x` matches `-isystem`
/// before falling through to `-I`.
fn try_rewrite_glued_path_flag(
    arg: &str,
    path_flags: &[&str],
    directory: &Path,
) -> Option<String> {
    // Sort prefixes longest-first so overlapping prefixes don't
    // misroute. The list is short and cached in a thread-local; per-
    // call sort is fine for our throughput.
    let mut prefixes: Vec<&&str> = path_flags.iter().collect();
    prefixes.sort_by(|a, b| b.len().cmp(&a.len()));
    for prefix in prefixes {
        if arg.starts_with(*prefix) && arg.len() > prefix.len() {
            let val = &arg[prefix.len()..];
            return Some(format!("{prefix}{}", absolutise(val, directory)));
        }
    }
    None
}

/// Resolve `value` against `directory` when `value` is a relative
/// path. Tokens that don't look like paths (start with `-`, `=`, or
/// are bare flags) pass through unchanged.
fn absolutise(value: &str, directory: &Path) -> String {
    if value.starts_with('-') || value.starts_with('=') || value.is_empty() {
        return value.to_string();
    }
    let p = Path::new(value);
    if p.is_absolute() {
        return value.to_string();
    }
    directory.join(p).to_string_lossy().into_owned()
}

/// Build the argv libclang sees: drop compile-driver args it
/// shouldn't, plus gcc-only flags libclang errors on, plus add
/// tolerance flags so we keep parsing TUs whose original compile
/// invocation used `-W` flags newer than our linked libclang knows
/// about.
///
/// Three concerns:
///
/// 1. **gcc-only flags** the kernel + busybox-style C uses.
///    `-fno-allow-store-data-races`, `-fconserve-stack`,
///    `-mrecord-mcount`, `-fsanitize=bounds-strict`, etc. clang
///    rejects these as **driver errors** before the AST is built —
///    no `-Wno-*` flag silences them. Stripped by exact match or
///    prefix.
///
/// 2. **Tolerance flags** PREPENDED + APPENDED:
///      - `-Wno-unknown-warning-option` — silences `unknown -W flag`.
///      - `-Wno-error` — downgrades any errors-from-warnings back to
///        warnings.
///    Prepended so they take effect for early `-Werror=...`; appended
///    so they win over a blanket `-Werror` later in the cmdline.
///
/// 3. **Driver args** libclang shouldn't see: `-o OUT`, `-c`, the
///    source-file argument itself.
fn filter_args(raw: &[String], src_file_forms: &[String]) -> Vec<CString> {
    const TOLERANCE: &[&str] = &["-Wno-unknown-warning-option", "-Wno-error"];
    let mut out: Vec<CString> = Vec::with_capacity(raw.len() + 2 * TOLERANCE.len());
    for flag in TOLERANCE {
        out.push(CString::new(*flag).unwrap());
    }
    let mut iter = raw.iter().enumerate();
    iter.next(); // skip argv[0]
    while let Some((_, a)) = iter.next() {
        if a == "-o" {
            iter.next();
            continue;
        }
        if a.starts_with("-o") && a.len() > 2 {
            continue;
        }
        if a == "-c" {
            continue;
        }
        if is_source_file_arg(a, src_file_forms) {
            continue;
        }
        if is_gcc_only_flag(a) {
            continue;
        }
        if let Ok(c) = CString::new(a.as_bytes()) {
            out.push(c);
        }
    }
    for flag in TOLERANCE {
        out.push(CString::new(*flag).unwrap());
    }
    out
}

/// Return true if `arg` names the source file the TU is parsing.
/// libclang takes the source file as a SEPARATE C-side argument
/// from the args list; if it also appears in args, libclang treats
/// it as a second input and fails to read the AST.
fn is_source_file_arg(arg: &str, src_forms: &[String]) -> bool {
    for form in src_forms {
        if arg == form { return true; }
        // Also catch `./init/main.c` when src_form is `init/main.c`.
        if form.starts_with('/') {
            // Absolute form — already exact-matched above.
            continue;
        }
        let with_dot = format!("./{form}");
        if arg == with_dot { return true; }
    }
    false
}

/// Return true for flags clang rejects with a driver error that
/// only gcc accepts. Curated from the Linux kernel's `KBUILD_CFLAGS`
/// and the gcc-vs-clang divergence list; flags clang has its own
/// equivalents for (e.g. `-fcf-protection=*`) are NOT stripped here.
///
/// The list is intentionally narrow — stripping anything beyond
/// confirmed gcc-only flags risks dropping configuration clang's
/// front-end actually needs. Add entries when a TU parse failure
/// surfaces a new gcc-only flag.
fn is_gcc_only_flag(arg: &str) -> bool {
    // Exact matches: simple flags that don't take a value.
    const EXACT: &[&str] = &[
        // gcc-only optimisation knobs
        "-fno-allow-store-data-races",
        "-fconserve-stack",
        "-fno-conserve-stack",
        "-fno-var-tracking-assignments",
        "-fno-var-tracking",
        "-femit-struct-debug-baseonly",
        "-fno-tree-loop-im",
        "-fno-ipa-sra",
        "-fno-ipa-cp-clone",
        "-fno-keep-static-consts",
        // gcc-only x86 micro-arch knobs the kernel uses
        "-mrecord-mcount",
        "-mno-fp-ret-in-387",
        "-mskip-rax-setup",
        "-mindirect-branch=thunk-extern",
        "-mindirect-branch-register",
        "-mfunction-return=thunk-extern",
        // gcc -W flags clang doesn't have (silenced by
        // -Wno-unknown-warning-option but listed here for clarity)
        "-Wno-format-truncation",
        "-Wno-format-overflow",
        "-Wno-stringop-truncation",
        "-Wno-stringop-overflow",
        "-Wno-restrict",
        "-Wno-maybe-uninitialized",
        "-Wno-packed-not-aligned",
    ];
    if EXACT.iter().any(|f| *f == arg) {
        return true;
    }
    // Prefixed matches: flags with embedded values.
    const PREFIX: &[&str] = &[
        "-fsanitize=bounds-strict",      // kernel UBSAN, gcc-only variant
        "-falign-loops=",                // clang has -falign-loops as no-value
        "-falign-jumps=",                // ditto
        "-mpreferred-stack-boundary=",   // gcc-only
        "-mindirect-branch=",            // gcc-only (clang has -mretpoline)
        "-mfunction-return=",            // gcc-only
    ];
    PREFIX.iter().any(|p| arg.starts_with(p))
}

/// Walk the TU's cursor tree and collect (path, offset, USR, kind).
///
/// Safety: caller must have already loaded libclang on this thread.
/// All clang_sys FFI calls are paired with their disposers; visitor
/// data is a pinned `&mut Vec` on this thread's stack.
unsafe fn parse_tu_unsafe(
    src: &Path,
    args: &[CString],
) -> Result<Vec<(String, u32, String, u8)>> {
    use clang_sys::*;
    let idx = unsafe { clang_createIndex(0, 0) };
    if idx.is_null() {
        return Err(anyhow!("clang_createIndex returned NULL"));
    }

    let src_c = CString::new(src.to_string_lossy().as_bytes())
        .context("source path → CString")?;
    let arg_ptrs: Vec<*const i8> =
        args.iter().map(|a| a.as_ptr().cast()).collect();

    let mut tu: CXTranslationUnit = ptr::null_mut();
    let err = unsafe {
        clang_parseTranslationUnit2(
            idx,
            src_c.as_ptr(),
            arg_ptrs.as_ptr().cast(),
            arg_ptrs.len() as c_int,
            ptr::null_mut(),
            0,
            CXTranslationUnit_DetailedPreprocessingRecord
                | CXTranslationUnit_SkipFunctionBodies,
            &raw mut tu,
        )
    };
    if err != CXError_Success || tu.is_null() {
        unsafe { clang_disposeIndex(idx) };
        return Err(anyhow!("clang_parseTranslationUnit2 failed: code {err}"));
    }

    let mut acc: Vec<(String, u32, String, u8)> = Vec::new();
    let root = unsafe { clang_getTranslationUnitCursor(tu) };
    unsafe {
        clang_visitChildren(
            root,
            visit_callback,
            (&raw mut acc).cast::<c_void>(),
        );
    }

    unsafe {
        clang_disposeTranslationUnit(tu);
        clang_disposeIndex(idx);
    }
    Ok(acc)
}

#[allow(non_upper_case_globals)] // CX* C-API constants
extern "C" fn visit_callback(
    cursor: clang_sys::CXCursor,
    _parent: clang_sys::CXCursor,
    data: clang_sys::CXClientData,
) -> clang_sys::CXChildVisitResult {
    use clang_sys::*;
    let acc = unsafe { &mut *data.cast::<Vec<(String, u32, String, u8)>>() };

    let kind = unsafe { clang_getCursorKind(cursor) };
    let record_kind: Option<u8> = match kind {
        CXCursor_FunctionDecl
        | CXCursor_CXXMethod
        | CXCursor_Constructor
        | CXCursor_Destructor
        | CXCursor_VarDecl
        | CXCursor_FieldDecl
        | CXCursor_StructDecl
        | CXCursor_ClassDecl
        | CXCursor_EnumDecl
        | CXCursor_TypedefDecl
        | CXCursor_Namespace => Some(0),
        CXCursor_CallExpr => Some(2),
        CXCursor_DeclRefExpr
        | CXCursor_MemberRefExpr
        | CXCursor_TypeRef
        | CXCursor_TemplateRef => Some(1),
        _ => None,
    };
    if let Some(k) = record_kind {
        let usr_cursor = if k == 0 {
            cursor
        } else {
            unsafe { clang_getCursorReferenced(cursor) }
        };
        if unsafe { clang_Cursor_isNull(usr_cursor) } == 0 {
            let usr_cx = unsafe { clang_getCursorUSR(usr_cursor) };
            let usr = unsafe { cxstring_to_owned(usr_cx) };
            if !usr.is_empty() {
                let loc = unsafe { clang_getCursorLocation(cursor) };
                let (path, offset) = unsafe { location_to_path_and_offset(loc) };
                if !path.is_empty() && !is_system_path(&path) {
                    acc.push((path, offset, usr, k));
                }
            }
        }
    }
    CXChildVisit_Recurse
}

/// Skip records in system headers — these blow up the sidecar size
/// without adding value (users grep their own code, not libstdc++).
fn is_system_path(p: &str) -> bool {
    p.starts_with("/usr/include/")
        || p.starts_with("/usr/lib/gcc/")
        || p.starts_with("/usr/lib/llvm-")
        || p.starts_with("/usr/lib/x86_64-linux-gnu/")
        || p.starts_with("/usr/local/include/")
}

unsafe fn cxstring_to_owned(s: clang_sys::CXString) -> String {
    use clang_sys::*;
    let raw = unsafe { clang_getCString(s) };
    let out = if raw.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(raw).to_string_lossy().into_owned() }
    };
    unsafe { clang_disposeString(s) };
    out
}

unsafe fn location_to_path_and_offset(
    loc: clang_sys::CXSourceLocation,
) -> (String, u32) {
    use clang_sys::*;
    let mut file: CXFile = ptr::null_mut();
    let mut offset: u32 = 0;
    unsafe {
        clang_getSpellingLocation(
            loc,
            &raw mut file,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut offset,
        );
    }
    if file.is_null() {
        return (String::new(), 0);
    }
    let name = unsafe { clang_getFileName(file) };
    (unsafe { cxstring_to_owned(name) }, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_two_token_include_to_absolute() {
        let raw = vec!["-I".into(), "./include".into()];
        let out = rewrite_relative_paths(&raw, Path::new("/build"));
        assert_eq!(out, vec!["-I".to_string(), "/build/./include".to_string()]);
    }

    #[test]
    fn rewrite_glued_include_to_absolute_no_duplicate_or_skip() {
        // The bug: nested for-loop `continue` continued the FOR loop
        // not the WHILE loop, so the original arg got duplicated and
        // the next arg got skipped. Regression test pins the right
        // semantics: one rewritten output per input, no duplication.
        let raw = vec![
            "-I./include".into(),
            "-D__KERNEL__".into(),
            "-Iarch/x86/include".into(),
        ];
        let out = rewrite_relative_paths(&raw, Path::new("/build"));
        assert_eq!(out, vec![
            "-I/build/./include".to_string(),
            "-D__KERNEL__".to_string(),
            "-I/build/arch/x86/include".to_string(),
        ]);
    }

    #[test]
    fn rewrite_keeps_absolute_paths_unchanged() {
        let raw = vec!["-I/abs/include".into(), "-I./rel".into()];
        let out = rewrite_relative_paths(&raw, Path::new("/build"));
        assert_eq!(out, vec![
            "-I/abs/include".to_string(),
            "-I/build/./rel".to_string(),
        ]);
    }

    #[test]
    fn rewrite_longest_prefix_wins() {
        // `-isystem./x` must NOT be split as `-I` + `system./x`.
        let raw = vec!["-isystem./x".into()];
        let out = rewrite_relative_paths(&raw, Path::new("/build"));
        assert_eq!(out, vec!["-isystem/build/./x".to_string()]);
    }

    #[test]
    fn rewrite_include_pair_value_absolutised() {
        let raw = vec!["-include".into(), "./linux/kconfig.h".into()];
        let out = rewrite_relative_paths(&raw, Path::new("/build"));
        assert_eq!(out, vec![
            "-include".to_string(),
            "/build/./linux/kconfig.h".to_string(),
        ]);
    }

    #[test]
    fn is_source_file_arg_matches_absolute_relative_and_dotted() {
        // src_forms = [abs, rel] per parse_one.
        let forms = vec![
            "/build/init/main.c".to_string(),
            "init/main.c".to_string(),
        ];
        assert!(is_source_file_arg("/build/init/main.c", &forms));
        assert!(is_source_file_arg("init/main.c", &forms));
        assert!(is_source_file_arg("./init/main.c", &forms));
        assert!(!is_source_file_arg("other.c", &forms));
        assert!(!is_source_file_arg("-Iinit/main.c", &forms));
    }

    #[test]
    fn filter_drops_source_file_in_all_forms() {
        let raw = vec![
            "gcc".into(),
            "-c".into(),
            "-o".into(), "init/main.o".into(),
            "-I/build/include".into(),
            "init/main.c".into(),       // relative form — must drop
        ];
        let forms = vec![
            "/build/init/main.c".to_string(),
            "init/main.c".to_string(),
        ];
        let out = filter_args(&raw, &forms);
        let strs: Vec<String> = out.iter().map(|c| c.to_string_lossy().into_owned()).collect();
        assert!(!strs.iter().any(|s| s.ends_with("main.c") || s == "init/main.c"),
                "source file leaked through: {strs:?}");
        // Sanity: tolerance flags + the include arg survived.
        assert!(strs.iter().any(|s| s == "-Wno-error"));
        assert!(strs.iter().any(|s| s == "-I/build/include"));
    }

    #[test]
    fn is_gcc_only_flag_recognises_kernel_flags() {
        assert!(is_gcc_only_flag("-fno-allow-store-data-races"));
        assert!(is_gcc_only_flag("-mindirect-branch=thunk-extern"));
        assert!(is_gcc_only_flag("-mindirect-branch-register"));
        assert!(is_gcc_only_flag("-falign-jumps=1"));
        assert!(is_gcc_only_flag("-fsanitize=bounds-strict"));
        // Clang-supported flags must NOT be stripped.
        assert!(!is_gcc_only_flag("-fno-common"));
        assert!(!is_gcc_only_flag("-fcf-protection=none"));
        assert!(!is_gcc_only_flag("-O2"));
        assert!(!is_gcc_only_flag("-Wno-sign-compare"));
    }
}
