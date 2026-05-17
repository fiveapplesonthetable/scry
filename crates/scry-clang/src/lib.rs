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
    let n = filtered.len();

    filtered.par_iter().for_each(|cmd| {
        match parse_one(cmd, max_file_bytes, &sidecar, &interner) {
            Ok(()) => {
                let done = parsed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if done % 50 == 0 || done == n {
                    eprintln!(
                        "[clang-index] progress: {done}/{n} TUs parsed, \
                         {} failed ({}s)",
                        failed.load(std::sync::atomic::Ordering::Relaxed),
                        t.elapsed().as_secs(),
                    );
                }
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
        "[clang-index] done: {} USRs, {} records, {} bytes → {} ({}s)",
        sidecar.usr_table.len(),
        sidecar.records.len(),
        buf.len(),
        out.display(),
        t.elapsed().as_secs(),
    );
    Ok(())
}

fn parse_one(
    cmd: &CompileCommand,
    max_bytes: u64,
    sidecar: &Mutex<UsrSidecar>,
    interner: &Mutex<std::collections::HashMap<String, u32>>,
) -> Result<()> {
    ensure_libclang_loaded()?;
    let src = PathBuf::from(&cmd.file);
    let src_abs = if src.is_absolute() {
        src
    } else {
        PathBuf::from(&cmd.directory).join(&src)
    };
    if let Ok(meta) = std::fs::metadata(&src_abs) {
        if max_bytes > 0 && meta.len() > max_bytes {
            return Ok(());
        }
    }

    let raw_args: Vec<String> = if !cmd.arguments.is_empty() {
        cmd.arguments.clone()
    } else if let Some(c) = &cmd.command {
        shell_words::split(c).with_context(|| format!("split command for {}", cmd.file))?
    } else {
        Vec::new()
    };
    let filtered_args = filter_args(&raw_args, &cmd.file);

    let records = unsafe { parse_tu_unsafe(&src_abs, &filtered_args)? };
    if records.is_empty() {
        return Ok(());
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
    Ok(())
}

/// Build the argv libclang sees: drop compile-driver args it shouldn't,
/// then prepend tolerance flags so we keep parsing TUs whose original
/// compile invocation used `-W` flags newer than our linked libclang
/// knows about (common on Chromium-flavored sub-repos in AOSP that
/// pass `-Wno-cast-function-type-mismatch` etc.). Without these
/// prefixes libclang emits `-Werror,-Wunknown-warning-option` and
/// aborts the entire TU — losing every symbol from that file even
/// though the parse would have otherwise succeeded.
///
/// Two flags, both load-bearing:
///   - `-Wno-unknown-warning-option` — silences "unknown -W flag".
///   - `-Wno-error` — downgrades any remaining errors-from-warnings
///     back to warnings (e.g. `-Werror=foo` in the original cmdline).
///
/// We PREPEND them so they take effect even if the original args
/// later set `-Werror`. libclang processes args left-to-right and
/// later flags win, so we then APPEND a second copy as belt-and-
/// braces: the prepend wins for `-Werror=...` (which appears once
/// near the front), the append wins for any blanket `-Werror`.
fn filter_args(raw: &[String], src_file: &str) -> Vec<CString> {
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
        if a == src_file || a.ends_with(&format!("/{src_file}")) {
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
