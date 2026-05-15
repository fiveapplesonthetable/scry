//! scry: semantic code search and cross-reference engine for AOSP and Linux.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use scry_lang::FormatRegistry;
use scry_store::{
    FileEntry, IndexStats, RefRecord, RootEntry, StoreReader, StoreWriter,
    SymbolKind, SymbolRecord,
};
use scry_walker::{collect_files, FileKind, Profile, RawFile};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "scry", version, about = "Semantic code search for AOSP and Linux")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Walk source root(s), parse files, and write the on-disk index.
    Index {
        /// Source root(s). Default: ~/dev/aosp + /mnt/agent/dev/linux if present.
        roots: Vec<PathBuf>,
        /// Override profile (aosp / linux / generic). Default: auto-detect per root.
        #[arg(long)]
        profile: Option<String>,
        /// Output index directory. Default: /mnt/agent/scry-index or $SCRY_INDEX_DIR.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
        /// Just walk and count — do not parse or write. (Phase-0 behavior.)
        #[arg(long)]
        count_only: bool,
        /// Limit per-root file count for quick smoke tests.
        #[arg(long)]
        limit: Option<usize>,
        /// Skip extracting references (much smaller index, no callers/ref queries).
        #[arg(long)]
        no_refs: bool,
        /// Limit the rayon thread pool. Default: all cores. Lower this on
        /// shared / memory-constrained hosts.
        #[arg(long)]
        workers: Option<usize>,
        /// Skip individual source files larger than this many bytes. Default
        /// 5 MiB. Most AOSP files over this size are auto-generated or
        /// binary-ish and slow the parser disproportionately.
        #[arg(long, default_value_t = 5 * 1024 * 1024)]
        max_file_bytes: u64,
    },
    /// Look up references to a name.
    Ref {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find callers of NAME (refs with kind=call).
    Callers {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Look up exact symbol definitions by name.
    Def {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Emit Markdown with code snippets (LLM-friendly).
        #[arg(long)]
        md: bool,
        /// Cap total output size in bytes (drops lowest-ranked results).
        #[arg(long)]
        budget: Option<usize>,
    },
    /// Prefix-match symbol names.
    Prefix {
        prefix: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Substring (fuzzy-ish) search over symbol names.
    Fuzzy {
        substr: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show index metadata.
    Stats {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Substring or regex search over indexed source files (rg-like).
    Grep {
        pattern: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long, value_name = "PREFIX")]
        in_: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// JSON-RPC server reading newline-delimited requests on stdin.
    /// Each request is {"id": N, "cmd": "def|ref|callers|prefix|fuzzy|grep|stats", "args": {...}}.
    /// Responses are one JSON object per request.
    Serve {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Show Soong modules matching NAME (sugar for `def NAME --kind soong`).
    Mod {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value = "20")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Which Soong module declares PATH as one of its sources?
    /// Looks up the file's basename across Soong Import refs.
    ModuleOf {
        path: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value = "10")]
        limit: usize,
    },
}

fn default_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let aosp = PathBuf::from(home).join("dev/aosp");
        if aosp.is_dir() {
            v.push(aosp);
        }
    }
    let linux = PathBuf::from("/mnt/agent/dev/linux");
    if linux.is_dir() {
        v.push(linux);
    }
    v
}

fn default_index_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SCRY_INDEX_DIR") {
        PathBuf::from(p)
    } else {
        PathBuf::from("/mnt/agent/scry-index")
    }
}

fn human_bytes(b: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut x = b as f64;
    let mut i = 0;
    while x >= 1024.0 && i + 1 < U.len() {
        x /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", x, U[i])
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { roots, profile, out, count_only, limit, no_refs, workers, max_file_bytes } => {
            cmd_index(roots, profile, out, count_only, limit, no_refs, workers, max_file_bytes)
        }
        Cmd::Def { name, index, lang, kind, limit, json, md, budget } => {
            cmd_def(name, index, lang, kind, limit, json, md, budget)
        }
        Cmd::Prefix { prefix, index, limit, json } => {
            cmd_prefix(prefix, index, limit, json)
        }
        Cmd::Fuzzy { substr, index, limit, json } => {
            cmd_fuzzy(substr, index, limit, json)
        }
        Cmd::Ref { name, index, lang, kind, limit, json } => {
            cmd_ref(name, index, lang, kind, limit, json)
        }
        Cmd::Callers { name, index, lang, limit, json } => {
            cmd_ref(name, index, lang, Some("call".to_string()), limit, json)
        }
        Cmd::Stats { index } => cmd_stats(index),
        Cmd::Grep { pattern, index, regex, lang, in_, limit, json } => {
            cmd_grep(pattern, index, regex, lang, in_, limit, json)
        }
        Cmd::Serve { index } => cmd_serve(index),
        Cmd::Mod { name, index, limit, json } => {
            cmd_def(name, index, None, Some("soong".into()), limit, json, false, None)
        }
        Cmd::ModuleOf { path, index, limit } => cmd_module_of(path, index, limit),
    }
}

// ---------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------

fn cmd_index(
    roots: Vec<PathBuf>,
    profile: Option<String>,
    out: Option<PathBuf>,
    count_only: bool,
    limit: Option<usize>,
    no_refs: bool,
    workers: Option<usize>,
    max_file_bytes: u64,
) -> Result<()> {
    if let Some(n) = workers {
        if n > 0 {
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build_global()
                .map_err(|e| anyhow::anyhow!("rayon pool init: {e}"))?;
            eprintln!("[index] rayon pool: {} workers", n);
        }
    }

    // Build the format registry once. Adding a new format in the future is:
    //   1. add a FileKind to scry-walker,
    //   2. implement a parser fn (kind, source) -> (Vec<RawSymbol>, Vec<RawRef>),
    //   3. register it in scry-lang::tree_sitter_parsers() or scry-aosp::aosp_parsers().
    let mut registry = FormatRegistry::new();
    for p in scry_lang::tree_sitter_parsers() { registry.register(p); }
    for p in scry_aosp::aosp_parsers() { registry.register(p); }
    eprintln!("[index] registered {} format parsers", registry.list().len());
    let registry = Arc::new(registry);
    let roots = if roots.is_empty() { default_roots() } else { roots };
    if roots.is_empty() {
        anyhow::bail!("no source roots: pass one or more paths");
    }
    let out_dir = out.unwrap_or_else(default_index_dir);

    let t_total = Instant::now();
    let mut writer = StoreWriter::new(&out_dir);
    let mut next_file_id: u32 = 0;
    let mut total_files_total: u64 = 0;
    let mut total_files_parsed: u64 = 0;
    let mut total_files_failed: u64 = 0;
    let mut total_bytes: u64 = 0;

    for (root_id, root) in roots.iter().enumerate() {
        if root_id > u8::MAX as usize {
            anyhow::bail!("too many roots (max 256)");
        }
        let root_id = root_id as u8;
        let prof = match &profile {
            Some(s) => Profile::parse(s)?,
            None => Profile::auto_detect(root),
        };
        eprintln!("[walk]  {} (profile: {:?})", root.display(), prof);
        let mut collected = collect_files(root, prof)?;
        if let Some(n) = limit { collected.files.truncate(n); }
        eprintln!(
            "[walk]  {} files / {} / {} ms",
            collected.files.len(),
            human_bytes(collected.total_bytes),
            collected.elapsed_ms
        );
        total_files_total += collected.files.len() as u64;
        total_bytes += collected.total_bytes;

        writer.roots.push(RootEntry {
            id: root_id,
            path: collected.root.display().to_string(),
            profile: prof,
        });

        // Assign file_ids and create FileEntry records, parallel-parse.
        let files_start_id = next_file_id;
        let file_entries: Vec<FileEntry> = collected
            .files
            .iter()
            .enumerate()
            .map(|(i, rf)| FileEntry {
                id: files_start_id + i as u32,
                root_id,
                relpath: rf.relpath.display().to_string(),
                kind: rf.kind,
                size: rf.size,
            })
            .collect();
        next_file_id += collected.files.len() as u32;

        if count_only {
            writer.files.extend(file_entries);
            continue;
        }

        // Parse in parallel. Drain directly into the shared writer to avoid
        // a 2x RAM doubling from rayon's map+collect intermediate.
        let parsed = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let symbols_total = Arc::new(AtomicU64::new(0));
        let refs_total = Arc::new(AtomicU64::new(0));

        let parse_start = Instant::now();
        // Take ownership of the writer's symbol+ref vecs behind mutexes so
        // worker threads can push directly. We move them back after parsing.
        let syms_sink = parking_lot::Mutex::new(std::mem::take(&mut writer.symbols));
        let refs_sink = parking_lot::Mutex::new(std::mem::take(&mut writer.refs));

        collected
            .files
            .par_iter()
            .zip(file_entries.par_iter())
            .for_each(|(rf, fe)| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    parse_one(rf, fe, root_id, no_refs, max_file_bytes, &registry)
                }));
                match result {
                    Ok(Ok((s, r))) => {
                        parsed.fetch_add(1, Ordering::Relaxed);
                        symbols_total.fetch_add(s.len() as u64, Ordering::Relaxed);
                        refs_total.fetch_add(r.len() as u64, Ordering::Relaxed);
                        if !s.is_empty() {
                            syms_sink.lock().extend(s);
                        }
                        if !r.is_empty() {
                            refs_sink.lock().extend(r);
                        }
                    }
                    _ => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

        writer.symbols = syms_sink.into_inner();
        writer.refs = refs_sink.into_inner();

        let parse_ms = parse_start.elapsed().as_millis();
        let parsed_n = parsed.load(Ordering::Relaxed);
        let failed_n = failed.load(Ordering::Relaxed);
        let syms_n = symbols_total.load(Ordering::Relaxed);
        let refs_n = refs_total.load(Ordering::Relaxed);
        eprintln!(
            "[parse] {} parsed, {} failed, {} symbols, {} refs, {} ms",
            parsed_n, failed_n, syms_n, refs_n, parse_ms
        );

        total_files_parsed += parsed_n;
        total_files_failed += failed_n;

        writer.files.extend(file_entries);
    }

    // Layer-1 resolution before writing.
    if !count_only && !no_refs {
        let t_res = Instant::now();
        writer.resolve_refs();
        let resolved = writer.refs.iter().filter(|r| r.resolved_to.is_some()).count();
        eprintln!(
            "[resolve] {} / {} refs resolved by name in {} ms",
            resolved,
            writer.refs.len(),
            t_res.elapsed().as_millis()
        );
    }

    let elapsed_ms = t_total.elapsed().as_millis();
    let stats = IndexStats {
        files_total: total_files_total,
        files_parsed: total_files_parsed,
        files_failed: total_files_failed,
        bytes_total: total_bytes,
        symbols: writer.symbols.len() as u64,
        refs: writer.refs.len() as u64,
        elapsed_ms,
    };
    let n_symbols = writer.symbols.len();
    let n_refs = writer.refs.len();
    eprintln!(
        "[write] {} symbols, {} refs across {} files / {} roots, finalizing -> {}",
        n_symbols,
        n_refs,
        writer.files.len(),
        writer.roots.len(),
        out_dir.display()
    );
    if !count_only {
        let t = Instant::now();
        writer.finalize(stats)?;
        eprintln!("[write] finalized in {} ms", t.elapsed().as_millis());
    } else {
        eprintln!("[write] count_only=true, not writing index");
    }
    eprintln!("\nDONE: {} files, {} symbols, total {} ms ({:.1} files/s)",
        total_files_total,
        n_symbols,
        elapsed_ms,
        total_files_total as f64 / (elapsed_ms.max(1) as f64 / 1000.0),
    );
    Ok(())
}

fn parse_one(
    rf: &RawFile,
    fe: &FileEntry,
    root_id: u8,
    no_refs: bool,
    max_file_bytes: u64,
    registry: &FormatRegistry,
) -> Result<(Vec<SymbolRecord>, Vec<RefRecord>)> {
    if !registry.supports(rf.kind) {
        return Ok((Vec::new(), Vec::new()));
    }
    if rf.size > max_file_bytes {
        return Ok((Vec::new(), Vec::new()));
    }
    let bytes = std::fs::read(&rf.path)
        .with_context(|| format!("read {}", rf.path.display()))?;
    let (raw_syms, raw_refs) = registry.parse(rf.kind, &bytes);
    let mut syms = Vec::with_capacity(raw_syms.len());
    let relpath = fe.relpath.clone();
    for r in raw_syms {
        let id = SymbolRecord::compute_id(
            root_id, &relpath, r.kind, &r.scope_path, &r.name, r.line,
        );
        let fqn = if r.scope_path.is_empty() {
            None
        } else {
            Some(format!("{}::{}", r.scope_path.join("::"), r.name))
        };
        syms.push(SymbolRecord {
            id,
            name: r.name,
            fqn,
            kind: r.kind,
            file_id: fe.id,
            byte_start: r.byte_start,
            byte_end: r.byte_end,
            line: r.line,
            col: r.col,
            scope_path: r.scope_path,
            lang: rf.kind,
        });
    }
    let refs = if no_refs {
        Vec::new()
    } else {
        raw_refs
            .into_iter()
            .map(|r| RefRecord {
                name: r.name,
                kind: r.kind,
                file_id: fe.id,
                byte_start: r.byte_start,
                byte_end: r.byte_end,
                line: r.line,
                col: r.col,
                scope_path: r.scope_path,
                lang: rf.kind,
                resolved_to: None,
            })
            .collect()
    };
    Ok((syms, refs))
}

// ---------------------------------------------------------------------------
// queries
// ---------------------------------------------------------------------------

fn open_index(index: Option<PathBuf>) -> Result<StoreReader> {
    let p = index.unwrap_or_else(default_index_dir);
    StoreReader::open(&p).with_context(|| format!("open index {}", p.display()))
}

fn cmd_def(
    name: String,
    index: Option<PathBuf>,
    lang: Option<String>,
    kind: Option<String>,
    limit: usize,
    json: bool,
    md: bool,
    budget: Option<usize>,
) -> Result<()> {
    let r = open_index(index)?;
    let results = r.lookup_exact(&name);
    let filtered: Vec<&SymbolRecord> = filter_results(results, lang.as_deref(), kind.as_deref());
    if md {
        print_results_md(&r, &filtered, limit, budget);
    } else {
        print_results(&r, &filtered, limit, json);
    }
    Ok(())
}

fn cmd_prefix(prefix: String, index: Option<PathBuf>, limit: usize, json: bool) -> Result<()> {
    let r = open_index(index)?;
    let results = r.lookup_prefix(&prefix, limit);
    print_results(&r, &results, limit, json);
    Ok(())
}

fn cmd_fuzzy(substr: String, index: Option<PathBuf>, limit: usize, json: bool) -> Result<()> {
    let r = open_index(index)?;
    let results = r.lookup_substring(&substr, limit);
    print_results(&r, &results, limit, json);
    Ok(())
}

fn cmd_ref(
    name: String,
    index: Option<PathBuf>,
    lang: Option<String>,
    kind: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let r = open_index(index)?;
    let results = r.lookup_refs_exact(&name);
    let filtered: Vec<&RefRecord> = results
        .into_iter()
        .filter(|rr| {
            if let Some(l) = &lang {
                if !format!("{:?}", rr.lang).eq_ignore_ascii_case(l) {
                    return false;
                }
            }
            if let Some(k) = &kind {
                if !rr.kind.short().eq_ignore_ascii_case(k) {
                    return false;
                }
            }
            true
        })
        .collect();
    print_refs(&r, &filtered, limit, json);
    Ok(())
}

fn cmd_stats(index: Option<PathBuf>) -> Result<()> {
    let r = open_index(index)?;
    println!("scry-version: {}", r.manifest.scry_version);
    println!("indexed-at:   {}", r.manifest.indexed_at);
    println!("roots:        {}", r.roots.len());
    for root in &r.roots {
        println!("  - {} ({:?})", root.path, root.profile);
    }
    println!("files-total:  {}", r.manifest.stats.files_total);
    println!("files-parsed: {}", r.manifest.stats.files_parsed);
    println!("files-failed: {}", r.manifest.stats.files_failed);
    println!("bytes-total:  {}", human_bytes(r.manifest.stats.bytes_total));
    println!("symbols:      {}", r.manifest.stats.symbols);
    println!("refs:         {}", r.manifest.stats.refs);
    println!("elapsed-ms:   {}", r.manifest.stats.elapsed_ms);

    let mut by_lang: std::collections::HashMap<FileKind, u64> = std::collections::HashMap::new();
    let mut by_kind: std::collections::HashMap<SymbolKind, u64> =
        std::collections::HashMap::new();
    for s in &r.symbols {
        *by_lang.entry(s.lang).or_default() += 1;
        *by_kind.entry(s.kind).or_default() += 1;
    }
    println!("\nby language:");
    let mut lv: Vec<_> = by_lang.into_iter().collect();
    lv.sort_by(|a, b| b.1.cmp(&a.1));
    for (l, c) in lv {
        println!("  {:>10}  {:?}", c, l);
    }
    println!("\nby kind:");
    let mut kv: Vec<_> = by_kind.into_iter().collect();
    kv.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, c) in kv {
        println!("  {:>10}  {}", c, k.short());
    }
    Ok(())
}

fn filter_results<'a>(
    syms: Vec<&'a SymbolRecord>,
    lang: Option<&str>,
    kind: Option<&str>,
) -> Vec<&'a SymbolRecord> {
    syms.into_iter()
        .filter(|s| {
            if let Some(l) = lang {
                if !format!("{:?}", s.lang).eq_ignore_ascii_case(l) {
                    return false;
                }
            }
            if let Some(k) = kind {
                if !s.kind.short().eq_ignore_ascii_case(k) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Markdown formatter: one section per result, optional code snippet. Stops
/// emitting if the byte budget is exhausted.
fn print_results_md(
    reader: &StoreReader,
    syms: &[&SymbolRecord],
    limit: usize,
    budget: Option<usize>,
) {
    let mut emitted_bytes: usize = 0;
    for s in syms.iter().take(limit) {
        let file = reader.files.get(s.file_id as usize);
        let path = file.map(|f| f.display_path(&reader.roots)).unwrap_or_default();
        let snippet = read_snippet(&path, s.line, 8);
        let scope_str = if s.scope_path.is_empty() {
            String::new()
        } else {
            format!("**scope**: `{}`  ", s.scope_path.join("::"))
        };
        let lang_label = format!("{:?}", s.lang);
        let section = format!(
            "### `{}`  ({} · {})\n\
             **location**: `{}:{}:{}`  \n\
             {}\n\
             ```{}\n{}\n```\n\n",
            s.name, s.kind.short(), lang_label.to_lowercase(),
            path, s.line, s.col,
            scope_str,
            short_lang(s.lang),
            snippet,
        );
        if let Some(b) = budget {
            if emitted_bytes + section.len() > b {
                println!("\n_(budget {} bytes reached, {} more results omitted)_",
                    b, syms.len().saturating_sub(syms.iter().take_while(|_| false).count()));
                break;
            }
        }
        emitted_bytes += section.len();
        print!("{}", section);
    }
}

/// Read a snippet of `total_lines` centered roughly on `line`.
fn read_snippet(path: &str, line: u32, total_lines: u32) -> String {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let src = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = src.lines().collect();
    let target = line.saturating_sub(1) as usize;
    let half = (total_lines / 2) as usize;
    let start = target.saturating_sub(half);
    let end = (target + half).min(lines.len().saturating_sub(1));
    lines[start..=end].join("\n")
}

fn print_results(reader: &StoreReader, syms: &[&SymbolRecord], limit: usize, json: bool) {
    if json {
        for s in syms.iter().take(limit) {
            let file = reader.files.get(s.file_id as usize);
            let path = file
                .map(|f| f.display_path(&reader.roots))
                .unwrap_or_default();
            let obj = serde_json::json!({
                "id": s.id,
                "name": s.name,
                "fqn": s.fqn,
                "kind": s.kind.short(),
                "lang": format!("{:?}", s.lang),
                "path": path,
                "line": s.line,
                "col": s.col,
                "scope": s.scope_path,
            });
            println!("{}", obj);
        }
        return;
    }
    for s in syms.iter().take(limit) {
        let file = reader.files.get(s.file_id as usize);
        let path = file
            .map(|f| f.display_path(&reader.roots))
            .unwrap_or_default();
        let scope = if s.scope_path.is_empty() {
            String::new()
        } else {
            format!("  [{}]", s.scope_path.join("::"))
        };
        println!(
            "{}:{}:{}  ({} {}){}  {}",
            path,
            s.line,
            s.col,
            s.kind.short(),
            short_lang(s.lang),
            scope,
            s.name,
        );
    }
    eprintln!("\n{} results (showing {})", syms.len(), syms.len().min(limit));
}

fn print_refs(reader: &StoreReader, refs: &[&RefRecord], limit: usize, json: bool) {
    if json {
        for r in refs.iter().take(limit) {
            let file = reader.files.get(r.file_id as usize);
            let path = file.map(|f| f.display_path(&reader.roots)).unwrap_or_default();
            let obj = serde_json::json!({
                "name": r.name,
                "ref_kind": r.kind.short(),
                "lang": format!("{:?}", r.lang),
                "path": path,
                "line": r.line,
                "col": r.col,
                "scope": r.scope_path,
            });
            println!("{}", obj);
        }
        return;
    }
    for r in refs.iter().take(limit) {
        let file = reader.files.get(r.file_id as usize);
        let path = file.map(|f| f.display_path(&reader.roots)).unwrap_or_default();
        let scope = if r.scope_path.is_empty() {
            String::new()
        } else {
            format!("  [{}]", r.scope_path.join("::"))
        };
        println!(
            "{}:{}:{}  ({} {}){}  {}",
            path, r.line, r.col,
            r.kind.short(),
            short_lang(r.lang),
            scope,
            r.name,
        );
    }
    eprintln!("\n{} refs (showing {})", refs.len(), refs.len().min(limit));
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

fn cmd_grep(
    pattern: String,
    index: Option<PathBuf>,
    is_regex: bool,
    lang: Option<String>,
    in_: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let r = open_index(index)?;
    let re = if is_regex {
        Some(regex::bytes::Regex::new(&pattern).context("invalid regex")?)
    } else {
        None
    };

    // Filter files
    let lang_lower = lang.as_ref().map(|s| s.to_ascii_lowercase());
    let prefix = in_.as_deref().unwrap_or("");
    let candidates: Vec<&FileEntry> = r
        .files
        .iter()
        .filter(|fe| {
            if let Some(ref l) = lang_lower {
                if !format!("{:?}", fe.kind).eq_ignore_ascii_case(l) {
                    return false;
                }
            }
            if !prefix.is_empty() {
                let full = fe.display_path(&r.roots);
                if !full.starts_with(prefix) {
                    return false;
                }
            }
            true
        })
        .collect();

    let total_files = candidates.len();
    eprintln!("[grep] scanning {} files", total_files);

    let hits: parking_lot::Mutex<Vec<Hit>> = parking_lot::Mutex::new(Vec::new());
    let hit_count = std::sync::atomic::AtomicUsize::new(0);
    candidates.par_iter().for_each(|fe| {
        if hit_count.load(std::sync::atomic::Ordering::Relaxed) >= limit * 8 {
            return; // bound work after we have plenty of candidates
        }
        let path = fe.display_path(&r.roots);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return,
        };
        // Find matches
        let mut local: Vec<Hit> = Vec::new();
        if let Some(re) = &re {
            for m in re.find_iter(&bytes) {
                let (line, col, snippet) = locate_match(&bytes, m.start(), m.end());
                local.push(Hit { file_id: fe.id, line, col, snippet });
                if local.len() >= limit { break; }
            }
        } else {
            let needle = pattern.as_bytes();
            let mut start = 0usize;
            while let Some(p) = memchr::memmem::find(&bytes[start..], needle) {
                let abs = start + p;
                let (line, col, snippet) = locate_match(&bytes, abs, abs + needle.len());
                local.push(Hit { file_id: fe.id, line, col, snippet });
                start = abs + needle.len();
                if local.len() >= limit { break; }
            }
        }
        if !local.is_empty() {
            hit_count.fetch_add(local.len(), std::sync::atomic::Ordering::Relaxed);
            hits.lock().extend(local);
        }
    });

    let mut hits = hits.into_inner();
    hits.truncate(limit);
    if json {
        for h in &hits {
            let path = r.files.get(h.file_id as usize)
                .map(|f| f.display_path(&r.roots)).unwrap_or_default();
            let obj = serde_json::json!({
                "path": path,
                "line": h.line,
                "col": h.col,
                "snippet": h.snippet,
            });
            println!("{}", obj);
        }
    } else {
        for h in &hits {
            let path = r.files.get(h.file_id as usize)
                .map(|f| f.display_path(&r.roots)).unwrap_or_default();
            println!("{}:{}:{}: {}", path, h.line, h.col, h.snippet);
        }
        eprintln!("\n{} hits across {} files", hits.len(), total_files);
    }
    Ok(())
}

struct Hit {
    file_id: u32,
    line: u32,
    col: u32,
    snippet: String,
}

fn locate_match(bytes: &[u8], start: usize, end: usize) -> (u32, u32, String) {
    // Count newlines up to start to find line; column is bytes since last newline.
    let mut line = 1u32;
    let mut last_nl: i64 = -1;
    for (i, b) in bytes.iter().enumerate().take(start) {
        if *b == b'\n' { line += 1; last_nl = i as i64; }
    }
    let col = (start as i64 - last_nl) as u32;
    // Find end of line
    let mut line_end = end;
    while line_end < bytes.len() && bytes[line_end] != b'\n' { line_end += 1; }
    let line_start = (last_nl + 1) as usize;
    let snippet = String::from_utf8_lossy(&bytes[line_start..line_end]).to_string();
    let snippet = if snippet.len() > 200 {
        format!("{}…", &snippet[..200])
    } else {
        snippet
    };
    (line, col, snippet)
}

// ---------------------------------------------------------------------------
// serve (JSON-RPC over stdin)
// ---------------------------------------------------------------------------

fn cmd_module_of(path: String, index: Option<PathBuf>, limit: usize) -> Result<()> {
    let r = open_index(index)?;
    // Heuristic: in Soong .bp files, a src is recorded by basename (relative
    // to the .bp's package). So we look up refs whose name matches the
    // basename, restrict to lang=Soong + kind=import.
    let pb = std::path::Path::new(&path);
    let basename = pb.file_name().and_then(|s| s.to_str()).unwrap_or(&path);
    let refs = r.lookup_refs_exact(basename);
    let mut out: Vec<&RefRecord> = refs.into_iter()
        .filter(|rr| matches!(rr.lang, FileKind::Soong))
        .collect();
    out.dedup_by(|a, b| a.scope_path == b.scope_path);
    if out.is_empty() {
        eprintln!("(no Soong module references basename {})", basename);
        return Ok(());
    }
    for rr in out.iter().take(limit) {
        let file = r.files.get(rr.file_id as usize);
        let bp_path = file.map(|f| f.display_path(&r.roots)).unwrap_or_default();
        let module_name = rr.scope_path.get(1).cloned().unwrap_or_default();
        let module_type = rr.scope_path.get(0).cloned().unwrap_or_default();
        println!("{} ({})  declared in {}", module_name, module_type, bp_path);
    }
    eprintln!("\n{} module(s)", out.len());
    Ok(())
}

fn cmd_serve(index: Option<PathBuf>) -> Result<()> {
    use std::io::{BufRead, Write};
    let reader = open_index(index)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let in_lock = stdin.lock();
    for line in in_lock.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() { continue; }
        let req: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = serde_json::json!({"error": format!("bad json: {e}")});
                writeln!(out, "{}", resp)?;
                out.flush()?;
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        let args = req.get("args").cloned().unwrap_or(serde_json::json!({}));
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let lang = args.get("lang").and_then(|v| v.as_str());
        let kind = args.get("kind").and_then(|v| v.as_str());

        let result = match cmd {
            "def" => serve_def(&reader, name, lang, kind, limit),
            "prefix" => serve_prefix(&reader, name, limit),
            "fuzzy" => serve_fuzzy(&reader, name, limit),
            "ref" => serve_ref(&reader, name, lang, kind, limit),
            "callers" => serve_ref(&reader, name, lang, Some("call"), limit),
            "stats" => serve_stats(&reader),
            other => serde_json::json!({"error": format!("unknown cmd: {other}")}),
        };

        let resp = serde_json::json!({
            "id": id,
            "result": result,
        });
        writeln!(out, "{}", resp)?;
        out.flush()?;
    }
    Ok(())
}

fn serve_def(
    r: &StoreReader,
    name: &str,
    lang: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let mut out = Vec::new();
    let syms = r.lookup_exact(name);
    for s in syms.into_iter().take(limit) {
        if let Some(l) = lang {
            if !format!("{:?}", s.lang).eq_ignore_ascii_case(l) { continue; }
        }
        if let Some(k) = kind {
            if !s.kind.short().eq_ignore_ascii_case(k) { continue; }
        }
        out.push(symbol_to_json(r, s));
    }
    serde_json::Value::Array(out)
}

fn serve_prefix(r: &StoreReader, prefix: &str, limit: usize) -> serde_json::Value {
    let v: Vec<_> = r.lookup_prefix(prefix, limit).into_iter()
        .map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(v)
}

fn serve_fuzzy(r: &StoreReader, substr: &str, limit: usize) -> serde_json::Value {
    let v: Vec<_> = r.lookup_substring(substr, limit).into_iter()
        .map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(v)
}

fn serve_ref(
    r: &StoreReader,
    name: &str,
    lang: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let mut out = Vec::new();
    for rr in r.lookup_refs_exact(name).into_iter().take(limit) {
        if let Some(l) = lang {
            if !format!("{:?}", rr.lang).eq_ignore_ascii_case(l) { continue; }
        }
        if let Some(k) = kind {
            if !rr.kind.short().eq_ignore_ascii_case(k) { continue; }
        }
        out.push(ref_to_json(r, rr));
    }
    serde_json::Value::Array(out)
}

fn serve_stats(r: &StoreReader) -> serde_json::Value {
    serde_json::json!({
        "scry_version": r.manifest.scry_version,
        "indexed_at": r.manifest.indexed_at,
        "roots": r.roots.iter().map(|x| serde_json::json!({
            "path": x.path, "profile": x.profile,
        })).collect::<Vec<_>>(),
        "files_total": r.manifest.stats.files_total,
        "symbols": r.manifest.stats.symbols,
        "refs": r.manifest.stats.refs,
        "bytes_total": r.manifest.stats.bytes_total,
        "elapsed_ms": r.manifest.stats.elapsed_ms,
    })
}

fn symbol_to_json(r: &StoreReader, s: &SymbolRecord) -> serde_json::Value {
    let path = r.files.get(s.file_id as usize)
        .map(|f| f.display_path(&r.roots)).unwrap_or_default();
    serde_json::json!({
        "id": s.id,
        "name": s.name,
        "fqn": s.fqn,
        "kind": s.kind.short(),
        "lang": format!("{:?}", s.lang),
        "path": path,
        "line": s.line,
        "col": s.col,
        "scope": s.scope_path,
    })
}

fn ref_to_json(r: &StoreReader, rr: &RefRecord) -> serde_json::Value {
    let path = r.files.get(rr.file_id as usize)
        .map(|f| f.display_path(&r.roots)).unwrap_or_default();
    serde_json::json!({
        "name": rr.name,
        "ref_kind": rr.kind.short(),
        "lang": format!("{:?}", rr.lang),
        "path": path,
        "line": rr.line,
        "col": rr.col,
        "scope": rr.scope_path,
    })
}

fn short_lang(k: FileKind) -> &'static str {
    use FileKind::*;
    match k {
        Java => "java",
        Kotlin => "kt",
        C => "c",
        Cpp => "cpp",
        Header => "h",
        HeaderCpp => "hpp",
        Rust => "rs",
        Go => "go",
        Python => "py",
        Bash => "sh",
        Proto => "proto",
        Aidl => "aidl",
        _ => "?",
    }
}
