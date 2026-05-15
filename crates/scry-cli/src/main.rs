//! scry: semantic code search and cross-reference engine for AOSP and Linux.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use scry_lang::{extract, extract_refs};
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
        Cmd::Index { roots, profile, out, count_only, limit, no_refs } => {
            cmd_index(roots, profile, out, count_only, limit, no_refs)
        }
        Cmd::Def { name, index, lang, kind, limit, json } => {
            cmd_def(name, index, lang, kind, limit, json)
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
) -> Result<()> {
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
        let t = Instant::now();
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

        // Parse in parallel. Each task returns (Vec<SymbolRecord>, Vec<RefRecord>).
        let parsed = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let symbols_total = Arc::new(AtomicU64::new(0));
        let refs_total = Arc::new(AtomicU64::new(0));

        let parse_start = Instant::now();
        let results: Vec<(Vec<SymbolRecord>, Vec<RefRecord>)> = collected
            .files
            .par_iter()
            .zip(file_entries.par_iter())
            .map(|(rf, fe)| -> (Vec<SymbolRecord>, Vec<RefRecord>) {
                match parse_one(rf, fe, root_id, no_refs) {
                    Ok((s, r)) => {
                        parsed.fetch_add(1, Ordering::Relaxed);
                        symbols_total.fetch_add(s.len() as u64, Ordering::Relaxed);
                        refs_total.fetch_add(r.len() as u64, Ordering::Relaxed);
                        (s, r)
                    }
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        (Vec::new(), Vec::new())
                    }
                }
            })
            .collect();

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
        for (sv, rv) in results {
            writer.symbols.extend(sv);
            writer.refs.extend(rv);
        }
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
) -> Result<(Vec<SymbolRecord>, Vec<RefRecord>)> {
    if !rf.kind.is_source() {
        return Ok((Vec::new(), Vec::new()));
    }
    if rf.size > 5 * 1024 * 1024 {
        return Ok((Vec::new(), Vec::new()));
    }
    let bytes = std::fs::read(&rf.path)
        .with_context(|| format!("read {}", rf.path.display()))?;
    let raw_syms = extract(rf.kind, &bytes)
        .with_context(|| format!("parse {}", rf.path.display()))?;
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
        let raw_refs = extract_refs(rf.kind, &bytes).unwrap_or_default();
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
) -> Result<()> {
    let r = open_index(index)?;
    let results = r.lookup_exact(&name);
    let filtered: Vec<&SymbolRecord> = filter_results(results, lang.as_deref(), kind.as_deref());
    print_results(&r, &filtered, limit, json);
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
