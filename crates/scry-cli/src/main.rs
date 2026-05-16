//! scry: semantic code search and cross-reference engine for AOSP and Linux.

#![forbid(unsafe_code)]

// jemalloc returns freed memory to the OS aggressively. Default glibc malloc
// keeps a high-water-mark — fine for short jobs, disastrous for our pattern
// of "allocate millions of Strings per batch, drop them, repeat". Switching
// the global allocator dropped index RSS by 10×+ in practice on AOSP.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use scry_lang::FormatRegistry;
use scry_store::{
    FileEntry, IndexStats, RefRecord, RootEntry, StoreReader, StoreWriter,
    SymbolKind, SymbolRecord,
};
use scry_walker::{collect_files, FileKind, Profile, RawFile};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// jemalloc runtime stats — we read `stats.allocated` periodically to log
// what the allocator is actually holding, separate from our application-
// level estimated_bytes(). This is the ground truth.
use tikv_jemalloc_ctl::{epoch, stats};

// Shared between the heartbeat thread (writes) and every parser worker
// (reads, for backpressure). Updated every 100 ms from jemalloc's
// `stats.allocated`. Workers wait if above BACKPRESSURE_CEILING.
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static BACKPRESSURE_CEILING: AtomicU64 = AtomicU64::new(0); // 0 = disabled

/// Block until the allocator reports we're below the soft ceiling.
/// Returns immediately if no ceiling is configured. Polls at ~5 ms.
fn await_memory_headroom() {
    let ceiling = BACKPRESSURE_CEILING.load(Ordering::Relaxed);
    if ceiling == 0 { return; }
    loop {
        let cur = ALLOCATED_BYTES.load(Ordering::Relaxed);
        if cur < ceiling { return; }
        std::thread::sleep(Duration::from_millis(5));
    }
}

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
        /// Hard refuse-to-touch ceiling (default 100 MiB). Files this large
        /// are binary blobs (.git packs, prebuilt jars). Every byte under
        /// this is parsed.
        #[arg(long, default_value_t = 100 * 1024 * 1024)]
        max_file_bytes: u64,
        /// Files larger than this are routed to a single-worker serial
        /// queue instead of the parallel pool. Tree-sitter can transiently
        /// allocate GB-scale memory on pathological inputs; running such
        /// files concurrently can OOM the host between memory-stats polls.
        /// Serializing them bounds peak RAM to one big parse at a time.
        /// Default 64 KiB — keeps the parallel hot path on small files
        /// while ensuring no Vec-init-list explosion goes parallel.
        #[arg(long, default_value_t = 64 * 1024)]
        big_file_bytes: u64,
        /// Soft memory ceiling in GiB. When jemalloc-reported allocated
        /// memory climbs above 80% of this value, parser workers WAIT
        /// (don't pick up new files) until the heap drains via batch
        /// flushes. This is the memory backpressure mechanism that lets
        /// scry safely index pathological data-dump files (a single 2.1 MB
        /// generated BLAS test_data.cpp transiently allocates ~9 GB in
        /// tree-sitter's AST). Naturally serializes such files without any
        /// size-based heuristic. 0 = no backpressure.
        #[arg(long, default_value_t = 0)]
        mem_cap: u32,
        /// Hard upper bound on files per batch. With --flush-bytes set this
        /// is just a sanity cap; the actual batch size adapts to hit the byte
        /// target. With --flush-bytes 0 this is the only knob (file-count
        /// flushing, with proxy-for-memory semantics).
        #[arg(long, default_value_t = 50_000)]
        flush_every: usize,
        /// Target in-RAM record bytes per batch (MiB). The batch size adapts
        /// every iteration from a rolling avg of bytes/file so accumulated
        /// records stay close to this target. Bounded above by --flush-every.
        /// 0 = disabled (fall back to file-count). Default 1024 MiB — bounds
        /// steady-state record RAM to ~1 GiB on top of transient parse
        /// allocation. Tune down on memory-constrained hosts.
        #[arg(long, default_value_t = 1024)]
        flush_bytes: u32,
        /// Resume from a previous run's checkpoint. If `<index>.tmp/`
        /// contains `batch.NNNNNN.done` markers, skip those batches' files
        /// and continue from the next one. Pairs with systemd
        /// `Restart=on-failure`: the cgroup hard-kills on OOM, systemd
        /// respawns, this flag lets us pick up where we left off.
        #[arg(long)]
        resume: bool,
        /// Build a trigram index alongside the symbol index. Doubles disk
        /// usage and adds ~20% to indexing time, but enables 100× faster
        /// literal `scry grep` queries (the index pre-filters candidate
        /// files via posting-list intersection, only opens files that
        /// COULD contain the literal substring).
        #[arg(long)]
        build_trigrams: bool,
    },
    /// Look up references to a name.
    Ref {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Language to filter by — `-t` for short, ripgrep-style.
        #[arg(long, short = 't')]
        lang: Option<String>,
        /// Kind to filter by (class, fn, method, soong, …).
        #[arg(long, short = 'k')]
        kind: Option<String>,
        /// Restrict to refs whose file path contains the SUBSTRING
        /// (subdir scope). Matches both root-relative ("frameworks/base/")
        /// and absolute prefixes; same semantics as gtags' --path filter.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find callers of NAME (refs with kind=call). LSP analogue:
    /// callHierarchy/incomingCalls.
    Callers {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, short = 't')]
        lang: Option<String>,
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Look up exact symbol definitions by name. LSP analogue:
    /// textDocument/definition; ctags/gtags analogue: tag lookup.
    Def {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, short = 't')]
        lang: Option<String>,
        #[arg(long, short = 'k')]
        kind: Option<String>,
        /// Restrict to symbols whose file path contains the SUBSTRING.
        /// Lets you query a subdir of an indexed root, e.g.
        /// `scry def Activity --in frameworks/base/services/`.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
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
    /// Subtree coverage: files / bytes / symbols broken down by FileKind
    /// for any directory within an indexed root. Useful for "what
    /// fraction of $repo did scry actually understand?" — point it at
    /// an internal subtree and see whether the right languages got
    /// picked up. Substring-matches the displayed file path, same as
    /// the `--in` flag elsewhere.
    Coverage {
        /// Path SUBSTRING (full or root-relative); e.g.
        /// `frameworks/base/services/`. Empty = whole index.
        path: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Also break symbol counts down by kind within each language.
        #[arg(long)]
        by_kind: bool,
        /// JSON output instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// List every symbol defined in a single file (sorted by line).
    ///
    /// PATH is matched against the indexed file paths via suffix —
    /// `outline frameworks/base/.../Activity.java` works, and so does
    /// the full absolute form. If multiple files match, scry picks the
    /// shortest match and warns; pass a longer suffix to disambiguate.
    Outline {
        path: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Limit the number of symbols printed (0 = all).
        #[arg(long, default_value = "0")]
        limit: usize,
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
        /// Cap rayon thread-pool size for this search. Useful on shared
        /// machines or to lower memory pressure (each worker mmaps files).
        #[arg(long)]
        workers: Option<usize>,
        /// Skip files larger than N bytes (default 10 MiB).
        #[arg(long, default_value_t = 10 * 1024 * 1024)]
        max_file_bytes: u64,
        /// Soft RSS cap in GiB. When grep's RSS exceeds 85% of this, search
        /// is cut early and the output is marked as truncated. 0 = unlimited.
        #[arg(long, default_value_t = 0)]
        mem_cap: u32,
    },
    /// Replay recent queries from ~/.scry/queries.log. Useful as a
    /// thin memory primitive for LLM agents that want to know "what
    /// did I already search for this session" without re-running the
    /// queries.
    Recall {
        /// Cap the number of entries returned. Default: 20.
        #[arg(long, default_value = "20")]
        last: usize,
        /// Only entries whose cmd matches (def, grep, callers, ...).
        #[arg(long)]
        cmd: Option<String>,
        /// Only entries whose query string contains this substring.
        #[arg(long)]
        grep: Option<String>,
        /// Override the log location (default: $SCRY_LOG, then
        /// $HOME/.scry/queries.log).
        #[arg(long)]
        log: Option<PathBuf>,
        /// Deduplicate consecutive identical (cmd, query) entries.
        /// Useful when a session re-runs the same query many times;
        /// off by default so the count reflects actual activity.
        #[arg(long)]
        dedup: bool,
        /// Machine-readable output: one JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// MCP (Model Context Protocol) server. Stdio JSON-RPC 2.0 over
    /// the standard MCP request/response shape; one MCP tool per scry
    /// command (def/ref/callers/prefix/fuzzy/grep/outline/coverage/
    /// stats). Drop straight into Claude Desktop, Cursor, or any MCP-
    /// aware agent without writing a custom shell-out wrapper.
    ///
    /// Implements:
    ///   initialize             → server info + capabilities
    ///   tools/list             → one entry per scry command with JSON schema
    ///   tools/call             → run the named tool, return its JSON result
    ///                           wrapped in MCP text-content shape
    ///   notifications/*        → silently consumed (no response per spec)
    Mcp {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// JSON-RPC server. Reads newline-delimited requests, writes
    /// newline-delimited responses. Each request is
    /// {"id": N, "cmd": "def|ref|callers|prefix|fuzzy|grep|outline|coverage|stats", "args": {...}}.
    ///
    /// Transport defaults to stdin/stdout (one-shot agent loops). Pass
    /// --listen unix:/tmp/scry.sock or --listen tcp:127.0.0.1:9999 to
    /// run as a persistent daemon that accepts multiple concurrent
    /// connections from a single warm StoreReader. The daemon mode
    /// holds the mmap'd index across all connections — per-query cold
    /// open cost (~50 ms) is paid once, not per process.
    Serve {
        #[arg(long)]
        index: Option<PathBuf>,
        /// Bind a listener instead of using stdin/stdout. Accepted forms:
        ///   unix:/path/to/sock     — Unix domain socket (preferred for
        ///                            local editor / agent integrations)
        ///   tcp:HOST:PORT          — TCP socket, e.g. tcp:127.0.0.1:9999
        /// Each accepted connection runs the same JSON-RPC loop as the
        /// stdin/stdout transport. The shared StoreReader is Sync (mmap'd
        /// + immutable) so concurrent queries are safe and don't block
        /// each other.
        #[arg(long)]
        listen: Option<String>,
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
    /// Build (or rebuild) the trigram index for an existing index dir.
    /// Use this when the original `scry index` run didn't pass
    /// --build-trigrams. Walks every file in the index's files.bin,
    /// extracts trigrams, writes trigrams.fst + trigram_postings.bin
    /// alongside the existing index — no re-parsing needed.
    BuildTrigrams {
        #[arg(long)]
        index: Option<PathBuf>,
        /// Worker count for parallel trigram extraction.
        #[arg(long)]
        workers: Option<usize>,
        /// Skip files larger than N bytes (default 5 MiB). Same default
        /// as scry index — keeps data-blob outliers from polluting.
        #[arg(long, default_value_t = 5 * 1024 * 1024)]
        max_file_bytes: u64,
    },
    /// Build the offsets sidecar files for an existing index. This is what
    /// enables the lazy/mmap StoreReader path: cold query latency drops
    /// from "deserialize 10 GB bincode Vec" to "mmap + single-record decode"
    /// (~10 ms vs several seconds). Walks the existing symbols.bin /
    /// refs.bin, recording each record's byte offset into the corresponding
    /// _offsets.bin sidecar. No re-indexing required.
    BuildOffsets {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Build the file→symbol-ids sidecar (file_symbols.bin +
    /// file_symbols_offsets.bin) for an existing index. Makes `outline`
    /// O(symbols-in-file) instead of O(total-symbols). Walks the lazy
    /// symbol vec once, grouping by file_id; no re-parsing needed.
    BuildFileSymbols {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Layer 2 ref-to-def resolution: per-ref override sidecar produced
    /// by walking the index ONCE post-finalize. For each unresolved or
    /// ambiguous ref, narrow candidates using language-specific context
    /// (Java today: same package + explicit imports; same fallback to
    /// name-match for everything else). Writes ref_resolutions.bin;
    /// the reader honors it automatically on get_ref.
    BuildResolutions {
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
        Cmd::Index {
            roots, profile, out, count_only, limit, no_refs, workers,
            max_file_bytes, big_file_bytes, mem_cap, flush_every, flush_bytes,
            resume, build_trigrams,
        } => cmd_index(
            roots, profile, out, count_only, limit, no_refs, workers,
            max_file_bytes, big_file_bytes, mem_cap, flush_every, flush_bytes,
            resume, build_trigrams,
        ),
        Cmd::Def { name, index, lang, kind, in_, limit, json, md, budget } => {
            cmd_def(name, index, lang, kind, in_, limit, json, md, budget)
        }
        Cmd::Prefix { prefix, index, limit, json } => {
            cmd_prefix(prefix, index, limit, json)
        }
        Cmd::Fuzzy { substr, index, limit, json } => {
            cmd_fuzzy(substr, index, limit, json)
        }
        Cmd::Ref { name, index, lang, kind, in_, limit, json } => {
            cmd_ref(name, index, lang, kind, in_, limit, json)
        }
        Cmd::Callers { name, index, lang, in_, limit, json } => {
            cmd_ref(name, index, lang, Some("call".to_string()), in_, limit, json)
        }
        Cmd::Stats { index } => cmd_stats(index),
        Cmd::Coverage { path, index, by_kind, json } => cmd_coverage(path, index, by_kind, json),
        Cmd::Outline { path, index, json, limit } => cmd_outline(path, index, json, limit),
        Cmd::Grep {
            pattern, index, regex, lang, in_, limit, json, workers,
            max_file_bytes, mem_cap,
        } => cmd_grep(
            pattern, index, regex, lang, in_, limit, json, workers,
            max_file_bytes, mem_cap,
        ),
        Cmd::Serve { index, listen } => cmd_serve(index, listen),
        Cmd::Mcp { index } => cmd_mcp(index),
        Cmd::Recall { last, cmd, grep, log, dedup, json } =>
            cmd_recall(last, cmd, grep, log, dedup, json),
        Cmd::Mod { name, index, limit, json } => {
            cmd_def(name, index, None, Some("soong".into()), None, limit, json, false, None)
        }
        Cmd::ModuleOf { path, index, limit } => cmd_module_of(path, index, limit),
        Cmd::BuildTrigrams { index, workers, max_file_bytes } => {
            cmd_build_trigrams(index, workers, max_file_bytes)
        }
        Cmd::BuildOffsets { index } => cmd_build_offsets(index),
        Cmd::BuildFileSymbols { index } => cmd_build_file_symbols(index),
        Cmd::BuildResolutions { index } => cmd_build_resolutions(index),
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
    big_file_bytes: u64,
    mem_cap: u32,
    flush_every: usize,
    flush_bytes: u32,
    resume: bool,
    build_trigrams: bool,
) -> Result<()> {
    if let Some(n) = workers {
        if n > 0 {
            if let Err(e) = rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build_global()
            {
                // build_global errors if invoked a second time in one
                // process (the global pool is set-once). For the CLI
                // that's never a real bug (each invocation is a fresh
                // process), but in a future in-process driver it would
                // silently drop the --workers flag. Surface it.
                eprintln!("[warn] rayon global pool already initialized: {e}; --workers ignored");
            }
            eprintln!("[index] rayon pool: {} workers", n);
        }
    }

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

    // -- Streaming-by-default --
    // The writer chunk-flushes symbols + refs to disk between batches so peak
    // RAM is bounded by `flush_every` files + the current batch's records.
    // Memory accounting is INTERNAL: we sum each record's estimated_bytes()
    // and call flush_*_chunk() when crossing the soft threshold from mem_cap
    // (or unconditionally at the end of every batch). No /proc polling.
    let streaming = flush_every > 0 || flush_bytes > 0 || mem_cap > 0;
    let mem_cap_bytes: u64 = (mem_cap as u64) * 1024 * 1024 * 1024;
    let soft_cap: u64 = if mem_cap_bytes == 0 { u64::MAX } else { (mem_cap_bytes as f64 * 0.85) as u64 };
    let batch_files_cap: usize = if flush_every == 0 { usize::MAX } else { flush_every };
    // Bytes-target flush: the batch size adapts each iteration to hit this
    // many bytes of records, using a rolling avg of bytes/file. flush_every
    // becomes a sanity ceiling. 0 = disabled (file-count only).
    let flush_bytes_target: u64 = (flush_bytes as u64) * 1024 * 1024;
    eprintln!(
        "[index] streaming={} flush_every={} flush_bytes={} MiB mem_cap={} GiB (soft {})",
        streaming, flush_every, flush_bytes, mem_cap,
        if mem_cap_bytes == 0 { "none".into() } else { human_bytes(soft_cap) },
    );

    let t_total = Instant::now();

    // -- Memory backpressure: a background thread polls jemalloc's
    // `stats.allocated` every 100 ms and publishes it into ALLOCATED_BYTES.
    // Parser workers consult that counter before picking up a file: if we're
    // above 80% of the mem-cap, they sleep briefly and retry. This degrades
    // parallelism gracefully under memory pressure (naturally serializes the
    // pathological data-dump files) without any per-file size heuristics.
    //
    // We also log a one-line heartbeat every 5 s so the operator can SEE
    // where the allocator is sitting separate from our application counters.
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let ceiling_bytes = (mem_cap as u64) * 1024 * 1024 * 1024;
    let soft_ceiling = (ceiling_bytes as f64 * 0.80) as u64;
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    BACKPRESSURE_CEILING.store(soft_ceiling, Ordering::Relaxed);
    let stop_clone = heartbeat_stop.clone();
    let _heartbeat = std::thread::spawn(move || {
        let e = epoch::mib().ok();
        let allocated = stats::allocated::mib().ok();
        let resident = stats::resident::mib().ok();
        let active = stats::active::mib().ok();
        let mut tick = 0u32;
        while !stop_clone.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            if let (Some(e), Some(a)) = (&e, &allocated) {
                if e.advance().is_ok() {
                    let alloc_b = a.read().unwrap_or(0) as u64;
                    ALLOCATED_BYTES.store(alloc_b, Ordering::Relaxed);
                    // 5 s heartbeat (50 × 100 ms)
                    if tick % 50 == 0 {
                        let res_b = resident.as_ref().and_then(|m| m.read().ok()).unwrap_or(0) as u64;
                        let act_b = active.as_ref().and_then(|m| m.read().ok()).unwrap_or(0) as u64;
                        let backpressured = if soft_ceiling > 0 && alloc_b >= soft_ceiling {
                            " BACKPRESSURE"
                        } else { "" };
                        eprintln!(
                            "[jemalloc] allocated={} active={} resident={}{}",
                            human_bytes(alloc_b), human_bytes(act_b), human_bytes(res_b),
                            backpressured,
                        );
                    }
                    tick = tick.wrapping_add(1);
                }
            }
        }
    });

    let mut writer = if streaming && !count_only {
        if resume {
            StoreWriter::resume_streaming(&out_dir)?
        } else {
            StoreWriter::new_streaming(&out_dir)?
        }
    } else {
        StoreWriter::new(&out_dir)
    };
    if build_trigrams && !count_only {
        writer.enable_trigrams();
        eprintln!("[index] trigram index ENABLED (chunks every batch)");
    }

    // -- Walk + sort + assign file_ids for every root up front --
    // Stable per-relpath ordering is what makes --resume safe: a second run
    // with the same args (same source tree state) assigns the exact same
    // file_ids, so chunks already on disk reference indices in the about-to-be-
    // rebuilt writer.files. The walker is parallel and emits in non-
    // deterministic order — without this sort, file_id 12345 might mean two
    // different files across runs and the index would be corrupt.
    struct PreparedRoot {
        root_id: u8,
        root_path: PathBuf,
        files: Vec<RawFile>,
        file_entries: Vec<FileEntry>,
    }
    let mut prepared: Vec<PreparedRoot> = Vec::with_capacity(roots.len());
    let mut next_file_id: u32 = 0;
    let mut total_files_total: u64 = 0;
    let mut total_files_parsed: u64 = 0;
    let mut total_files_failed: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut grand_syms: u64 = 0;
    let mut grand_refs: u64 = 0;

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
        // Deterministic order is required for --resume correctness.
        collected.files.sort_unstable_by(|a, b| a.relpath.cmp(&b.relpath));
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
        // File entries are deterministic from the sorted walk — push them now
        // so writer.files is complete even for batches we skip on resume.
        writer.files.extend(file_entries.iter().cloned());
        prepared.push(PreparedRoot {
            root_id,
            root_path: collected.root,
            files: collected.files,
            file_entries,
        });
    }

    // -- OOM auto-skiplist --
    // After each cgroup OOM-kill, on resume we read last_attempted.txt
    // (written by parse_one for big-bucket files before tree-sitter is
    // invoked). If present, that's the file we crashed on. We append it
    // to oom_skiplist.txt and remove it; subsequent parse_one calls
    // [skip-oomed] it. This makes the restart loop self-healing: each
    // OOM teaches scry one more file to avoid, and progress eventually
    // unblocks. The user can clear oom_skiplist.txt to retry.
    let mut oom_skiplist: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(tmp) = writer.tmp_dir.as_ref() {
        let last_attempted = tmp.join("last_attempted.txt");
        let skiplist_path = tmp.join("oom_skiplist.txt");
        if last_attempted.exists() {
            if let Ok(s) = std::fs::read_to_string(&last_attempted) {
                let s = s.trim();
                if !s.is_empty() {
                    eprintln!(
                        "[resume] previous run OOM-killed while parsing: {}\n\
                         [resume] adding to OOM skiplist so the loop can advance",
                        s,
                    );
                    let mut existing = std::fs::read_to_string(&skiplist_path).unwrap_or_default();
                    if !existing.lines().any(|l| l == s) {
                        if !existing.is_empty() && !existing.ends_with('\n') {
                            existing.push('\n');
                        }
                        existing.push_str(s);
                        existing.push('\n');
                        let _ = std::fs::write(&skiplist_path, existing);
                    }
                }
            }
            let _ = std::fs::remove_file(&last_attempted);
        }
        if let Ok(s) = std::fs::read_to_string(&skiplist_path) {
            for line in s.lines() {
                let line = line.trim();
                if !line.is_empty() { oom_skiplist.insert(line.to_string()); }
            }
            if !oom_skiplist.is_empty() {
                eprintln!("[resume] OOM skiplist loaded: {} file(s)", oom_skiplist.len());
            }
        }
    }

    // -- Resume watermark + orphan-chunk cleanup --
    // Watermark = number of file_ids that are fully done. file_ids [0, watermark)
    // are flushed to chunk files on disk. progress.json is written atomically
    // (via rename) AFTER each batch's chunks are flushed. If we crash between
    // chunk flush and progress write, the on-disk chunk_count exceeds the
    // progress.json record — those are orphans and we discard them before
    // resuming, so we don't double-count any file's records in the final index.
    let progress_path = writer.tmp_dir.as_ref().map(|t| t.join("progress.json"));
    let mut watermark: u32 = 0;
    if resume {
        if let Some(p) = progress_path.as_ref() {
            if p.exists() {
                let s = std::fs::read_to_string(p)
                    .with_context(|| format!("read {}", p.display()))?;
                let v: serde_json::Value = serde_json::from_str(&s)
                    .with_context(|| format!("parse {}", p.display()))?;
                watermark = v.get("completed_files").and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                let saved_sym = v.get("symbol_chunks").and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                let saved_ref = v.get("ref_chunks").and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                // Verify the roots signature matches: if the user reindexes
                // with different roots or the source tree changed size, file_ids
                // would be reassigned and chunks on disk would be misaligned.
                // Fail loudly rather than silently producing a corrupt index.
                let want = v.get("roots").and_then(|x| x.as_array()).cloned()
                    .unwrap_or_default();
                if want.len() != prepared.len() {
                    anyhow::bail!(
                        "resume: root count mismatch ({} in {} vs {} now); \
                         remove {} and re-index without --resume",
                        want.len(), p.display(), prepared.len(),
                        writer.tmp_dir.as_ref().unwrap().display(),
                    );
                }
                // File-count drift between runs is the common case in AOSP
                // (background processes touch the tree). A small drift means
                // some files were inserted into the sorted walk, which COULD
                // shift file_ids and misalign chunks. We warn loudly and let
                // the operator decide — bailing on every drift makes the
                // resume loop unusable in production.
                let mut any_path_changed = false;
                for (i, rj) in want.iter().enumerate() {
                    let want_path = rj.get("path").and_then(|x| x.as_str()).unwrap_or("");
                    let want_n = rj.get("n_files").and_then(|x| x.as_u64()).unwrap_or(0);
                    let cur = &prepared[i];
                    let cur_path = cur.root_path.display().to_string();
                    if cur_path != want_path {
                        any_path_changed = true;
                        eprintln!(
                            "[resume] WARN: root[{}] path mismatch (now {} vs progress {})",
                            i, cur_path, want_path,
                        );
                    }
                    let drift = (cur.files.len() as i64) - (want_n as i64);
                    if drift != 0 {
                        eprintln!(
                            "[resume] WARN: root[{}] file count drift {:+} ({} now vs {} in progress) \
                             — file_ids past the insertion point may be misaligned; \
                             chunks may contain stale references.",
                            i, drift, cur.files.len(), want_n,
                        );
                    }
                }
                if any_path_changed {
                    anyhow::bail!(
                        "resume: root path(s) changed — cannot continue. \
                         Remove {} and re-index without --resume.",
                        writer.tmp_dir.as_ref().unwrap().display(),
                    );
                }
                // Discard any orphan chunks past what progress.json knows about.
                let tmp = writer.tmp_dir.as_ref().unwrap().clone();
                let drop_orphans = |kind: &str, keep: u32, on_disk: &mut u32, lens: &mut Vec<u64>| {
                    while *on_disk > keep {
                        let n = *on_disk - 1;
                        let p = tmp.join(format!("{kind}.chunk.{:06}.bin", n));
                        let np = tmp.join(format!("{}_names.chunk.{:06}.bin",
                            kind.trim_end_matches('s'), n));
                        let _ = std::fs::remove_file(&p);
                        let _ = std::fs::remove_file(&np);
                        lens.pop();
                        *on_disk = n;
                    }
                };
                if writer.symbol_chunk_count > saved_sym {
                    eprintln!("[resume] discarding {} orphan symbol chunk(s) past progress",
                        writer.symbol_chunk_count - saved_sym);
                    drop_orphans("symbols", saved_sym,
                        &mut writer.symbol_chunk_count, &mut writer.symbol_chunk_lens);
                }
                if writer.ref_chunk_count > saved_ref {
                    eprintln!("[resume] discarding {} orphan ref chunk(s) past progress",
                        writer.ref_chunk_count - saved_ref);
                    drop_orphans("refs", saved_ref,
                        &mut writer.ref_chunk_count, &mut writer.ref_chunk_lens);
                }
                eprintln!("[resume] watermark = {} files done (sym chunks {}, ref chunks {})",
                    watermark, writer.symbol_chunk_count, writer.ref_chunk_count);
                // Seed cumulative tallies so the final manifest reflects the
                // whole index, not just THIS run's delta. Symbols/refs counts
                // come from the chunk headers we already read. We assume files
                // below the watermark all parsed successfully (the resume
                // marker is only written after a batch flush succeeded).
                total_files_parsed = watermark as u64;
                grand_syms = writer.symbol_chunk_lens.iter().sum();
                grand_refs = writer.ref_chunk_lens.iter().sum();
            } else {
                eprintln!("[resume] no progress.json under {} — starting fresh",
                    writer.tmp_dir.as_ref().unwrap().display());
            }
        }
    }

    // ----- per-root batched parse -----
    for pr in &prepared {
        let root_id = pr.root_id;
        let files: &[RawFile] = &pr.files;
        let file_entries: &[FileEntry] = &pr.file_entries;
        if count_only { continue; }
        let n_files = files.len();
        // Rolling avg of bytes-of-records per file. Seeded with a pessimistic
        // prior so the very first batch (no observations yet) stays small.
        // Updated after every batch as a 70/30 EMA — slow enough to ride
        // through one bad file, fast enough to react to a region shift
        // (e.g., entering AOSP's massive generated-Java test trees).
        let mut avg_bytes_per_file: f64 = 8_000.0;
        let mut batch_no = 0usize;
        let mut start = 0usize;
        let parse_total = Instant::now();
        while start < n_files {
            let batch_files: usize = if flush_bytes_target > 0 {
                let by_bytes = (flush_bytes_target as f64
                    / avg_bytes_per_file.max(100.0)) as usize;
                by_bytes.clamp(100, batch_files_cap)
            } else {
                batch_files_cap
            };
            let end = (start + batch_files).min(n_files);
            let batch_files_slice = &files[start..end];
            let batch_entries_slice = &file_entries[start..end];
            batch_no += 1;
            let batch_end_id = batch_entries_slice.last().unwrap().id;
            // Estimate remaining batches just for the log line; not used for
            // anything load-bearing.
            let remaining = n_files.saturating_sub(end);
            let total_batches = batch_no + remaining.div_ceil(batch_files.max(1));
            if resume && batch_end_id < watermark {
                start = end;
                continue;
            }

            let parsed = Arc::new(AtomicU64::new(0));
            let failed = Arc::new(AtomicU64::new(0));
            let symbols_total = Arc::new(AtomicU64::new(0));
            let refs_total = Arc::new(AtomicU64::new(0));
            let est_bytes = Arc::new(AtomicU64::new(0));

            let syms_sink = parking_lot::Mutex::new(std::mem::take(&mut writer.symbols));
            let refs_sink = parking_lot::Mutex::new(std::mem::take(&mut writer.refs));
            // Trigram sink: parallel-friendly batch buffer of (trigram, file_id)
            // tuples produced by parse_one. Drained at batch end into the writer.
            let trigrams_sink: parking_lot::Mutex<Vec<(scry_store::trigram::Trigram, u32)>> =
                parking_lot::Mutex::new(Vec::with_capacity(if build_trigrams { 1 << 20 } else { 0 }));

            let batch_start = Instant::now();
            // Counter for in-batch progress logging (every 1000 files).
            let progress_step: u64 = 1000;
            // Outlier thresholds — log per-file diagnostic if exceeded.
            let outlier_ms: u128 = 250;
            let outlier_records: usize = 5_000;

            // SIZE-ROUTED SCHEDULING: pre-split this batch into "small"
            // (parallel-safe) and "big" (must serialize) buckets BEFORE
            // opening any files — we already know rf.size from the walker.
            // Big-bucket files run one-at-a-time so a single pathological
            // tree-sitter parse can't compound across workers.
            let (small_items, big_items): (Vec<_>, Vec<_>) = batch_files_slice
                .iter()
                .zip(batch_entries_slice.iter())
                .partition(|(rf, _)| rf.size <= big_file_bytes);
            if !big_items.is_empty() {
                eprintln!(
                    "[route] batch {}: {} small (parallel), {} big (serial, > {})",
                    batch_no, small_items.len(), big_items.len(),
                    human_bytes(big_file_bytes),
                );
            }

            // Helper closure — inlined parse + sink push + diagnostics.
            // Called from both the parallel small pass and the serial big pass.
            // Big-bucket files get their path recorded to a tmp sidecar
            // BEFORE parse; if the cgroup OOM-kills us mid-parse, the next
            // --resume run reads this and adds the file to oom_skiplist
            // (self-healing — pathological files exclude themselves after
            // one OOM, instead of looping forever on the same batch). When
            // workers=1 (the safest defensive config), small-bucket files
            // also mark_attempted since there's no concurrent write contention.
            let last_attempted_path = writer.tmp_dir.as_ref()
                .map(|t| t.join("last_attempted.txt"));
            let process_one = |rf: &RawFile, fe: &FileEntry, mark_attempted: bool| {
                await_memory_headroom();
                let t_file = Instant::now();
                let attempted = if mark_attempted { last_attempted_path.as_deref() } else { None };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    parse_one(rf, fe, root_id, no_refs, max_file_bytes, &registry,
                              &oom_skiplist, attempted, build_trigrams)
                }));
                let elapsed_file_ms = t_file.elapsed().as_millis();
                match result {
                    Ok(Ok((s, r, tgs))) => {
                        if !tgs.is_empty() {
                            let mut sink = trigrams_sink.lock();
                            sink.reserve(tgs.len());
                            for t in tgs { sink.push((t, fe.id)); }
                        }
                        let total_recs = s.len() + r.len();
                        if elapsed_file_ms > outlier_ms || total_recs > outlier_records {
                            eprintln!(
                                "[slow]  {} kind={:?} size={} records={} ({}+{}) elapsed={}ms",
                                rf.path.display(), rf.kind,
                                human_bytes(rf.size),
                                total_recs, s.len(), r.len(),
                                elapsed_file_ms,
                            );
                        }
                        parsed.fetch_add(1, Ordering::Relaxed);
                        symbols_total.fetch_add(s.len() as u64, Ordering::Relaxed);
                        refs_total.fetch_add(r.len() as u64, Ordering::Relaxed);
                        let mut batch_inc: u64 = 0;
                        for x in &s { batch_inc += x.estimated_bytes() as u64; }
                        for x in &r { batch_inc += x.estimated_bytes() as u64; }
                        est_bytes.fetch_add(batch_inc, Ordering::Relaxed);
                        if !s.is_empty() { syms_sink.lock().extend(s); }
                        if !r.is_empty() { refs_sink.lock().extend(r); }
                        let p = parsed.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed);
                        if p % progress_step == 0 {
                            eprintln!(
                                "[batch{}] {} files done, {} syms, {} refs, ~{} in-RAM",
                                batch_no, p,
                                symbols_total.load(Ordering::Relaxed),
                                refs_total.load(Ordering::Relaxed),
                                human_bytes(est_bytes.load(Ordering::Relaxed)),
                            );
                        }
                    }
                    _ => { failed.fetch_add(1, Ordering::Relaxed); }
                }
            };

            // 1. Parallel pass over small files (the bulk of the corpus).
            // At workers=1 there's no concurrent write race on last_attempted,
            // so small files mark too — needed to identify single-worker OOMs.
            let small_marks_attempted = rayon::current_num_threads() == 1;
            small_items.par_iter().for_each(|(rf, fe)| process_one(rf, fe, small_marks_attempted));

            // 2. Serial pass over big files — guarantees ONE big tree-sitter
            //    parse in flight at a time. Peak transient RAM ≈ one big
            //    file's pathological allocation (bounded to a few GB) rather
            //    than N × that. This is the actual fix for the 100ms-burst
            //    OOM that polling-backpressure can't catch.
            for (rf, fe) in &big_items {
                process_one(rf, fe, true);
            }
            // Clear the last-attempted marker after the batch's big files
            // finished — only an UNCLEARED marker on resume means OOM.
            if let Some(p) = last_attempted_path.as_ref() {
                let _ = std::fs::remove_file(p);
            }

            writer.symbols = syms_sink.into_inner();
            writer.refs = refs_sink.into_inner();
            // Drain the trigram sink into the writer's pending buffer.
            if build_trigrams {
                let sink = trigrams_sink.into_inner();
                if !sink.is_empty() {
                    if let Some(buf) = writer.trigrams.as_mut() {
                        buf.reserve(sink.len());
                        buf.extend(sink);
                    }
                }
            }

            let parsed_n = parsed.load(Ordering::Relaxed);
            let failed_n = failed.load(Ordering::Relaxed);
            let syms_n = symbols_total.load(Ordering::Relaxed);
            let refs_n = refs_total.load(Ordering::Relaxed);
            let bytes_n = est_bytes.load(Ordering::Relaxed);
            total_files_parsed += parsed_n;
            total_files_failed += failed_n;
            grand_syms += syms_n;
            grand_refs += refs_n;

            // Flush this batch's accumulation to disk if streaming.
            if streaming {
                let sf = writer.flush_symbols_chunk()?;
                let rf_ = writer.flush_refs_chunk()?;
                let tf = writer.flush_trigrams_chunk()?;
                let _ = (sf, rf_, tf);
                // Atomically record progress AFTER chunks land on disk. If we
                // crash here, the next --resume reads the previous (or no)
                // marker, finds extra chunks past saved_chunks, and drops
                // them as orphans before reprocessing this batch.
                if let Some(pp) = progress_path.as_ref() {
                    let new_completed = batch_end_id + 1;
                    write_progress_atomic(
                        pp,
                        new_completed,
                        writer.symbol_chunk_count,
                        writer.ref_chunk_count,
                        &prepared.iter().map(|p| (p.root_path.display().to_string(),
                            p.files.len() as u64)).collect::<Vec<_>>(),
                    )?;
                }
            }

            // Update rolling avg bytes/file. EMA over batches keeps a single
            // pathological batch from blowing up the next batch's size.
            let files_in_batch = batch_files_slice.len() as u64;
            if files_in_batch > 0 && parsed_n > 0 {
                let observed = (bytes_n as f64) / (parsed_n as f64);
                avg_bytes_per_file = avg_bytes_per_file * 0.7 + observed * 0.3;
            }
            eprintln!(
                "[parse] batch {}/{}  {} files / {} syms / {} refs / ~{} in-RAM / {} ms (avg {} B/file)",
                batch_no, total_batches, parsed_n, syms_n, refs_n,
                human_bytes(bytes_n), batch_start.elapsed().as_millis(),
                avg_bytes_per_file as u64,
            );

            // Soft cap warning if a single batch already exceeded it.
            if mem_cap_bytes > 0 && bytes_n > soft_cap {
                eprintln!(
                    "[index] WARN: batch produced {} > soft cap {} — consider --flush-every smaller",
                    human_bytes(bytes_n), human_bytes(soft_cap),
                );
            }

            start = end;
        }
        eprintln!("[parse] root done in {} ms", parse_total.elapsed().as_millis());
    }

    // In streaming mode we skip in-memory resolve (would require all records).
    // A streaming resolve pass over chunk files can be added later.
    if !streaming && !count_only && !no_refs {
        let t_res = Instant::now();
        writer.resolve_refs();
        let resolved = writer.refs.iter().filter(|r| r.resolved_to.is_some()).count();
        eprintln!(
            "[resolve] {} / {} refs resolved by name in {} ms",
            resolved, writer.refs.len(), t_res.elapsed().as_millis(),
        );
    }

    let elapsed_ms = t_total.elapsed().as_millis();
    let stats = IndexStats {
        files_total: total_files_total,
        files_parsed: total_files_parsed,
        files_failed: total_files_failed,
        bytes_total: total_bytes,
        symbols: grand_syms,
        refs: grand_refs,
        elapsed_ms,
    };
    eprintln!(
        "[write] {} symbols, {} refs across {} files / {} roots, finalizing -> {}",
        grand_syms, grand_refs, writer.files.len(), writer.roots.len(), out_dir.display(),
    );
    if !count_only {
        let t = Instant::now();
        if streaming {
            writer.finalize_streaming(stats)?;
        } else {
            writer.finalize(stats)?;
        }
        eprintln!("[write] finalized in {} ms", t.elapsed().as_millis());
    } else {
        eprintln!("[write] count_only=true, not writing index");
    }
    eprintln!("\nDONE: {} files, {} symbols, {} refs, total {} ms ({:.1} files/s)",
        total_files_total, grand_syms, grand_refs, elapsed_ms,
        total_files_total as f64 / (elapsed_ms.max(1) as f64 / 1000.0),
    );
    heartbeat_stop.store(true, Ordering::Relaxed);
    Ok(())
}

/// Write `progress.json` via tmp-then-rename so the file either reflects the
/// previous batch or the new one — never a half-written mix. Called after
/// every successful chunk flush so a cgroup OOM-kill always lands somewhere
/// recoverable.
fn write_progress_atomic(
    path: &Path,
    completed_files: u32,
    symbol_chunks: u32,
    ref_chunks: u32,
    roots: &[(String, u64)],
) -> Result<()> {
    let v = serde_json::json!({
        "version": 1,
        "completed_files": completed_files,
        "symbol_chunks": symbol_chunks,
        "ref_chunks": ref_chunks,
        "roots": roots.iter().map(|(p, n)| serde_json::json!({"path": p, "n_files": n}))
            .collect::<Vec<_>>(),
    });
    let s = serde_json::to_string(&v)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, s.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn parse_one(
    rf: &RawFile,
    fe: &FileEntry,
    root_id: u8,
    no_refs: bool,
    max_file_bytes: u64,
    registry: &FormatRegistry,
    oom_skiplist: &std::collections::HashSet<String>,
    last_attempted_path: Option<&Path>,
    build_trigrams: bool,
) -> Result<(Vec<SymbolRecord>, Vec<RefRecord>, Vec<scry_store::trigram::Trigram>)> {
    if !registry.supports(rf.kind) {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    if rf.size > max_file_bytes {
        // Loud skip so the operator can audit which files we never opened.
        // These are almost always data-blobs masquerading as source —
        // sqlite3.c amalgamation, crypto test vectors, audio sample headers,
        // generated math constant tables. Tree-sitter ASTs over multi-MB
        // text data routinely OOM workers, so we refuse to even read them.
        eprintln!(
            "[skip-large] {} kind={:?} size={} > max-file-bytes={}",
            rf.path.display(), rf.kind,
            human_bytes(rf.size), human_bytes(max_file_bytes),
        );
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let path_str = rf.path.display().to_string();
    if oom_skiplist.contains(&path_str) {
        // We OOM-killed previously while parsing this exact file. Skip
        // it permanently so the restart loop can make forward progress.
        // The user can re-enable by deleting the oom_skiplist.txt file
        // under <index>.tmp/.
        eprintln!(
            "[skip-oomed] {} kind={:?} size={} (previous run OOMed on this file)",
            rf.path.display(), rf.kind, human_bytes(rf.size),
        );
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    // Stamp the path to disk BEFORE we touch tree-sitter, so if the cgroup
    // OOM-kills us mid-parse the next --resume run can identify the culprit.
    // Only for serial-bucket files where we'd otherwise have ambiguity over
    // which of the concurrent workers crashed us.
    if let Some(p) = last_attempted_path {
        let tmp = p.with_extension("txt.tmp");
        if std::fs::write(&tmp, path_str.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }
    let bytes = std::fs::read(&rf.path)
        .with_context(|| format!("read {}", rf.path.display()))?;
    // Stamp the filename on the worker thread so tree-sitter timeout /
    // abort logs can name the offending file. Cleared after the call.
    let (raw_syms, raw_refs) = scry_lang::with_current_file(
        rf.path.display().to_string(),
        || registry.parse(rf.kind, &bytes),
    );
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
    let trigrams = if build_trigrams {
        scry_store::trigram::extract_sorted(&bytes)
    } else {
        Vec::new()
    };
    Ok((syms, refs, trigrams))
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
    in_: Option<String>,
    limit: usize,
    json: bool,
    md: bool,
    budget: Option<usize>,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let results = r.lookup_exact(&name);
    let mut filtered: Vec<SymbolRecord> = filter_results(results, lang.as_deref(), kind.as_deref());
    if let Some(prefix) = in_.as_deref() {
        filtered.retain(|s| match r.files.get(s.file_id as usize) {
            Some(fe) => fe.display_path(&r.roots).contains(prefix),
            None => false,
        });
    }
    rank_symbols(&mut filtered, &r);
    if md {
        print_results_md(&r, &filtered, limit, budget);
    } else {
        print_results(&r, &filtered, limit, json);
    }
    log_query(&r, "def", &name, filtered.len(), filtered.len().min(limit), t);
    Ok(())
}

fn cmd_prefix(prefix: String, index: Option<PathBuf>, limit: usize, json: bool) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    // Over-fetch then rank; the FST gives unordered hits and the limit
    // should land on the BEST matches, not just the first ones the FST
    // happens to encounter.
    let mut results = r.lookup_prefix(&prefix, limit.saturating_mul(8).max(limit));
    rank_symbols(&mut results, &r);
    let shown = limit.min(results.len());
    print_results(&r, &results[..shown], limit, json);
    log_query(&r, "prefix", &prefix, results.len(), shown, t);
    Ok(())
}

fn cmd_fuzzy(substr: String, index: Option<PathBuf>, limit: usize, json: bool) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let mut results = r.lookup_substring(&substr, limit.saturating_mul(8).max(limit));
    rank_symbols(&mut results, &r);
    let shown = limit.min(results.len());
    print_results(&r, &results[..shown], limit, json);
    log_query(&r, "fuzzy", &substr, results.len(), shown, t);
    Ok(())
}

fn cmd_ref(
    name: String,
    index: Option<PathBuf>,
    lang: Option<String>,
    kind: Option<String>,
    in_: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let results = r.lookup_refs_exact(&name);
    let filtered: Vec<RefRecord> = results
        .into_iter()
        .filter(|rr| {
            if let Some(prefix) = &in_ {
                match r.files.get(rr.file_id as usize) {
                    Some(fe) => if !fe.display_path(&r.roots).contains(prefix.as_str()) {
                        return false;
                    },
                    None => return false,
                }
            }
            if let Some(l) = &lang {
                if !rr.lang.as_str().eq_ignore_ascii_case(l) {
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
    let label = if kind.as_deref() == Some("call") { "callers" } else { "ref" };
    log_query(&r, label, &name, filtered.len(), filtered.len().min(limit), t);
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
    for s in r.iter_symbols() {
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

/// Subtree coverage stats — files, bytes, symbols, broken down per
/// FileKind (and optionally per SymbolKind within each language).
///
/// Performance: with the file_symbols sidecar present, per-file
/// symbol counts come from a single O(1) lookup each, so this runs
/// in O(N files matching PATH). Without the sidecar, falls back to
/// a full symbol-vec scan once and groups in memory (still bounded
/// by the corpus, not the subtree).
fn cmd_coverage(
    path: String,
    index: Option<PathBuf>,
    by_kind: bool,
    json: bool,
) -> Result<()> {
    use std::collections::HashMap;
    let t = Instant::now();
    let r = open_index(index)?;
    let prefix = path.trim();

    // Pass 1: find matching files, group totals by FileKind.
    struct LangBucket {
        files: u64,
        bytes: u64,
        symbols: u64,
        by_kind: HashMap<SymbolKind, u64>,
    }
    impl LangBucket {
        fn new() -> Self { Self { files: 0, bytes: 0, symbols: 0, by_kind: HashMap::new() } }
    }
    let mut by_lang: HashMap<FileKind, LangBucket> = HashMap::new();
    let matching_ids: Vec<(u32, FileKind, u64)> = r.files.iter()
        .filter(|fe| {
            if prefix.is_empty() { true }
            else { fe.display_path(&r.roots).contains(prefix) }
        })
        .map(|fe| (fe.id, fe.kind, fe.size))
        .collect();
    for (_id, kind, size) in &matching_ids {
        let b = by_lang.entry(*kind).or_insert_with(LangBucket::new);
        b.files += 1;
        b.bytes += *size;
    }

    // Pass 2: symbol counts. Fast path uses file_symbols sidecar.
    let has_sidecar = r.symbols_for_file(0).is_some()
        || matching_ids.first().map(|(id, _, _)| r.symbols_for_file(*id).is_some()).unwrap_or(false);
    if has_sidecar && (!by_kind || matching_ids.len() <= 50_000) {
        // O(N matching files) lookups. When by_kind is true we have to
        // decode every symbol anyway, which gets expensive on huge
        // subtrees — fall through to the linear scan in that case.
        for (id, kind, _) in &matching_ids {
            let idxs = r.symbols_for_file(*id).unwrap_or_default();
            if !by_kind {
                if let Some(b) = by_lang.get_mut(kind) {
                    b.symbols += idxs.len() as u64;
                }
            } else {
                for i in &idxs {
                    if let Some(s) = r.get_symbol(*i) {
                        let b = by_lang.entry(s.lang).or_insert_with(LangBucket::new);
                        b.symbols += 1;
                        *b.by_kind.entry(s.kind).or_insert(0) += 1;
                    }
                }
            }
        }
    } else {
        // Slow path: linear scan of every symbol, filter by matching file_id set.
        let matching_set: std::collections::HashSet<u32> =
            matching_ids.iter().map(|(id, _, _)| *id).collect();
        for s in r.iter_symbols() {
            if !matching_set.contains(&s.file_id) { continue; }
            let b = by_lang.entry(s.lang).or_insert_with(LangBucket::new);
            b.symbols += 1;
            if by_kind {
                *b.by_kind.entry(s.kind).or_insert(0) += 1;
            }
        }
    }

    let total_files: u64 = by_lang.values().map(|b| b.files).sum();
    let total_bytes: u64 = by_lang.values().map(|b| b.bytes).sum();
    let total_symbols: u64 = by_lang.values().map(|b| b.symbols).sum();

    if json {
        let by_lang_json: serde_json::Map<String, serde_json::Value> = by_lang.iter()
            .map(|(k, b)| {
                let mut o = serde_json::json!({
                    "files": b.files,
                    "bytes": b.bytes,
                    "symbols": b.symbols,
                });
                if by_kind {
                    let kinds: serde_json::Map<String, serde_json::Value> = b.by_kind.iter()
                        .map(|(sk, c)| (sk.short().to_string(), serde_json::json!(c)))
                        .collect();
                    o["by_kind"] = serde_json::Value::Object(kinds);
                }
                (k.as_str().to_string(), o)
            })
            .collect();
        let out = serde_json::json!({
            "path": prefix,
            "files_total": total_files,
            "bytes_total": total_bytes,
            "symbols_total": total_symbols,
            "by_lang": by_lang_json,
        });
        println!("{}", out);
    } else {
        println!("subtree:      {}", if prefix.is_empty() { "<entire index>" } else { prefix });
        println!("files-total:  {}", total_files);
        println!("bytes-total:  {}", human_bytes(total_bytes));
        println!("symbols:      {}", total_symbols);
        println!();
        println!("{:>10}  {:>14}  {:>12}  lang", "files", "bytes", "symbols");
        println!("{:>10}  {:>14}  {:>12}  ----", "-----", "-----", "-------");
        let mut sorted: Vec<(&FileKind, &LangBucket)> = by_lang.iter().collect();
        sorted.sort_by_key(|(_, b)| std::cmp::Reverse(b.files));
        for (lang, b) in sorted {
            println!("{:>10}  {:>14}  {:>12}  {}",
                     b.files, human_bytes(b.bytes), b.symbols, lang.as_str());
            if by_kind && !b.by_kind.is_empty() {
                let mut kinds: Vec<(&SymbolKind, &u64)> = b.by_kind.iter().collect();
                kinds.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
                for (sk, c) in kinds.iter().take(10) {
                    println!("{:>10}  {:>14}  {:>12}    └─ {}", "", "", c, sk.short());
                }
            }
        }
    }
    log_query(&r, "coverage", prefix, total_files as usize, total_files as usize, t);
    Ok(())
}

/// Resolve a path argument (full or suffix) to a single file_id.
///
/// Match rules:
///   1. Exact match on full display_path → use it.
///   2. Otherwise, any indexed path that ends with `/<arg>` matches —
///      this is what makes `outline frameworks/base/.../Activity.java`
///      work without having to spell out the host root.
///   3. If multiple suffix matches, return the shortest one and emit a
///      warning to stderr so the user knows to disambiguate.
///   4. No match → None.
fn resolve_file_id(r: &StoreReader, arg: &str) -> Option<u32> {
    let arg = arg.trim();
    let mut exact: Option<u32> = None;
    let mut suffix_hits: Vec<(usize, u32, String)> = Vec::new();
    let suf_pat = format!("/{}", arg.trim_start_matches('/'));
    for fe in r.files.iter() {
        let p = fe.display_path(&r.roots);
        if p == arg {
            exact = Some(fe.id);
            break;
        }
        if p.ends_with(&suf_pat) || p == arg.trim_start_matches('/') {
            suffix_hits.push((p.len(), fe.id, p));
        }
    }
    if let Some(id) = exact { return Some(id); }
    suffix_hits.sort_by_key(|t| t.0); // shortest first
    if suffix_hits.len() > 1 {
        eprintln!("[outline] {} files match '{}'; using shortest match {}",
                  suffix_hits.len(), arg, suffix_hits[0].2);
        for (_, _, p) in suffix_hits.iter().skip(1).take(5) {
            eprintln!("  also: {}", p);
        }
    }
    suffix_hits.first().map(|t| t.1)
}

fn cmd_outline(path: String, index: Option<PathBuf>, json: bool, limit: usize) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let file_id = match resolve_file_id(&r, &path) {
        Some(id) => id,
        None => anyhow::bail!("no indexed file matches '{}'", path),
    };
    let fe = r.files.get(file_id as usize)
        .ok_or_else(|| anyhow::anyhow!("file_id {} out of range", file_id))?;
    let display = fe.display_path(&r.roots);

    // Fast path: the file_symbols sidecar gives the symbol indices for
    // this file in O(1), so we decode only the records that actually
    // belong to it — ~1ms on the live 22.8 M-symbol index. Falls back to
    // a linear scan on old indexes lacking the sidecar.
    let mut found: Vec<SymbolRecord> = match r.symbols_for_file(file_id) {
        Some(ids) => {
            let mut v = Vec::with_capacity(ids.len());
            if let Some(lz) = r.lazy_symbols.as_ref() {
                for i in ids { if let Some(s) = lz.get(i as usize) { v.push(s); } }
            } else {
                for i in ids {
                    if let Some(s) = r.symbols.get(i as usize) { v.push(s.clone()); }
                }
            }
            v
        }
        None => r.iter_symbols().filter(|s| s.file_id == file_id).collect(),
    };
    // Stable: sort by (line, col, name).
    found.sort_by(|a, b| (a.line, a.col, &a.name).cmp(&(b.line, b.col, &b.name)));
    let take = if limit == 0 { found.len() } else { limit.min(found.len()) };

    if json {
        let arr: Vec<_> = found.iter().take(take).map(|s| symbol_to_json(&r, s)).collect();
        let out = serde_json::json!({
            "path": display,
            "lang": fe.kind.as_str(),
            "symbols_total": found.len(),
            "symbols_shown": take,
            "symbols": arr,
        });
        println!("{}", out);
    } else {
        println!("# {}  ({:?})", display, fe.kind);
        println!("# {} symbols", found.len());
        for s in found.iter().take(take) {
            let scope = if s.scope_path.is_empty() { String::new() }
                        else { format!("  [{}]", s.scope_path.join("::")) };
            println!("{:>5}:{:<3}  {:<12}  {}{}",
                     s.line, s.col, s.kind.short(), s.name, scope);
        }
        if take < found.len() {
            println!("... ({} more — pass --limit 0 to see all)", found.len() - take);
        }
    }
    log_query(&r, "outline", &path, found.len(), take, t);
    Ok(())
}

/// Sort symbol hits by descending desirability. Composes the kind/lang/
/// scope heuristic from SymbolRecord::rank_score with a path-shape signal
/// the store can't see (path depth, presence of `test/` segments — a test
/// fixture is rarely the canonical hit).
///
/// Stable: ties resolve by (path, line, col) so the output is reproducible
/// across runs of the same query.
fn rank_symbols(syms: &mut [SymbolRecord], r: &StoreReader) {
    syms.sort_by(|a, b| {
        let pa = r.files.get(a.file_id as usize).map(|f| f.display_path(&r.roots)).unwrap_or_default();
        let pb = r.files.get(b.file_id as usize).map(|f| f.display_path(&r.roots)).unwrap_or_default();
        let sa = symbol_total_score(a, &pa);
        let sb = symbol_total_score(b, &pb);
        // descending score; tie-break ascending (path, line, col) for
        // deterministic output.
        sb.cmp(&sa).then_with(|| (&pa, a.line, a.col).cmp(&(&pb, b.line, b.col)))
    });
}

/// Combine SymbolRecord::rank_score with path-shape signals only the CLI
/// (which holds the StoreReader) can compute. Negative adjustments for
/// fixtures/tests/sample paths; small positive for the shortest paths
/// (closest to repo root = usually canonical).
// Ranking weights. Tuned by eyeballing real `def Activity` /
// `def Foo` results against the live AOSP+Linux index; tweak with
// regression tests, not in isolation. All values are deductions —
// the kind score from SymbolRecord::rank_score() provides the
// positive baseline.
const PENALTY_TEST_PATH: i64 = 25;
const PENALTY_SAMPLE_PATH: i64 = 15;
const PENALTY_GENERATED_PATH: i64 = 20;
/// Path depth past PATH_DEPTH_FREE_SEGMENTS starts costing 1 point per
/// extra slash, up to PATH_DEPTH_MAX_PENALTY. Mild signal — won't
/// dominate the kind/lang ordering. ("free" = the first N slashes are
/// the repo root and don't say anything about the file's canonicalness.)
const PATH_DEPTH_FREE_SEGMENTS: i64 = 6;
const PATH_DEPTH_MAX_PENALTY: i64 = 8;

fn symbol_total_score(s: &SymbolRecord, path: &str) -> i64 {
    let mut score = s.rank_score();
    let path_lower = path.to_ascii_lowercase();
    if path_lower.contains("/test/") || path_lower.contains("/tests/")
        || path_lower.contains("/testing/") {
        score -= PENALTY_TEST_PATH;
    }
    if path_lower.contains("/sample") || path_lower.contains("/example") {
        score -= PENALTY_SAMPLE_PATH;
    }
    if path_lower.contains("/generated/") || path_lower.contains("/gen/")
        || path_lower.contains(".pb.") || path_lower.contains("_pb2.") {
        score -= PENALTY_GENERATED_PATH;
    }
    let depth = path.bytes().filter(|b| *b == b'/').count() as i64;
    score -= (depth - PATH_DEPTH_FREE_SEGMENTS).max(0).min(PATH_DEPTH_MAX_PENALTY);
    score
}

fn filter_results(
    syms: Vec<SymbolRecord>,
    lang: Option<&str>,
    kind: Option<&str>,
) -> Vec<SymbolRecord> {
    syms.into_iter()
        .filter(|s| {
            if let Some(l) = lang {
                if !s.lang.as_str().eq_ignore_ascii_case(l) {
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
    syms: &[SymbolRecord],
    limit: usize,
    budget: Option<usize>,
) {
    let mut emitted_bytes: usize = 0;
    let mut emitted_count: usize = 0;
    let cap = syms.len().min(limit);
    for s in syms.iter().take(cap) {
        let file = reader.files.get(s.file_id as usize);
        let path = file.map(|f| f.display_path(&reader.roots)).unwrap_or_default();
        let snippet = read_snippet(&path, s.line, 8);
        let scope_str = if s.scope_path.is_empty() {
            String::new()
        } else {
            format!("**scope**: `{}`  ", s.scope_path.join("::"))
        };
        let lang_label = s.lang.as_str();
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
                // Actually-correct remaining count: total - what we emitted.
                // The previous expression (syms.len() - syms.iter().take_while(|_|false).count())
                // was always syms.len() - 0, lying about the number omitted.
                println!("\n_(budget {} bytes reached, {} more results omitted)_",
                    b, cap.saturating_sub(emitted_count));
                break;
            }
        }
        emitted_bytes += section.len();
        emitted_count += 1;
        print!("{}", section);
    }
}

/// Read a snippet of `total_lines` centered roughly on `line`.
/// Default path for the per-query ops log. Honors $SCRY_LOG, otherwise
/// $HOME/.scry/queries.log. Returns None on a non-Unicode or missing
/// HOME (no log = best-effort skip, not an error).
fn query_log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCRY_LOG") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".scry").join("queries.log"))
}

/// Print a one-line stats footer to stderr AND append a JSON line to
/// the ops log (~/.scry/queries.log by default). Both are best-effort
/// — a log-write failure never affects the query's exit status.
///
/// `total_files` defaults to the index's `files_total` (every search
/// looks at every file conceptually); commands that genuinely narrow
/// (grep with trigram pre-filter) should call log_query_with_files
/// to surface the candidate count.
fn log_query(
    r: &StoreReader,
    cmd: &str,
    query: &str,
    hits: usize,
    shown: usize,
    t_start: Instant,
) {
    log_query_with_files(r, cmd, query, hits, shown, t_start, None);
}

fn log_query_with_files(
    r: &StoreReader,
    cmd: &str,
    query: &str,
    hits: usize,
    shown: usize,
    t_start: Instant,
    candidate_files: Option<usize>,
) {
    let elapsed_ms = t_start.elapsed().as_millis();
    let total_files = r.manifest.stats.files_total;
    let cands = candidate_files
        .map(|c| format!(" cands={c}"))
        .unwrap_or_default();
    eprintln!(
        "[scry] cmd={cmd} q={query:?} hits={hits} shown={shown} files={total_files}{cands} elapsed={elapsed_ms}ms",
    );

    // Best-effort JSON append. Errors are swallowed — we never want a
    // log-write failure to corrupt a query's exit code or stdout.
    if let Some(path) = query_log_path() {
        let _ = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true).append(true).open(&path)?;
            let line = serde_json::json!({
                "ts": now_unix_secs(),
                "cmd": cmd,
                "query": query,
                "hits": hits,
                "shown": shown,
                "files_total": total_files,
                "candidate_files": candidate_files,
                "elapsed_ms": elapsed_ms,
                "index": r.paths.root.display().to_string(),
            });
            use std::io::Write;
            writeln!(f, "{}", line)?;
            Ok(())
        })();
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0)
}

fn read_snippet(path: &str, line: u32, total_lines: u32) -> String {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let src = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = src.lines().collect();
    // Defensive: an empty file produces lines.len() == 0, so the prior
    // `lines[start..=end]` would index an empty Vec and panic. Caller
    // is from the Markdown formatter which iterates ranking results
    // and reads each file independently — one empty file shouldn't
    // sink the whole query.
    if lines.is_empty() { return String::new(); }
    let target = line.saturating_sub(1) as usize;
    let half = (total_lines / 2) as usize;
    let start = target.saturating_sub(half);
    let end = (target + half).min(lines.len() - 1);
    lines[start..=end].join("\n")
}

fn print_results(reader: &StoreReader, syms: &[SymbolRecord], limit: usize, json: bool) {
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
                "lang": s.lang.as_str(),
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

fn print_refs(reader: &StoreReader, refs: &[RefRecord], limit: usize, json: bool) {
    if json {
        for r in refs.iter().take(limit) {
            let file = reader.files.get(r.file_id as usize);
            let path = file.map(|f| f.display_path(&reader.roots)).unwrap_or_default();
            let obj = serde_json::json!({
                "name": r.name,
                "ref_kind": r.kind.short(),
                "lang": r.lang.as_str(),
                "path": path,
                "line": r.line,
                "col": r.col,
                "scope": r.scope_path,
                "resolved_to": r.resolved_to,
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
        let resolved = r.resolved_to
            .map(|id| format!("  → def:{:x}", id))
            .unwrap_or_default();
        println!(
            "{}:{}:{}  ({} {}){}  {}{}",
            path, r.line, r.col,
            r.kind.short(),
            short_lang(r.lang),
            scope,
            r.name,
            resolved,
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
    workers: Option<usize>,
    max_file_bytes: u64,
    mem_cap: u32,
) -> Result<()> {
    let t = Instant::now();
    if let Some(n) = workers {
        if n > 0 {
            if let Err(e) = rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build_global()
            {
                // build_global errors if invoked a second time in one
                // process (the global pool is set-once). For the CLI
                // that's never a real bug (each invocation is a fresh
                // process), but in a future in-process driver it would
                // silently drop the --workers flag. Surface it.
                eprintln!("[warn] rayon global pool already initialized: {e}; --workers ignored");
            }
        }
    }
    // Internal cap (no /proc polling). Each candidate file may temporarily
    // hold up to max_file_bytes in RAM during scan. With N workers running
    // in parallel, in-flight read buffers ≤ workers * max_file_bytes. We
    // refuse to start if mem_cap (when set) can't accommodate that.
    if mem_cap > 0 {
        let workers_n = rayon::current_num_threads() as u64;
        let in_flight = workers_n * max_file_bytes;
        let cap_bytes = (mem_cap as u64) * 1024 * 1024 * 1024;
        if in_flight > cap_bytes {
            anyhow::bail!(
                "grep would peak at ~{} (workers {} × max_file_bytes {}) > mem_cap {} GiB; \
                 lower --workers or --max-file-bytes",
                human_bytes(in_flight), workers_n, human_bytes(max_file_bytes), mem_cap,
            );
        }
    }
    let r = open_index(index)?;
    let re = if is_regex {
        Some(regex::bytes::Regex::new(&pattern).context("invalid regex")?)
    } else {
        None
    };

    // Filter files
    let lang_lower = lang.as_ref().map(|s| s.to_ascii_lowercase());
    let prefix = in_.as_deref().unwrap_or("");
    // Trigram pre-filter: for LITERAL patterns of >= 3 bytes, query the
    // trigram index to get the set of files that COULD contain the needle.
    // This is the 100× rg path: instead of scanning every file matching
    // lang/prefix, we open only the files containing the pattern's trigrams.
    // Regex queries skip this (a regex could match anything).
    let trigram_candidates: Option<std::collections::HashSet<u32>> = if !is_regex {
        let t_tg = Instant::now();
        let cs = r.grep_candidates(pattern.as_bytes());
        if let Some(ref c) = cs {
            eprintln!("[grep] trigram pre-filter: {} candidate files in {} ms",
                c.len(), t_tg.elapsed().as_millis());
        }
        cs
    } else {
        // Regex pre-filter: extract literal substrings from the pattern
        // via regex-syntax HIR analysis, trigram-intersect each literal,
        // then UNION across alternatives. Russ Cox / livegrep style.
        // If extraction yields no useful trigrams we fall back to a full
        // scan rather than over-narrow.
        let t_tg = Instant::now();
        let cs = grep_candidates_for_regex(&r, &pattern);
        if let Some(ref c) = cs {
            eprintln!("[grep] regex→trigram pre-filter: {} candidate files in {} ms",
                c.len(), t_tg.elapsed().as_millis());
        } else {
            eprintln!("[grep] regex has no extractable literal — full scan in {} ms decision",
                t_tg.elapsed().as_millis());
        }
        cs
    };
    let candidates: Vec<&FileEntry> = r
        .files
        .iter()
        .filter(|fe| {
            if let Some(ref tg) = trigram_candidates {
                if !tg.contains(&fe.id) {
                    return false;
                }
            }
            if let Some(ref l) = lang_lower {
                if !fe.kind.as_str().eq_ignore_ascii_case(l) {
                    return false;
                }
            }
            if !prefix.is_empty() {
                // Same semantics as cmd_def/cmd_ref: --in is a substring
                // of the absolute path so the caller can pass either a
                // root-relative subdir ("frameworks/base/") or an absolute
                // one and have both work.
                let full = fe.display_path(&r.roots);
                if !full.contains(prefix) {
                    return false;
                }
            }
            true
        })
        .collect();

    let total_files = candidates.len();
    let tg_label = if trigram_candidates.is_some() { " (trigram-filtered)" } else { "" };
    eprintln!("[grep] scanning {} files{}", total_files, tg_label);

    // Prefault the candidate files into the page cache before kicking
    // off the scan loop. perf stat decomposition shows cold grep is
    // page-fault dominated (1.37s sys vs 0.6s user on a 680ms query);
    // hinting the kernel to start pulling pages NOW lets disk IO
    // overlap with the parallel memchr-scan loop below. Best-effort,
    // bounded — only fires when the trigram pre-filter narrowed
    // enough that prefaulting all candidates is cheap (≤ 8k files).
    // Past that, the bookkeeping cost > the prefetch win.
    if total_files > 0 && total_files <= 8000 {
        let t_pf = Instant::now();
        candidates.par_iter().for_each(|fe| {
            let path = fe.display_path(&r.roots);
            scry_store::prefault_path(std::path::Path::new(&path));
        });
        eprintln!("[grep] prefaulted {} files in {} ms",
                  total_files, t_pf.elapsed().as_millis());
    }

    let hits: parking_lot::Mutex<Vec<Hit>> = parking_lot::Mutex::new(Vec::new());
    let hit_count = std::sync::atomic::AtomicUsize::new(0);
    candidates.par_iter().for_each(|fe| {
        if hit_count.load(std::sync::atomic::Ordering::Relaxed) >= limit * 8 {
            return; // bound work after we have plenty of candidates
        }
        let path = fe.display_path(&r.roots);
        let md = std::fs::metadata(&path).ok();
        if let Some(m) = md.as_ref() {
            if m.len() > max_file_bytes { return; }
        }
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
    let label = if is_regex { "grep-regex" } else { "grep" };
    log_query_with_files(&r, label, &pattern, hits.len(), hits.len(), t, Some(total_files));
    Ok(())
}

struct Hit {
    file_id: u32,
    line: u32,
    col: u32,
    snippet: String,
}

// ---------------------------------------------------------------------------
// build-offsets (standalone — add offsets sidecars for lazy reader)
// ---------------------------------------------------------------------------

fn cmd_build_offsets(index: Option<PathBuf>) -> Result<()> {
    use std::io::{BufReader, BufWriter, Read, Write};
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[offsets] target index: {}", index_dir.display());
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let t_total = Instant::now();

    // Generic walker: stream-decode each bincode Vec<T>, recording the
    // byte position of each record into a u64-LE sidecar.
    fn build_one<T: for<'de> serde::Deserialize<'de> + serde::Serialize>(
        data_path: &Path, offsets_path: &Path, label: &str,
    ) -> Result<u64> {
        if !data_path.exists() {
            eprintln!("[offsets] {} missing — skipping", data_path.display());
            return Ok(0);
        }
        let f = std::fs::File::open(data_path)
            .with_context(|| format!("open {}", data_path.display()))?;
        let mut reader = BufReader::with_capacity(8 << 20, f);
        let mut len_buf = [0u8; 8];
        reader.read_exact(&mut len_buf)?;
        let total = u64::from_le_bytes(len_buf);
        let mut ow = BufWriter::with_capacity(8 << 20, std::fs::File::create(offsets_path)?);
        let mut byte_pos: u64 = 8;
        let t = Instant::now();
        for i in 0..total {
            ow.write_all(&byte_pos.to_le_bytes())?;
            let r: T = bincode::deserialize_from(&mut reader)
                .with_context(|| format!("decode {label} record {i}"))?;
            let sz = bincode::serialized_size(&r)
                .with_context(|| format!("size {label} record {i}"))?;
            byte_pos += sz;
            if i % 1_000_000 == 0 && i > 0 {
                eprintln!("[offsets] {label}: {i}/{total} records ({} ms)", t.elapsed().as_millis());
            }
        }
        ow.flush()?;
        eprintln!("[offsets] {label}: {total} records → {} in {} ms",
            human_bytes(std::fs::metadata(offsets_path).map(|m| m.len()).unwrap_or(0)),
            t.elapsed().as_millis(),
        );
        Ok(total)
    }

    let n_syms = build_one::<scry_store::SymbolRecord>(
        &paths.symbols(), &paths.symbol_offsets(), "symbols"
    )?;
    let n_refs = build_one::<scry_store::RefRecord>(
        &paths.refs(), &paths.ref_offsets(), "refs"
    )?;
    eprintln!(
        "[offsets] DONE.  {} symbols + {} refs offsets written in {} ms",
        n_syms, n_refs, t_total.elapsed().as_millis(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// build-file-symbols (standalone — file→symbol-ids sidecar)
// ---------------------------------------------------------------------------

fn cmd_build_file_symbols(index: Option<PathBuf>) -> Result<()> {
    use std::io::{BufWriter, Write};
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[fsyms] target index: {}", index_dir.display());
    let paths = scry_store::StorePaths::new(index_dir.clone());

    // Need the file count + the symbol vec (lazy is fine — we walk it
    // exactly once). Open through StoreReader so the offsets sidecar is
    // available and we avoid loading the whole 10 GB symbols.bin into RAM.
    let r = scry_store::StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    let n_files = r.files.len();
    eprintln!("[fsyms] {} files, {} symbols — building reverse map", n_files, r.n_symbols());

    let t = Instant::now();
    let mut by_file: Vec<Vec<u32>> = vec![Vec::new(); n_files];
    let mut sym_idx: u32 = 0;
    for s in r.iter_symbols() {
        let fid = s.file_id as usize;
        if fid < by_file.len() {
            by_file[fid].push(sym_idx);
        }
        sym_idx += 1;
        if sym_idx % 1_000_000 == 0 {
            eprintln!("[fsyms] grouped {} M symbols ({} ms)", sym_idx / 1_000_000, t.elapsed().as_millis());
        }
    }
    eprintln!("[fsyms] grouping done in {} ms; writing sidecars", t.elapsed().as_millis());

    // Atomic-ish: write to .tmp paths then rename. The reader picks up
    // whichever pair is present at open time, so an interrupted run
    // leaves the old (or no) sidecar — never a torn one.
    let data_tmp = paths.file_symbols().with_extension("bin.tmp");
    let off_tmp = paths.file_symbols_offsets().with_extension("bin.tmp");
    {
        let mut w = BufWriter::with_capacity(8 << 20, std::fs::File::create(&data_tmp)?);
        let mut ow = BufWriter::with_capacity(8 << 20, std::fs::File::create(&off_tmp)?);
        let mut byte_pos: u64 = 0;
        for ids in &by_file {
            ow.write_all(&byte_pos.to_le_bytes())?;
            let count = ids.len() as u32;
            w.write_all(&count.to_le_bytes())?;
            for id in ids {
                w.write_all(&id.to_le_bytes())?;
            }
            byte_pos += 4 + 4 * (ids.len() as u64);
        }
        w.flush()?;
        ow.flush()?;
    }
    std::fs::rename(&data_tmp, paths.file_symbols())?;
    std::fs::rename(&off_tmp, paths.file_symbols_offsets())?;

    eprintln!("[fsyms] DONE in {} ms. file_symbols={} offsets={}",
        t.elapsed().as_millis(),
        human_bytes(std::fs::metadata(paths.file_symbols()).map(|m| m.len()).unwrap_or(0)),
        human_bytes(std::fs::metadata(paths.file_symbols_offsets()).map(|m| m.len()).unwrap_or(0)),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// build-resolutions (Layer 2 ref→def resolution sidecar)
// ---------------------------------------------------------------------------

/// One-shot pass over the finalized index producing a u64-LE per-ref
/// override (`ref_resolutions.bin`). Algorithm:
///   1. Walk symbols once → name → Vec<(sym_idx, sym_id, file_id, lang)>
///      and per_file_pkg (Java only) from kind=Package symbols.
///   2. Walk refs once → for each ref, narrow candidates with the
///      file's package + imports, fall back to plain name match, write
///      the chosen def's u64 id to the sidecar (or 0 if unresolved).
fn cmd_build_resolutions(index: Option<PathBuf>) -> Result<()> {
    use std::collections::HashMap;
    use std::io::{BufWriter, Write};
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[res] target index: {}", index_dir.display());
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let r = scry_store::StoreReader::open(&index_dir)?;
    let n_refs = r.n_refs();
    let n_syms = r.n_symbols();
    if n_refs == 0 {
        eprintln!("[res] no refs to resolve — skipping");
        return Ok(());
    }
    eprintln!("[res] {} symbols, {} refs", n_syms, n_refs);

    // --- Pass 1: build the symbol index. ---
    let t1 = Instant::now();
    let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
    // Per-file Java package (extracted via SymbolKind::Package). Computed
    // in the same pass to avoid double-iterating the symbol vec.
    let mut per_file_pkg: HashMap<u32, String> = HashMap::new();
    let mut pass1 = |s: scry_store::SymbolRecord| {
        if matches!(s.kind, scry_store::SymbolKind::Package)
            && matches!(s.lang, scry_walker::FileKind::Java) {
            per_file_pkg.insert(s.file_id, s.name.clone());
        }
        by_name.entry(s.name.clone()).or_default().push(ResolveDef {
            id: s.id, file_id: s.file_id, lang: s.lang,
            pkg: None, // filled in pass1b for Java type-defs
        });
    };
    for s in r.iter_symbols() { pass1(s); }
    // Pass1b: stamp pkg on Java type-defs (Class/Interface/Enum) — small
    // overhead, lets the resolver short-circuit by-package lookups
    // without re-resolving file_id → pkg on every ref.
    for entries in by_name.values_mut() {
        for e in entries.iter_mut() {
            if matches!(e.lang, scry_walker::FileKind::Java) {
                if let Some(pkg) = per_file_pkg.get(&e.file_id) {
                    e.pkg = Some(pkg.clone());
                }
            }
        }
    }
    eprintln!("[res] pass 1 (by-name + per-file-pkg) in {} ms", t1.elapsed().as_millis());

    // --- Pass 2: per-file import lists (Java). ---
    let t2 = Instant::now();
    let mut per_file_imports: HashMap<u32, Vec<(String, Option<String>)>> = HashMap::new();
    let mut process_import = |rr: &scry_store::RefRecord| {
        if !matches!(rr.kind, scry_store::RefKind::Import) { return; }
        if !matches!(rr.lang, scry_walker::FileKind::Java) { return; }
        // For Java, the importer emits the import ref with name = the full
        // qualified path. Split into pkg + simple name.
        let (pkg, simple) = match rr.name.rsplit_once('.') {
            Some((p, s)) => (Some(p.to_string()), s.to_string()),
            None => (None, rr.name.clone()),
        };
        per_file_imports.entry(rr.file_id).or_default().push((simple, pkg));
    };
    for rr in r.iter_refs() { process_import(&rr); }
    eprintln!("[res] pass 2 (per-file imports: {} files) in {} ms",
              per_file_imports.len(), t2.elapsed().as_millis());

    // --- Pass 3: resolve every ref, write sidecar. ---
    let t3 = Instant::now();
    let tmp = paths.ref_resolutions().with_extension("bin.tmp");
    let mut ow = BufWriter::with_capacity(8 << 20, std::fs::File::create(&tmp)?);
    let mut resolved_count: u64 = 0;
    let mut narrowed_count: u64 = 0;
    let mut resolve_ref = |rr: &scry_store::RefRecord| -> Result<()> {
        let chosen_id = resolve_one(rr, &by_name, &per_file_pkg, &per_file_imports,
                                     &mut narrowed_count);
        if chosen_id != 0 { resolved_count += 1; }
        ow.write_all(&chosen_id.to_le_bytes())?;
        Ok(())
    };
    for rr in r.iter_refs() { resolve_ref(&rr)?; }
    ow.flush()?;
    drop(ow);
    std::fs::rename(&tmp, paths.ref_resolutions())?;
    eprintln!("[res] pass 3 (resolve {} refs, {} resolved, {} narrowed via Java context) in {} ms",
              n_refs, resolved_count, narrowed_count, t3.elapsed().as_millis());
    eprintln!("[res] DONE. {} bytes written → {}",
        std::fs::metadata(paths.ref_resolutions()).map(|m| m.len()).unwrap_or(0),
        paths.ref_resolutions().display());
    Ok(())
}

/// Pick the best def for one ref. Returns the def's u64 id or 0 = unresolved.
/// Updates `narrowed_count` when Java-aware narrowing makes the choice (vs
/// plain name-match fallback).
fn resolve_one(
    rr: &scry_store::RefRecord,
    by_name: &std::collections::HashMap<String, Vec<ResolveDef>>,
    per_file_pkg: &std::collections::HashMap<u32, String>,
    per_file_imports: &std::collections::HashMap<u32, Vec<(String, Option<String>)>>,
    narrowed: &mut u64,
) -> u64 {
    let cands = match by_name.get(&rr.name) {
        Some(c) if !c.is_empty() => c,
        _ => return 0,
    };
    // Single candidate: trivially resolve.
    if cands.len() == 1 { return cands[0].id; }

    // Same-lang preference (mirrors the old Layer 1 behavior).
    let same_lang: Vec<&ResolveDef> = cands.iter().filter(|c| c.lang == rr.lang).collect();
    let pool: &[&ResolveDef] = if !same_lang.is_empty() { &same_lang[..] } else {
        // Fall back to all candidates if no same-lang match (rare).
        // Need to convert &[ResolveDef] to &[&ResolveDef] for the type.
        // Simpler: pick first cand and return.
        return cands[0].id;
    };
    if pool.len() == 1 { return pool[0].id; }

    // Java-aware narrowing: prefer (same package) > (imported) > (java.lang) > anything.
    if matches!(rr.lang, scry_walker::FileKind::Java) {
        let my_pkg = per_file_pkg.get(&rr.file_id);
        let imports = per_file_imports.get(&rr.file_id);

        // 1. Same-package match.
        if let Some(pkg) = my_pkg {
            for c in pool {
                if c.pkg.as_deref() == Some(pkg.as_str()) {
                    *narrowed += 1;
                    return c.id;
                }
            }
        }
        // 2. Explicit import match: `import x.y.Bar;` → resolve `Bar` only
        //    if c's package == "x.y".
        if let Some(imps) = imports {
            for (simple, pkg) in imps {
                if simple == &rr.name {
                    if let Some(p) = pkg {
                        for c in pool {
                            if c.pkg.as_deref() == Some(p.as_str()) {
                                *narrowed += 1;
                                return c.id;
                            }
                        }
                    }
                }
                if simple == "*" {
                    if let Some(p) = pkg {
                        for c in pool {
                            if c.pkg.as_deref() == Some(p.as_str()) {
                                *narrowed += 1;
                                return c.id;
                            }
                        }
                    }
                }
            }
        }
        // 3. java.lang fallback.
        for c in pool {
            if c.pkg.as_deref() == Some("java.lang") {
                *narrowed += 1;
                return c.id;
            }
        }
    }

    // 4. Layer 1 fallback: pick first same-lang candidate.
    pool[0].id
}

/// One candidate definition for a ref. Loaded from the lazy symbols
/// vec during the resolution pre-pass; the resolver then narrows by
/// pkg (Java) or returns the first same-lang candidate. `file_id` is
/// kept so pass1b can stamp each Java type-def with the package its
/// file declares (resolver short-circuit: by-package lookup without
/// re-resolving file_id → pkg on every ref).
#[derive(Clone)]
struct ResolveDef {
    id: u64,
    file_id: u32,
    lang: scry_walker::FileKind,
    pkg: Option<String>,
}

// ---------------------------------------------------------------------------
// build-trigrams (standalone — add trigram index to an existing index)
// ---------------------------------------------------------------------------

fn cmd_build_trigrams(
    index: Option<PathBuf>,
    workers: Option<usize>,
    max_file_bytes: u64,
) -> Result<()> {
    if let Some(n) = workers {
        if n > 0 {
            if let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(n).build_global() {
                eprintln!("[warn] rayon global pool already initialized: {e}; --workers ignored");
            }
            eprintln!("[trigrams] rayon pool: {} workers", n);
        }
    }
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[trigrams] target index: {}", index_dir.display());
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open {}", index_dir.display()))?;
    eprintln!("[trigrams] {} files in index", r.files.len());

    // Per-batch sink + chunk staging dir. We piggyback on the index's
    // <index>.trigrams_tmp/ to keep artifacts off the live index dir until
    // we atomically rename them in.
    let tmp = index_dir.with_extension("trigrams_tmp");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .with_context(|| format!("clean stale {}", tmp.display()))?;
    }
    std::fs::create_dir_all(&tmp)?;

    let batch_size: usize = 5000;
    let n_files = r.files.len();
    let total_batches = (n_files + batch_size - 1) / batch_size.max(1);
    let mut chunk_count: u32 = 0;
    let t_total = Instant::now();
    let total_failed = std::sync::atomic::AtomicU64::new(0);
    let total_skipped = std::sync::atomic::AtomicU64::new(0);
    let total_trigram_pushes = std::sync::atomic::AtomicU64::new(0);

    let mut start = 0usize;
    let mut batch_no = 0usize;
    while start < n_files {
        let end = (start + batch_size).min(n_files);
        batch_no += 1;
        let slice = &r.files[start..end];
        let sink: parking_lot::Mutex<Vec<(scry_store::trigram::Trigram, u32)>> =
            parking_lot::Mutex::new(Vec::with_capacity(slice.len() * 4096));
        let t_batch = Instant::now();
        slice.par_iter().for_each(|fe| {
            if fe.size > max_file_bytes {
                total_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            let path = fe.display_path(&r.roots);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => { total_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed); return; }
            };
            let trigrams = scry_store::trigram::extract_sorted(&bytes);
            if trigrams.is_empty() { return; }
            let mut s = sink.lock();
            s.reserve(trigrams.len());
            for t in &trigrams { s.push((*t, fe.id)); }
            total_trigram_pushes.fetch_add(trigrams.len() as u64, std::sync::atomic::Ordering::Relaxed);
        });
        // Flush this batch's tuples to a sorted chunk file (same format the
        // writer's flush_trigrams_chunk uses, so kway_merge can consume them).
        let mut buf = sink.into_inner();
        buf.sort_unstable();
        let chunk_path = tmp.join(format!("trigrams.chunk.{:06}.bin", chunk_count));
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&chunk_path)?);
        use std::io::Write as _;
        for (t, f) in &buf {
            w.write_all(t)?;
            w.write_all(&f.to_le_bytes())?;
        }
        w.flush()?;
        let chunk_bytes = std::fs::metadata(&chunk_path).map(|m| m.len()).unwrap_or(0);
        chunk_count += 1;
        eprintln!(
            "[trigrams] batch {}/{}  {} files / {} tuples / chunk {} ({}) / {} ms",
            batch_no, total_batches, slice.len(), buf.len(),
            chunk_count - 1, human_bytes(chunk_bytes),
            t_batch.elapsed().as_millis(),
        );
        start = end;
    }

    eprintln!(
        "[trigrams] all batches done in {} ms. failed reads: {}, skipped (>max-file-bytes): {}, total tuples: {}",
        t_total.elapsed().as_millis(),
        total_failed.load(std::sync::atomic::Ordering::Relaxed),
        total_skipped.load(std::sync::atomic::Ordering::Relaxed),
        total_trigram_pushes.load(std::sync::atomic::Ordering::Relaxed),
    );

    // K-way merge into the final trigrams.fst + trigram_postings.bin
    // (in the tmp dir, then atomically rename into the index).
    let t_merge = Instant::now();
    let chunk_paths: Vec<PathBuf> = (0..chunk_count)
        .map(|n| tmp.join(format!("trigrams.chunk.{:06}.bin", n)))
        .collect();
    let staged_fst = tmp.join("trigrams.fst");
    let staged_postings = tmp.join("trigram_postings.bin");
    scry_store::kway_merge_trigrams_to_fst_public(&chunk_paths, &staged_fst, &staged_postings)?;
    eprintln!("[trigrams] k-way merge done in {} ms", t_merge.elapsed().as_millis());

    // Move staged files into the index dir, replacing any existing.
    let target_fst = index_dir.join("trigrams.fst");
    let target_postings = index_dir.join("trigram_postings.bin");
    if target_fst.exists() { std::fs::remove_file(&target_fst).ok(); }
    if target_postings.exists() { std::fs::remove_file(&target_postings).ok(); }
    std::fs::rename(&staged_fst, &target_fst)?;
    std::fs::rename(&staged_postings, &target_postings)?;
    // Drop chunk files
    for p in &chunk_paths { let _ = std::fs::remove_file(p); }
    std::fs::remove_dir_all(&tmp).ok();

    let fst_sz = std::fs::metadata(&target_fst).map(|m| m.len()).unwrap_or(0);
    let post_sz = std::fs::metadata(&target_postings).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "[trigrams] DONE.  trigrams.fst: {}, trigram_postings.bin: {}, total {} (in {} ms)",
        human_bytes(fst_sz), human_bytes(post_sz), human_bytes(fst_sz + post_sz),
        t_total.elapsed().as_millis(),
    );
    Ok(())
}

/// Extract literal substrings from a regex for trigram pre-filtering,
/// using the given extraction direction (Prefix or Suffix). Returns
/// None when the result would over-broaden — empty Seq, > 64-wide
/// alternation, or any literal shorter than 3 bytes (too short to
/// trigram).
fn regex_literals_kind(
    pattern: &str,
    kind: regex_syntax::hir::literal::ExtractKind,
) -> Option<Vec<Vec<u8>>> {
    use regex_syntax::hir::literal::Extractor;
    let hir = regex_syntax::parse(pattern).ok()?;
    let seq = Extractor::new().kind(kind).extract(&hir);
    let lits = seq.literals()?;
    if lits.is_empty() { return None; }
    if lits.len() > 64 { return None; }
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(lits.len());
    for lit in lits {
        let bytes = lit.as_bytes();
        if bytes.len() < 3 { return None; }
        out.push(bytes.to_vec());
    }
    Some(out)
}

/// Prefix-only literal extraction; used only by the regex_lit_* unit
/// tests that pin the prefix-direction extractor's output shape. The
/// production grep path goes through grep_candidates_for_regex which
/// calls regex_literals_kind for both directions directly.
#[cfg(test)]
fn regex_literals_for_trigram(pattern: &str) -> Option<Vec<Vec<u8>>> {
    regex_literals_kind(pattern, regex_syntax::hir::literal::ExtractKind::Prefix)
}

/// Compute the candidate file set for ONE direction (prefix OR suffix).
/// UNIONs across the alternatives in that direction (the regex matches
/// if ANY alternative matches).
fn candidates_for_literals(
    r: &StoreReader,
    lits: &[Vec<u8>],
) -> Option<std::collections::HashSet<u32>> {
    let mut union: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for lit in lits {
        let part = r.grep_candidates(lit)?;
        if part.is_empty() { continue; }
        union.extend(part);
    }
    Some(union)
}

/// For a regex pattern, return the trigram candidate set if any can be
/// extracted. Returns None ⇒ caller should fall back to a full scan.
///
/// Russ Cox 2012 / livegrep extract MULTIPLE literals from a regex and
/// AND-intersect: any matching file must contain a prefix literal AND
/// (when extractable) a suffix literal too. For a regex like
/// `r"foo.*bar"` that means the file must contain both "foo" and
/// "bar" — a far tighter filter than the prefix-only "contains foo".
///
/// The extractor often gives different shapes from each direction:
/// `r"(a|b|c)(x|y|z)[A-Z]+foo"` yields inexact 2-byte prefix
/// alternatives (BAIL on <3 bytes) but a clean "foo" suffix —
/// pre-2026-05-16 we'd fall back to full scan; now suffix narrowing
/// kicks in.
fn grep_candidates_for_regex(
    r: &StoreReader,
    pattern: &str,
) -> Option<std::collections::HashSet<u32>> {
    use regex_syntax::hir::literal::ExtractKind;
    let prefix_cands = regex_literals_kind(pattern, ExtractKind::Prefix)
        .and_then(|lits| candidates_for_literals(r, &lits));
    let suffix_cands = regex_literals_kind(pattern, ExtractKind::Suffix)
        .and_then(|lits| candidates_for_literals(r, &lits));
    match (prefix_cands, suffix_cands) {
        (Some(p), Some(s)) => {
            // AND-intersect smaller-into-larger to minimize hash lookups.
            let (mut keep, drop_) = if p.len() <= s.len() { (p, s) } else { (s, p) };
            keep.retain(|f| drop_.contains(f));
            Some(keep)
        }
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
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
        // Truncate at a char boundary — naive `&snippet[..200]` panics
        // when the 200th byte falls mid-codepoint (real-world hit on
        // UTF-8 source files with non-ASCII identifiers or comments).
        let cut = (0..=200).rev().find(|i| snippet.is_char_boundary(*i)).unwrap_or(0);
        format!("{}…", &snippet[..cut])
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
    let mut out: Vec<RefRecord> = refs.into_iter()
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

/// One parsed entry from the `~/.scry/queries.log` ops log. The fields
/// mirror what `log_query_with_files` writes — keep in sync.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct RecallEntry {
    ts: u64,
    cmd: String,
    query: String,
    hits: u64,
    shown: u64,
    files_total: u64,
    #[serde(default)]
    candidate_files: Option<u64>,
    elapsed_ms: u64,
    index: String,
}

/// Read and parse an ops log. Returns entries in *file order* (oldest
/// first); callers reverse + take(last) to get the most-recent window.
/// Malformed lines are silently skipped so a partial-write at the tail
/// (the indexer was SIGKILL'd mid-line) doesn't break recall.
fn parse_recall_log<R: std::io::BufRead>(rd: R) -> Vec<RecallEntry> {
    let mut out = Vec::new();
    for line in rd.lines().map_while(|r| r.ok()) {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Ok(e) = serde_json::from_str::<RecallEntry>(line) {
            out.push(e);
        }
    }
    out
}

/// Human-friendly relative-time formatter. Returns strings like
/// "3m ago" / "1h ago" / "2d ago" / "now". Output is at most a few
/// characters so it stays terminal-friendly in a column.
fn format_relative_time(now_ts: u64, then_ts: u64) -> String {
    if then_ts > now_ts {
        // Clock-skew or future timestamp; just say "now".
        return "now".to_string();
    }
    let delta = now_ts - then_ts;
    match delta {
        0..=4          => "now".to_string(),
        5..=59         => format!("{}s ago", delta),
        60..=3599      => format!("{}m ago", delta / 60),
        3600..=86_399  => format!("{}h ago", delta / 3600),
        _              => format!("{}d ago", delta / 86_400),
    }
}

/// `scry recall` — replay the recent ops log. Filters by --cmd and
/// --grep; optionally dedupes consecutive (cmd, query) repeats so
/// "ran the same def 50 times in this session" collapses to one line.
fn cmd_recall(
    last: usize,
    cmd: Option<String>,
    grep: Option<String>,
    log: Option<PathBuf>,
    dedup: bool,
    json: bool,
) -> Result<()> {
    let path = log.or_else(query_log_path).ok_or_else(|| {
        anyhow::anyhow!("no ops log path (set SCRY_LOG or $HOME, or pass --log)")
    })?;
    let f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            // Empty log is not an error — the agent might just be in
            // a fresh session. Print an empty result and exit 0.
            if e.kind() == std::io::ErrorKind::NotFound {
                if !json {
                    eprintln!("(no ops log at {} — no queries yet)", path.display());
                }
                return Ok(());
            }
            return Err(anyhow::anyhow!("open {}: {e}", path.display()));
        }
    };
    let entries = parse_recall_log(std::io::BufReader::new(f));
    let total = entries.len();

    // Apply filters (cmd, grep), then dedup, then take the last `last`
    // entries in *reverse* (newest first).
    let filtered: Vec<RecallEntry> = entries.into_iter()
        .filter(|e| cmd.as_deref().map(|c| e.cmd == c).unwrap_or(true))
        .filter(|e| grep.as_deref().map(|g| e.query.contains(g)).unwrap_or(true))
        .collect();
    let mut deduped: Vec<RecallEntry> = if dedup {
        let mut out: Vec<RecallEntry> = Vec::with_capacity(filtered.len());
        for e in filtered {
            match out.last() {
                Some(prev) if prev.cmd == e.cmd && prev.query == e.query => {
                    // Same key as the previous entry — overwrite so the
                    // *latest* timestamp/hits/elapsed wins.
                    *out.last_mut().unwrap() = e;
                }
                _ => out.push(e),
            }
        }
        out
    } else {
        filtered
    };
    // Newest first.
    deduped.reverse();
    deduped.truncate(last);

    let now = now_unix_secs();
    if json {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        use std::io::Write;
        for e in &deduped {
            writeln!(out, "{}", serde_json::to_string(e)?)?;
        }
    } else {
        if deduped.is_empty() {
            println!("(no matching queries — log has {total} entries total)");
            return Ok(());
        }
        println!("recent queries (last {} of {total} total):", deduped.len());
        for e in &deduped {
            let cand = e.candidate_files
                .map(|c| format!(" ({c} cand)"))
                .unwrap_or_default();
            println!(
                "  {:9}  {:<8}  {:<40}  {} hits in {}ms{}",
                format_relative_time(now, e.ts),
                e.cmd,
                truncate_query_for_display(&e.query, 40),
                e.hits,
                e.elapsed_ms,
                cand,
            );
        }
    }
    Ok(())
}

/// Truncate a query string for the recall display column. Long regex
/// or path patterns get a `…` suffix so the layout doesn't break.
fn truncate_query_for_display(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Entry point for the `mcp` subcommand. MCP — Model Context Protocol
/// — is a stdio JSON-RPC 2.0 protocol used by Claude Desktop, Cursor,
/// and other agent runtimes to call out to external tools. scry's MCP
/// surface exposes one tool per existing `serve` command.
///
/// Wire shape (one line per message):
///   {"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}
///   {"jsonrpc":"2.0","id":2,"method":"tools/list"}
///   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"def","arguments":{...}}}
///
/// Notifications (no `id`) are consumed silently as the spec requires.
fn cmd_mcp(index: Option<PathBuf>) -> Result<()> {
    use std::io::{BufRead, Write};
    let reader = open_index(index)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let in_lock = stdin.lock();
    for line in in_lock.lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        let line = line.trim();
        if line.is_empty() { continue; }
        let req: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")},
                });
                writeln!(out, "{}", resp)?;
                out.flush()?;
                continue;
            }
        };
        // Notifications carry no `id` per JSON-RPC 2.0; MCP uses them
        // for `notifications/initialized` etc. Acknowledge by doing
        // nothing — emitting a response would be a protocol violation.
        if req.get("id").is_none() {
            continue;
        }
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(serde_json::json!({}));
        let resp = mcp_dispatch(&reader, &id, method, &params);
        writeln!(out, "{}", resp)?;
        out.flush()?;
    }
    Ok(())
}

/// MCP method dispatcher. Pure function of reader + method + params;
/// returns the full JSON-RPC envelope (including `jsonrpc: "2.0"` and
/// the echoed `id`) ready to be written to the wire.
fn mcp_dispatch(
    reader: &StoreReader,
    id: &serde_json::Value,
    method: &str,
    params: &serde_json::Value,
) -> serde_json::Value {
    let result = match method {
        "initialize" => Ok(mcp_initialize_result()),
        "tools/list" => Ok(mcp_tools_list_result()),
        "tools/call" => mcp_tools_call(reader, params),
        // ping is part of the spec for liveness checks.
        "ping" => Ok(serde_json::json!({})),
        // Anything else is unknown.
        other => Err(format!("method not found: {other}")),
    };
    match result {
        Ok(v) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err(msg) => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": msg},
        }),
    }
}

/// Reply to MCP `initialize`. Reports our protocol version and the
/// `tools` capability — we don't (yet) implement prompts, resources,
/// or sampling, so we don't advertise them.
fn mcp_initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": { "listChanged": false },
        },
        "serverInfo": {
            "name": "scry",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Reply to MCP `tools/list`. One entry per scry command. The
/// `inputSchema` is a small JSON Schema describing the args each
/// tool accepts; MCP clients use it to validate arguments before
/// calling and to render UI hints. We keep the schemas tight but
/// not exhaustive — the agent's prompt teaches the semantic flags,
/// the schema teaches the shape.
fn mcp_tools_list_result() -> serde_json::Value {
    fn tool(name: &str, desc: &str, schema: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "description": desc,
            "inputSchema": schema,
        })
    }
    fn obj(req: &[&str], props: serde_json::Value) -> serde_json::Value {
        let req_arr: Vec<_> = req.iter().map(|s| serde_json::json!(*s)).collect();
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": req_arr,
        })
    }
    // Shared property fragments.
    let name_prop = serde_json::json!({"name": {"type": "string"}});
    let lang_prop = serde_json::json!({"type": "string", "description": "java|kotlin|cpp|rust|go|python|soong|aidl|..."});
    let in_prop = serde_json::json!({"type": "string", "description": "path substring filter (e.g. frameworks/base/)"});
    let limit_prop = serde_json::json!({"type": "integer", "default": 20});

    let tools = vec![
        tool("def", "Find exact-name symbol definitions.", obj(&["name"], serde_json::json!({
            "name": {"type": "string"},
            "lang": lang_prop,
            "kind": {"type": "string", "description": "class|method|fn|aidl.iface|soong|init.svc|sepolicy|..."},
            "in":   in_prop,
            "limit": limit_prop,
        }))),
        tool("ref", "Find references to a name.", obj(&["name"], serde_json::json!({
            "name": {"type": "string"},
            "lang": lang_prop,
            "kind": {"type": "string", "description": "call|ctor|inherit|import|..."},
            "in":   in_prop,
            "limit": limit_prop,
        }))),
        tool("callers", "Find call sites (references with kind=call).", obj(&["name"], serde_json::json!({
            "name": {"type": "string"},
            "lang": lang_prop,
            "in":   in_prop,
            "limit": limit_prop,
        }))),
        tool("prefix", "Symbols whose name starts with PREFIX.", obj(&["prefix"], serde_json::json!({
            "prefix": {"type": "string"},
            "in":    in_prop,
            "limit": limit_prop,
        }))),
        tool("fuzzy", "Symbols whose name contains SUBSTR.", obj(&["substr"], serde_json::json!({
            "substr": {"type": "string"},
            "in":    in_prop,
            "limit": limit_prop,
        }))),
        tool("grep", "Content search; literal pattern unless --regex is set on the request (default literal).", obj(&["pattern"], serde_json::json!({
            "pattern": {"type": "string"},
            "lang":    lang_prop,
            "in":      in_prop,
            "limit":   limit_prop,
        }))),
        tool("outline", "All symbols in a file, ordered by line.", obj(&["path"], serde_json::json!({
            "path":  {"type": "string", "description": "full or suffix-style path (e.g. app_main.cpp)"},
            "limit": limit_prop,
        }))),
        tool("coverage", "Subtree stats: files / bytes / symbols per language.", obj(&["path"], serde_json::json!({
            "path":    {"type": "string", "description": "path prefix to scope (empty = whole index)"},
            "by_kind": {"type": "boolean", "default": false},
        }))),
        tool("stats", "Index metadata (size, files, freshness).", serde_json::json!({
            "type": "object", "properties": serde_json::json!({}),
        })),
    ];
    // Silence the unused name_prop warning — it's kept for symmetry
    // with the other shared property fragments above and may be
    // referenced again as more tools land.
    let _ = name_prop;
    serde_json::json!({ "tools": tools })
}

/// Dispatch an MCP `tools/call`. Translates the MCP-shaped request
/// into a `serve_one_request`-shaped one and wraps the JSON result
/// in MCP's `{content: [{type: "text", text: ...}]}` envelope.
///
/// We deliberately reuse the serve_one_request code path rather than
/// re-implementing the tool bodies: any future change to the serve
/// commands (new arg names, ranking tweaks, schema changes) is picked
/// up automatically by the MCP surface.
fn mcp_tools_call(reader: &StoreReader, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    let name = params.get("name").and_then(|v| v.as_str()).ok_or_else(|| "missing 'name'".to_string())?;
    let arguments = params.get("arguments").cloned().unwrap_or(serde_json::json!({}));
    // Build a serve_one_request-shaped envelope and route it through.
    let req = serde_json::json!({
        "id": 1, "cmd": name, "args": arguments,
    });
    let line = req.to_string();
    let mut buf: Vec<u8> = Vec::new();
    serve_one_request(reader, &line, &mut buf)
        .map_err(|e| format!("serve error: {e:#}"))?;
    let resp_line = String::from_utf8(buf).map_err(|e| format!("utf8 error: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(resp_line.trim())
        .map_err(|e| format!("response parse error: {e}"))?;
    if let Some(err) = resp.get("error") {
        return Err(err.to_string());
    }
    // MCP requires content[] of typed parts. We use a single text
    // part holding the pretty-printed JSON; clients are responsible
    // for parsing it back if they want structure. (MCP doesn't yet
    // have a standard "json content type"; text is the lowest common
    // denominator.)
    let result = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);
    let text = serde_json::to_string(&result).map_err(|e| format!("encode: {e}"))?;
    Ok(serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
}

/// Entry point for the `serve` subcommand. Dispatches to the requested
/// transport — stdin/stdout (default) or a bound listener (unix / tcp).
/// The shared `StoreReader` lives for the whole process and is borrowed
/// by every connection; mmap-backed and immutable, so no synchronization
/// is needed across concurrent clients.
fn cmd_serve(index: Option<PathBuf>, listen: Option<String>) -> Result<()> {
    let reader = open_index(index)?;
    match listen.as_deref() {
        None => serve_stdio(&reader),
        Some(spec) => serve_listener(&reader, spec),
    }
}

/// Stdin/stdout transport: one-shot agent loops, ad-hoc CLI experiments.
/// Reads requests one line at a time; writes responses through the
/// shared `serve_one_request` writer path (may be one line or many,
/// depending on whether the request set `stream: true`).
fn serve_stdio(reader: &StoreReader) -> Result<()> {
    use std::io::{BufRead, Write};
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
        serve_one_request(reader, line, &mut out)?;
        out.flush()?;
    }
    Ok(())
}

/// Bound-listener transport: keep the warm StoreReader resident across
/// many client connections. Spec syntax:
///   `unix:/path/to/socket`     — Unix domain socket
///   `tcp:HOST:PORT`            — TCP socket
///
/// Each accepted connection runs the same per-line request loop as
/// stdio mode, on its own OS thread. The `StoreReader` is `Sync` (only
/// mmaps + immutable Vecs) so concurrent reads need no synchronization.
///
/// On Unix-socket mode, the socket file is unlinked before bind (handles
/// stale sockets from a crashed prior run) and cleaned up on the most
/// common exit paths. SIGINT/SIGKILL still leave it behind; the next
/// start will reclaim it.
fn serve_listener(reader: &StoreReader, spec: &str) -> Result<()> {
    use std::sync::Arc;
    use std::thread;
    let reader = Arc::new(reader_clone_for_share(reader)?);
    match spec.split_once(':') {
        Some(("unix", path)) => {
            use std::os::unix::net::UnixListener;
            // Best-effort cleanup of a stale socket from a prior crashed
            // run. If the file isn't actually a socket we'll fail to bind
            // below with a clear error — safer than silently overwriting.
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)
                .with_context(|| format!("bind unix:{path}"))?;
            eprintln!("[scry serve] listening on unix:{path}");
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[scry serve] accept: {e}"); continue; }
                };
                let r = Arc::clone(&reader);
                thread::spawn(move || {
                    if let Err(e) = serve_connection(&r, &stream, &stream) {
                        eprintln!("[scry serve] connection: {e:#}");
                    }
                });
            }
            Ok(())
        }
        Some(("tcp", addr)) => {
            use std::net::TcpListener;
            let listener = TcpListener::bind(addr)
                .with_context(|| format!("bind tcp:{addr}"))?;
            eprintln!("[scry serve] listening on tcp:{addr}");
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[scry serve] accept: {e}"); continue; }
                };
                let r = Arc::clone(&reader);
                thread::spawn(move || {
                    let read = match stream.try_clone() {
                        Ok(s) => s,
                        Err(e) => { eprintln!("[scry serve] dup: {e}"); return; }
                    };
                    if let Err(e) = serve_connection(&r, &read, &stream) {
                        eprintln!("[scry serve] connection: {e:#}");
                    }
                });
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "invalid --listen spec '{spec}': expected unix:PATH or tcp:HOST:PORT"
        ),
    }
}

/// One client connection's read/write loop. Lifted from `serve_stdio` so
/// both transports share the request-handling code path verbatim.
fn serve_connection<R: std::io::Read, W: std::io::Write>(
    reader: &StoreReader,
    rd: R,
    mut wr: W,
) -> Result<()> {
    use std::io::{BufRead, BufReader};
    let buf = BufReader::new(rd);
    for line in buf.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() { continue; }
        serve_one_request(reader, line, &mut wr)?;
        wr.flush()?;
    }
    Ok(())
}

/// Open the index a *second* time for sharing across listener threads.
/// `StoreReader` is `Sync`, so in principle we could `Arc<StoreReader>`
/// the original directly — but `open_index` ergonomically returns
/// `StoreReader` (not `Arc<StoreReader>`), and we want every code path
/// that holds a reference to be obvious in its lifetime. Cheap: this is
/// just re-mmapping the same files (~ms, no IO).
fn reader_clone_for_share(r: &StoreReader) -> Result<StoreReader> {
    StoreReader::open(&r.paths.root)
        .with_context(|| format!("re-open index at {} for listener mode", r.paths.root.display()))
}

/// Handle a single newline-delimited JSON-RPC request by writing its
/// response(s) to `wr`. Shared by all transports — stdio, unix socket,
/// tcp socket, MCP. The shape of what gets written depends on the
/// request:
///
/// - **Default (non-streaming)**: one JSON line of the form
///   `{"id":N,"result":VALUE}`. If `budget: BYTES` is set on the
///   request and the serialized response exceeds it, fields are
///   stripped progressively (snippet → scope → fqn) and finally the
///   result array is truncated. A `truncated` field is added to
///   record what was dropped.
/// - **Streaming (`"stream": true`)**: one JSON line per hit of the
///   form `{"id":N,"hit":VALUE}`, then a closing
///   `{"id":N,"done":true,"shown":K}` envelope. The hit shape is the
///   same as a single element of the non-streaming result array. For
///   commands whose result is a scalar/object (outline, coverage,
///   stats), streaming has no effect — they emit one regular response.
///
/// Returns IO errors from `wr` so a broken pipe propagates cleanly.
fn serve_one_request<W: std::io::Write>(
    reader: &StoreReader,
    line: &str,
    wr: &mut W,
) -> Result<()> {
    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            let resp = serde_json::json!({"error": format!("bad json: {e}")});
            writeln!(wr, "{}", resp)?;
            return Ok(());
        }
    };
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
    let args = req.get("args").cloned().unwrap_or(serde_json::json!({}));
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let lang = args.get("lang").and_then(|v| v.as_str());
    let kind = args.get("kind").and_then(|v| v.as_str());
    let in_ = args.get("in").and_then(|v| v.as_str());
    let stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let budget = req.get("budget").and_then(|v| v.as_u64()).map(|n| n as usize);

    // Per-command primary arg name. Each command also accepts "name"
    // as a fallback so existing callers don't break, but the
    // semantic field is what the docs and examples use:
    //   def/ref/callers      → "name"
    //   prefix               → "prefix"
    //   fuzzy                → "substr"
    //   grep                 → "pattern"
    //   outline              → "path"
    let arg_str = |primary: &str| -> &str {
        args.get(primary).and_then(|v| v.as_str())
            .or_else(|| args.get("name").and_then(|v| v.as_str()))
            .unwrap_or("")
    };

    let mut result = match cmd {
        "def"     => serve_def(reader, arg_str("name"), lang, kind, in_, limit),
        "prefix"  => serve_prefix(reader, arg_str("prefix"), in_, limit),
        "fuzzy"   => serve_fuzzy(reader, arg_str("substr"), in_, limit),
        "ref"     => serve_ref(reader, arg_str("name"), lang, kind, in_, limit),
        "callers" => serve_ref(reader, arg_str("name"), lang, Some("call"), in_, limit),
        "grep"    => serve_grep(reader, arg_str("pattern"), lang, in_, limit),
        "outline" => serve_outline(reader, arg_str("path"), limit),
        "coverage" => serve_coverage(reader, arg_str("path"),
            args.get("by_kind").and_then(|v| v.as_bool()).unwrap_or(false)),
        "stats"   => serve_stats(reader),
        other     => {
            let resp = serde_json::json!({
                "id": id, "error": format!("unknown cmd: {other}"),
            });
            writeln!(wr, "{}", resp)?;
            return Ok(());
        }
    };

    // Streaming path: only meaningful when the result is an array
    // (the multi-hit commands: def, prefix, fuzzy, ref, callers, grep).
    // For scalar/object results (outline, coverage, stats), streaming
    // degrades to a single regular response.
    if stream {
        if let serde_json::Value::Array(hits) = &mut result {
            let mut shown = 0usize;
            for hit in hits.drain(..) {
                let mut hit = hit;
                if let Some(b) = budget {
                    // Per-hit budget cap, applied to each hit's serialized
                    // size independently. We use a smaller per-hit budget
                    // (budget/shown_limit) to avoid one heavy hit
                    // monopolizing the bytes.
                    let per_hit = b.saturating_sub(64).max(128) / limit.max(1);
                    let _ = apply_budget_to_hit(&mut hit, per_hit);
                }
                let env = serde_json::json!({"id": id, "hit": hit});
                writeln!(wr, "{}", env)?;
                shown += 1;
            }
            let done = serde_json::json!({"id": id, "done": true, "shown": shown});
            writeln!(wr, "{}", done)?;
            return Ok(());
        }
    }

    // Non-streaming path: optionally apply the budget to the whole
    // response, then write one line.
    let truncated = budget
        .and_then(|b| apply_budget_to_response(&mut result, b));
    let mut envelope = serde_json::json!({"id": id, "result": result});
    if let Some(tag) = truncated {
        envelope.as_object_mut().unwrap()
            .insert("truncated".to_string(), serde_json::Value::String(tag.to_string()));
    }
    writeln!(wr, "{}", envelope)?;
    Ok(())
}

/// Strip a single response value's optional fields in priority order
/// until the serialized size fits within `budget` bytes. Returns the
/// name of the deepest trim applied (`"snippet"`, `"snippet+scope"`,
/// `"snippet+scope+fqn"`, `"snippet+scope+fqn+truncated"`) or `None`
/// if the original already fit.
///
/// Trimming order is *information-preserving first*: snippets are the
/// largest expendable field (kilobytes per hit), then scope_path
/// (helpful but reconstructible from path + line), then fqn (a
/// derivative of name + scope), and finally truncation of the result
/// array as the last resort. The caller is responsible for setting a
/// sensible `limit`; budget should not be the only cap.
fn apply_budget_to_response(value: &mut serde_json::Value, budget: usize) -> Option<&'static str> {
    if serialized_len(value) <= budget { return None; }
    strip_field_recursive(value, "snippet");
    if serialized_len(value) <= budget { return Some("snippet"); }
    strip_field_recursive(value, "scope");
    if serialized_len(value) <= budget { return Some("snippet+scope"); }
    strip_field_recursive(value, "fqn");
    if serialized_len(value) <= budget { return Some("snippet+scope+fqn"); }
    // Last resort: truncate the array (keeping highest-ranked hits at
    // the front, since serve_* functions already returned them sorted).
    truncate_array_to_budget(value, budget);
    Some("snippet+scope+fqn+truncated")
}

/// Per-hit budget trim used in streaming mode. Same priority order as
/// the non-streaming variant but operates on one hit at a time —
/// streaming has no way to retroactively trim already-emitted hits.
fn apply_budget_to_hit(hit: &mut serde_json::Value, budget: usize) -> Option<&'static str> {
    if serialized_len(hit) <= budget { return None; }
    strip_field_recursive(hit, "snippet");
    if serialized_len(hit) <= budget { return Some("snippet"); }
    strip_field_recursive(hit, "scope");
    if serialized_len(hit) <= budget { return Some("snippet+scope"); }
    strip_field_recursive(hit, "fqn");
    Some("snippet+scope+fqn")
}

/// Serialized length in bytes — used by the budget machinery to decide
/// when to stop trimming. Cheap: serde_json's writer is fast and we're
/// only calling this on intermediate sizes, not the response itself.
fn serialized_len(value: &serde_json::Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(usize::MAX)
}

/// Remove every occurrence of `field` from any object inside `value`,
/// walking the tree. Used by the budget code to drop snippets / scope
/// / fqn fields without disturbing the rest of the response shape.
fn strip_field_recursive(value: &mut serde_json::Value, field: &str) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove(field);
            for v in map.values_mut() { strip_field_recursive(v, field); }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() { strip_field_recursive(v, field); }
        }
        _ => {}
    }
}

/// Walk `value`'s top-level array (or `symbols` array inside an
/// outline-shaped object) and drop elements from the tail until the
/// serialized size fits within `budget`. No-op if `value` isn't an
/// array.
fn truncate_array_to_budget(value: &mut serde_json::Value, budget: usize) {
    let arr = match value {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(map) => {
            // outline returns {symbols: [...]}; reach inside.
            match map.get_mut("symbols") {
                Some(serde_json::Value::Array(a)) => a,
                _ => return,
            }
        }
        _ => return,
    };
    while !arr.is_empty() && serialized_len(&serde_json::Value::Array(arr.clone())) > budget {
        arr.pop();
    }
}

/// Does the symbol/ref live under the given subdir prefix?
///
/// `display_path` returns the full absolute path (root.path + relpath),
/// so a caller-supplied filter like "frameworks/base/" — a repo-root-
/// relative substring — must match via `contains`, not `starts_with`
/// (the path always starts with the root, never with the subdir).
/// This matches the semantics of CLI cmd_def/cmd_ref (lines 1178/1220).
fn file_in_prefix(r: &StoreReader, file_id: u32, prefix: &str) -> bool {
    if prefix.is_empty() { return true; }
    match r.files.get(file_id as usize) {
        Some(fe) => fe.display_path(&r.roots).contains(prefix),
        None => false,
    }
}

fn serve_def(
    r: &StoreReader,
    name: &str,
    lang: Option<&str>,
    kind: Option<&str>,
    in_: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let prefix = in_.unwrap_or("");
    let mut filtered: Vec<SymbolRecord> = r.lookup_exact(name).into_iter()
        .filter(|s| {
            if let Some(l) = lang {
                if !s.lang.as_str().eq_ignore_ascii_case(l) { return false; }
            }
            if let Some(k) = kind {
                if !s.kind.short().eq_ignore_ascii_case(k) { return false; }
            }
            file_in_prefix(r, s.file_id, prefix)
        })
        .collect();
    rank_symbols(&mut filtered, r);
    let out: Vec<_> = filtered.iter().take(limit).map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(out)
}

fn serve_prefix(r: &StoreReader, prefix: &str, in_: Option<&str>, limit: usize) -> serde_json::Value {
    let in_prefix = in_.unwrap_or("");
    // Over-fetch then rank+filter — the limit should land on the BEST
    // matches, not just the first ones the FST happens to return.
    let cap = limit.saturating_mul(8).max(limit);
    let mut filtered: Vec<SymbolRecord> = r.lookup_prefix(prefix, cap).into_iter()
        .filter(|s| file_in_prefix(r, s.file_id, in_prefix))
        .collect();
    rank_symbols(&mut filtered, r);
    let v: Vec<_> = filtered.iter().take(limit).map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(v)
}

fn serve_fuzzy(r: &StoreReader, substr: &str, in_: Option<&str>, limit: usize) -> serde_json::Value {
    let in_prefix = in_.unwrap_or("");
    let cap = limit.saturating_mul(8).max(limit);
    let mut filtered: Vec<SymbolRecord> = r.lookup_substring(substr, cap).into_iter()
        .filter(|s| file_in_prefix(r, s.file_id, in_prefix))
        .collect();
    rank_symbols(&mut filtered, r);
    let v: Vec<_> = filtered.iter().take(limit).map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(v)
}

fn serve_ref(
    r: &StoreReader,
    name: &str,
    lang: Option<&str>,
    kind: Option<&str>,
    in_: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let prefix = in_.unwrap_or("");
    let mut out = Vec::new();
    for rr in r.lookup_refs_exact(name).into_iter() {
        if out.len() >= limit { break; }
        if let Some(l) = lang {
            if !rr.lang.as_str().eq_ignore_ascii_case(l) { continue; }
        }
        if let Some(k) = kind {
            if !rr.kind.short().eq_ignore_ascii_case(k) { continue; }
        }
        if !file_in_prefix(r, rr.file_id, prefix) { continue; }
        out.push(ref_to_json(r, &rr));
    }
    serde_json::Value::Array(out)
}

/// Literal-only grep over the warm reader. Uses the trigram pre-filter
/// when present, then scans candidate files in-process. Single-threaded
/// (one RPC at a time); for ad-hoc parallel batch grep, use the CLI.
fn serve_grep(
    r: &StoreReader,
    pattern: &str,
    lang: Option<&str>,
    in_: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    if pattern.is_empty() {
        return serde_json::json!({"error": "empty pattern"});
    }
    let prefix = in_.unwrap_or("");
    let needle = pattern.as_bytes();
    let candidates: Option<std::collections::HashSet<u32>> = r.grep_candidates(needle);
    let mut out: Vec<serde_json::Value> = Vec::new();
    // Soft cap on files scanned even when trigram returns many — keeps a
    // bad query (e.g. "the") from blocking the serve loop for seconds.
    const MAX_FILES_SCANNED: usize = 5000;
    let mut scanned = 0usize;
    for fe in r.files.iter() {
        if out.len() >= limit { break; }
        if scanned >= MAX_FILES_SCANNED { break; }
        if let Some(ref tg) = candidates {
            if !tg.contains(&fe.id) { continue; }
        }
        if let Some(l) = lang {
            if !fe.kind.as_str().eq_ignore_ascii_case(l) { continue; }
        }
        if !prefix.is_empty() {
            // Substring match — same semantics as file_in_prefix and
            // CLI cmd_grep; absolute paths never start with a root-
            // relative subdir.
            let p = fe.display_path(&r.roots);
            if !p.contains(prefix) { continue; }
        }
        scanned += 1;
        let path = fe.display_path(&r.roots);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // memmem search through the file; cap matches per-file to avoid
        // pathological hits (e.g. every line) eating the limit.
        let mut start_at = 0usize;
        let mut per_file = 0usize;
        while let Some(off) = memchr::memmem::find(&bytes[start_at..], needle) {
            let abs = start_at + off;
            let (line, col, snippet) = locate_match(&bytes, abs, abs + needle.len());
            out.push(serde_json::json!({
                "path": path,
                "line": line,
                "col": col,
                "snippet": snippet,
                "lang": fe.kind.as_str(),
            }));
            per_file += 1;
            if out.len() >= limit || per_file >= 16 { break; }
            start_at = abs + needle.len().max(1);
            if start_at >= bytes.len() { break; }
        }
    }
    serde_json::Value::Array(out)
}

/// JSON-RPC outline: returns every symbol defined in the given file,
/// sorted by line. Accepts a full or suffix-style path arg under "path"
/// in the request. Single-threaded; one scan of the (lazy) symbol vec
/// per call. Limit 0 means "no cap" (whole file).
fn serve_outline(r: &StoreReader, path: &str, limit: usize) -> serde_json::Value {
    if path.is_empty() {
        return serde_json::json!({"error": "missing 'path' arg"});
    }
    let file_id = match resolve_file_id(r, path) {
        Some(id) => id,
        None => return serde_json::json!({"error": format!("no indexed file matches '{}'", path)}),
    };
    let fe = match r.files.get(file_id as usize) {
        Some(f) => f,
        None => return serde_json::json!({"error": "file_id out of range"}),
    };
    let mut found: Vec<SymbolRecord> = match r.symbols_for_file(file_id) {
        Some(ids) => {
            let mut v = Vec::with_capacity(ids.len());
            if let Some(lz) = r.lazy_symbols.as_ref() {
                for i in ids { if let Some(s) = lz.get(i as usize) { v.push(s); } }
            } else {
                for i in ids {
                    if let Some(s) = r.symbols.get(i as usize) { v.push(s.clone()); }
                }
            }
            v
        }
        None => r.iter_symbols().filter(|s| s.file_id == file_id).collect(),
    };
    found.sort_by(|a, b| (a.line, a.col, &a.name).cmp(&(b.line, b.col, &b.name)));
    let take = if limit == 0 { found.len() } else { limit.min(found.len()) };
    let arr: Vec<_> = found.iter().take(take).map(|s| symbol_to_json(r, s)).collect();
    serde_json::json!({
        "path": fe.display_path(&r.roots),
        "lang": fe.kind.as_str(),
        "symbols_total": found.len(),
        "symbols_shown": take,
        "symbols": arr,
    })
}

/// JSON-RPC coverage: subtree stats. Same shape as the CLI's
/// `scry coverage --json`. by_kind=true includes per-symbol-kind
/// counts inside each language; default false to keep responses
/// compact (typical agent use case is "what's in this dir" not
/// "how many ctors").
fn serve_coverage(r: &StoreReader, path: &str, by_kind: bool) -> serde_json::Value {
    use std::collections::HashMap;
    let matching: Vec<(u32, FileKind, u64)> = r.files.iter()
        .filter(|fe| path.is_empty() || fe.display_path(&r.roots).contains(path))
        .map(|fe| (fe.id, fe.kind, fe.size))
        .collect();
    struct LangBucket {
        files: u64, bytes: u64, symbols: u64,
        by_kind: HashMap<SymbolKind, u64>,
    }
    let mut by_lang: HashMap<FileKind, LangBucket> = HashMap::new();
    for (_id, kind, size) in &matching {
        let b = by_lang.entry(*kind).or_insert_with(|| LangBucket {
            files: 0, bytes: 0, symbols: 0, by_kind: HashMap::new(),
        });
        b.files += 1;
        b.bytes += *size;
    }
    // Symbol counts via the file_symbols sidecar fast path when present.
    let has_sidecar = matching.first()
        .map(|(id, _, _)| r.symbols_for_file(*id).is_some())
        .unwrap_or(false);
    if has_sidecar && (!by_kind || matching.len() <= 50_000) {
        for (id, kind, _) in &matching {
            let idxs = r.symbols_for_file(*id).unwrap_or_default();
            if !by_kind {
                if let Some(b) = by_lang.get_mut(kind) { b.symbols += idxs.len() as u64; }
            } else {
                for i in &idxs {
                    if let Some(s) = r.get_symbol(*i) {
                        let b = by_lang.entry(s.lang).or_insert_with(|| LangBucket {
                            files: 0, bytes: 0, symbols: 0, by_kind: HashMap::new(),
                        });
                        b.symbols += 1;
                        *b.by_kind.entry(s.kind).or_insert(0) += 1;
                    }
                }
            }
        }
    } else {
        let matching_set: std::collections::HashSet<u32> =
            matching.iter().map(|(id, _, _)| *id).collect();
        for s in r.iter_symbols() {
            if !matching_set.contains(&s.file_id) { continue; }
            let b = by_lang.entry(s.lang).or_insert_with(|| LangBucket {
                files: 0, bytes: 0, symbols: 0, by_kind: HashMap::new(),
            });
            b.symbols += 1;
            if by_kind { *b.by_kind.entry(s.kind).or_insert(0) += 1; }
        }
    }
    let by_lang_json: serde_json::Map<String, serde_json::Value> = by_lang.iter()
        .map(|(k, b)| {
            let mut o = serde_json::json!({
                "files": b.files, "bytes": b.bytes, "symbols": b.symbols,
            });
            if by_kind {
                let kinds: serde_json::Map<String, serde_json::Value> = b.by_kind.iter()
                    .map(|(sk, c)| (sk.short().to_string(), serde_json::json!(c)))
                    .collect();
                o["by_kind"] = serde_json::Value::Object(kinds);
            }
            (k.as_str().to_string(), o)
        })
        .collect();
    serde_json::json!({
        "path": path,
        "files_total": by_lang.values().map(|b| b.files).sum::<u64>(),
        "bytes_total": by_lang.values().map(|b| b.bytes).sum::<u64>(),
        "symbols_total": by_lang.values().map(|b| b.symbols).sum::<u64>(),
        "by_lang": by_lang_json,
    })
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
        "lang": s.lang.as_str(),
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
        "lang": rr.lang.as_str(),
        "path": path,
        "line": rr.line,
        "col": rr.col,
        "scope": rr.scope_path,
        // resolved_to is the Layer 2 sidecar override (or Layer 1
        // in-memory name match); 0 / null means we don't know the
        // exact definition this ref points to.
        "resolved_to": rr.resolved_to,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal pattern → identity extraction.
    #[test]
    fn regex_lit_pure_literal() {
        let lits = regex_literals_for_trigram("ZygoteInit").unwrap();
        assert_eq!(lits, vec![b"ZygoteInit".to_vec()]);
    }

    /// `foo|bar` → two alternatives that we union over.
    #[test]
    fn regex_lit_alternation() {
        let mut lits = regex_literals_for_trigram("foobar|bazqux").unwrap();
        lits.sort();
        assert_eq!(lits, vec![b"bazqux".to_vec(), b"foobar".to_vec()]);
    }

    /// Prefix + class — we should at least keep the prefix.
    #[test]
    fn regex_lit_prefix_then_class() {
        // \bTransaction\d+ has prefix literal "Transaction"
        let lits = regex_literals_for_trigram(r"Transaction\d+").unwrap();
        // The extractor may emit "Transaction" as a single inexact prefix,
        // or it may expand into a small set — accept either as long as
        // every entry STARTS WITH the literal prefix and is ≥3 bytes.
        assert!(!lits.is_empty(), "got: {:?}", lits);
        for l in &lits {
            assert!(l.starts_with(b"Transaction"), "literal {:?} doesn't start with Transaction", l);
            assert!(l.len() >= 3);
        }
    }

    /// Patterns with no extractable literal must bail (return None) so
    /// the caller knows to fall back rather than over-narrow.
    #[test]
    fn regex_lit_bails_on_short_literal() {
        // `a.*b` extracts "a" (1 byte) — too short to trigram.
        assert!(regex_literals_for_trigram("a.*b").is_none());
    }

    #[test]
    fn regex_lit_bails_on_unbounded() {
        // `.*` is unbounded — no literals.
        assert!(regex_literals_for_trigram(".*").is_none());
        assert!(regex_literals_for_trigram("[a-z]+").is_none());
    }

    #[test]
    fn regex_lit_bails_on_invalid() {
        // Unclosed group: parse fails → None.
        assert!(regex_literals_for_trigram("(foo").is_none());
    }

    /// Wide alternation must bail — we'd be UNIONing across too many
    /// posting lists for the pre-filter to actually narrow.
    #[test]
    fn regex_lit_bails_on_wide_alternation() {
        // Build a pattern with 65 alternatives of 3-byte literals.
        let alts: Vec<String> = (0..65)
            .map(|i| format!("a{:02x}", i))  // "a00", "a01", ..., "a40" — all 3 bytes
            .collect();
        let pat = alts.join("|");
        assert!(regex_literals_for_trigram(&pat).is_none(), "should bail at 65 alternatives");
    }

    /// Suffix extraction picks up trailing literals that prefix extraction
    /// can't reach — the canonical case is a pattern starting with a too-
    /// broad construct ("[A-Z]+") but ending in a fixed string. Pre-2026-
    /// 05-16 we'd have bailed on prefix and skipped pre-filtering entirely;
    /// the suffix path keeps narrowing.
    #[test]
    fn regex_lit_suffix_finds_trailing_literal() {
        use regex_syntax::hir::literal::ExtractKind;
        // [A-Z]+ prefix is unbounded but "ZygoteInit" is a clean suffix.
        let suf = regex_literals_kind(r"[A-Z]+ZygoteInit", ExtractKind::Suffix).unwrap();
        assert_eq!(suf.len(), 1);
        assert!(suf[0].ends_with(b"ZygoteInit"), "got {:?}", suf[0]);
        // Prefix should bail (literals would be "[A-Z]+" expansion).
        // The exact prefix output depends on regex-syntax internals; we
        // just assert the SUFFIX path finds something usable when prefix
        // does not.
    }

    /// Pure literal: prefix and suffix both extract the same thing.
    /// We don't assert equality (the extractor's internal encoding may
    /// differ) — we only assert both directions produce ≥1 literal so
    /// the AND-intersection in grep_candidates_for_regex still has both
    /// sides to constrain on.
    #[test]
    fn regex_lit_pure_literal_both_directions() {
        use regex_syntax::hir::literal::ExtractKind;
        let p = regex_literals_kind("ZygoteInit", ExtractKind::Prefix).unwrap();
        let s = regex_literals_kind("ZygoteInit", ExtractKind::Suffix).unwrap();
        assert!(!p.is_empty());
        assert!(!s.is_empty());
    }

    /// Prefix.*Suffix: both directions extract a useful literal. This is
    /// the case the AND-intersect was added for — file must contain
    /// BOTH "frameworks" AND "ActivityManager" to match, far tighter
    /// than either alone.
    #[test]
    fn regex_lit_prefix_dotstar_suffix_separates() {
        use regex_syntax::hir::literal::ExtractKind;
        let p = regex_literals_kind(r"frameworks.*ActivityManager", ExtractKind::Prefix).unwrap();
        let s = regex_literals_kind(r"frameworks.*ActivityManager", ExtractKind::Suffix).unwrap();
        // Prefix literal should contain "frameworks"; suffix should
        // contain "ActivityManager". The extractor may add trailing
        // bytes for inexactness, so we use contains, not equality.
        assert!(p.iter().any(|l| l.windows(10).any(|w| w == b"frameworks")),
                "prefix should mention 'frameworks', got {:?}", p);
        assert!(s.iter().any(|l| l.windows(15).any(|w| w == b"ActivityManager")),
                "suffix should mention 'ActivityManager', got {:?}", s);
    }

    // ---------------- Layer 2 resolution (resolve_one) ----------------
    //
    // resolve_one is the heart of the build-resolutions sidecar — it
    // picks the def-id for each ref using same-lang preference and
    // Java pkg/import narrowing. Until these tests it had zero
    // assertions; a refactor of the narrowing rules would have shipped
    // with no signal.

    use std::collections::HashMap;
    use scry_store::RefKind;

    fn mk_def(id: u64, lang: scry_walker::FileKind, pkg: Option<&str>) -> ResolveDef {
        ResolveDef { id, file_id: 0, lang, pkg: pkg.map(String::from) }
    }
    fn mk_ref(name: &str, lang: scry_walker::FileKind, file_id: u32) -> scry_store::RefRecord {
        scry_store::RefRecord {
            name: name.into(), kind: RefKind::Call, file_id,
            byte_start: 0, byte_end: 0, line: 1, col: 1,
            scope_path: vec![], lang, resolved_to: None,
        }
    }

    #[test]
    fn resolve_one_single_candidate_trivial() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Foo".into(), vec![mk_def(42, scry_walker::FileKind::Java, None)]);
        let r = mk_ref("Foo", scry_walker::FileKind::Java, 0);
        let mut n = 0u64;
        let chosen = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &mut n);
        assert_eq!(chosen, 42);
        assert_eq!(n, 0, "single-cand shortcut shouldn't count as a Java-narrowed win");
    }

    #[test]
    fn resolve_one_no_match_returns_zero() {
        let by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        let r = mk_ref("Foo", scry_walker::FileKind::Java, 0);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &mut n), 0);
    }

    #[test]
    fn resolve_one_same_lang_preference_wins_over_cross_lang() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Foo".into(), vec![
            mk_def(100, scry_walker::FileKind::Cpp, None),
            mk_def(200, scry_walker::FileKind::Java, None),
            mk_def(300, scry_walker::FileKind::Python, None),
        ]);
        let r = mk_ref("Foo", scry_walker::FileKind::Java, 0);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &mut n), 200);
    }

    /// Java narrowing: file's package matches ONE candidate's package →
    /// that candidate wins even when other same-lang Java candidates
    /// exist. Counts as a "narrowed" win.
    #[test]
    fn resolve_one_java_same_package_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Activity".into(), vec![
            mk_def(11, scry_walker::FileKind::Java, Some("com.other")),
            mk_def(22, scry_walker::FileKind::Java, Some("android.app")),
            mk_def(33, scry_walker::FileKind::Java, Some("com.third")),
        ]);
        let mut pkg = HashMap::new();
        pkg.insert(5u32, "android.app".to_string());
        let r = mk_ref("Activity", scry_walker::FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &pkg, &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1, "same-package narrowing should bump counter");
    }

    /// Java narrowing: no same-package match, but an explicit
    /// `import x.y.Foo;` resolves it.
    #[test]
    fn resolve_one_java_import_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Binder".into(), vec![
            mk_def(11, scry_walker::FileKind::Java, Some("com.other")),
            mk_def(22, scry_walker::FileKind::Java, Some("android.os")),
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![("Binder".to_string(), Some("android.os".to_string()))]);
        let r = mk_ref("Binder", scry_walker::FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &imports, &mut n), 22);
        assert_eq!(n, 1, "explicit-import narrowing should bump counter");
    }

    /// Wildcard `import x.y.*;` resolves any class in x.y.
    #[test]
    fn resolve_one_java_wildcard_import_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Binder".into(), vec![
            mk_def(11, scry_walker::FileKind::Java, Some("com.other")),
            mk_def(22, scry_walker::FileKind::Java, Some("android.os")),
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![("*".to_string(), Some("android.os".to_string()))]);
        let r = mk_ref("Binder", scry_walker::FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &imports, &mut n), 22);
        assert_eq!(n, 1);
    }

    /// java.lang fallback: name like "String" with no same-pkg or import
    /// match should land on the java.lang.String candidate.
    #[test]
    fn resolve_one_java_lang_fallback() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("String".into(), vec![
            mk_def(11, scry_walker::FileKind::Java, Some("com.other")),
            mk_def(22, scry_walker::FileKind::Java, Some("java.lang")),
        ]);
        let r = mk_ref("String", scry_walker::FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1, "java.lang fallback should bump counter");
    }

    /// When NO Java context narrows, falls back to the first same-lang
    /// candidate (Layer 1 behavior). Counter should NOT bump.
    #[test]
    fn resolve_one_java_no_narrowing_fallback() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Foo".into(), vec![
            mk_def(11, scry_walker::FileKind::Java, Some("com.other")),
            mk_def(22, scry_walker::FileKind::Java, Some("com.third")),
        ]);
        let r = mk_ref("Foo", scry_walker::FileKind::Java, 5);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &mut n);
        assert_eq!(got, 11, "should pick first same-lang candidate");
        assert_eq!(n, 0, "no narrowing happened");
    }

    // ------------------------------------------------------------------
    // Budget machinery for the streaming serve transport (Phase B).
    // ------------------------------------------------------------------

    /// strip_field_recursive must walk both arrays and nested objects;
    /// nothing else in the value should change.
    #[test]
    fn strip_field_recursive_walks_nested() {
        let mut v = serde_json::json!({
            "result": [
                {"name": "Foo", "snippet": "very long source bytes"},
                {"name": "Bar", "snippet": "more bytes", "inner": {"snippet": "also here"}},
            ],
        });
        strip_field_recursive(&mut v, "snippet");
        assert!(!serde_json::to_string(&v).unwrap().contains("snippet"),
                "snippet should be gone everywhere; got {v}");
        // Names preserved.
        assert!(serde_json::to_string(&v).unwrap().contains("Foo"));
    }

    /// Original-fits-in-budget should be a no-op (return None).
    #[test]
    fn budget_noop_when_fits() {
        let mut v = serde_json::json!([{"name": "Foo"}]);
        let r = apply_budget_to_response(&mut v, 1000);
        assert!(r.is_none(), "expected None, got {r:?}");
    }

    /// Snippet-only trim is enough → returns Some("snippet").
    #[test]
    fn budget_drops_snippets_first() {
        let big_snippet = "x".repeat(500);
        let mut v = serde_json::json!([
            {"name": "A", "snippet": big_snippet.clone()},
            {"name": "B", "snippet": big_snippet},
        ]);
        let r = apply_budget_to_response(&mut v, 200);
        assert_eq!(r, Some("snippet"));
        assert!(!serde_json::to_string(&v).unwrap().contains("xxxx"),
                "snippets must be gone");
        // The names survive.
        assert!(serde_json::to_string(&v).unwrap().contains("\"A\""));
        assert!(serde_json::to_string(&v).unwrap().contains("\"B\""));
    }

    /// Snippet + scope trim required → returns Some("snippet+scope").
    #[test]
    fn budget_drops_scope_after_snippet() {
        let big = "x".repeat(100);
        let scope = vec!["a", "b", "c", "d", "e", "f"];
        let mut v = serde_json::json!([
            {"name": "A", "snippet": big.clone(), "scope": scope},
            {"name": "B", "snippet": big.clone(), "scope": ["a","b","c","d","e","f"]},
            {"name": "C", "snippet": big, "scope": ["a","b","c","d","e","f"]},
        ]);
        let r = apply_budget_to_response(&mut v, 80);
        // Either snippet+scope or further; just verify scope is gone.
        assert!(r.is_some());
        assert!(!serde_json::to_string(&v).unwrap().contains("scope"),
                "scope must be gone; got {v}");
    }

    /// Truncation as last resort: even with snippet+scope+fqn dropped,
    /// the array is still too big, so we drop elements from the tail.
    #[test]
    fn budget_truncates_array_as_last_resort() {
        // 50 minimal hits at ~30 bytes each → ~1500 bytes serialized.
        let hits: Vec<serde_json::Value> = (0..50)
            .map(|i| serde_json::json!({"name": format!("name_{i}_padding_padding")}))
            .collect();
        let mut v = serde_json::Value::Array(hits);
        let r = apply_budget_to_response(&mut v, 200);
        assert_eq!(r, Some("snippet+scope+fqn+truncated"));
        let final_len = v.as_array().unwrap().len();
        assert!(final_len < 50, "expected truncation, got {final_len}/50 still present");
        assert!(final_len > 0, "shouldn't truncate to empty if budget permits some");
    }

    /// truncate_array_to_budget should also descend into the
    /// outline-shape {"symbols": [...]} envelope.
    #[test]
    fn truncate_array_descends_into_outline_envelope() {
        let hits: Vec<serde_json::Value> = (0..20)
            .map(|i| serde_json::json!({"name": format!("sym_{i}")}))
            .collect();
        let mut v = serde_json::json!({
            "path": "/some/file.java",
            "symbols": hits,
        });
        truncate_array_to_budget(&mut v, 100);
        let n = v["symbols"].as_array().unwrap().len();
        assert!(n < 20, "expected outline symbols truncated, got {n}/20");
    }

    /// Hit-level trim is the streaming-mode variant: snippet → scope →
    /// fqn but no truncation (you can't un-emit lines).
    #[test]
    fn hit_budget_strips_in_order() {
        let mut hit = serde_json::json!({
            "name": "Foo",
            "snippet": "x".repeat(300),
            "scope": ["a","b","c"],
            "fqn": "com.android.foo.Foo",
        });
        let r = apply_budget_to_hit(&mut hit, 50);
        assert!(r.is_some());
        // The bare "name" field always survives.
        assert_eq!(hit["name"], "Foo");
    }

    // ------------------------------------------------------------------
    // Recall: ops-log parser + relative-time formatter.
    // ------------------------------------------------------------------

    /// Parser tolerates a partial-write at the tail (incomplete final
    /// line from a SIGKILL'd writer) and silently drops it instead of
    /// erroring out — recall should always return something useful.
    #[test]
    fn parse_recall_log_skips_malformed_tail() {
        let buf = b"{\"ts\":1,\"cmd\":\"def\",\"query\":\"Foo\",\"hits\":1,\"shown\":1,\"files_total\":100,\"candidate_files\":null,\"elapsed_ms\":10,\"index\":\"/i\"}\n{partial-write-no-newline";
        let entries = parse_recall_log(&buf[..]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cmd, "def");
        assert_eq!(entries[0].query, "Foo");
    }

    /// Multi-entry log: all entries parse, order preserved.
    #[test]
    fn parse_recall_log_multi_entry() {
        let buf = b"{\"ts\":1,\"cmd\":\"def\",\"query\":\"Foo\",\"hits\":1,\"shown\":1,\"files_total\":1,\"candidate_files\":null,\"elapsed_ms\":5,\"index\":\"/i\"}\n\
                    {\"ts\":2,\"cmd\":\"grep\",\"query\":\"bar\",\"hits\":7,\"shown\":7,\"files_total\":1,\"candidate_files\":15,\"elapsed_ms\":42,\"index\":\"/i\"}\n";
        let entries = parse_recall_log(&buf[..]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].ts, 1);
        assert_eq!(entries[1].cmd, "grep");
        assert_eq!(entries[1].candidate_files, Some(15));
    }

    /// candidate_files is optional in the log shape; missing or null
    /// should both parse cleanly as None.
    #[test]
    fn parse_recall_log_optional_candidate_files() {
        let buf = b"{\"ts\":1,\"cmd\":\"def\",\"query\":\"X\",\"hits\":1,\"shown\":1,\"files_total\":1,\"elapsed_ms\":5,\"index\":\"/i\"}\n";
        let entries = parse_recall_log(&buf[..]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].candidate_files, None);
    }

    /// Relative-time covers the boundaries: 0 (now), seconds, minutes,
    /// hours, days. The clock-skew case (then > now) collapses to
    /// "now" rather than producing a negative string.
    #[test]
    fn format_relative_time_buckets() {
        let now: u64 = 1_000_000;
        assert_eq!(format_relative_time(now, now), "now");
        assert_eq!(format_relative_time(now, now - 2), "now");        // ≤4 s
        assert_eq!(format_relative_time(now, now - 10), "10s ago");
        assert_eq!(format_relative_time(now, now - 120), "2m ago");   // 120 s
        assert_eq!(format_relative_time(now, now - 3600 * 2), "2h ago");
        assert_eq!(format_relative_time(now, now - 86_400 * 3), "3d ago");
        // Future timestamp (clock skew) collapses to "now".
        assert_eq!(format_relative_time(now, now + 1000), "now");
    }

    /// Display truncation never panics on multi-byte UTF-8 (the unicode
    /// `…` ellipsis must land on a char boundary).
    #[test]
    fn truncate_query_for_display_handles_utf8() {
        let q = "αβγδεζηθικλμνξοπρστυφχψω"; // 24 chars, 48 bytes
        let out = truncate_query_for_display(q, 10);
        assert!(out.ends_with('…'));
        // 9 chars + the ellipsis = 10 visible.
        assert_eq!(out.chars().count(), 10);
    }
}
