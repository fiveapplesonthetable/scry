//! scry: semantic code search and cross-reference engine for AOSP and Linux.

#![forbid(unsafe_code)]

mod bridge_subcmds;
mod build_adapter;
mod clangd;
mod finalize;
mod health;
mod precision_subcmds;

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
        /// Unset (the default) auto-scales: when `--mem-cap N` is set,
        /// target ~N × 16 KiB capped at 4 MiB (so a 100 GiB cap gets a
        /// 1.6 MiB threshold — more files run parallel, peak transient
        /// stays well under cap even under adversarial allocation
        /// ratios); otherwise 64 KiB (the conservative default that
        /// catches Vec-init-list-explosion regressions).
        #[arg(long)]
        big_file_bytes: Option<u64>,
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
        /// flushing, with proxy-for-memory semantics). Unset (the default)
        /// auto-scales: with `--mem-cap N` we raise the cap proportional to
        /// the cap so the bytes-target flush_bytes can actually be reached
        /// — otherwise the file-count cap fires first and the bytes target
        /// is dead weight (e.g. 50k × 8 KB ≈ 400 MB while flush_bytes=25 GiB).
        /// Formula: N × 50000 files per GiB of mem_cap, capped at 5 M files.
        /// Without `--mem-cap`, defaults to 50 000.
        #[arg(long)]
        flush_every: Option<usize>,
        /// Target in-RAM record bytes per batch (MiB). The batch size adapts
        /// every iteration from a rolling avg of bytes/file so accumulated
        /// records stay close to this target. Bounded above by --flush-every.
        /// 0 = disabled (fall back to file-count). Unset (the default)
        /// auto-scales: when `--mem-cap` is set, target ≈ 25 % of cap so
        /// finalize merges fewer chunks on big-memory hosts; otherwise
        /// 1024 MiB. Tune down on memory-constrained hosts; tune up
        /// when you have headroom you want spent on bigger batches.
        #[arg(long)]
        flush_bytes: Option<u32>,
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
        /// Incremental mode: open the existing index at --out, diff
        /// the current source-tree state against the stored
        /// file_digests, parse ONLY the changed + added files, and
        /// replay the unchanged files' records from the old index.
        /// Atomic: writes to a staging dir and renames into place; the
        /// old index stays queryable for the duration. Falls back to
        /// a full index if --out has no existing manifest or no
        /// file_digests.bin (run `scry build-digests` first).
        ///
        /// Skips the trigram index unless --build-trigrams is also
        /// passed (trigrams are re-extracted from disk for unchanged
        /// files, adding ~25 s to the full corpus).
        #[arg(long)]
        incremental: bool,
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
        /// Drop refs whose file path contains SUBSTR. Symmetric to
        /// `--in`; useful for "show me refs but not in tests" type
        /// queries, e.g. `scry ref Activity --not-in /tests/`.
        /// Applied after `--in`, so both can be combined.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Compact output. `count` emits just `N refs` — cheapest
        /// possible "how many references does X have?" reply.
        /// Mutually exclusive with --json.
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
        /// Use lexical (tree-sitter) name match only — skip the
        /// compiler-backed precision filters that auto-engage when
        /// the clang USR / SCIP symbol sidecars are present.
        /// Default behavior: precision filters are ON whenever
        /// their sidecar exists, so you get Kythe-class structured
        /// narrowing for free wherever the build produced an
        /// indexer artifact (`compile_commands.json` / `*.scip`),
        /// and graceful fallback to lexical-only on uncovered code.
        #[arg(long)]
        lexical: bool,
        // Below are the individual precision-filter knobs from
        // v0.1.12–v0.1.16. They still work but are hidden from
        // --help since the single `--lexical` flag covers the
        // 95 % case; users who want fine-grained control can still
        // pass them explicitly.
        #[arg(long, hide = true)]
        reachable: bool,
        #[arg(long, hide = true)]
        clang_precise: bool,
        #[arg(long, hide = true)]
        scip_precise: bool,
        /// Keep only refs whose enclosing scope_path contains
        /// SCOPE as an exact segment. Example:
        /// `--scope BroadcastQueueImpl` drops every ref outside
        /// that class. Cheap exact match; for partial / fuzzy
        /// class matching use `--in` on the file path instead.
        #[arg(long, value_name = "CLASS")]
        scope: Option<String>,
        /// Narrow which definition of NAME the refs must point at,
        /// by path substring of the def's file. Useful for
        /// overloaded names like `close()` where the index has
        /// many distinct defs across the corpus:
        ///   `scry ref close --def-in PerfettoTrace.java`
        /// keeps only refs whose Layer 2 resolution (resolved_to)
        /// points at a def in PerfettoTrace.java. Refs that
        /// couldn't be resolved (resolved_to=None) pass through —
        /// we'd rather over-include than silently drop the
        /// unresolved ones. Build the resolutions sidecar via
        /// `scry build-resolutions` (or `scry finalize`) for the
        /// strongest narrowing.
        #[arg(long, value_name = "PATH")]
        def_in: Option<String>,
        /// Strict mode: drop refs whose Layer 2 resolution didn't
        /// land on a specific def (resolved_to=None). With --def-in
        /// PATH, also drops refs resolving to a def outside PATH —
        /// no permissive over-include. Useful when you want HIGH
        /// PRECISION at the cost of recall: only refs the resolver
        /// can confidently attribute survive. Without --def-in,
        /// just shows refs that resolved to ANY specific def.
        #[arg(long)]
        strict: bool,
    },
    /// Find callers of NAME (refs with kind=call). LSP analogue:
    /// callHierarchy/incomingCalls.
    ///
    /// With --precise: route to clangd via LSP for type-aware C++
    /// resolution. Closes the 10-20% accuracy gap on overloaded
    /// method names by asking the real compiler instead of relying
    /// on scry's heuristic name match. Requires `clangd` on PATH
    /// and a `compile_commands.json` somewhere in the file's
    /// ancestry; errors with an actionable message otherwise.
    Callers {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, short = 't')]
        lang: Option<String>,
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop callers whose file path contains SUBSTR. See `scry ref
        /// --not-in`.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Use clangd for precise (type-aware) reference resolution.
        /// C++ only today (clangd is C++-shaped). Falls back to the
        /// heuristic path with a clear error if clangd or
        /// compile_commands.json are missing.
        #[arg(long)]
        precise: bool,
        /// Use lexical (tree-sitter) name match only. See
        /// `scry ref --lexical` for the full explanation.
        #[arg(long)]
        lexical: bool,
        // Hidden back-compat: individual precision knobs.
        #[arg(long, hide = true)]
        reachable: bool,
        #[arg(long, hide = true)]
        clang_precise: bool,
        #[arg(long, hide = true)]
        scip_precise: bool,
        /// Keep only callers whose enclosing scope_path contains
        /// SCOPE as an exact segment. Big win on hub functions:
        /// `scry callers traceBegin --scope BroadcastQueueImpl`
        /// drops the 1400+ traceBegin sites outside that class.
        #[arg(long, value_name = "CLASS")]
        scope: Option<String>,
        /// Narrow which definition of NAME the callers must target,
        /// by path substring of the def's file. See `scry ref
        /// --def-in` for the full description.
        #[arg(long, value_name = "PATH")]
        def_in: Option<String>,
        /// Strict mode: drop callers whose Layer 2 resolution didn't
        /// land on a specific def. See `scry ref --strict`.
        #[arg(long)]
        strict: bool,
        /// Compact output. `count` emits just `N callers` — cheapest
        /// possible "how many callers does X have?" reply. Mutually
        /// exclusive with --json.
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
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
        /// Drop symbols whose file path contains SUBSTR. Symmetric to
        /// `--in`; useful for "all defs except in tests/generated":
        /// `scry def Activity --not-in /tests/`.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
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
    /// One-shot post-finalize pipeline: rebuilds every sidecar
    /// scry's query path knows how to use, in one command.
    /// Equivalent to running `build-offsets`, `build-file-symbols`,
    /// `build-trigrams`, `build-resolutions` (always), plus
    /// `build-modgraph` for each `--build-<kind> ROOT` flag passed,
    /// plus `scip-import` for each `--scip FILE` flag passed.
    /// Reports per-stage timings.
    Finalize {
        #[arg(long, value_name = "DIR")]
        index: Option<PathBuf>,
        /// Soong project root → write `module_graph.json`.
        #[arg(long, value_name = "PATH")]
        build_soong: Option<PathBuf>,
        /// Linux kernel project root → write `module_graph.json`
        /// (overrides build_soong if both are set; only one
        /// module_graph fits per index).
        #[arg(long, value_name = "PATH")]
        build_kernel: Option<PathBuf>,
        /// GN project root.
        #[arg(long, value_name = "PATH")]
        build_gn: Option<PathBuf>,
        /// Bazel project root.
        #[arg(long, value_name = "PATH")]
        build_bazel: Option<PathBuf>,
        /// Cargo workspace root.
        #[arg(long, value_name = "PATH")]
        build_cargo: Option<PathBuf>,
        /// SCIP index file to import.
        #[arg(long, value_name = "PATH")]
        scip: Option<PathBuf>,
        /// `scry clang-index` input: compile_commands.json. Set
        /// alongside this you can pass `--clang-root` to filter.
        #[arg(long, value_name = "PATH")]
        clang_compile_commands: Option<PathBuf>,
        /// Optional `--root` to pass to scry clang-index.
        #[arg(long, value_name = "PATH")]
        clang_root: Option<PathBuf>,
        /// Build-output directory to walk for compile_commands.json
        /// and `*.scip` artifacts during auto-discovery. Repeatable.
        /// Unlike walking the indexed source roots (which honors
        /// .gitignore), `--build-out` paths are walked verbatim —
        /// most build systems write outputs to gitignored dirs
        /// (`out/soong/...`, `build/`, `target/`, `.gradle/`), so
        /// the standard walker can't see them by default. Use this
        /// flag to point at e.g. `out/soong/development/ide/compdb`
        /// or your CMake build dir.
        #[arg(long = "build-out", value_name = "PATH")]
        build_out: Vec<PathBuf>,
        /// Workers passed through to the sub-commands.
        #[arg(long, default_value_t = 16)]
        workers: usize,
    },
    /// Outgoing edges from NAME's body: what does NAME call /
    /// reference? Symmetric counterpart to `scry callers NAME`.
    /// Resolves NAME to one or more SymbolRecords, computes each
    /// body's byte range via the enclosing_function heuristic, then
    /// returns every ref whose byte_start falls in that range.
    /// Requires the file_refs sidecar (`scry build-file-refs`) for
    /// O(refs-in-file) lookup; falls back to a full scan if missing.
    Uses {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Disambiguate which def of NAME to introspect when the
        /// name has multiple definitions: keep only defs whose
        /// path contains this substring.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop candidate defs whose file path contains SUBSTR.
        /// Symmetric to `--in`; lets you exclude noisy test/generated
        /// defs when introspecting a method's outgoing edges.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        /// Filter outgoing refs by kind (call, type, field, …).
        /// Default: all kinds.
        #[arg(long, short = 'k')]
        kind: Option<String>,
        /// Strict mode: drop outgoing edges whose Layer 2 resolution
        /// didn't pin a target def. Useful for "what does NAME call
        /// that we know the target of?" — strips out unresolved
        /// calls that the heuristic resolver couldn't attribute.
        #[arg(long)]
        strict: bool,
        /// Compact output. `count` emits just `N edges`. `paths`
        /// emits deduped sorted file paths of the outgoing refs —
        /// "which files does NAME touch?". Same shape as on ref /
        /// callers. Mutually exclusive with --json.
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Recursive callers tree for NAME. Walks the call graph
    /// upwards: for each caller, find ITS callers, up to `--depth`
    /// levels. Cycle-safe (visited-set). Outputs an indented tree
    /// or a JSON tree depending on --json. LLM-shaped query for
    /// "how does control flow reach this function?".
    ///
    /// Caller identity is `RefRecord.scope_path.last()` — the
    /// enclosing function the ref site is inside. Same scope
    /// resolution as `subclasses`.
    Callgraph {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Restrict to call sites in files whose path contains SUBSTR.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop call sites in files whose path contains SUBSTR.
        /// Symmetric to `--in`. Applied at every walk level — useful
        /// for pruning entire subtrees (e.g. `--not-in /tests/`).
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        /// How many levels of caller-of-caller to walk.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Soft cap on total tree nodes; stops expansion when hit.
        /// Defaults to enough for typical traces, not for "expand
        /// every reachable caller in AOSP".
        #[arg(long, default_value_t = 200)]
        max_nodes: usize,
        /// Compose with build-graph reachability: keep only callers
        /// whose owning module can reach NAME's module.
        #[arg(long)]
        reachable: bool,
        /// Narrow ROOT-LEVEL callers by callee location — same shape
        /// as `scry ref --def-in PATH`. Only NAME's callers whose
        /// Layer 2 resolution points at a def in PATH (or whose
        /// resolution is None, permissively) are walked. Deeper
        /// levels are not narrowed because the callgraph walker
        /// doesn't track per-frame def context.
        #[arg(long, value_name = "PATH")]
        def_in: Option<String>,
        /// Strict mode: drop root-level callers whose Layer 2
        /// resolution didn't land on a specific def. See
        /// `scry ref --strict`.
        #[arg(long)]
        strict: bool,
        /// Use lexical (tree-sitter) name match only. Default
        /// auto-engages clang USR + SCIP symbol identity filters
        /// when their sidecars are present (root level only —
        /// deeper recursion stays lexical). See `scry ref --lexical`.
        #[arg(long)]
        lexical: bool,
        #[arg(long)]
        json: bool,
    },
    /// "What breaks if I change NAME?" — composes callers +
    /// subclasses (transitive) into a single deduped impact set
    /// of files + symbols. Useful before refactors and as an LLM-
    /// shaped pre-flight check ("is this change small or huge?").
    /// Composes with --reachable (defaults to off; on means the
    /// impact set is build-graph-pruned).
    Impact {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Restrict to symbols/files whose path contains SUBSTR.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop symbols/files whose path contains SUBSTR. Symmetric
        /// to `--in`. Lets you ask "what breaks if I change X, ignoring
        /// tests?" via `--not-in /tests/`.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        /// Walk subclass hierarchy this many levels deep.
        #[arg(long, default_value_t = 2)]
        subclass_depth: usize,
        /// Build-graph reachability filter on callers; off by default.
        #[arg(long)]
        reachable: bool,
        /// Narrow the CALLERS portion of impact by callee location —
        /// same shape as `scry ref --def-in PATH`. Doesn't affect the
        /// subclass walk (subclasses are about the type, not the
        /// method). Use when impact is being asked about ONE specific
        /// `close()` overload, not every method named close.
        #[arg(long, value_name = "PATH")]
        def_in: Option<String>,
        /// Strict mode: drop callers whose Layer 2 resolution didn't
        /// land on a specific def. See `scry ref --strict`.
        #[arg(long)]
        strict: bool,
        /// Use lexical (tree-sitter) name match only. Default
        /// auto-engages clang USR + SCIP symbol identity filters
        /// on the callers leg when their sidecars are present.
        /// See `scry ref --lexical`.
        #[arg(long)]
        lexical: bool,
        #[arg(long, default_value = "200")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find direct (or transitive) subclasses of a type. LSP analogue:
    /// typeHierarchy/subtypes. Walks tree-sitter `InheritFrom` refs;
    /// the child class is resolved via scope_path. Works across all
    /// languages that emit inherit refs (Java/Kotlin/C++/Rust impls/…).
    Subclasses {
        /// Parent type/class/interface name.
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Restrict to children whose file path contains the SUBSTRING.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop children whose file path contains SUBSTR (v0.1.55).
        /// Symmetric to `--in`.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        /// Walk the hierarchy this many levels deep. 0 = direct only.
        #[arg(long, default_value_t = 0)]
        depth: usize,
        /// Compact output. `count` emits `N subclasses` only. `paths`
        /// emits deduped sorted file paths of the children — useful
        /// for "which files define a subtype of X?" agent queries.
        /// Mutually exclusive with --json (`paths` supports --json).
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Find implementations of an interface (Java/Kotlin idiom).
    /// Alias for `subclasses`; symmetric LSP analogue.
    Implementations {
        name: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop implementations whose file path contains SUBSTR (v0.1.55).
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        #[arg(long, default_value_t = 0)]
        depth: usize,
        /// See `scry subclasses --format`.
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
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
        /// Restrict to symbols whose file path contains SUBSTR.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop symbols whose file path contains SUBSTR (v0.1.55).
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Typo-tolerant fuzzy search over symbol names. Combines a
    /// substring match (catches "ParcelFile" → "ParcelFileDescriptor")
    /// with a Levenshtein-automaton walk bounded by --distance, then
    /// re-ranks the union by Wagner-Fischer edit distance so the
    /// closest names land first. The `distance` field on each result
    /// is the actual edit distance from the query to the symbol name.
    Fuzzy {
        substr: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Restrict results to symbols whose file path contains SUBSTR.
        /// Same semantics as `--in` on `def` / `ref` / `callers` —
        /// applied post-rank, so the `--limit` cap counts post-filter
        /// hits.
        #[arg(long, value_name = "SUBSTR")]
        in_: Option<String>,
        /// Drop symbols whose file path contains SUBSTR (v0.1.55).
        /// Symmetric to `--in`; combines for "scope and exclude".
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        /// Levenshtein distance bound for typo tolerance. The actual
        /// per-result distance shown in output is computed exactly
        /// via Wagner-Fischer; this flag only caps the candidate set
        /// that the FST automaton walks. Defaults to 2 — high enough
        /// for one-character typos in either direction, low enough
        /// that the automaton stays cheap.
        #[arg(long, default_value = "2")]
        distance: u32,
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show index metadata.
    Stats {
        #[arg(long)]
        index: Option<PathBuf>,
        /// Emit one JSON object instead of the human-readable text.
        /// Includes by_lang and by_kind histograms. Stable shape; new
        /// fields may be appended but existing keys won't move.
        #[arg(long)]
        json: bool,
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
        /// Include the first N source lines of each symbol as a
        /// `snippet` field (JSON) or trailing block (plain). Saves a
        /// round-trip when an agent needs both "what's in this file"
        /// and "what does each symbol look like". 0 = no snippets
        /// (default; preserves the cheap shape).
        #[arg(long, default_value = "0", value_name = "N_LINES")]
        with_snippets: usize,
    },
    /// One-call file summary: filename, language, total symbol count,
    /// per-kind breakdown, top 3 ranked symbols, and the first
    /// non-blank line of the file (often the package decl or a leading
    /// docstring). Designed for "what does this file do?" agent
    /// queries where `outline + N × def` would otherwise burn 5-10×
    /// the tokens.
    ///
    /// PATH matches by suffix (same as `outline`); on multiple
    /// matches scry picks the shortest and notes the others.
    Tldr {
        path: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Substring or regex search over indexed source files (rg-like).
    Grep {
        pattern: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        regex: bool,
        /// Case-insensitive match. Works for literal and regex patterns;
        /// the trigram pre-filter expands each query trigram across its
        /// ASCII case variants so this stays fast on big indexes.
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long, value_name = "PREFIX")]
        in_: Option<String>,
        /// Drop hits in files whose path contains SUBSTR (v0.1.55).
        /// Symmetric to `--in`; useful for `--not-in /tests/`-style
        /// pruning of test/generated noise.
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        #[arg(long, default_value = "100")]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Compact output format. `lines` emits `path:line:col\tsnippet`
        /// (rg-shaped) one hit per line — cuts token cost ~5-10× vs JSON
        /// when the agent only needs "is X referenced anywhere?".
        /// `count` emits just `N hits across M files` with no per-hit
        /// rows. Mutually exclusive with --json.
        #[arg(long, value_name = "FORMAT")]
        format: Option<String>,
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
        /// Dump the query plan to stderr instead of running the search:
        /// extracted literals + per-trigram posting sizes + intersection
        /// candidate count + a rough scan-cost estimate. Use when a grep
        /// feels too slow and you want to know why. Implies --limit 0
        /// (no rows printed); suppresses the JSON / lines / count
        /// formats.
        #[arg(long)]
        explain: bool,
    },
    /// Symbols and references in files that changed since a git
    /// commit. Useful for code review and for agents working on a
    /// PR — "what callers exist for the function I just modified" is
    /// `scry diff --since main --then-callers`-shaped without scry
    /// having to know about the PR.
    ///
    /// For each indexed root that is a git repo, shells out to:
    ///     git -C ROOT diff --name-only COMMITISH..HEAD
    /// then intersects the changed paths with the file table and emits
    /// per-file symbol counts (and the symbols themselves with --verbose).
    /// Roots that aren't git trees are skipped with a one-line warning.
    Diff {
        /// Commit-ish to compare HEAD against. Anything `git rev-parse`
        /// accepts works: a SHA, a tag, HEAD~N, a branch name.
        #[arg(long)]
        since: String,
        /// Optional path-prefix filter, same semantics as elsewhere
        /// (substring match against the display path).
        #[arg(long = "in")]
        in_: Option<String>,
        /// Print every changed symbol, not just per-file counts.
        #[arg(long)]
        verbose: bool,
        /// Cap the number of files reported. Default 50.
        #[arg(long, default_value = "50")]
        limit: usize,
        /// Index dir override.
        #[arg(long)]
        index: Option<PathBuf>,
        /// Machine-readable output: one JSON object per changed file.
        #[arg(long)]
        json: bool,
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
        /// Cap simultaneous client connections. New `accept()`s past
        /// the cap wait briefly then are dropped with a one-line
        /// stderr log. 0 = unlimited (default). Set this on shared
        /// hosts or when the workload could fan a thousand agents at
        /// the daemon — each connection runs grep with rayon
        /// concurrency, so unbounded fan-in × unbounded fan-out
        /// can OOM. A safe ceiling for a single 16-core box is
        /// 32-64. Ignored in stdin/stdout mode.
        #[arg(long, default_value_t = 0)]
        max_conns: u32,
    },
    /// Build the canonical scry v1 `module_graph.json` from a project's
    /// native build metadata. Once written into the index dir, queries
    /// with `--reachable` get build-graph-aware filtering — refs in
    /// modules that can't actually reach the queried name are dropped.
    ///
    /// Supported KINDs:
    ///   cargo  — Rust workspaces. Reads `Cargo.toml` at ROOT, recurses
    ///            into workspace members, builds the module + intra-
    ///            workspace dep graph from each member's `dependencies`.
    ///   soong  — AOSP. Reads `ROOT/out/soong/module-graph.json` (must
    ///            be generated first via `m json-module-graph`). Skeleton
    ///            implementation; validate against your AOSP output.
    ///   kernel — Linux Kbuild. Not yet implemented (queued v0.1.12).
    ///   gn     — GN/ninja (perfetto / Chromium). Not yet implemented.
    BuildModgraph {
        /// Build system to read from.
        #[arg(long, value_name = "KIND")]
        kind: String,
        /// Project root containing the build metadata.
        #[arg(long, value_name = "PATH")]
        root: PathBuf,
        /// Where to write module_graph.json (typically <index_dir>/module_graph.json).
        #[arg(long, short = 'o', value_name = "PATH")]
        output: PathBuf,
    },
    /// Generate the clang USR sidecar at `<index>/clang_usrs.bin`
    /// from a compile_commands.json. Per-TU libclang parse, USR
    /// interning, system-header filtering. Path B precision.
    ClangIndex {
        /// Path to compile_commands.json (Bazel, CMake, Soong, GN,
        /// or anything Bear-wrapped emits this).
        #[arg(long, value_name = "PATH")]
        compile_commands: PathBuf,
        /// Existing scry index dir; sidecar lands here.
        #[arg(long, value_name = "DIR")]
        index: Option<PathBuf>,
        /// Only index TUs whose source is under this prefix.
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,
        /// Parallel parse workers (0 = num_cpus).
        #[arg(long, default_value_t = 0)]
        workers: usize,
        /// Skip TUs whose source exceeds this size (bytes). 0 = no cap.
        #[arg(long, default_value_t = 4 * 1024 * 1024)]
        max_file_bytes: u64,
    },
    /// Report stats from the optional clang USR sidecar at
    /// `<index>/clang_usrs.bin` (produced by `scry clang-index`).
    /// Useful for verifying that Path B precision is wired up
    /// before issuing `--clang-usr` queries.
    ClangStats {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// `scry build-jvm-scip` — Soong-native Java + Kotlin SCIP pipeline.
    ///
    /// Walks AOSP's Soong intermediates and replays each javac /
    /// kotlinc invocation with the appropriate SemanticDB compiler
    /// plugin attached. Both languages emit per-source `.semanticdb`
    /// files into a shared targetroot; a single
    /// `scip-java index-semanticdb` merge pass produces one SCIP
    /// index that imports into the scry sidecar at
    /// `<index>/scip_index.bin`.
    ///
    /// This is the bridge that makes strict-precise queries work on
    /// AOSP Java/Kotlin code without depending on Gradle / Maven —
    /// neither of which Soong uses.
    BuildJvmScip {
        /// AOSP source root (the parent of `out/soong/`).
        #[arg(long, value_name = "PATH")]
        source_root: PathBuf,
        /// Soong build dir. Defaults to `<source_root>/out/soong`.
        #[arg(long, value_name = "PATH")]
        soong_build_dir: Option<PathBuf>,
        /// Existing scry index dir; sidecar lands at <index>/scip_index.bin.
        #[arg(long, value_name = "DIR")]
        index: Option<PathBuf>,
        /// Override the javac binary. AOSP ships its own at
        /// `prebuilts/jdk/jdk21/linux-x86/bin/javac` — pass that
        /// for byte-exact reproducibility with the build.
        #[arg(long, value_name = "PATH")]
        javac: Option<PathBuf>,
        /// Override the `scip-java` binary used in the merge step.
        #[arg(long, value_name = "PATH")]
        scip_java: Option<PathBuf>,
        /// Override the path to the semanticdb-javac plugin jar.
        /// Auto-discovered under `~/.m2/repository/com/sourcegraph/`
        /// when not set.
        #[arg(long, value_name = "PATH")]
        semanticdb_javac_jar: Option<PathBuf>,
        /// Override the kotlinc launcher. Must load the embeddable
        /// jar (see install_indexers.sh's `kotlinc-embeddable`).
        #[arg(long, value_name = "PATH")]
        kotlinc: Option<PathBuf>,
        /// Override the path to the semanticdb-kotlinc plugin jar.
        /// Auto-discovered under `~/.m2/repository/com/sourcegraph/`
        /// when not set.
        #[arg(long, value_name = "PATH")]
        semanticdb_kotlinc_jar: Option<PathBuf>,
        /// Where the per-compilation .semanticdb shards land.
        /// Defaults to `$SCRY_TMP_DIR/scry-semanticdb`
        /// (i.e. `/mnt/agent/tmp/scry-semanticdb` unless overridden).
        #[arg(long, value_name = "PATH")]
        targetroot: Option<PathBuf>,
        /// Filter compilations by substring of the module name.
        /// Useful for incremental testing — e.g.
        /// `--only-module libcore` runs just the libcore modules.
        #[arg(long, value_name = "SUBSTR")]
        only_module: Option<String>,
        /// Cap the number of compilations processed. Combine with
        /// `--only-module` to test the pipeline on a small slice
        /// before running the full AOSP set.
        #[arg(long, value_name = "N")]
        max_compilations: Option<usize>,
        /// Skip Kotlin compilations.
        #[arg(long)]
        skip_kotlin: bool,
        /// Skip Java compilations.
        #[arg(long)]
        skip_java: bool,
    },
    /// `scry build-symbols` — Kythe-class symbol-identity sidecars.
    ///
    /// One command, one explicit `--build-{soong,gn,kbuild,cmake,cargo}`
    /// flag, produces the matching sidecars:
    ///   - Soong  → SCIP for Java + Kotlin via the Soong bridge.
    ///   - GN     → clang USRs from `compile_commands.json`.
    ///   - Kbuild → same (kernel C), via the kernel's
    ///              `scripts/clang-tools/gen_compile_commands.py`.
    ///   - CMake  → same; `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON`.
    ///   - Cargo  → polyglot pass only (Rust via rust-analyzer scip).
    ///
    /// `--with-polyglot` also runs the polyglot pass (Rust / Go /
    /// TypeScript / Python) over the source root regardless of the
    /// build type, so a Soong tree that also has Python tooling gets
    /// both passes.
    BuildSymbols {
        /// Source root.
        #[arg(long, value_name = "PATH")]
        source_root: PathBuf,
        /// Soong out dir (typically `<source>/out/soong`). Mutually
        /// exclusive with the other --build-* flags.
        #[arg(long, value_name = "PATH", group = "build")]
        build_soong: Option<PathBuf>,
        /// GN build out dir (the one containing `args.gn`).
        #[arg(long, value_name = "PATH", group = "build")]
        build_gn: Option<PathBuf>,
        /// Path to the `gn` binary. Defaults to `gn` on PATH.
        #[arg(long, value_name = "PATH")]
        gn_binary: Option<PathBuf>,
        /// Linux kernel build out dir (the one containing `.config`).
        #[arg(long, value_name = "PATH", group = "build")]
        build_kbuild: Option<PathBuf>,
        /// CMake build out dir (the one containing `CMakeCache.txt`).
        #[arg(long, value_name = "PATH", group = "build")]
        build_cmake: Option<PathBuf>,
        /// Path to the `cmake` binary. Defaults to `cmake` on PATH.
        #[arg(long, value_name = "PATH")]
        cmake_binary: Option<PathBuf>,
        /// Cargo workspace — runs polyglot pass only.
        #[arg(long, group = "build")]
        build_cargo: bool,
        /// Existing scry index dir.
        #[arg(long, value_name = "DIR")]
        index: Option<PathBuf>,
        /// Also run polyglot pass (Rust / Go / TS / Python). Implied
        /// when --build-cargo is set.
        #[arg(long)]
        with_polyglot: bool,
        /// Skip Rust during the polyglot pass.
        #[arg(long)]
        no_rust: bool,
        /// Skip Go during the polyglot pass.
        #[arg(long)]
        no_go: bool,
        /// Skip TypeScript during the polyglot pass.
        #[arg(long)]
        no_typescript: bool,
        /// Skip Python during the polyglot pass.
        #[arg(long)]
        no_python: bool,
        /// Workers for clang-index. 0 = auto (one per CPU).
        #[arg(long, default_value_t = 0)]
        workers: usize,
        /// Targetroot for the JVM pipeline (Soong only).
        #[arg(long, value_name = "PATH")]
        targetroot: Option<PathBuf>,
        /// Per-target `.scip` files (polyglot only).
        #[arg(long, value_name = "PATH")]
        scip_out_dir: Option<PathBuf>,
        /// Override the javac binary used by the JVM pipeline (Soong only).
        /// AOSP ships its own at `prebuilts/jdk/jdk21/linux-x86/bin/javac` —
        /// pass that for byte-exact reproducibility with the build.
        #[arg(long, value_name = "PATH")]
        javac: Option<PathBuf>,
        /// Override the `scip-java` binary used in the JVM merge step.
        #[arg(long, value_name = "PATH")]
        scip_java: Option<PathBuf>,
        /// Override the path to the semanticdb-javac plugin jar.
        /// Auto-discovered under `~/.m2/repository/com/sourcegraph/`
        /// when not set.
        #[arg(long, value_name = "PATH")]
        semanticdb_javac_jar: Option<PathBuf>,
        /// Override the kotlinc launcher (Soong only). Must load the
        /// embeddable jar — see install_indexers.sh's `kotlinc-embeddable`.
        #[arg(long, value_name = "PATH")]
        kotlinc: Option<PathBuf>,
        /// Override the path to the semanticdb-kotlinc plugin jar.
        /// Auto-discovered under `~/.m2/repository/com/sourcegraph/`
        /// when not set.
        #[arg(long, value_name = "PATH")]
        semanticdb_kotlinc_jar: Option<PathBuf>,
        /// Filter Soong compilations by substring of the module name.
        /// Useful for incremental testing.
        #[arg(long, value_name = "SUBSTR")]
        only_module: Option<String>,
        /// Cap the number of Soong compilations processed.
        #[arg(long, value_name = "N")]
        max_compilations: Option<usize>,
        /// Skip Kotlin compilations on the Soong path.
        #[arg(long)]
        skip_kotlin: bool,
        /// Skip Java compilations on the Soong path.
        #[arg(long)]
        skip_java: bool,
        /// Override the rust-analyzer binary used by the polyglot pass.
        #[arg(long, value_name = "PATH")]
        rust_analyzer: Option<PathBuf>,
        /// Override the scip-go binary used by the polyglot pass.
        #[arg(long, value_name = "PATH")]
        scip_go: Option<PathBuf>,
        /// Override the scip-typescript binary used by the polyglot pass.
        #[arg(long, value_name = "PATH")]
        scip_typescript: Option<PathBuf>,
        /// Override the scip-python binary used by the polyglot pass.
        #[arg(long, value_name = "PATH")]
        scip_python: Option<PathBuf>,
        /// Filter polyglot targets by substring of their root path.
        #[arg(long, value_name = "SUBSTR")]
        only_root: Option<String>,
        /// Cap the number of polyglot targets processed.
        #[arg(long, value_name = "N")]
        max_targets: Option<usize>,
    },
    /// `scry build-polyglot-scip` — Rust + Go + TypeScript + Python.
    ///
    /// Walks the source root for native project markers
    /// (`Cargo.toml`, `go.mod`, `tsconfig.json`, or `.py` files) and
    /// runs the corresponding indexer per project. Each indexer's
    /// `.scip` output lands in the scry sidecar via APPEND-mode
    /// import, so this command composes cleanly with
    /// `build-jvm-scip` and `clang-index` (which already wrote
    /// SCIP / clang USR sidecars).
    BuildPolyglotScip {
        /// Source root to walk for project markers.
        #[arg(long, value_name = "PATH")]
        source_root: PathBuf,
        /// Existing scry index dir; sidecar lands at <index>/scip_index.bin.
        #[arg(long, value_name = "DIR")]
        index: Option<PathBuf>,
        /// Per-target `.scip` files land here. Defaults to
        /// `$SCRY_TMP_DIR/scry-polyglot-scip`
        /// (i.e. `/mnt/agent/tmp/scry-polyglot-scip` unless overridden).
        #[arg(long, value_name = "PATH")]
        scip_out_dir: Option<PathBuf>,
        /// Override the rust-analyzer binary.
        #[arg(long, value_name = "PATH")]
        rust_analyzer: Option<PathBuf>,
        /// Override the scip-go binary.
        #[arg(long, value_name = "PATH")]
        scip_go: Option<PathBuf>,
        /// Override the scip-typescript binary.
        #[arg(long, value_name = "PATH")]
        scip_typescript: Option<PathBuf>,
        /// Override the scip-python binary.
        #[arg(long, value_name = "PATH")]
        scip_python: Option<PathBuf>,
        /// Skip Rust.
        #[arg(long)]
        no_rust: bool,
        /// Skip Go.
        #[arg(long)]
        no_go: bool,
        /// Skip TypeScript.
        #[arg(long)]
        no_typescript: bool,
        /// Skip Python.
        #[arg(long)]
        no_python: bool,
        /// Filter project roots by substring.
        #[arg(long, value_name = "SUBSTR")]
        only_root: Option<String>,
        /// Cap the number of targets processed.
        #[arg(long, value_name = "N")]
        max_targets: Option<usize>,
    },
    /// Import a SCIP index (https://github.com/sourcegraph/scip)
    /// produced by scip-java / scip-kotlin / gopls / rust-analyzer /
    /// scip-typescript / etc., into the scry sidecar
    /// `<index>/scip_index.bin`. Powers `--scip-precise` queries.
    ScipImport {
        /// Path to the SCIP index file (protobuf, typically named
        /// `index.scip` or `*.scip`).
        #[arg(long, value_name = "PATH")]
        scip: PathBuf,
        /// Existing scry index dir; sidecar lands here.
        #[arg(long, value_name = "DIR")]
        index: Option<PathBuf>,
        /// Override the SCIP index's `project_root` for path
        /// resolution. Use when the SCIP file was generated under
        /// a different working tree (CI vs local).
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,
    },
    /// Report stats from the optional SCIP sidecar at
    /// `<index>/scip_index.bin` (produced by `scry scip-import`).
    ScipStats {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Look up the SCIP symbol ID for a (path, byte_offset) pair
    /// against the sidecar. Empty stdout when no record covers
    /// the site.
    ScipLookup {
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        path: String,
        #[arg(long)]
        offset: u32,
    },
    /// Look up the clang USR for a (path, byte_offset) pair against
    /// the sidecar. Returns the empty string if no record covers
    /// that exact site.
    ClangLookup {
        #[arg(long)]
        index: Option<PathBuf>,
        /// Absolute source path (matches what clang saw).
        #[arg(long)]
        path: String,
        /// Byte offset of the cursor location within the file.
        #[arg(long)]
        offset: u32,
    },
    /// Prewarm the OS page cache with every sidecar in the index, so
    /// subsequent queries land warm (sub-10 ms) instead of cold (50–
    /// hundreds of ms). Sequential parallel read of every file in
    /// the index dir; uses available RAM as page cache. `scry serve`
    /// and `scry mcp` auto-run this on startup; use the standalone
    /// command after a fresh boot before issuing queries, or before
    /// a perf bench so you're measuring warm latency.
    Warm {
        #[arg(long)]
        index: Option<PathBuf>,
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
    /// --build-trigrams. Walks every file in the index's files_packed.bin,
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
    /// Build the file→ref-ids sidecar (file_refs.bin +
    /// file_refs_offsets.bin). Symmetric to file_symbols but indexes
    /// refs.bin. Powers `scry uses`: outgoing edges from a function
    /// body are found by intersecting "refs in NAME's file" with
    /// "byte range of NAME's body". Without this sidecar, `uses`
    /// must linearly scan all 63M refs.
    BuildFileRefs {
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
    /// Compute and store per-file blake3 content digests
    /// (file_digests.bin) for an existing index. The digest is what
    /// `scry index --incremental` reads to decide which files actually
    /// changed between two builds — without it, only full reindex
    /// works. Walks every indexed file once, hashes in parallel via
    /// rayon. Cheap: ~25 s for the full AOSP+Linux corpus.
    BuildDigests {
        #[arg(long)]
        index: Option<PathBuf>,
        /// Parallelism for the hashing pass; default = num_cpus.
        #[arg(long, default_value = "0")]
        workers: usize,
    },
    /// Rewrite the index dropping any tombstoned records (file_ids
    /// marked deleted by a prior `scry index --incremental`). Reclaims
    /// space and resets the tombstone bitmap. Atomic: writes to a
    /// .tmp/ then swaps. Safe to run while readers are open — they
    /// keep the old mmap until they re-open.
    Compact {
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Preview what an incremental reindex would do: walk the source
    /// roots, hash every file, compare against the existing index's
    /// file_digests sidecar. Reports counts (added / changed /
    /// unchanged / removed) and optionally lists the files. Does
    /// not modify the index — safe to run on a live one.
    IndexDiff {
        roots: Vec<PathBuf>,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value = "all")]
        profile: String,
        /// Show every changed/added/removed file (not just counts).
        #[arg(long)]
        verbose: bool,
        #[arg(long, default_value = "0")]
        workers: usize,
        #[arg(long)]
        json: bool,
    },
    /// Mark a specific file as tombstoned. The next query of any kind
    /// will skip records belonging to that file. Use case: "I just
    /// deleted this file and want immediate query freshness without
    /// running a full incremental." Idempotent; safe to run on a file
    /// that's already tombstoned.
    Tombstone {
        path: PathBuf,
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Validate the on-disk index: confirm every required artifact
    /// exists and is non-empty, the manifest is parseable, the lazy
    /// sidecars round-trip a small sample of records, and the
    /// optional sidecars (file_symbols, ref_resolutions, file_digests,
    /// trigrams, chunks/embeddings) are either present-and-valid or
    /// absent (each has its own build subcommand). Reports the state
    /// of each. Exits non-zero on any required-artifact failure.
    Health {
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// OWNERS lookup: walk up from PATH collecting OWNERS entries
    /// from the nearest enclosing OWNERS files until the root. The
    /// closest-to-PATH owner list comes first, more-distant
    /// inherited owners after — matches Gerrit's evaluation order.
    ///
    /// Default shows the nearest non-empty owner set. --include-deep
    /// shows every layer. --accumulate emits the *union* of emails
    /// across every layer the walk visited (the Gerrit "approvers"
    /// set). All three modes respect `set noparent`: when an OWNERS
    /// file declares it, the walk stops at that level and inherited
    /// owners above are not considered.
    Owner {
        path: PathBuf,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        include_deep: bool,
        /// Emit the union of emails across all visited OWNERS layers
        /// (the Gerrit "potential approvers" set), sorted and
        /// deduplicated.
        #[arg(long)]
        accumulate: bool,
        #[arg(long)]
        json: bool,
    },
    /// Compute and store per-chunk text embeddings (chunks.bin +
    /// embeddings.bin). Default model is a deterministic hash-based
    /// bag-of-tokens embedding (no model download, no extra deps);
    /// good enough for vocabulary-overlap retrieval which catches
    /// most "how do I X" code-search questions. Powers `scry ask`.
    ///
    /// Storage: ~70 MB chunks + (chunk_count × dim × 4) bytes for
    /// embeddings. At default dim=64 and ~3 M chunks that's ~770 MB.
    BuildEmbeddings {
        #[arg(long)]
        index: Option<PathBuf>,
        /// Vector dimension. Trade-off: higher = better discrimination,
        /// more storage. Default 64 (good for code-search vocabulary
        /// overlap; ~770 MB on full corpus).
        #[arg(long, default_value = "64")]
        dim: usize,
        /// Chunk size in lines. Standard RAG sizing for code is
        /// 50–150; default 100.
        #[arg(long, default_value = "100")]
        chunk_lines: usize,
        /// Overlap between consecutive chunks, in lines. Catches
        /// definitions that straddle chunk boundaries.
        #[arg(long, default_value = "20")]
        chunk_overlap: usize,
        /// Parallelism for the embedding pass. Default = num_cpus.
        #[arg(long, default_value = "0")]
        workers: usize,
    },
    /// Semantic retrieval: find code chunks whose embedded text is
    /// most similar to QUERY. Useful for "how do I parse TOML in this
    /// codebase" — questions where you don't know the identifier name
    /// to grep for. Requires `scry build-embeddings` to have run.
    Ask {
        query: String,
        #[arg(long)]
        index: Option<PathBuf>,
        /// Substring path filter, same semantics as elsewhere.
        #[arg(long = "in")]
        in_: Option<String>,
        /// Drop chunks whose file path contains SUBSTR (v0.1.55).
        #[arg(long, value_name = "SUBSTR")]
        not_in: Option<String>,
        /// Number of top-K chunks to return.
        #[arg(long, default_value = "10")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Emit shell-completion script for the given shell to stdout.
    /// Pipe to the shell's standard completion directory or source
    /// inline. Example: `scry completions bash > /etc/bash_completion.d/scry`.
    Completions {
        /// Target shell. Accepts: bash, zsh, fish, powershell, elvish.
        shell: clap_complete::Shell,
    },
    /// Emit a roff-formatted man page for `scry` to stdout. Pipe to
    /// `gzip > /usr/local/share/man/man1/scry.1.gz` to install
    /// system-wide.
    Man,
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

pub(crate) fn default_index_dir() -> PathBuf {
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

/// Format a duration in seconds as a short human label suitable for
/// inline progress output: `45s`, `12m30s`, `2h05m`. Caps at hours;
/// indexing jobs that legitimately take days are out of scope.
fn format_eta(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return String::from("—");
    }
    let total = secs.round() as u64;
    if total < 60 {
        return format!("{}s", total);
    }
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m{s:02}s")
    }
}

fn main() -> Result<()> {
    // CLI tools must die quietly when their stdout pipe closes
    // (e.g. `scry grep PATTERN | head`); the helper lives in
    // scry-store because that's the only crate that allows unsafe.
    scry_store::restore_default_sigpipe();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index {
            roots, profile, out, count_only, limit, no_refs, workers,
            max_file_bytes, big_file_bytes, mem_cap, flush_every, flush_bytes,
            resume, build_trigrams, incremental,
        } => {
            if incremental {
                cmd_index_incremental(roots, out, profile, workers,
                                      max_file_bytes, build_trigrams)
            } else {
                // Auto-scale flush_every with mem_cap so the bytes-target
                // flush_bytes is actually reachable. Without this the
                // file-count cap (default 50_000) fires first and we
                // never approach the bytes target.
                let flush_every: usize = match flush_every {
                    Some(v) => v,
                    None => {
                        if mem_cap > 0 {
                            ((mem_cap as usize).saturating_mul(50_000)).min(5_000_000)
                        } else {
                            50_000
                        }
                    }
                };
                cmd_index(
                    roots, profile, out, count_only, limit, no_refs, workers,
                    max_file_bytes, mem_cap, flush_every, flush_bytes,
                    big_file_bytes, resume, build_trigrams,
                )
            }
        }
        Cmd::Def { name, index, lang, kind, in_, not_in, limit, json, md, budget } => {
            cmd_def(name, index, lang, kind, in_, not_in, limit, json, md, budget)
        }
        Cmd::Prefix { prefix, index, in_, not_in, limit, json } => {
            cmd_prefix(prefix, index, in_, not_in, limit, json)
        }
        Cmd::Fuzzy { substr, index, in_, not_in, distance, limit, json } => {
            cmd_fuzzy(substr, index, in_, not_in, distance, limit, json)
        }
        Cmd::Ref { name, index, lang, kind, in_, not_in, limit, json, format, lexical, reachable, clang_precise, scip_precise, scope, def_in, strict } => {
            let (reachable, clang_precise, scip_precise) =
                resolve_precision(lexical, reachable, clang_precise, scip_precise);
            cmd_ref(name, index, lang, kind, in_, not_in, limit, json, format, reachable, clang_precise, scip_precise, scope, def_in, strict)
        }
        Cmd::Callers { name, index, lang, in_, not_in, limit, json, precise, lexical, reachable, clang_precise, scip_precise, scope, def_in, strict, format } => {
            if precise {
                return cmd_callers_precise(name, index, lang, in_, not_in, limit, json);
            }
            let (reachable, clang_precise, scip_precise) =
                resolve_precision(lexical, reachable, clang_precise, scip_precise);
            cmd_ref(name, index, lang, Some("call".to_string()), in_, not_in, limit, json, format, reachable, clang_precise, scip_precise, scope, def_in, strict)
        }
        Cmd::Stats { index, json } => cmd_stats(index, json),
        Cmd::Coverage { path, index, by_kind, json } => cmd_coverage(path, index, by_kind, json),
        Cmd::Outline { path, index, json, limit, with_snippets } =>
            cmd_outline(path, index, json, limit, with_snippets),
        Cmd::Tldr { path, index, json } => cmd_tldr(path, index, json),
        Cmd::Grep {
            pattern, index, regex, ignore_case, lang, in_, not_in, limit, json, workers,
            max_file_bytes, mem_cap, format, explain,
        } => cmd_grep(
            pattern, index, regex, ignore_case, lang, in_, not_in, limit, json, workers,
            max_file_bytes, mem_cap, format, explain,
        ),
        Cmd::Serve { index, listen, max_conns } => cmd_serve(index, listen, max_conns),
        Cmd::Mcp { index } => cmd_mcp(index),
        Cmd::Warm { index } => cmd_warm(index),
        Cmd::BuildModgraph { kind, root, output } => cmd_build_modgraph(&kind, &root, &output),
        Cmd::ClangIndex { compile_commands, index, root, workers, max_file_bytes } =>
            precision_subcmds::cmd_clang_index(compile_commands, index, root, workers, max_file_bytes),
        Cmd::ClangStats { index } => precision_subcmds::cmd_clang_stats(index),
        Cmd::ClangLookup { index, path, offset } => precision_subcmds::cmd_clang_lookup(index, &path, offset),
        Cmd::BuildJvmScip { source_root, soong_build_dir, index, javac, scip_java,
                            semanticdb_javac_jar, kotlinc, semanticdb_kotlinc_jar,
                            targetroot, only_module, max_compilations,
                            skip_kotlin, skip_java } => bridge_subcmds::cmd_build_jvm_scip(
            source_root, soong_build_dir, index, javac, scip_java,
            semanticdb_javac_jar, kotlinc, semanticdb_kotlinc_jar,
            targetroot, only_module, max_compilations,
            skip_kotlin, skip_java,
        ),
        Cmd::BuildPolyglotScip { source_root, index, scip_out_dir, rust_analyzer,
                                  scip_go, scip_typescript, scip_python,
                                  no_rust, no_go, no_typescript, no_python,
                                  only_root, max_targets } =>
            bridge_subcmds::cmd_build_polyglot_scip(
                source_root, index, scip_out_dir, rust_analyzer, scip_go,
                scip_typescript, scip_python, no_rust, no_go, no_typescript,
                no_python, only_root, max_targets,
            ),
        Cmd::BuildSymbols { source_root, build_soong, build_gn, gn_binary,
                            build_kbuild, build_cmake, cmake_binary, build_cargo,
                            index, with_polyglot, no_rust, no_go, no_typescript,
                            no_python, workers, targetroot, scip_out_dir,
                            javac, scip_java, semanticdb_javac_jar,
                            kotlinc, semanticdb_kotlinc_jar,
                            only_module, max_compilations, skip_kotlin, skip_java,
                            rust_analyzer, scip_go, scip_typescript, scip_python,
                            only_root, max_targets } => {
            let build = match (build_soong, build_gn, build_kbuild, build_cmake, build_cargo) {
                (Some(d), None, None, None, false) => bridge_subcmds::BuildKind::Soong { build_dir: d },
                (None, Some(d), None, None, false) => bridge_subcmds::BuildKind::Gn { build_dir: d, gn_binary },
                (None, None, Some(d), None, false) => bridge_subcmds::BuildKind::Kbuild { build_dir: d },
                (None, None, None, Some(d), false) => bridge_subcmds::BuildKind::Cmake { build_dir: d, cmake_binary },
                (None, None, None, None, true)     => bridge_subcmds::BuildKind::Cargo,
                _ => anyhow::bail!(
                    "build-symbols requires exactly one --build-{{soong,gn,kbuild,cmake,cargo}} flag"
                ),
            };
            bridge_subcmds::cmd_build_symbols(bridge_subcmds::BuildSymbolsArgs {
                source_root, build, index, with_polyglot,
                no_rust, no_go, no_typescript, no_python,
                workers, targetroot, scip_out_dir,
                javac, scip_java, semanticdb_javac_jar,
                kotlinc, semanticdb_kotlinc_jar,
                only_module, max_compilations, skip_kotlin, skip_java,
                rust_analyzer, scip_go, scip_typescript, scip_python,
                only_root, max_targets,
            })
        }
        Cmd::ScipImport { scip, index, root } => precision_subcmds::cmd_scip_import(scip, index, root),
        Cmd::ScipStats { index } => precision_subcmds::cmd_scip_stats(index),
        Cmd::ScipLookup { index, path, offset } => precision_subcmds::cmd_scip_lookup(index, &path, offset),
        Cmd::Impact { name, index, in_, not_in, subclass_depth, reachable, def_in, strict, lexical, limit, json } =>
            cmd_impact(name, index, in_, not_in, subclass_depth, reachable, def_in, strict, lexical, limit, json),
        Cmd::Callgraph { name, index, in_, not_in, depth, max_nodes, reachable, def_in, strict, lexical, json } =>
            cmd_callgraph(name, index, in_, not_in, depth, max_nodes, reachable, def_in, strict, lexical, json),
        Cmd::Uses { name, index, in_, not_in, kind, strict, format, limit, json } =>
            cmd_uses(name, index, in_, not_in, kind, strict, format, limit, json),
        Cmd::Finalize {
            index, build_soong, build_kernel, build_gn, build_bazel, build_cargo,
            scip, clang_compile_commands, clang_root, build_out, workers,
        } => finalize::cmd_finalize(
            index, build_soong, build_kernel, build_gn, build_bazel, build_cargo,
            scip, clang_compile_commands, clang_root, build_out, workers,
        ),
        Cmd::Subclasses { name, index, in_, not_in, depth, format, limit, json } =>
            cmd_subclasses(name, index, in_, not_in, depth, format, limit, json),
        Cmd::Implementations { name, index, in_, not_in, depth, format, limit, json } =>
            cmd_subclasses(name, index, in_, not_in, depth, format, limit, json),
        Cmd::Recall { last, cmd, grep, log, dedup, json } =>
            cmd_recall(last, cmd, grep, log, dedup, json),
        Cmd::Diff { since, in_, verbose, limit, index, json } =>
            cmd_diff(since, in_, verbose, limit, index, json),
        Cmd::ModuleOf { path, index, limit } => cmd_module_of(path, index, limit),
        Cmd::Health { index, json } => health::cmd_health(index, json),
        Cmd::Owner { path, index, include_deep, accumulate, json } =>
            cmd_owner(path, index, include_deep, accumulate, json),
        Cmd::BuildTrigrams { index, workers, max_file_bytes } => {
            cmd_build_trigrams(index, workers, max_file_bytes)
        }
        Cmd::BuildOffsets { index } => cmd_build_offsets(index),
        Cmd::BuildFileSymbols { index } => cmd_build_file_symbols(index),
        Cmd::BuildFileRefs { index } => cmd_build_file_refs(index),
        Cmd::BuildResolutions { index } => cmd_build_resolutions(index),
        Cmd::BuildDigests { index, workers } => cmd_build_digests(index, workers),
        Cmd::Compact { index } => cmd_compact(index),
        Cmd::IndexDiff { roots, index, profile, verbose, workers, json } =>
            cmd_index_diff(roots, index, profile, verbose, workers, json),
        Cmd::Tombstone { path, index } => cmd_tombstone(path, index),
        Cmd::BuildEmbeddings { index, dim, chunk_lines, chunk_overlap, workers } =>
            cmd_build_embeddings(index, dim, chunk_lines, chunk_overlap, workers),
        Cmd::Ask { query, index, in_, not_in, limit, json } =>
            cmd_ask(query, index, in_, not_in, limit, json),
        Cmd::Completions { shell } => cmd_completions(shell),
        Cmd::Man => cmd_man(),
    }
}

/// Emit a shell-completion script for `shell` to stdout. Wraps
/// `clap_complete::generate` against the live `Args` derivation —
/// no manual sync needed when subcommands or flags change.
fn cmd_completions(shell: clap_complete::Shell) -> Result<()> {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
    Ok(())
}

/// Emit a roff-formatted man page for `scry` (1) to stdout. Wraps
/// `clap_mangen::Man` against the live `Args` derivation. Includes
/// the top-level scry(1) page; per-subcommand pages can be derived
/// from the same factory if a downstream packager wants them.
fn cmd_man() -> Result<()> {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut std::io::stdout())
        .map_err(|e| anyhow::anyhow!("render man page: {e}"))?;
    Ok(())
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
    mem_cap: u32,
    flush_every: usize,
    flush_bytes: Option<u32>,
    big_file_bytes: Option<u64>,
    resume: bool,
    build_trigrams: bool,
) -> Result<()> {
    // Auto-scale `flush_bytes` if the user didn't set it. On big-memory
    // hosts (e.g. --mem-cap 100), the static 1024 MiB default leaves
    // tens of GiB of RAM unused while writing more chunks than needed,
    // which lengthens finalize. Target ~25 % of the cap so workers can
    // still run, mmap'd source files still page in, and there's
    // headroom for transient tree-sitter allocations.
    let flush_bytes: u32 = match flush_bytes {
        Some(v) => v,
        None => {
            if mem_cap > 0 {
                let auto_mib = (mem_cap as u64).saturating_mul(1024) / 4;
                // Sanity cap at u32::MAX MiB (4 TiB target; absurdly high).
                auto_mib.min(u32::MAX as u64) as u32
            } else {
                1024
            }
        }
    };
    // Auto-scale `big_file_bytes` similarly. The serial-bucket threshold
    // exists to bound peak transient parse RAM across N workers. With a
    // generous --mem-cap we can afford to keep more files on the parallel
    // path. Scale: N × 16 KiB, capped at 4 MiB. A 100 GiB cap → 1.6 MiB
    // threshold, which is large enough to keep most legitimate AOSP
    // source on the parallel hot path (typical Java/C++ files are
    // 50 KB–1 MB) while still serializing the multi-MB generated test
    // fixtures that historically OOM'd the host.
    let big_file_bytes: u64 = match big_file_bytes {
        Some(v) => v,
        None => {
            if mem_cap > 0 {
                let scaled = (mem_cap as u64).saturating_mul(16 * 1024);
                scaled.min(4 * 1024 * 1024)
            } else {
                64 * 1024
            }
        }
    };
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
                probe_oom_skiplist(&mut oom_skiplist, &skiplist_path);
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
                watermark = v.get("completed_files").and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32;
                let saved_sym = v.get("symbol_chunks").and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as u32;
                let saved_ref = v.get("ref_chunks").and_then(serde_json::Value::as_u64)
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
                // File-count drift between runs means the deterministic
                // walk order produced a different number of files for a
                // root, which silently shifts file_ids — any chunk record
                // referencing file_id N past the insertion point would
                // resolve to a different file than the one that was parsed
                // when the chunk was written. The resulting index would be
                // CORRUPT in a non-obvious way (lookups returning wrong
                // paths). We refuse to resume rather than warn-and-hope.
                let mut any_path_changed = false;
                let mut any_drift = false;
                let mut drift_details: Vec<String> = Vec::new();
                for (i, rj) in want.iter().enumerate() {
                    let want_path = rj.get("path").and_then(|x| x.as_str()).unwrap_or("");
                    let want_n = rj.get("n_files").and_then(serde_json::Value::as_u64).unwrap_or(0);
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
                        any_drift = true;
                        drift_details.push(format!(
                            "root[{}] {:+} files ({} now vs {} in progress)",
                            i, drift, cur.files.len(), want_n,
                        ));
                    }
                }
                if any_path_changed {
                    anyhow::bail!(
                        "resume: root path(s) changed — cannot continue. \
                         Remove {} and re-index without --resume.",
                        writer.tmp_dir.as_ref().unwrap().display(),
                    );
                }
                if any_drift {
                    anyhow::bail!(
                        "resume: file-count drift detected — refusing to continue \
                         because file_ids past the insertion point would shift \
                         and silently corrupt the index. Details:\n  {}\n\
                         Remove {} and re-index without --resume.",
                        drift_details.join("\n  "),
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

    // Anchor for whole-job progress. Used by the per-1000-file progress
    // line below so users see "N of TOTAL_FILES_TOTAL files (P%)" with a
    // throughput in files/sec — not just a per-batch counter that hides
    // how close indexing is to done on a 1 M-file corpus.
    let job_start = Instant::now();
    let mut files_done_at_root_start: u64 = 0;

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
            // Monotonic high-water mark over emitted progress milestones,
            // measured in units of progress_step. Workers race past
            // boundaries in parallel; without this they'd each see
            // `p % step == 0` for the same milestone and print duplicates
            // (observed in smoke test: "1000/51682" twice). fetch_max
            // ensures exactly one thread crosses any given milestone.
            let progress_milestone = Arc::new(AtomicU64::new(0));

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

            // Unified scheduling: every file goes through the parallel
            // pool. The historical "big" bucket was serialized to bound
            // peak transient RAM, but that cost 75+ seconds on a single
            // 27 MB generated CPP file while all 64 workers sat idle.
            // The user's explicit guidance is to USE the --mem-cap /
            // --workers budget and rely on the existing backpressure
            // stack: `await_memory_headroom` parks workers when
            // jemalloc-reported allocation exceeds 85 % of `--mem-cap`,
            // and if we still OOM the cgroup hard-kills and `--resume`
            // recovers from the last batch flush.
            //
            // Sort hint: smallest-first. Counter-intuitive but right —
            // largest-first would dispatch the multi-MB generated CPP
            // monsters to workers immediately, leaving 60 workers idle
            // waiting for the slowest 4 to finish 60–70 s parses.
            // Smallest-first keeps all workers saturated through the
            // bulk of the batch and only blocks on the giant tail in
            // the final seconds. Total wall-clock is dominated by the
            // slowest file regardless, but workers stay productive
            // until then instead of starving from the start.
            let mut all_items: Vec<_> = batch_files_slice
                .iter()
                .zip(batch_entries_slice.iter())
                .collect();
            all_items.sort_by_key(|(rf, _)| rf.size);
            let n_big = all_items.iter().filter(|(rf, _)| rf.size > big_file_bytes).count();
            if n_big > 0 {
                eprintln!(
                    "[route] batch {}: {} files ({} larger than {} processed parallel under mem-budget)",
                    batch_no, all_items.len(), n_big, human_bytes(big_file_bytes),
                );
            }

            // Per-worker accumulator. The parallel small-file pass uses
            // rayon's fold/reduce so each worker thread builds up its own
            // (syms, refs, trigrams) vecs across files in its work
            // share, and we only touch the global sinks O(workers) times
            // per batch instead of O(files). This removes the per-file
            // mutex-contention bottleneck that capped CPU at ~1700% on a
            // 64-worker pool. The serial big-file pass writes through
            // its own local accumulator and merges in once at the end.
            #[derive(Default)]
            struct LocalAccum {
                syms: Vec<SymbolRecord>,
                refs: Vec<RefRecord>,
                trigrams: Vec<(scry_store::trigram::Trigram, u32)>,
            }
            impl LocalAccum {
                fn merge(&mut self, other: LocalAccum) {
                    self.syms.extend(other.syms);
                    self.refs.extend(other.refs);
                    self.trigrams.extend(other.trigrams);
                }
            }

            // Helper closure — inlined parse + LOCAL accumulator push +
            // diagnostics. Called from both the parallel small pass and
            // the serial big pass. Big-bucket files get their path
            // recorded to a tmp sidecar BEFORE parse; if the cgroup
            // OOM-kills us mid-parse, the next --resume run reads this
            // and adds the file to oom_skiplist (self-healing —
            // pathological files exclude themselves after one OOM,
            // instead of looping forever on the same batch). When
            // workers=1 (the safest defensive config), small-bucket
            // files also mark_attempted since there's no concurrent
            // write contention.
            let last_attempted_path = writer.tmp_dir.as_ref()
                .map(|t| t.join("last_attempted.txt"));
            let process_one = |rf: &RawFile, fe: &FileEntry, mark_attempted: bool, acc: &mut LocalAccum| {
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
                            acc.trigrams.reserve(tgs.len());
                            for t in tgs { acc.trigrams.push((t, fe.id)); }
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
                        if !s.is_empty() { acc.syms.extend(s); }
                        if !r.is_empty() { acc.refs.extend(r); }
                        let p = parsed.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed);
                        let m = p / progress_step;
                        let prev = progress_milestone.fetch_max(m, Ordering::Relaxed);
                        if m > prev && p > 0 {
                            // Whole-job progress: files done across all
                            // roots / total walked. files/sec is over the
                            // full job (not just this batch) so it stays
                            // a useful ETA signal across batch boundaries.
                            let job_done = files_done_at_root_start + p;
                            let job_total = total_files_total.max(job_done);
                            let pct = if job_total > 0 {
                                (job_done as f64 / job_total as f64) * 100.0
                            } else { 100.0 };
                            let secs = job_start.elapsed().as_secs_f64().max(0.001);
                            let fps = job_done as f64 / secs;
                            let eta = if fps > 0.0 && job_total > job_done {
                                let remaining = (job_total - job_done) as f64 / fps;
                                format_eta(remaining)
                            } else { String::from("—") };
                            eprintln!(
                                "[progress] {}/{} files ({:.1}%) · {:.0} f/s · ETA {} · batch {} · {} syms · {} refs",
                                job_done, job_total, pct, fps, eta,
                                batch_no,
                                symbols_total.load(Ordering::Relaxed),
                                refs_total.load(Ordering::Relaxed),
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        // Parser-level error (tree-sitter returned no tree,
                        // I/O failure reading the file, format registry
                        // refused the kind, etc.). Logged at one line per
                        // file so operators can `grep ^\[fail\]` the build
                        // log and triage what didn't parse. The counter
                        // still increments so the final DONE line reports
                        // it under `files_failed`.
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "[fail] {} kind={:?} size={} reason={}",
                            rf.path.display(), rf.kind,
                            human_bytes(rf.size), e,
                        );
                    }
                    Err(panic_payload) => {
                        // catch_unwind caught a panic inside tree-sitter or
                        // one of the extractors. The payload is usually
                        // String / &str; format defensively in case it
                        // isn't, so we always print something useful.
                        failed.fetch_add(1, Ordering::Relaxed);
                        let msg: String = if let Some(s) = panic_payload.downcast_ref::<String>() {
                            s.clone()
                        } else if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                            (*s).to_string()
                        } else {
                            String::from("<panic payload not a string>")
                        };
                        eprintln!(
                            "[fail-panic] {} kind={:?} size={} panic={}",
                            rf.path.display(), rf.kind,
                            human_bytes(rf.size), msg,
                        );
                    }
                }
            };

            // Unified parallel pass: every file (small or big) goes
            // through fold + reduce per-worker accumulation. With the
            // largest-first sort above, the work-stealing queue picks
            // up the most expensive parses first; smaller files fill
            // in around them. Marks-attempted is OFF unless workers=1
            // (with N parallel writers to last_attempted.txt the file
            // is racy and recovers the wrong path on OOM); we rely on
            // cgroup + --resume + skiplist-probe instead.
            let marks_attempted = rayon::current_num_threads() == 1;
            let batch_accum: LocalAccum = all_items.par_iter()
                .fold(
                    LocalAccum::default,
                    |mut acc, (rf, fe)| {
                        process_one(rf, fe, marks_attempted, &mut acc);
                        acc
                    },
                )
                .reduce(
                    LocalAccum::default,
                    |mut a, b| { a.merge(b); a },
                );
            // Clear the last-attempted marker after the batch finished
            // — only an UNCLEARED marker on resume means OOM during
            // workers=1 mode.
            if let Some(p) = last_attempted_path.as_ref() {
                let _ = std::fs::remove_file(p);
            }

            // Merge the batch accumulator back into the writer. Three
            // appends total per batch regardless of file count or
            // worker count — this is the contention fix landed earlier.
            // (syms_sink / refs_sink / trigrams_sink are now unused
            // for the parse pass; we keep them only as the pre-existing
            // storage that writer.symbols / writer.refs were
            // temporarily taken from at the top of the batch.)
            let mut combined_syms = syms_sink.into_inner();
            let mut combined_refs = refs_sink.into_inner();
            let mut combined_trigrams = trigrams_sink.into_inner();
            combined_syms.reserve(batch_accum.syms.len());
            combined_syms.extend(batch_accum.syms);
            combined_refs.reserve(batch_accum.refs.len());
            combined_refs.extend(batch_accum.refs);
            combined_trigrams.reserve(batch_accum.trigrams.len());
            combined_trigrams.extend(batch_accum.trigrams);
            writer.symbols = combined_syms;
            writer.refs = combined_refs;
            // Drain the trigram accumulator into the writer's pending buffer.
            if build_trigrams && !combined_trigrams.is_empty() {
                if let Some(buf) = writer.trigrams.as_mut() {
                    buf.reserve(combined_trigrams.len());
                    buf.extend(combined_trigrams);
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
            // Roll the whole-job counter forward by THIS batch's contribution
            // (parsed + failed = every attempted file). Done after the batch
            // log so the next batch's [progress] line reflects post-batch
            // state — important when a batch lands hundreds of files past
            // the last 1000-step boundary.
            files_done_at_root_start += parsed_n + failed_n;

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

/// Re-test each entry in the OOM skiplist with the current binary and the
/// current per-parse timeout. Drop entries that now parse cleanly (or that
/// no longer exist / no longer classify as source). Self-heals stale
/// entries from older binaries that didn't have the parse timeout.
///
/// Safe to call unconditionally: the per-parse timeout caps any retry at
/// ~5 s, so even an entry that truly hangs the parser can't reblock the
/// indexer. If the retry succeeds, the file goes back into the rotation;
/// if it times out again, the entry stays in the skiplist for the run.
fn probe_oom_skiplist(
    skiplist: &mut std::collections::HashSet<String>,
    skiplist_path: &Path,
) {
    if skiplist.is_empty() { return; }
    let probe_start = Instant::now();
    let mut to_drop: Vec<String> = Vec::new();
    for path_str in skiplist.iter() {
        let p = Path::new(path_str);
        if !p.exists() {
            to_drop.push(path_str.clone());
            continue;
        }
        let kind = match FileKind::classify(p) {
            Some(k) => k,
            None => {
                // File still exists but no longer classifies as a source
                // we care about — irrelevant going forward.
                to_drop.push(path_str.clone());
                continue;
            }
        };
        let md = match std::fs::metadata(p) { Ok(m) => m, _ => continue };
        // Skip the probe for very large files — they'd time out anyway and
        // probing isn't free. The walker's per-file size cap is what bounds
        // these at index time.
        if md.len() > 10 * 1024 * 1024 { continue; }
        let bytes = match std::fs::read(p) { Ok(b) => b, _ => continue };
        if scry_lang::extract(kind, &bytes).is_ok() {
            to_drop.push(path_str.clone());
        }
    }
    let drop_count = to_drop.len();
    for p in &to_drop { skiplist.remove(p); }
    if drop_count > 0 {
        let new_contents = if skiplist.is_empty() {
            String::new()
        } else {
            let mut v: Vec<&String> = skiplist.iter().collect();
            v.sort();
            v.into_iter().cloned().collect::<Vec<_>>().join("\n") + "\n"
        };
        let _ = std::fs::write(skiplist_path, new_contents);
        eprintln!(
            "[resume] OOM skiplist probe: dropped {} stale entry/entries ({} ms; {} remain)",
            drop_count, probe_start.elapsed().as_millis(), skiplist.len(),
        );
    } else {
        eprintln!(
            "[resume] OOM skiplist probe: all {} entries still problematic ({} ms)",
            skiplist.len(), probe_start.elapsed().as_millis(),
        );
    }
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
    let (mut raw_syms, raw_refs) = scry_lang::with_current_file(
        rf.path.display().to_string(),
        || registry.parse(rf.kind, &bytes),
    );
    // Path-aware post-processing. Today only AIDL needs it: a parse
    // emits AidlInterface; if the source lives under aidl_api/<pkg>/<N>/
    // it's actually a frozen-version snapshot, which we promote to
    // AidlFrozen so agents can filter by surface version vs the live
    // development copy. The parser doesn't see the path, hence here.
    if rf.kind == FileKind::Aidl && scry_aosp::aidl::is_frozen_path(&path_str) {
        scry_aosp::aidl::apply_frozen_post(&mut raw_syms);
    }
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
    // Friendlier error for the most common first-run failure mode:
    // user runs `scry def Foo` before ever running `scry index`.
    // Without this, they see `open manifest.json: No such file or
    // directory (os error 2)` and don't know what to do next.
    if !p.exists() {
        anyhow::bail!(
            "no scry index at {}\n\n\
             scry queries need a pre-built index. Build one with:\n  \
               scry index <SOURCE_ROOT> -o {}\n\n\
             For AOSP + Linux corpora the full build takes ~13 min on 16 workers;\n\
             see docs/USAGE.md for incremental rebuilds and OPERATIONS.md for the\n\
             production recipe.",
            p.display(), p.display(),
        );
    }
    let reader = StoreReader::open(&p)
        .with_context(|| format!("open index {}", p.display()))?;
    warn_if_index_stale(&reader);
    Ok(reader)
}

/// Emit a one-line stderr warning if the index was built with a
/// different scry version than the running binary. Silent on match,
/// silent on absent `scry_version` field (very old indexes); never
/// fails or blocks the query. Set `SCRY_QUIET=1` to suppress.
///
/// Triggered automatically on every index open so users don't have
/// to remember to run `scry health` themselves — the most common
/// stale-index symptom (e.g. the pre-0.1.2 Java scope_path bug)
/// surfaces the moment it could mislead a query result.
fn warn_if_index_stale(r: &StoreReader) {
    if std::env::var("SCRY_QUIET").is_ok_and(|v| !v.is_empty()) {
        return;
    }
    let built_with = r.manifest.scry_version.as_str();
    let running = env!("CARGO_PKG_VERSION");
    if built_with.is_empty() || built_with == running {
        return;
    }
    // Patch-level mismatches (0.1.17 vs 0.1.24, etc.) are mostly
    // bugfix releases that DON'T require an index rebuild. Don't
    // warn for those — only flag major.minor drift where the
    // on-disk format or query semantics actually shifted.
    let bw_mm = major_minor(built_with);
    let rn_mm = major_minor(running);
    if bw_mm == rn_mm { return; }
    eprintln!(
        "[scry] WARNING: this index was built with scry {built_with}; \
         running {running}. Older builds may have stale records (e.g. the \
         Java/C++ scope_path bug fixed in 0.1.2). Rebuild with `scry index \
         <ROOT> -o {}` or `scry index --incremental <ROOT> -o {}`. \
         Suppress this warning with SCRY_QUIET=1.",
        r.paths.root.display(), r.paths.root.display(),
    );
}

/// `"0.1.17"` → `Some("0.1")`. Returns the input unchanged if it
/// can't be split (preserves the conservative-warning shape for
/// non-semver tags).
fn major_minor(v: &str) -> &str {
    let mut dots = 0;
    for (i, c) in v.char_indices() {
        if c == '.' {
            dots += 1;
            if dots == 2 { return &v[..i]; }
        }
    }
    v
}

fn cmd_def(
    name: String,
    index: Option<PathBuf>,
    lang: Option<String>,
    kind: Option<String>,
    in_: Option<String>,
    not_in: Option<String>,
    limit: usize,
    json: bool,
    md: bool,
    budget: Option<usize>,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let results = r.lookup_exact(&name);
    let mut filtered: Vec<SymbolRecord> = filter_results(results, lang.as_deref(), kind.as_deref());
    if in_.is_some() || not_in.is_some() {
        filtered.retain(|s| match r.display_path_cached(s.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => false,
        });
    }
    // v0.1.50 — collapse Package symbols by (name, lang) to one entry
    // per unique package. Each Java/Kotlin file emits its own
    // package_declaration → without dedup, `def android.os` returns
    // 352 visibly-identical rows. JSON mode preserves all entries for
    // programmatic consumers; only the human-readable + markdown
    // paths collapse.
    if !json {
        dedupe_package_symbols(&mut filtered);
    }
    rank_symbols(&mut filtered, &r);
    if md {
        print_results_md(&r, &filtered, limit, budget);
    } else {
        print_results(&r, &filtered, limit, json);
    }
    // v0.1.52 — fuzzy "did you mean" hint on 0 hits. Only fires when
    // NO filter was passed; with --in/--not-in/--lang/--kind set, a
    // 0 hit means "filtered out", not "name unknown", so the fuzzy
    // suggestion would mislead.
    if filtered.is_empty()
        && in_.is_none() && not_in.is_none()
        && lang.is_none() && kind.is_none()
    {
        if let Some(hint) = suggest_similar(&r, &name) {
            eprintln!("[scry] {hint}");
        }
    }
    log_query(&r, "def", &name, filtered.len(), filtered.len().min(limit), t);
    Ok(())
}

// `cmd_finalize` + the auto-discovery helper live in
// crate::finalize.

/// `scry callgraph NAME` — recursive callers tree.
///
/// At each level we ask `lookup_refs_exact(name) → kind=call`,
/// take the enclosing function (`scope_path.last()`) as the
/// caller, and recurse on its name. A `BTreeMap<String, Node>`
/// dedups repeats; a global node cap (`--max-nodes`) plus the
/// `--depth` cap bound the work on hub functions (e.g. `log()`,
/// `assert`).
#[allow(clippy::too_many_arguments)]
fn cmd_callgraph(
    name: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    depth: usize,
    max_nodes: usize,
    reachable: bool,
    def_in: Option<String>,
    strict: bool,
    lexical: bool,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;

    // Optional reachability pruning: precompute callee modules
    // (set of modules that define `name`).
    let callee_modules: Option<std::collections::HashSet<u32>> = if reachable {
        r.module_graph().map(|mg| {
            r.lookup_exact(&name)
                .iter()
                .filter_map(|s| mg.module_of_file(s.file_id))
                .collect()
        })
    } else { None };

    // v0.1.43 — --def-in PATH narrows ROOT-LEVEL callers by the
    // callee's def location. Same shape as cmd_ref's --def-in.
    // Empty target set ⇒ diagnostic + no narrowing.
    let root_def_target_ids: Option<std::collections::HashSet<u64>> =
        def_in.as_deref().map(|p| {
            let ids: std::collections::HashSet<u64> = r.lookup_exact(&name)
                .iter()
                .filter(|s| r.display_path_cached(s.file_id)
                    .is_some_and(|dp| dp.contains(p)))
                .map(|s| s.id)
                .collect();
            if ids.is_empty() {
                eprintln!(
                    "[scry] --def-in: no def of {name:?} found in any file \
                     containing {p:?}; root-level callers will not be narrowed.",
                );
            }
            ids
        });

    // Root-level precision filter (clang USR + SCIP symbol identity).
    // Auto-engages when sidecars present unless --lexical was passed.
    // Deeper recursion stays lexical because callee names at depth >0
    // are caller-function names — those are def-style queries, not the
    // same NAME the user asked about, and their precision answer would
    // need a separate sidecar lookup per name (not free).
    let (_reach_unused, clang_precise, scip_precise) =
        resolve_precision(lexical, false, false, false);
    let root_precise_sites: Option<std::collections::HashSet<(u32, u32)>> =
        if !lexical && (clang_precise || scip_precise) {
            let raw_root = r.lookup_refs_exact(&name);
            let kept = apply_precision_filter(
                &r, &name, raw_root, clang_precise, scip_precise,
            )?;
            Some(kept.into_iter().map(|rr| (rr.file_id, rr.byte_start)).collect())
        } else {
            None
        };

    /// One node in the callers tree. Children are callers of this
    /// function (i.e. parents on the call stack).
    #[derive(Debug, Default, serde::Serialize)]
    struct Node {
        /// Number of distinct call sites pointing at this name's parent.
        call_sites: usize,
        /// At most one example site for human-readable output.
        first_site: Option<(String, u32, u32)>,
        /// Callers of THIS function — same shape, recursive.
        callers: std::collections::BTreeMap<String, Node>,
    }

    #[allow(clippy::too_many_arguments)]
    fn expand(
        r: &StoreReader,
        callee: &str,
        depth_left: usize,
        in_prefix: &str,
        not_in_prefix: &str,
        callee_modules: Option<&std::collections::HashSet<u32>>,
        // v0.1.43: root-level only. Some(set) ⇒ filter by Layer 2
        // resolved_to ∈ set (with strict toggle). None ⇒ no filter
        // (also the case for all non-root recursive levels).
        root_def_target_ids: Option<&std::collections::HashSet<u64>>,
        // Root-level precision filter: Some(set of (file_id, byte_start))
        // ⇒ keep only refs whose site is in the set. None at deeper
        // recursion levels (precision is not threaded down — see
        // root_precise_sites computation in cmd_callgraph).
        root_precise_sites: Option<&std::collections::HashSet<(u32, u32)>>,
        strict: bool,
        visited: &mut std::collections::HashSet<String>,
        budget: &mut usize,
    ) -> std::collections::BTreeMap<String, Node> {
        if depth_left == 0 || *budget == 0 { return Default::default(); }
        if !visited.insert(callee.to_string()) {
            return Default::default();
        }
        let mut out: std::collections::BTreeMap<String, Node> = std::collections::BTreeMap::new();
        for rr in r.lookup_refs_exact(callee).into_iter() {
            if rr.kind != scry_store::RefKind::Call { continue; }
            if !in_prefix.is_empty() || !not_in_prefix.is_empty() {
                let Some(p) = r.display_path_cached(rr.file_id) else { continue };
                if !in_prefix.is_empty() && !p.contains(in_prefix) { continue; }
                if !not_in_prefix.is_empty() && p.contains(not_in_prefix) { continue; }
            }
            // Reachability filter on the caller side.
            if let (Some(mg), Some(cms)) =
                (r.module_graph(), callee_modules) {
                if !cms.is_empty() {
                    if let Some(caller_mod) = mg.module_of_file(rr.file_id) {
                        if !cms.iter().any(|cm| mg.is_reachable(caller_mod, *cm)) {
                            continue;
                        }
                    }
                }
            }
            // Root-level --def-in / --strict filter (v0.1.43).
            // Non-root recursive levels skip this branch because
            // root_def_target_ids is None then.
            if let Some(tids) = root_def_target_ids {
                if !tids.is_empty() {
                    match rr.resolved_to {
                        Some(id) if !tids.contains(&id) => continue,
                        None if strict => continue,
                        _ => {}
                    }
                }
            }
            if root_def_target_ids.is_none() && strict && rr.resolved_to.is_none() {
                continue;
            }
            // Root-level precision filter (clang USR / SCIP symbol
            // identity). Only Some at the topmost call.
            if let Some(sites) = root_precise_sites {
                if !sites.contains(&(rr.file_id, rr.byte_start)) {
                    continue;
                }
            }
            // Prefer the byte-range enclosing function (more accurate
            // than scope_path.last() which reports the class on Java).
            // Fall back to scope_path when file_symbols is missing.
            let caller_name = r.enclosing_function(rr.file_id, rr.byte_start)
                .map(|s| s.name)
                .or_else(|| rr.scope_path.last().cloned());
            let Some(caller_name) = caller_name else { continue };
            let entry = out.entry(caller_name.clone()).or_default();
            entry.call_sites += 1;
            if entry.first_site.is_none() {
                let path = r.file_display_path(rr.file_id).unwrap_or_default();
                entry.first_site = Some((path, rr.line, rr.col));
            }
            *budget = budget.saturating_sub(1);
            if *budget == 0 { break; }
        }
        // Recurse into each caller, expanding their callers. Pass
        // None for root_def_target_ids / root_precise_sites so the
        // narrowing only fires at the topmost level (we don't have
        // per-frame def or per-name precision context).
        for (caller_name, node) in &mut out {
            node.callers = expand(
                r, caller_name, depth_left - 1, in_prefix, not_in_prefix,
                callee_modules, None, None, strict, visited, budget,
            );
        }
        visited.remove(callee);
        out
    }

    let mut budget = max_nodes;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let prefix = in_.as_deref().unwrap_or("");
    let neg_prefix = not_in.as_deref().unwrap_or("");
    let tree = expand(&r, &name, depth, prefix, neg_prefix, callee_modules.as_ref(),
                      root_def_target_ids.as_ref(), root_precise_sites.as_ref(),
                      strict, &mut visited, &mut budget);

    if json {
        println!("{}", serde_json::json!({
            "callee": name,
            "depth": depth,
            "max_nodes": max_nodes,
            "callers": tree,
        }));
    } else {
        println!("callgraph (incoming, depth {depth}) of {name:?}:");
        fn render(
            out: &std::collections::BTreeMap<String, Node>,
            indent: usize,
        ) {
            for (k, v) in out {
                let site = v.first_site.as_ref()
                    .map(|(p, l, c)| format!(" — {p}:{l}:{c}"))
                    .unwrap_or_default();
                println!(
                    "{:indent$}{} ({} call site{}){}",
                    "", k, v.call_sites,
                    if v.call_sites == 1 { "" } else { "s" },
                    site,
                    indent = indent,
                );
                render(&v.callers, indent + 2);
            }
        }
        if tree.is_empty() {
            println!("  (no callers found)");
            // v0.1.54 — typo hint when nothing matched. Gated on
            // no narrowing flags (same logic as cmd_def / cmd_ref).
            if in_.is_none() && not_in.is_none() && def_in.is_none() {
                if let Some(hint) = suggest_similar(&r, &name) {
                    eprintln!("[scry] {hint}");
                }
            }
        } else {
            render(&tree, 2);
        }
        eprintln!(
            "[scry] cmd=callgraph q={:?} depth={} nodes_used={} elapsed={}ms",
            name, depth, max_nodes - budget, t.elapsed().as_millis(),
        );
    }
    Ok(())
}

/// `scry impact NAME` — composes callers + transitive subclasses
/// into a single deduped impact set. The reported counts are what
/// you'd need to review if you renamed/changed NAME's signature.
///
/// Output: a stdout summary (or JSON) listing
///   - direct callers (RefRecord with kind=call)
///   - subclasses at depth ≤ `subclass_depth`
///   - the union of files those two sets touch
///
/// With `--reachable`, the callers list is build-graph-pruned (same
/// semantics as `scry callers --reachable`). The subclass set is not
/// reachability-filtered because inheritance edges don't respect
/// module deps (a child class can live anywhere that imports the
/// parent's header).
#[allow(clippy::too_many_arguments)]
fn cmd_impact(
    name: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    subclass_depth: usize,
    reachable: bool,
    def_in: Option<String>,
    strict: bool,
    lexical: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;

    // Callers — lookup_refs_exact, filter by kind=call, then apply
    // build-symbol precision (auto-on when sidecars are present —
    // `lexical` opt-out skips it) and path filters.
    let raw_callers: Vec<RefRecord> = r.lookup_refs_exact(&name)
        .into_iter()
        .filter(|rr| rr.kind == scry_store::RefKind::Call)
        .collect();
    let (_reach, clang_precise, scip_precise) =
        resolve_precision(lexical, false, false, false);
    let callers_precise = apply_precision_filter(
        &r, &name, raw_callers, clang_precise, scip_precise,
    )?;
    let mut callers: Vec<RefRecord> = callers_precise.into_iter()
        .filter(|rr| match r.display_path_cached(rr.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => in_.is_none() && not_in.is_none(),
        })
        .collect();
    // v0.1.45 — narrow callers by callee location (same as ref --def-in).
    // Doesn't affect subclasses (which are about the type, not the method).
    if let Some(def_path) = def_in.as_deref() {
        let target_ids: std::collections::HashSet<u64> = r.lookup_exact(&name)
            .iter()
            .filter(|s| r.display_path_cached(s.file_id)
                .is_some_and(|dp| dp.contains(def_path)))
            .map(|s| s.id)
            .collect();
        if target_ids.is_empty() {
            eprintln!(
                "[scry] impact --def-in: no def of {name:?} found in any file \
                 containing {def_path:?}; callers will not be narrowed.",
            );
        } else {
            let before = callers.len();
            callers.retain(|rr| match rr.resolved_to {
                Some(id) => target_ids.contains(&id),
                None => !strict,
            });
            eprintln!(
                "[scry] impact --def-in {def_path:?}{}: {} → {} callers",
                if strict { " --strict" } else { "" },
                before, callers.len(),
            );
        }
    } else if strict {
        let before = callers.len();
        callers.retain(|rr| rr.resolved_to.is_some());
        eprintln!(
            "[scry] impact --strict: {} → {} callers (unresolved dropped)",
            before, callers.len(),
        );
    }
    if reachable {
        if let Some(mg) = r.module_graph() {
            let defs = r.lookup_exact(&name);
            let callee_modules: std::collections::HashSet<u32> = defs.iter()
                .filter_map(|s| mg.module_of_file(s.file_id)).collect();
            if !callee_modules.is_empty() {
                callers.retain(|rr| match mg.module_of_file(rr.file_id) {
                    Some(cm) => callee_modules.iter().any(|m| mg.is_reachable(cm, *m)),
                    None => true,
                });
            }
        }
    }

    // Subclasses — transitive BFS, then path filter.
    let subclasses: Vec<SymbolRecord> = r
        .subclasses_transitive(&name, subclass_depth)
        .into_iter()
        .filter(|s| match r.display_path_cached(s.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => in_.is_none() && not_in.is_none(),
        })
        .collect();

    // Affected files: union of caller files + subclass files.
    let mut files_touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rr in &callers {
        if let Some(p) = r.display_path_cached(rr.file_id) {
            files_touched.insert(p.to_string());
        }
    }
    for s in &subclasses {
        if let Some(p) = r.display_path_cached(s.file_id) {
            files_touched.insert(p.to_string());
        }
    }

    if json {
        let v = serde_json::json!({
            "name": name,
            "callers": callers.iter().take(limit).map(|rr| ref_to_json(&r, rr))
                .collect::<Vec<_>>(),
            "subclasses": subclasses.iter().take(limit).map(|s| symbol_to_json(&r, s))
                .collect::<Vec<_>>(),
            "files_touched": files_touched.iter().take(limit).collect::<Vec<_>>(),
            "totals": {
                "callers": callers.len(),
                "subclasses": subclasses.len(),
                "files_touched": files_touched.len(),
            },
        });
        println!("{v}");
    } else {
        println!(
            "impact of {name:?}: {} callers, {} subclasses (depth {}), {} files touched",
            callers.len(), subclasses.len(), subclass_depth, files_touched.len(),
        );
        // v0.1.54 — typo hint when impact yields nothing AND no filter
        // narrowed the search. Helps users who typo the name (the no-op
        // shape would otherwise be silently confusing).
        if callers.is_empty() && subclasses.is_empty()
            && in_.is_none() && not_in.is_none() && def_in.is_none()
        {
            if let Some(hint) = suggest_similar(&r, &name) {
                eprintln!("[scry] {hint}");
            }
        }
        if !callers.is_empty() {
            println!("\n== callers (showing {}) ==", callers.len().min(limit));
            for rr in callers.iter().take(limit) {
                let path = r.display_path_cached(rr.file_id).unwrap_or("<unknown>");
                println!("  {path}:{}:{}  {}", rr.line, rr.col,
                    rr.scope_path.last().map_or("", String::as_str));
            }
        }
        if !subclasses.is_empty() {
            println!("\n== subclasses (showing {}) ==", subclasses.len().min(limit));
            for s in subclasses.iter().take(limit) {
                let path = r.display_path_cached(s.file_id).unwrap_or("<unknown>");
                println!("  {path}:{}:{}  {}", s.line, s.col, s.name);
            }
        }
        eprintln!(
            "[scry] cmd=impact q={:?} callers={} subclasses={} files={} elapsed={}ms",
            name, callers.len(), subclasses.len(), files_touched.len(),
            t.elapsed().as_millis(),
        );
    }
    Ok(())
}

/// `scry subclasses NAME` / `scry implementations NAME` — type-hierarchy
/// lookup. `depth = 0` returns direct children; higher walks transitively.
/// Output is one-line-per-child in the same scheme as `scry def`, or
/// JSON when --json is set.
fn cmd_subclasses(
    name: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    depth: usize,
    format: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    if let Some(f) = format.as_deref() {
        if !matches!(f, "count" | "paths") {
            anyhow::bail!("--format must be 'count' or 'paths' (got '{f}')");
        }
    }
    if json && format.as_deref() == Some("count") {
        anyhow::bail!("--json and --format=count are mutually exclusive");
    }
    let t = Instant::now();
    let r = open_index(index)?;
    let results = if depth == 0 {
        r.subclasses(&name)
    } else {
        r.subclasses_transitive(&name, depth)
    };
    let mut filtered: Vec<SymbolRecord> = results.into_iter()
        .filter(|s| match r.display_path_cached(s.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => in_.is_none() && not_in.is_none(),
        })
        .collect();
    rank_symbols(&mut filtered, &r);
    // v0.1.58 — --format count / paths (symmetric with ref/callers/uses).
    match format.as_deref() {
        Some("count") => {
            println!("{} subclasses", filtered.len());
        }
        Some("paths") => {
            print_symbols_paths(&r, &filtered, limit, json);
        }
        _ => {
            print_results(&r, &filtered, limit, json);
        }
    }
    // v0.1.54 — fuzzy "Did you mean" hint when no subclasses found.
    // Gated on no filter narrowing the search: with --in set, an empty
    // result means "no subclass in that subtree", not "name unknown".
    if filtered.is_empty() && in_.is_none() && not_in.is_none() {
        if let Some(hint) = suggest_similar(&r, &name) {
            eprintln!("[scry] {hint}");
        }
    }
    if !json && format.is_none() {
        eprintln!(
            "[scry] cmd=subclasses q={:?} depth={} hits={} shown={} elapsed={}ms",
            name, depth, filtered.len(),
            filtered.len().min(limit), t.elapsed().as_millis(),
        );
    }
    Ok(())
}

fn cmd_prefix(
    prefix: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    // Over-fetch then rank; the FST gives unordered hits and the limit
    // should land on the BEST matches, not just the first ones the FST
    // happens to encounter.
    let cap = limit.saturating_mul(8).max(limit);
    let mut results = r.lookup_prefix(&prefix, cap);
    if in_.is_some() || not_in.is_some() {
        results.retain(|s| match r.display_path_cached(s.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => false,
        });
    }
    rank_symbols(&mut results, &r);
    let shown = limit.min(results.len());
    print_results(&r, &results[..shown], limit, json);
    log_query(&r, "prefix", &prefix, results.len(), shown, t);
    Ok(())
}

fn cmd_fuzzy(
    substr: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    distance: u32,
    limit: usize,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    // Ranked path: substring matches + Levenshtein-bounded matches,
    // deduped, re-sorted by exact Wagner-Fischer distance. Apply
    // --in / --not-in AFTER the ranked walk so the ranker sees the
    // full candidate set first; the path filters are then a cheap
    // substring test on the (typically small) ranked output.
    let mut scored: Vec<(SymbolRecord, u32)> = r.lookup_fuzzy_ranked(&substr, distance, limit);
    if in_.is_some() || not_in.is_some() {
        scored.retain(|(s, _)| match r.display_path_cached(s.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => false,
        });
    }
    let shown = scored.len();
    print_fuzzy_results(&r, &scored, json);
    log_query(&r, "fuzzy", &substr, shown, shown, t);
    Ok(())
}

/// Print a fuzzy result set. Distance is shown alongside each hit so
/// callers can see *why* a particular entry ranked where it did.
fn print_fuzzy_results(r: &StoreReader, scored: &[(SymbolRecord, u32)], json: bool) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if json {
        for (s, d) in scored {
            let mut j = symbol_to_json(r, s);
            j.as_object_mut().unwrap()
                .insert("distance".to_string(), serde_json::json!(d));
            let _ = writeln!(out, "{}", j);
        }
        return;
    }
    for (s, d) in scored {
        let path = r.display_path_cached(s.file_id).unwrap_or("");
        let scope = if s.scope_path.is_empty() {
            String::new()
        } else {
            format!("  [{}]", s.scope_path.join("."))
        };
        let _ = writeln!(out, "{}:{}:{}  (d={})  ({} {}){}  {}",
            path, s.line, s.col, d,
            s.kind.short(), s.lang.as_str(), scope, s.name);
    }
    let _ = writeln!(out, "\n{} result{} (showing {})",
        scored.len(),
        if scored.len() == 1 { "" } else { "s" },
        scored.len());
}

/// Resolve the precision flags. Default-on precision picks up
/// clang USR + SCIP identity filters (both cheap: small sidecars,
/// O(1) hash lookups, no-op if missing). Build-graph reachability
/// (`--reachable`) stays explicit opt-in because the AOSP
/// `module_graph.json` is 256MB and its eager parse + Warshall
/// closure costs ~30s cold — paying that on every CLI invocation
/// would crush the per-query latency the user expects from a
/// grep-class tool. `--lexical` turns everything off, leaving
/// pure tree-sitter name match.
fn resolve_precision(
    lexical: bool,
    explicit_reachable: bool,
    _explicit_clang: bool,
    _explicit_scip: bool,
) -> (bool, bool, bool) {
    if lexical {
        return (false, false, false);
    }
    // Cheap filters auto-engage; expensive reachability is opt-in.
    // `_explicit_clang` / `_explicit_scip` are accepted from older
    // hidden flags but ignored — clang+scip are already on by
    // default. Setting them again is a no-op.
    (explicit_reachable, true, true)
}

/// Apply build-symbol precision filtering (clang USR + SCIP symbol
/// identity) to a candidate set of refs.
///
/// **Strict-by-default semantics (Kythe parity).** When precision is
/// engaged we treat every ref as guilty until proven innocent: a ref
/// survives only if its byte-position resolves to the same identity
/// (USR / SCIP symbol) as one of NAME's defs. The previous
/// "permissive over-include when uncovered" behaviour was a
/// tree-sitter heuristic — it kept refs whose TU the build indexer
/// hadn't seen, masking the gap and silently degrading precision.
/// That fallback is gone; the only way back to tree-sitter
/// behaviour is `--lexical`, which short-circuits this function
/// entirely.
///
/// **Per-language sidecar policy.** C/C++/ObjC files use the clang
/// USR sidecar; everything else uses SCIP. The two filters operate
/// independently:
///   - C-family ref + clang sidecar present  →  must match a def USR.
///   - C-family ref + clang sidecar absent   →  unverifiable; drop.
///   - Non-C ref     + SCIP sidecar present  →  must match a def sym.
///   - Non-C ref     + SCIP sidecar absent   →  unverifiable; drop.
///
/// **Hard error only when BOTH sidecars are absent AND both filters
/// were requested.** Otherwise we silently drop unverifiable refs
/// (with a diagnostic). This lets a Python-only or Rust-only index
/// run the default-on precision path without insisting on
/// `clang_usrs.bin`, while still refusing to silently degrade to
/// tree-sitter when nothing precision-grade exists.
pub(crate) fn apply_precision_filter(
    r: &StoreReader,
    name: &str,
    refs: Vec<RefRecord>,
    clang_precise: bool,
    scip_precise: bool,
) -> Result<Vec<RefRecord>> {
    if !clang_precise && !scip_precise {
        return Ok(refs);
    }
    // Window covers the offset drift between tree-sitter (identifier
    // position) and the indexer's cursor position (clang sometimes
    // sits at the keyword for class/struct/typedef). 64 bytes covers
    // every real-world identifier without bridging adjacent decls.
    const WINDOW: u32 = 64;
    let defs = r.lookup_exact(name);

    // Cached lazy accessors. First call per process decodes the
    // sidecar (~17 s on AOSP-scale SCIP); subsequent calls borrow
    // the in-memory index. Daemon (`serve`/`mcp`) pays the cost
    // once at startup, CLI pays it per process until the packed-
    // mmap follow-up lands.
    let cusr_opt: Option<&scry_store::clang_usrs::ClangUsrIndex> = if clang_precise {
        r.clang_usrs()
    } else {
        None
    };
    let sidx_opt: Option<&scry_store::scip_index::ScipIndex> = if scip_precise {
        r.scip_index()
    } else {
        None
    };

    if cusr_opt.is_none() && sidx_opt.is_none() {
        anyhow::bail!(
            "precision query but no precision sidecars at {} (looked for \
             clang_usrs.bin and scip_index.bin). Run \
             `scry clang-index --compile-commands FILE --index DIR` or \
             `scry scip-import --scip FILE.scip --index DIR` first, or \
             pass `--lexical` to opt into tree-sitter name match.",
            r.paths.clang_usrs().parent().map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    }

    // Build a per-file_id cache of (display_path, is_c_family) so the
    // per-ref filter loop below avoids reallocating both on every
    // record. On AOSP+kernel scale this loop fires for 100k+ refs;
    // hoisting display_path turns the inner cost from "string alloc
    // + HashMap<String> hash + strcmp" into "Vec::get + bool check".
    let n_files = r.file_count();
    let mut path_by_id: Vec<Option<(String, bool)>> = vec![None; n_files];
    for fe in r.iter_files() {
        let p = fe.display_path();
        let cf = is_c_family(&p);
        path_by_id[fe.id as usize] = Some((p, cf));
    }
    // Drive the sidecar precompute step from the same cache so each
    // sidecar's per-file lookup table is also file_id-indexed.
    let cusr_lookup = cusr_opt.map(|cusr| {
        cusr.precompute_by_file_ids(
            path_by_id.iter().enumerate().filter_map(|(i, slot)| {
                slot.as_ref().map(|(p, _)| (i as u32, p.as_str()))
            }),
            n_files,
        )
    });
    let sidx_lookup = sidx_opt.map(|sidx| {
        sidx.precompute_by_file_ids(
            path_by_id.iter().enumerate().filter_map(|(i, slot)| {
                slot.as_ref().map(|(p, _)| (i as u32, p.as_str()))
            }),
            n_files,
        )
    });

    // Gather def identities once per sidecar. Empty sets are fine:
    // they signal "this name has no defs the sidecar attributes to a
    // symbol", which then forces every ref in that family to drop
    // (Kythe parity — no def attribution means no ref attribution).
    let def_usrs: std::collections::HashSet<String> = match &cusr_lookup {
        Some(cl) => defs.iter()
            .filter_map(|s| {
                let (_, cf) = path_by_id.get(s.file_id as usize)?.as_ref()?;
                if !cf { return None; }
                cl.usr_for_window(s.file_id, s.byte_start, WINDOW)
                    .map(str::to_string)
            })
            .collect(),
        None => Default::default(),
    };
    let def_syms: std::collections::HashSet<String> = match &sidx_lookup {
        Some(sl) => defs.iter()
            .filter_map(|s| {
                let (_, cf) = path_by_id.get(s.file_id as usize)?.as_ref()?;
                if *cf { return None; }
                sl.symbol_for_window(s.file_id, s.byte_start, WINDOW)
                    .map(str::to_string)
            })
            .collect(),
        None => Default::default(),
    };

    let before = refs.len();
    let mut c_dropped_uncov = 0usize;
    let mut c_dropped_id = 0usize;
    let mut s_dropped_uncov = 0usize;
    let mut s_dropped_id = 0usize;
    let kept: Vec<RefRecord> = refs.into_iter().filter(|rr| {
        let Some((_, c_family)) = path_by_id
            .get(rr.file_id as usize).and_then(|o| o.as_ref())
        else { return false; };
        if *c_family {
            // C-family ref: clang sidecar owns the verdict.
            let Some(cl) = cusr_lookup.as_ref() else {
                c_dropped_uncov += 1;
                return false;
            };
            match cl.usr_for_window(rr.file_id, rr.byte_start, WINDOW) {
                Some(u) => {
                    let ok = def_usrs.contains(u);
                    if !ok { c_dropped_id += 1; }
                    ok
                }
                None => {
                    c_dropped_uncov += 1;
                    false
                }
            }
        } else {
            // Non-C ref: SCIP sidecar owns the verdict.
            let Some(sl) = sidx_lookup.as_ref() else {
                s_dropped_uncov += 1;
                return false;
            };
            match sl.symbol_for_window(rr.file_id, rr.byte_start, WINDOW) {
                Some(s) => {
                    let ok = def_syms.contains(s);
                    if !ok { s_dropped_id += 1; }
                    ok
                }
                None => {
                    s_dropped_uncov += 1;
                    false
                }
            }
        }
    }).collect();

    // Always log when precision was engaged — quiet success is
    // indistinguishable from quiet pass-through, and we want users
    // (and tests) to be able to see that strict-precise actually ran.
    let sidecars = match (cusr_opt.is_some(), sidx_opt.is_some()) {
        (true, true)  => "clang_usrs + scip_index",
        (true, false) => "clang_usrs",
        (false, true) => "scip_index",
        // unreachable: bailed above on (false, false)
        (false, false) => "(none)",
    };
    eprintln!(
        "[scry] precise ({sidecars}): {before} → {} refs (clang: {} id-mismatch, \
         {} uncovered TU; SCIP: {} id-mismatch, {} uncovered TU; \
         {} def USRs, {} def SCIP symbols)",
        kept.len(),
        c_dropped_id, c_dropped_uncov,
        s_dropped_id, s_dropped_uncov,
        def_usrs.len(), def_syms.len(),
    );
    Ok(kept)
}

/// File-path classifier used by [`apply_precision_filter`] to decide
/// which precision sidecar owns a given ref. C/C++/ObjC live in the
/// clang USR sidecar; everything else lives in SCIP.
fn is_c_family(path: &str) -> bool {
    matches!(
        path.rsplit('.').next().unwrap_or(""),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx"
        | "ipp" | "inl" | "m" | "mm"
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_ref(
    name: String,
    index: Option<PathBuf>,
    lang: Option<String>,
    kind: Option<String>,
    in_: Option<String>,
    not_in: Option<String>,
    limit: usize,
    json: bool,
    format: Option<String>,
    reachable: bool,
    clang_precise: bool,
    scip_precise: bool,
    scope: Option<String>,
    def_in: Option<String>,
    strict: bool,
) -> Result<()> {
    if let Some(f) = format.as_deref() {
        if !matches!(f, "count" | "by-def" | "paths") {
            anyhow::bail!("--format must be 'count', 'by-def', or 'paths' (got '{f}')");
        }
    }
    // --json + --format=count is meaningless (count is a one-line
    // total, not a multi-record stream). --json + --format=by-def is
    // useful — emits the histogram as a JSON array. --json alone
    // emits per-ref JSONL. Anything else conflicts.
    if json && format.as_deref() == Some("count") {
        anyhow::bail!("--json and --format=count are mutually exclusive");
    }
    let t = Instant::now();
    let r = open_index(index)?;
    let results = r.lookup_refs_exact(&name);
    let filtered: Vec<RefRecord> = results
        .into_iter()
        .filter(|rr| {
            if in_.is_some() || not_in.is_some() {
                match r.display_path_cached(rr.file_id) {
                    Some(p) => if !path_matches(p, in_.as_deref(), not_in.as_deref()) {
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
            // --scope CLASS: keep only refs whose enclosing scope
            // chain contains CLASS as an exact segment. Matches Java
            // nested classes (`["pkg","Outer","Inner"]` with
            // `--scope Outer` keeps), but not partial-name collisions
            // (`--scope Foo` won't match `["FooBar"]`). For partial
            // matching use --in on file path instead.
            if let Some(sc) = &scope {
                if !rr.scope_path.iter().any(|seg| seg == sc) {
                    return false;
                }
            }
            true
        })
        .collect();
    // --def-in PATH: only keep refs whose resolved_to points at a
    // def whose file path contains PATH. Refs with resolved_to=None
    // (build-resolutions couldn't narrow them) pass through — we'd
    // rather over-include than silently drop the ones Layer 2
    // resolution didn't reach.
    let filtered = if let Some(def_path) = def_in.as_deref() {
        let target_ids: std::collections::HashSet<u64> = r.lookup_exact(&name)
            .iter()
            .filter(|s| r.display_path_cached(s.file_id)
                .is_some_and(|dp| dp.contains(def_path)))
            .map(|s| s.id)
            .collect();
        if target_ids.is_empty() {
            eprintln!(
                "[scry] --def-in: no def of {name:?} found in any file containing \
                 {def_path:?}; returning all refs unfiltered.",
            );
            filtered
        } else {
            let before = filtered.len();
            let kept: Vec<RefRecord> = filtered.into_iter().filter(|rr| {
                match rr.resolved_to {
                    Some(id) => target_ids.contains(&id),
                    // Strict mode (v0.1.34): drop unresolved instead of
                    // permissively keeping them. Trades recall for
                    // precision — only refs the resolver could confidently
                    // attribute to PATH survive.
                    None => !strict,
                }
            }).collect();
            let resolved_kept = kept.iter().filter(|rr| rr.resolved_to.is_some()).count();
            let unresolved = kept.len() - resolved_kept;
            if strict {
                eprintln!(
                    "[scry] --def-in {def_path:?} --strict: {} → {} refs \
                     ({} resolved to a def in scope; unresolved dropped)",
                    before, kept.len(), resolved_kept,
                );
            } else if resolved_kept == 0 {
                // v0.1.48 — special-case the "0 resolved" diagnostic.
                // The generic message ("pass --strict to drop unresolved")
                // is misleading here — strict would just return 0 hits,
                // which doesn't help the user. The real issue is one of:
                //   (a) the def has no callers in the corpus,
                //   (b) the resolver couldn't attribute any caller to it
                //       (typical for Java method dispatch without
                //        receiver-type inference — `obj.foo()` calls
                //        often resolve to an interface or parent class
                //        method, not the override),
                //   (c) the build-resolutions sidecar is stale.
                eprintln!(
                    "[scry] --def-in {def_path:?}: {} → {} refs (0 resolved to a \
                     def in scope, all {} permissively kept). The over-include \
                     is the best the heuristic resolver can do — receiver-type \
                     inference would be needed to confidently attribute callers \
                     to a specific override of {name:?}. Try \
                     `scry callers {name} --format by-def` to see which defs \
                     the callers actually resolve to.",
                    before, kept.len(), unresolved,
                );
            } else {
                eprintln!(
                    "[scry] --def-in {def_path:?}: {} → {} refs ({} resolved to a \
                     def in scope, {} unresolved-but-kept; pass --strict to drop \
                     unresolved, or run `scry build-resolutions` for tighter narrowing)",
                    before, kept.len(), resolved_kept, unresolved,
                );
            }
            kept
        }
    } else if strict {
        // --strict without --def-in: drop refs the resolver couldn't
        // attribute to any specific def. Useful for "show me only the
        // confidently-resolved call sites of X".
        let before = filtered.len();
        let kept: Vec<RefRecord> = filtered.into_iter()
            .filter(|rr| rr.resolved_to.is_some()).collect();
        eprintln!(
            "[scry] --strict: {} → {} refs (unresolved dropped)",
            before, kept.len(),
        );
        kept
    } else {
        filtered
    };
    // --reachable: drop refs whose owning module can't transitively
    // reach any module that defines the name, per the Soong / GN /
    // kernel module-graph sidecar. No-op without the sidecar
    // (logged once so the user knows to rebuild with --build).
    let filtered = if reachable {
        match r.module_graph() {
            None => {
                eprintln!(
                    "[scry] --reachable: this index has no module_graph.json sidecar; \
                     rebuild with `scry index --build soong/gn/kernel` to enable \
                     build-graph-aware filtering. Returning unfiltered.",
                );
                filtered
            }
            Some(mg) => {
                // The "callee" is any def of `name`. Collect the set of
                // modules that own a def; we keep a ref iff its caller
                // module can reach AT LEAST ONE of them.
                let defs = r.lookup_exact(&name);
                let callee_modules: std::collections::HashSet<u32> = defs
                    .iter()
                    .filter_map(|s| mg.module_of_file(s.file_id))
                    .collect();
                if callee_modules.is_empty() {
                    eprintln!(
                        "[scry] --reachable: no def of {name:?} attributed to any \
                         module in the graph; returning unfiltered.",
                    );
                    filtered
                } else {
                    let before = filtered.len();
                    let kept: Vec<RefRecord> = filtered.into_iter().filter(|rr| {
                        match mg.module_of_file(rr.file_id) {
                            Some(caller_mod) => callee_modules.iter()
                                .any(|cm| mg.is_reachable(caller_mod, *cm)),
                            // Unattributed callers pass through (we
                            // can't prove unreachability without data).
                            None => true,
                        }
                    }).collect();
                    eprintln!(
                        "[scry] --reachable: {} → {} refs after module-graph \
                         reachability filter ({} callee modules)",
                        before, kept.len(), callee_modules.len(),
                    );
                    kept
                }
            }
        }
    } else {
        filtered
    };
    let filtered = apply_precision_filter(
        &r, &name, filtered, clang_precise, scip_precise,
    )?;
    let label = if kind.as_deref() == Some("call") { "callers" } else { "ref" };
    // --format count: just the totals, no per-hit rows. Pays off for
    // "how many callers does X have?" agent queries — one short line
    // regardless of how many hits the index actually holds.
    match format.as_deref() {
        Some("count") => {
            println!("{} {label}", filtered.len());
        }
        Some("by-def") => {
            // Group refs by resolved_to (None bucketed as "unresolved").
            // Sort descending by count, show top `limit` groups with
            // the friendly def annotation. Answers "WHICH def do the
            // callers actually target?" at a glance — invaluable for
            // figuring out how a polymorphic name like `close` or
            // `onCreate` is dispatched across the corpus.
            print_refs_by_def(&r, &filtered, &name, limit, json);
        }
        Some("paths") => {
            // v0.1.56 — unique sorted file paths only. Common LLM-agent
            // shape: "which files contain refs to X?" without the
            // line/col/scope noise. JSON emits ["path1", "path2", ...];
            // human format is one path per line.
            print_refs_paths(&r, &filtered, limit, json);
        }
        _ => {
            print_refs(&r, &filtered, limit, json);
        }
    }
    // v0.1.52 — fuzzy "did you mean" hint on 0 hits. Same gating as
    // cmd_def: only fire when no filter narrowed the search, otherwise
    // a 0-result is "filtered away" not "name unknown".
    if filtered.is_empty()
        && in_.is_none() && not_in.is_none()
        && lang.is_none()
        && scope.is_none() && def_in.is_none()
    {
        if let Some(hint) = suggest_similar(&r, &name) {
            eprintln!("[scry] {hint}");
        }
    }
    log_query(&r, label, &name, filtered.len(), filtered.len().min(limit), t);
    Ok(())
}

/// Histogram of refs grouped by their Layer 2 resolved_to target.
/// Human format: `<count>  → file:line [scope]` per group,
/// descending by count, capped at `limit` groups. Unresolved refs
/// are bucketed into a single `<unresolved>` row at the bottom.
/// JSON format: `[{"count": N, "def": {"path": ..., "line": ...,
/// "scope": [...], "id": "0x..."}}, ..., {"count": M, "def": null}]`.
fn print_refs_by_def(reader: &StoreReader, refs: &[RefRecord], name: &str, limit: usize, json: bool) {
    use std::collections::HashMap;
    let mut by_id: HashMap<Option<u64>, usize> = HashMap::new();
    for r in refs {
        *by_id.entry(r.resolved_to).or_insert(0) += 1;
    }
    let unresolved = by_id.remove(&None).unwrap_or(0);
    let mut groups: Vec<(u64, usize)> = by_id.into_iter()
        .map(|(k, v)| (k.unwrap(), v))
        .collect();
    groups.sort_unstable_by_key(|g| std::cmp::Reverse(g.1));
    // Build a name→symbols map ONCE so we don't pay lookup_exact
    // per row. The ref's name is the same for the whole group.
    let candidates = reader.lookup_exact(name);
    let by_def_id: HashMap<u64, &SymbolRecord> = candidates.iter()
        .map(|s| (s.id, s)).collect();

    if json {
        let mut out: Vec<serde_json::Value> = Vec::with_capacity(
            groups.len().min(limit) + if unresolved > 0 { 1 } else { 0 });
        for (def_id, count) in groups.iter().take(limit) {
            let def_val = by_def_id.get(def_id).map(|s| {
                let path = reader.display_path_cached(s.file_id).unwrap_or("");
                serde_json::json!({
                    "path": path,
                    "line": s.line,
                    "col": s.col,
                    "scope": s.scope_path,
                    "kind": s.kind.short(),
                    "id": format!("{:x}", def_id),
                })
            }).unwrap_or_else(|| serde_json::json!({
                // Resolution points at an id we can't find by name —
                // cross-build mismatch or stale sidecar. Surface
                // the hex id so callers can debug.
                "id": format!("{:x}", def_id),
            }));
            out.push(serde_json::json!({"count": count, "def": def_val}));
        }
        if unresolved > 0 {
            out.push(serde_json::json!({"count": unresolved, "def": null}));
        }
        println!("{}", serde_json::Value::Array(out));
        return;
    }

    for (def_id, count) in groups.iter().take(limit) {
        let annot = by_def_id.get(def_id)
            .map(|s| {
                let path = reader.display_path_cached(s.file_id).unwrap_or("");
                let scope = if s.scope_path.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", s.scope_path.join("::"))
                };
                format!("{}:{}{}", short_path_suffix(path), s.line, scope)
            })
            .unwrap_or_else(|| format!("def:{:x}", def_id));
        println!("{:>8}  → {}", count, annot);
    }
    if unresolved > 0 {
        println!("{:>8}  → <unresolved>", unresolved);
    }
    let shown_groups = groups.len().min(limit) + if unresolved > 0 { 1 } else { 0 };
    let total_groups = groups.len() + if unresolved > 0 { 1 } else { 0 };
    eprintln!(
        "\n{} refs in {} group{} (showing {})",
        refs.len(), total_groups,
        if total_groups == 1 { "" } else { "s" },
        shown_groups,
    );
}

/// Symbol-set analogue of `print_refs_paths`: deduped sorted file
/// paths for a Vec<SymbolRecord> (used by `--format paths` on
/// `subclasses` / `implementations`). Same JSON + human shape.
fn print_symbols_paths(reader: &StoreReader, syms: &[SymbolRecord], limit: usize, json: bool) {
    use std::collections::BTreeSet;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for s in syms {
        if let Some(p) = reader.display_path_cached(s.file_id) {
            paths.insert(p.to_string());
            if paths.len() >= limit { break; }
        }
    }
    if json {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let arr: Vec<&String> = paths.iter().collect();
        let _ = writeln!(out, "{}", serde_json::to_string(&arr).unwrap());
    } else {
        for p in &paths {
            println!("{p}");
        }
        eprintln!("\n{} unique file{} (from {} symbols)",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" },
            syms.len());
    }
}

/// Unique sorted file paths only. Replaces the per-ref output for the
/// "which files reference X?" use case. JSON emits a flat array of
/// strings; human format is one path per line so it can pipe straight
/// into `xargs` / `vim`. Deduplication happens before --limit, so the
/// cap counts unique files, not raw refs.
fn print_refs_paths(reader: &StoreReader, refs: &[RefRecord], limit: usize, json: bool) {
    use std::collections::BTreeSet;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for r in refs {
        if let Some(p) = reader.display_path_cached(r.file_id) {
            paths.insert(p.to_string());
            if paths.len() >= limit { break; }
        }
    }
    if json {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let arr: Vec<&String> = paths.iter().collect();
        let _ = writeln!(out, "{}", serde_json::to_string(&arr).unwrap());
    } else {
        for p in &paths {
            println!("{p}");
        }
        eprintln!("\n{} unique file{} (from {} refs)",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" },
            refs.len());
    }
}

fn cmd_stats(index: Option<PathBuf>, json: bool) -> Result<()> {
    let r = open_index(index)?;

    let mut by_lang: std::collections::HashMap<FileKind, u64> = std::collections::HashMap::new();
    let mut by_kind: std::collections::HashMap<SymbolKind, u64> =
        std::collections::HashMap::new();
    for s in r.iter_symbols() {
        *by_lang.entry(s.lang).or_default() += 1;
        *by_kind.entry(s.kind).or_default() += 1;
    }

    // Resolution sidecar coverage (v0.1.41). None ⇒ sidecar absent
    // (no `scry build-resolutions` was run).
    let resolved_count = r.count_resolved_refs();
    let total_refs = r.manifest.stats.refs;
    let resolved_pct = resolved_count.map(|n|
        if total_refs == 0 { 0.0 }
        else { (n as f64) * 100.0 / (total_refs as f64) });

    if json {
        let by_lang_map: serde_json::Map<String, serde_json::Value> = by_lang.iter()
            .map(|(k, c)| (k.as_str().to_string(), serde_json::json!(c)))
            .collect();
        let by_kind_map: serde_json::Map<String, serde_json::Value> = by_kind.iter()
            .map(|(k, c)| (k.short().to_string(), serde_json::json!(c)))
            .collect();
        let roots: Vec<serde_json::Value> = r.roots.iter()
            .map(|root| serde_json::json!({"path": root.path, "profile": format!("{:?}", root.profile)}))
            .collect();
        let out = serde_json::json!({
            "scry_version":    r.manifest.scry_version,
            "manifest_version": r.manifest.version,
            "indexed_at":      r.manifest.indexed_at,
            "roots":           roots,
            "files_total":     r.manifest.stats.files_total,
            "files_parsed":    r.manifest.stats.files_parsed,
            "files_failed":    r.manifest.stats.files_failed,
            "bytes_total":     r.manifest.stats.bytes_total,
            "symbols":         r.manifest.stats.symbols,
            "refs":            r.manifest.stats.refs,
            "refs_resolved":   resolved_count,
            "refs_resolved_pct": resolved_pct,
            "elapsed_ms":      r.manifest.stats.elapsed_ms,
            "by_lang":         serde_json::Value::Object(by_lang_map),
            "by_kind":         serde_json::Value::Object(by_kind_map),
        });
        println!("{}", out);
        return Ok(());
    }

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
    match (resolved_count, resolved_pct) {
        (Some(n), Some(p)) => println!("refs-resolved: {n} ({p:.1}%)"),
        _ => println!(
            "refs-resolved: <no sidecar — run `scry build-resolutions` to enable>",
        ),
    }
    println!("elapsed-ms:   {}", r.manifest.stats.elapsed_ms);

    println!("\nby language:");
    let mut lv: Vec<_> = by_lang.into_iter().collect();
    lv.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (l, c) in lv {
        println!("  {c:>10}  {l:?}");
    }
    println!("\nby kind:");
    let mut kv: Vec<_> = by_kind.into_iter().collect();
    kv.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
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
    let matching_ids: Vec<(u32, FileKind, u64)> = r.iter_files()
        .filter(|fe| {
            if prefix.is_empty() { true }
            else { fe.display_path().contains(prefix) }
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
    // Hands off to the cached resolver on StoreReader. First call
    // pays a one-time ~70 MB display_path materialization + HashMap
    // build (~50 ms on the AOSP+Linux 1M-file index); subsequent
    // resolutions are one HashMap probe. Was 600 ms per call before.
    r.resolve_file_id(arg)
}

fn cmd_outline(
    path: String, index: Option<PathBuf>, json: bool,
    limit: usize, with_snippets: usize,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let file_id = match resolve_file_id(&r, &path) {
        Some(id) => id,
        None => anyhow::bail!("no indexed file matches '{}'", path),
    };
    let fe = r.file_view(file_id)
        .ok_or_else(|| anyhow::anyhow!("file_id {} out of range", file_id))?;
    let display = fe.display_path();
    // For --with-snippets > 0 we need the file bytes once. Read up
    // front so we don't re-read per symbol; the file is in the page
    // cache from open_index's mmap path anyway, so this is cheap.
    let file_bytes: Option<Vec<u8>> = if with_snippets > 0 {
        std::fs::read(&display).ok()
    } else { None };

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

    // Render a snippet: starting at `line` (1-based), take up to
    // `n_lines` lines from `bytes`. Lines are clipped to 200 chars
    // each to avoid token-bombing on lines with embedded blobs.
    let render_snippet = |line: u32, n_lines: usize| -> Option<String> {
        let bytes = file_bytes.as_ref()?;
        let text = std::str::from_utf8(bytes).ok()?;
        let start_line = (line as usize).saturating_sub(1);
        let mut out = String::new();
        for (i, ln) in text.lines().enumerate().skip(start_line).take(n_lines) {
            if i > start_line { out.push('\n'); }
            if ln.len() > 200 { out.push_str(&ln[..200]); out.push_str(" …"); }
            else { out.push_str(ln); }
        }
        if out.is_empty() { None } else { Some(out) }
    };

    if json {
        let arr: Vec<_> = found.iter().take(take).map(|s| {
            let mut obj = symbol_to_json(&r, s);
            if with_snippets > 0 {
                if let Some(snip) = render_snippet(s.line, with_snippets) {
                    if let Some(m) = obj.as_object_mut() {
                        m.insert("snippet".into(), serde_json::Value::String(snip));
                    }
                }
            }
            obj
        }).collect();
        let out = serde_json::json!({
            "path": display,
            "lang": fe.kind.as_str(),
            "symbols_total": found.len(),
            "symbols_shown": take,
            "symbols": arr,
        });
        println!("{out}");
    } else {
        println!("# {}  ({:?})", display, fe.kind);
        println!("# {} symbols", found.len());
        for s in found.iter().take(take) {
            let scope = if s.scope_path.is_empty() { String::new() }
                        else { format!("  [{}]", s.scope_path.join("::")) };
            println!("{:>5}:{:<3}  {:<12}  {}{}",
                     s.line, s.col, s.kind.short(), s.name, scope);
            if with_snippets > 0 {
                if let Some(snip) = render_snippet(s.line, with_snippets) {
                    for ln in snip.lines() {
                        println!("       │ {ln}");
                    }
                }
            }
        }
        if take < found.len() {
            println!("... ({} more — pass --limit 0 to see all)", found.len() - take);
        }
    }
    log_query(&r, "outline", &path, found.len(), take, t);
    Ok(())
}

/// One-call file summary. Built from the same per-file symbol set
/// `outline` uses, but compressed to "what's the shape of this
/// file?" — language, total symbol count, per-kind breakdown,
/// top 3 ranked symbols (using `SymbolRecord::rank_score`), and
/// the file's first non-blank line (often a package decl or
/// leading docstring).
///
/// Saves a round-trip when the agent's question is "what does
/// this file do?" rather than "show me every symbol." Cuts ~70%
/// of the tokens vs `outline + N×def` for the same answer.
fn cmd_tldr(path: String, index: Option<PathBuf>, json: bool) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    let file_id = match resolve_file_id(&r, &path) {
        Some(id) => id,
        None => anyhow::bail!("no indexed file matches '{}'", path),
    };
    let fe = r.file_view(file_id)
        .ok_or_else(|| anyhow::anyhow!("file_id {} out of range", file_id))?;
    let display = fe.display_path();

    // Gather this file's symbols via the file_symbols sidecar (O(1)
    // per file) with a fallback to the linear scan.
    let mut syms: Vec<SymbolRecord> = match r.symbols_for_file(file_id) {
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

    // Per-kind histogram. Stable order (kind short name) so the same
    // file gives the same output across runs.
    let mut by_kind: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for s in &syms {
        *by_kind.entry(s.kind.short()).or_default() += 1;
    }

    // Top 3 ranked symbols. rank_score handles the (Class > Method >
    // Field) tier ordering + scope penalty; we don't re-rank by
    // file-shape because every symbol here lives in the same file.
    syms.sort_by_key(|s| std::cmp::Reverse(s.rank_score()));
    let top: Vec<&SymbolRecord> = syms.iter().take(3).collect();

    // First non-blank line. Often the package declaration in Java/Go/
    // Kotlin, the leading `//` doc comment in Rust, or the `#!`
    // shebang in scripts. A real docstring extractor would be
    // language-aware; this is the 90%-good heuristic.
    let first_line = std::fs::read_to_string(&display).ok()
        .and_then(|src| src.lines().find(|l| !l.trim().is_empty())
            .map(|l| {
                if l.len() > 200 { format!("{}…", &l[..200]) }
                else { l.to_string() }
            }));

    if json {
        let kinds: Vec<_> = by_kind.iter()
            .map(|(k, n)| serde_json::json!({"kind": k, "count": n}))
            .collect();
        let top_arr: Vec<_> = top.iter()
            .map(|s| serde_json::json!({
                "name": s.name, "kind": s.kind.short(),
                "line": s.line, "col": s.col,
                "scope": s.scope_path,
            }))
            .collect();
        let out = serde_json::json!({
            "path": display,
            "lang": fe.kind.as_str(),
            "symbols_total": syms.len(),
            "by_kind": kinds,
            "top": top_arr,
            "first_line": first_line,
        });
        println!("{out}");
    } else {
        println!("# {}  ({:?})", display, fe.kind);
        println!("# {} symbols", syms.len());
        if let Some(fl) = &first_line {
            println!("#");
            println!("# first line: {fl}");
        }
        if !by_kind.is_empty() {
            println!("#");
            print!("# kinds:");
            for (k, n) in &by_kind {
                print!("  {n}×{k}");
            }
            println!();
        }
        if !top.is_empty() {
            println!("#");
            println!("# top {}:", top.len());
            for s in &top {
                let scope = if s.scope_path.is_empty() { String::new() }
                            else { format!("  [{}]", s.scope_path.join("::")) };
                println!("  {:>5}:{:<3}  {:<12}  {}{}",
                         s.line, s.col, s.kind.short(), s.name, scope);
            }
        }
    }
    log_query(&r, "tldr", &path, syms.len(), top.len(), t);
    Ok(())
}

/// Sort symbol hits by descending desirability. Composes the kind/lang/
/// scope heuristic from SymbolRecord::rank_score with a path-shape signal
/// the store can't see (path depth, presence of `test/` segments — a test
/// v0.1.50 — collapse Package symbols (and other duplicate-by-name
/// kinds) so `def some.package` doesn't return one row per Java file
/// in the package. Keeps the FIRST occurrence per (kind=Package,
/// name, lang) bucket — that one is enough to tell the user the
/// package exists; deeper exploration goes via `--in some/package/`.
/// Operates only on Package; other kinds (classes, methods) are
/// genuinely distinct per file even when same-named.
fn dedupe_package_symbols(syms: &mut Vec<SymbolRecord>) {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, FileKind)> = HashSet::new();
    syms.retain(|s| {
        if !matches!(s.kind, SymbolKind::Package) { return true; }
        seen.insert((s.name.clone(), s.lang))
    });
}

/// fixture is rarely the canonical hit).
///
/// Stable: ties resolve by (path, line, col) so the output is reproducible
/// across runs of the same query.
fn rank_symbols(syms: &mut [SymbolRecord], r: &StoreReader) {
    // Decorate-Sort-Undecorate: precompute (display_path, score) per
    // symbol ONCE, then sort by index into the decoration vector.
    // The previous comparator called `display_path` twice per
    // comparison — for `def main` (thousands of candidates), N log N
    // comparisons × 2 path allocations is millions of String allocs
    // (measured: 4.4 s for ~5 k matches). After this fix the sort
    // does O(N) allocations + O(N log N) borrow compares.
    let decorated: Vec<(String, i64)> = syms.iter()
        .map(|s| {
            let p = r.display_path_cached(s.file_id).unwrap_or("").to_string();
            let score = symbol_total_score(s, &p);
            (p, score)
        })
        .collect();
    let mut order: Vec<usize> = (0..syms.len()).collect();
    order.sort_by(|&i, &j| {
        let (pa, sa) = (&decorated[i].0, decorated[i].1);
        let (pb, sb) = (&decorated[j].0, decorated[j].1);
        // descending score; tie-break ascending (path, line, col)
        // for deterministic output.
        sb.cmp(&sa).then_with(|| (pa, syms[i].line, syms[i].col)
            .cmp(&(pb, syms[j].line, syms[j].col)))
    });
    // Apply the permutation in place.
    permute_in_place(syms, &order);
}

/// Reorder `slice` so that the new index 0 holds what was at
/// `order[0]`, new index 1 holds `order[1]`, etc. Stable across
/// `Clone`-free types via cycle-following swaps. Used by
/// `rank_symbols` to apply the precomputed decoration's
/// permutation without cloning every SymbolRecord.
fn permute_in_place<T>(slice: &mut [T], order: &[usize]) {
    debug_assert_eq!(slice.len(), order.len());
    // Inverse-permutation cycle walk. Mark visited via a separate
    // bitmap to keep `order` immutable.
    let n = slice.len();
    let mut visited = vec![false; n];
    for start in 0..n {
        if visited[start] || order[start] == start { visited[start] = true; continue; }
        let mut current = start;
        loop {
            let next = order[current];
            visited[current] = true;
            if next == start { break; }
            slice.swap(current, next);
            current = next;
        }
    }
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
    score -= (depth - PATH_DEPTH_FREE_SEGMENTS).clamp(0, PATH_DEPTH_MAX_PENALTY);
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

/// Shared `--in` / `--not-in` substring filter. Returns true if
/// `path` passes both filters (contains `in_` AND does not contain
/// `not_in`). Either filter may be absent. Empty string filters
/// match conservatively: an empty `in_` matches everything, an
/// empty `not_in` rejects nothing — matches what a user expects
/// when an upstream caller forwards Option<String> from CLI without
/// trimming.
fn path_matches(path: &str, in_: Option<&str>, not_in: Option<&str>) -> bool {
    if let Some(p) = in_ {
        if !p.is_empty() && !path.contains(p) { return false; }
    }
    if let Some(p) = not_in {
        if !p.is_empty() && path.contains(p) { return false; }
    }
    true
}

/// When a `def`/`ref`/`callers` lookup returns 0 hits, run a
/// distance-2 fuzzy match against the symbol FST and surface the
/// top 3 distinct names as a "Did you mean: …" stderr hint. Tiny
/// (~ms on a 31M-symbol index) and catches the common typo case
/// without users needing to know about `scry fuzzy`. Returns None
/// for very short names (high false-positive rate) or no matches.
///
/// Re-sorts fuzzy hits by pure Levenshtein distance (then length
/// closeness, then alphabetical) — the store's `lookup_fuzzy_ranked`
/// favors substring matches, which is the right call for explicit
/// `scry fuzzy QUERY` (where a user typing a real substring expects
/// substring hits). For typo suggestions the user typed a misspelled
/// identifier, so a closer Levenshtein match (`Activity` from
/// `Activty`) is more useful than a longer name that happens to
/// substring-contain the typo (`MainActivty`).
fn suggest_similar(reader: &StoreReader, name: &str) -> Option<String> {
    if name.len() < 3 { return None; }
    let mut hits = reader.lookup_fuzzy_ranked(name, 2, 32);
    if hits.is_empty() { return None; }
    // Drop pathologically long candidate names — the HTML/Javadoc
    // indexer surfaces anchor IDs like `Z_handleUnknownTypeId-…` that
    // can be 200+ chars; they're never what a user typed when
    // hand-typing an identifier name. Cap at 4× query length, with a
    // hard floor of 64 so short queries still see reasonable matches.
    let max_len = (name.len() * 4).max(64);
    hits.retain(|(s, _d)| s.name.len() <= max_len);
    if hits.is_empty() { return None; }
    // Closer Levenshtein first; tie-break by name-length proximity to
    // the query (the typo and the real name usually differ by ≤ 2 chars),
    // then alphabetical for determinism.
    let q_len = name.len() as i32;
    hits.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| (a.0.name.len() as i32 - q_len).abs()
                          .cmp(&(b.0.name.len() as i32 - q_len).abs()))
            .then_with(|| a.0.name.cmp(&b.0.name))
    });
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for (s, _d) in hits {
        if seen.insert(s.name.clone()) {
            names.push(s.name);
            if names.len() >= 3 { break; }
        }
    }
    if names.is_empty() { return None; }
    Some(format!("Did you mean: {}? (run `scry fuzzy {name}` for the full list.)",
                 names.join(", ")))
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
    // Not a simple iterator counter: incremented only when we actually
    // print a section (skipped on budget exhaustion mid-loop), then
    // reported back to the user.
    let mut emitted_count: usize = 0;
    let cap = syms.len().min(limit);
    #[allow(clippy::explicit_counter_loop)]
    for s in syms.iter().take(cap) {
        let path = reader.display_path_cached(s.file_id).unwrap_or("");
        let snippet = read_snippet(path, s.line, 8);
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

/// Default path for the per-query ops log. Honors $SCRY_LOG,
/// otherwise $HOME/.scry/queries.log. Returns None on a non-Unicode
/// or missing HOME (no log = best-effort skip, not an error).
///
/// Set `SCRY_LOG=` (empty string) to disable logging entirely —
/// useful in long-running MCP sessions where the log would otherwise
/// grow without bound during tight agent loops.
fn query_log_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCRY_LOG") {
        if p.is_empty() { return None; }
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".scry").join("queries.log"))
}

/// Maximum size in bytes before the ops log is rotated. Read from
/// `$SCRY_LOG_MAX_BYTES` (default 100 MiB). `0` disables rotation.
/// On crossing the cap, the active log is renamed to `<path>.1`
/// (single backup, overwriting any prior `.1`) and a fresh log
/// starts. Bounded total disk = 2 × max_bytes.
///
/// Cap chosen so a tight MCP loop (e.g. one query / 100 ms over 24 h
/// ≈ 860K rows × ~300 bytes/row ≈ 260 MB) survives a single day even
/// without rotation; with the default 100 MB cap it rotates roughly
/// twice/day under that load. Adjust upward on instrumented
/// production hosts where you want longer in-band history; downward
/// on disk-constrained hosts.
fn query_log_max_bytes() -> u64 {
    std::env::var("SCRY_LOG_MAX_BYTES").ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(100 * 1024 * 1024)
}

/// Pure rotation logic: if `path` exists and is over `cap` bytes,
/// rename it to `{path}.1` (overwriting any prior `.1`) so the
/// next append starts a fresh file. `cap == 0` disables rotation
/// entirely. Best-effort: on any error we silently skip the
/// rename and let the file keep growing — the alternative would
/// be to refuse to append, which is worse than an oversized log.
///
/// Split from the env-reading wrapper so the rotation contract is
/// testable without env mutation (scry-cli is
/// `#![forbid(unsafe_code)]` and Rust 2024 marks env::set_var as
/// unsafe due to multi-thread race risk).
fn rotate_log_if_oversized(path: &Path, cap: u64) {
    if cap == 0 { return; }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return, // file doesn't exist yet; nothing to rotate
    };
    if meta.len() <= cap { return; }
    let backup = path.with_extension(
        path.extension().and_then(|e| e.to_str())
            .map(|e| format!("{e}.1"))
            .unwrap_or_else(|| "1".to_string()),
    );
    let _ = std::fs::rename(path, &backup);
}

/// Env-reading wrapper around [`rotate_log_if_oversized`]. Called
/// from the live append path; tests target the pure helper.
fn maybe_rotate_log(path: &Path) {
    rotate_log_if_oversized(path, query_log_max_bytes());
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
            // Rotate if oversized BEFORE opening so the append lands
            // in the fresh file. Best-effort: a failed rotation just
            // means the next append targets the oversized file.
            maybe_rotate_log(&path);
            let mut f = std::fs::OpenOptions::new()
                .create(true).append(true).open(&path)?;
            // Schema: a flat JSON object per line, suitable for
            // ingestion with `jq -c`, DuckDB's `read_ndjson_auto`,
            // BigQuery's `NEWLINE_DELIMITED_JSON`, or
            // `pandas.read_json(lines=True)`. Field semantics:
            //   ts              — unix-epoch seconds (UTC).
            //   cmd             — scry subcommand (def, grep, callers, ...).
            //   query           — the user-supplied pattern / name / path.
            //   hits            — total matching records found (pre-truncate).
            //   shown           — what the caller actually rendered (post-limit).
            //   files_total     — file count in the index at query time.
            //   candidate_files — files the trigram pre-filter narrowed
            //                     down to (grep only); null otherwise.
            //   elapsed_ms      — wall-clock from CLI entry to log call.
            //   index           — absolute path of the index dir.
            //   scry_version    — version of the binary that ran the
            //                     query (correlate latency with code
            //                     changes across a deploy).
            //   pid             — disambiguate parallel calls from the
            //                     same user / agent.
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
                "scry_version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
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
            let path = reader.display_path_cached(s.file_id).unwrap_or("");
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
        let path = reader.display_path_cached(s.file_id).unwrap_or("");
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
    emit_narrow_hint(reader, syms, limit);
}

/// Heuristic auto-narrow hint. When the result list saturates the
/// limit AND there are visibly more candidates than shown, append a
/// one-line stderr suggestion pointing at the most useful filter
/// dimension: the dominant directory prefix of the shown results.
/// Suppressed by SCRY_QUIET=1 (matches the stale-index warning).
///
/// We only fire when:
///   - the caller asked for ≥ 2 results (limit=1 is a deliberate
///     "I want the top hit" — no point nudging)
///   - we hit the cap (syms.len() >= limit)
///   - the visible hits actually share a common 2-segment prefix
///     deeper than the indexed roots (so the suggestion is concrete)
fn emit_narrow_hint(reader: &StoreReader, syms: &[SymbolRecord], limit: usize) {
    if limit < 2 || syms.len() < limit { return; }
    if std::env::var("SCRY_QUIET").map(|v| v == "1").unwrap_or(false) { return; }
    // Collect display paths of the shown rows.
    let paths: Vec<String> = syms.iter().take(limit)
        .filter_map(|s| reader.display_path_cached(s.file_id).map(str::to_string))
        .collect();
    if paths.len() < limit { return; }
    // Find the longest common path prefix (segment-wise) and shave
    // until it's at least 2 segments deep — single-segment hints
    // like "--in frameworks/" aren't useful.
    let split: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect()).collect();
    let max = split.iter().map(Vec::len).min().unwrap_or(0);
    let mut common = 0;
    for i in 0..max {
        let first = split[0][i];
        if split.iter().all(|s| s[i] == first) { common += 1; } else { break; }
    }
    if common < 2 { return; }
    // Drop the last segment if it looks like a filename; we want a
    // directory prefix, not a single file.
    while common >= 2 {
        let seg = split[0][common - 1];
        if seg.contains('.') { common -= 1; } else { break; }
    }
    if common < 2 { return; }
    let hint = split[0][..common].join("/");
    eprintln!("[scry] hit --limit; try `--in {hint}/` to narrow, or tighten --kind / --lang.");
}

fn print_refs(reader: &StoreReader, refs: &[RefRecord], limit: usize, json: bool) {
    if json {
        for r in refs.iter().take(limit) {
            let path = reader.display_path_cached(r.file_id).unwrap_or("");
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
        let path = reader.display_path_cached(r.file_id).unwrap_or("");
        let scope = if r.scope_path.is_empty() {
            String::new()
        } else {
            format!("  [{}]", r.scope_path.join("::"))
        };
        let resolved = r.resolved_to
            .map(|id| format_resolved_def(reader, &r.name, id))
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

/// Render the `→ def:...` suffix for a ref whose Layer 2 resolution
/// picked a specific def. Shows the def's filename + line + scope so
/// users can SEE which class's method the ref targets — the previous
/// `→ def:707c64fb...` hex id was unintelligible and made it
/// impossible to spot bad resolutions without a JSON dump. Falls
/// back to the hex form if the id can't be located among same-name
/// symbols (e.g. cross-build mismatch where ref.name != def.name).
fn format_resolved_def(reader: &StoreReader, ref_name: &str, def_id: u64) -> String {
    let def = reader.lookup_exact(ref_name).into_iter().find(|s| s.id == def_id);
    match def {
        Some(s) => {
            let path = reader.display_path_cached(s.file_id).unwrap_or("");
            let scope = if s.scope_path.is_empty() {
                String::new()
            } else {
                format!(" [{}]", s.scope_path.join("::"))
            };
            format!("  → {}:{}{}", short_path_suffix(path), s.line, scope)
        }
        None => format!("  → def:{:x}", def_id),
    }
}

/// Last two non-empty path components, joined with `/`. Used for
/// compact ref/by-def annotations so users can disambiguate the
/// many `MainActivity.java` files in a big corpus without
/// dumping the full absolute path. v0.1.39.
fn short_path_suffix(path: &str) -> &str {
    // Find the last '/' and the one before it.
    match path.rfind('/') {
        None => path,
        Some(last) => {
            match path[..last].rfind('/') {
                None => path,                 // already only one component before basename
                Some(prev) => &path[prev + 1..],
            }
        }
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

fn cmd_grep(
    pattern: String,
    index: Option<PathBuf>,
    is_regex: bool,
    ignore_case: bool,
    lang: Option<String>,
    in_: Option<String>,
    not_in: Option<String>,
    limit: usize,
    json: bool,
    workers: Option<usize>,
    max_file_bytes: u64,
    mem_cap: u32,
    format: Option<String>,
    explain: bool,
) -> Result<()> {
    if json && format.is_some() {
        anyhow::bail!("--json and --format are mutually exclusive");
    }
    let format = format.as_deref();
    if let Some(f) = format {
        if !matches!(f, "lines" | "count") {
            anyhow::bail!(
                "--format must be one of: lines, count (got '{f}')"
            );
        }
    }
    if explain {
        // --explain short-circuits the actual scan: dump the query
        // plan to stdout, exit. Works on literal patterns; for regex
        // we report that the pre-filter does literal-extraction
        // analysis instead.
        let r = open_index(index)?;
        if is_regex {
            println!("regex pattern; trigram pre-filter runs literal-extraction analysis.");
            println!("(use a literal pattern with --explain for a per-trigram breakdown)");
            // For regex, still show whether ANY trigram filter is feasible.
            let cs = grep_candidates_for_regex(&r, &pattern);
            match cs {
                Some(c) => println!("regex→trigram pre-filter candidates: {}", c.len()),
                None => println!("regex has no extractable literal — would full-scan."),
            }
            return Ok(());
        }
        let exp = r.grep_explain(pattern.as_bytes());
        println!("query:      {:?}", pattern);
        match exp {
            None => {
                println!("plan:       no trigram pre-filter (pattern < 3 bytes OR no trigram index)");
                println!("scan-cost:  full-scan of every file matching --lang/--in (worst case)");
            }
            Some(e) => {
                println!("trigrams ({} extracted, smallest-first intersection):", e.per_trigram.len());
                // Sort visualization smallest-first so the reader can see
                // which trigram drove the intersection.
                let mut rows = e.per_trigram.clone();
                rows.sort_by_key(|(_, n)| *n);
                for (t, n) in &rows {
                    println!("  {:<6} {:>10} files", format!("{:?}", t), n);
                }
                println!("candidates: {} files post-intersection", e.candidates);
                // Rough scan cost: average file size on this index × candidates.
                let avg_bytes = if r.file_count() > 0 {
                    let total: u64 = r.iter_files().map(|f| f.size).sum();
                    total / r.file_count() as u64
                } else { 0 };
                let est_bytes = (e.candidates as u64).saturating_mul(avg_bytes);
                println!("scan-cost:  ~{} estimated I/O ({} candidates × {} avg file size)",
                    human_bytes(est_bytes), e.candidates, human_bytes(avg_bytes));
            }
        }
        return Ok(());
    }
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
    // `re` is set when either --regex is on, OR --ignore-case is on for a
    // literal pattern (we route literal-CI through regex::bytes with
    // case_insensitive(true) so the inner matcher Just Works).
    let re = if is_regex {
        Some(regex::bytes::RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .build()
            .context("invalid regex")?)
    } else if ignore_case {
        Some(regex::bytes::RegexBuilder::new(&regex::escape(&pattern))
            .case_insensitive(true)
            .build()
            .expect("escaped literal must compile"))
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
    // Regex queries skip this (a regex could match anything). In CI mode
    // we expand each trigram across its ASCII case variants and union
    // their postings (≤ 8× per trigram, then intersect across positions).
    let trigram_candidates: Option<std::collections::HashSet<u32>> = if !is_regex {
        let t_tg = Instant::now();
        let cs = if ignore_case {
            r.grep_candidates_ci(pattern.as_bytes())
        } else {
            r.grep_candidates(pattern.as_bytes())
        };
        if let Some(ref c) = cs {
            eprintln!("[grep] trigram pre-filter{}: {} candidate files in {} ms",
                if ignore_case { " (CI)" } else { "" },
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
    let candidates: Vec<scry_store::FileView<'_>> = r
        .iter_files()
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
            if !prefix.is_empty() || not_in.is_some() {
                // Same semantics as cmd_def/cmd_ref: --in is a substring
                // of the absolute path so the caller can pass either a
                // root-relative subdir ("frameworks/base/") or an absolute
                // one and have both work. --not-in (v0.1.55) drops paths
                // containing SUBSTR — useful for `--not-in /tests/`.
                let full = fe.display_path();
                if !prefix.is_empty() && !full.contains(prefix) {
                    return false;
                }
                if let Some(neg) = not_in.as_deref() {
                    if !neg.is_empty() && full.contains(neg) {
                        return false;
                    }
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
            let path = fe.display_path();
            scry_store::prefault_path(Path::new(&path));
        });
        eprintln!("[grep] prefaulted {} files in {} ms",
                  total_files, t_pf.elapsed().as_millis());
    }

    let hits: parking_lot::Mutex<Vec<Hit>> = parking_lot::Mutex::new(Vec::new());
    let hit_count = std::sync::atomic::AtomicUsize::new(0);
    candidates.par_iter().for_each(|fe| {
        if hit_count.load(Ordering::Relaxed) >= limit * 8 {
            return; // bound work after we have plenty of candidates
        }
        let path = fe.display_path();
        let mut local: Vec<Hit> = Vec::new();
        if let Some(re) = &re {
            // Regex path: need full bytes in memory for the regex
            // engine. Keep std::fs::read here — same allocation cost
            // as before, but the regex needle is rare enough that this
            // path dominates less of the wall time.
            let md = std::fs::metadata(&path).ok();
            if let Some(m) = md.as_ref() {
                if m.len() > max_file_bytes { return; }
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return,
            };
            for m in re.find_iter(&bytes) {
                let (line, col, snippet) = locate_match(&bytes, m.start(), m.end());
                local.push(Hit { file_id: fe.id, line, col, snippet });
                if local.len() >= limit { break; }
            }
        } else {
            // Literal path: mmap + memchr via the new scan_file_literal
            // helper. Avoids the per-file Vec<u8> alloc + copy; lets
            // the kernel manage memory; overlaps cold-cache page
            // faults with the memmem scan. Measurable cold-cache win
            // vs the previous std::fs::read approach.
            let needle = pattern.as_bytes();
            let offsets = scry_store::scan_file_literal(
                Path::new(&path),
                needle, limit, max_file_bytes,
            );
            if offsets.is_empty() { return; }
            // To produce snippets we still need to read the file once
            // for line/col conversion. (locate_match needs full bytes.)
            // The page cache is already hot from scan_file_literal so
            // this read is cheap.
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return,
            };
            for abs in offsets {
                let (line, col, snippet) = locate_match(&bytes, abs, abs + needle.len());
                local.push(Hit { file_id: fe.id, line, col, snippet });
                if local.len() >= limit { break; }
            }
        }
        if !local.is_empty() {
            hit_count.fetch_add(local.len(), Ordering::Relaxed);
            hits.lock().extend(local);
        }
    });

    let mut hits = hits.into_inner();
    hits.truncate(limit);
    match (json, format) {
        (true, _) => {
            for h in &hits {
                let path = r.display_path_cached(h.file_id).unwrap_or("");
                let obj = serde_json::json!({
                    "path": path,
                    "line": h.line,
                    "col": h.col,
                    "snippet": h.snippet,
                });
                println!("{obj}");
            }
        }
        // --format=count: just the totals; no per-hit rows. Pays off
        // for "is this referenced AT ALL?" agent queries — the token
        // cost is one short line regardless of hit count.
        (false, Some("count")) => {
            println!("{} hits across {} files", hits.len(), total_files);
        }
        // --format=lines: rg-shaped one-per-line, no JSON envelope.
        // For "list the call sites of foo" this is ~5-10× smaller
        // than the JSON output and easier for grep-savvy users to
        // pipe into awk / xargs.
        (false, Some("lines")) => {
            for h in &hits {
                let path = r.display_path_cached(h.file_id).unwrap_or("");
                println!("{}:{}:{}\t{}", path, h.line, h.col, h.snippet);
            }
        }
        _ => {
            for h in &hits {
                let path = r.display_path_cached(h.file_id).unwrap_or("");
                println!("{}:{}:{}: {}", path, h.line, h.col, h.snippet);
            }
            eprintln!("\n{} hits across {} files", hits.len(), total_files);
        }
    }
    let label = match (is_regex, ignore_case) {
        (true,  true)  => "grep-regex-i",
        (true,  false) => "grep-regex",
        (false, true)  => "grep-i",
        (false, false) => "grep",
    };
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

pub(crate) fn cmd_build_offsets(index: Option<PathBuf>) -> Result<()> {
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

    let n_syms = build_one::<SymbolRecord>(
        &paths.symbols(), &paths.symbol_offsets(), "symbols"
    )?;
    let n_refs = build_one::<RefRecord>(
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

pub(crate) fn cmd_build_file_symbols(index: Option<PathBuf>) -> Result<()> {
    use std::io::{BufWriter, Write};
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[fsyms] target index: {}", index_dir.display());
    let paths = scry_store::StorePaths::new(index_dir.clone());

    // Need the file count + the symbol vec (lazy is fine — we walk it
    // exactly once). Open through StoreReader so the offsets sidecar is
    // available and we avoid loading the whole 10 GB symbols.bin into RAM.
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    let n_files = r.file_count();
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

/// `scry build-file-refs` — symmetric to `scry build-file-symbols`
/// but groups refs.bin entries by file_id. Powers `scry uses`.
/// Walks the lazy ref vec once; ~140MB sidecar on AOSP+Linux
/// (63M refs × 4 bytes per id + offsets).
pub(crate) fn cmd_build_file_refs(index: Option<PathBuf>) -> Result<()> {
    use std::io::{BufWriter, Write};
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[frefs] target index: {}", index_dir.display());
    let paths = scry_store::StorePaths::new(index_dir.clone());

    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    let n_files = r.file_count();
    eprintln!("[frefs] {} files, {} refs — building reverse map", n_files, r.n_refs());

    let t = Instant::now();
    let mut by_file: Vec<Vec<u32>> = vec![Vec::new(); n_files];
    let mut ref_idx: u32 = 0;
    for rr in r.iter_refs() {
        let fid = rr.file_id as usize;
        if fid < by_file.len() {
            by_file[fid].push(ref_idx);
        }
        ref_idx += 1;
        if ref_idx % 5_000_000 == 0 {
            eprintln!("[frefs] grouped {} M refs ({} ms)",
                ref_idx / 1_000_000, t.elapsed().as_millis());
        }
    }
    eprintln!("[frefs] grouping done in {} ms; writing sidecars",
        t.elapsed().as_millis());

    let data_tmp = paths.file_refs().with_extension("bin.tmp");
    let off_tmp = paths.file_refs_offsets().with_extension("bin.tmp");
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
    std::fs::rename(&data_tmp, paths.file_refs())?;
    std::fs::rename(&off_tmp, paths.file_refs_offsets())?;

    eprintln!("[frefs] DONE in {} ms. file_refs={} offsets={}",
        t.elapsed().as_millis(),
        human_bytes(std::fs::metadata(paths.file_refs()).map(|m| m.len()).unwrap_or(0)),
        human_bytes(std::fs::metadata(paths.file_refs_offsets()).map(|m| m.len()).unwrap_or(0)),
    );
    Ok(())
}

/// `scry uses NAME` — outgoing edges from NAME's body. For each
/// def of NAME, computes the body byte range (via the
/// enclosing_function heuristic from v0.1.20: byte_start of NAME
/// up to the next function's byte_start in the same file), then
/// returns every ref inside that range. With the `file_refs`
/// sidecar this is O(refs_in_file × log F); without it we'd
/// linearly scan all refs (much slower at AOSP scale, hence the
/// stern stderr nudge).
fn cmd_uses(
    name: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    kind: Option<String>,
    strict: bool,
    format: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    if let Some(f) = format.as_deref() {
        if !matches!(f, "count" | "paths") {
            anyhow::bail!("--format must be 'count' or 'paths' (got '{f}')");
        }
    }
    if json && format.as_deref() == Some("count") {
        anyhow::bail!("--json and --format=count are mutually exclusive");
    }
    let t = Instant::now();
    let r = open_index(index)?;
    let defs: Vec<SymbolRecord> = r.lookup_exact(&name).into_iter()
        .filter(|s| match r.display_path_cached(s.file_id) {
            Some(p) => path_matches(p, in_.as_deref(), not_in.as_deref()),
            None => in_.is_none() && not_in.is_none(),
        })
        .collect();
    if defs.is_empty() {
        let mut hint = String::new();
        if let Some(p) = in_.as_deref() { hint.push_str(&format!(" matching --in {p:?}")); }
        if let Some(p) = not_in.as_deref() { hint.push_str(&format!(" (excluding --not-in {p:?})")); }
        eprintln!("[scry] uses: no def of {name:?} found{hint}");
        // v0.1.54 — typo hint when no path filter ruled defs out.
        if in_.is_none() && not_in.is_none() {
            if let Some(h) = suggest_similar(&r, &name) {
                eprintln!("[scry] {h}");
            }
        }
        return Ok(());
    }

    // For each def, determine its body byte range. Use the same
    // partition-point logic as enclosing_function: body ends where
    // the next function-like symbol in the same file begins.
    let mut out_refs: Vec<RefRecord> = Vec::new();
    let mut seen_idx: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let kind_filter = kind.as_deref();
    for def in &defs {
        let body_end = next_function_byte_start(&r, def.file_id, def.byte_start)
            .unwrap_or(u32::MAX);
        let refs_in_file: Vec<u32> = match r.refs_for_file(def.file_id) {
            Some(v) => v,
            None => {
                eprintln!(
                    "[scry] uses: file_refs sidecar missing; run \
                     `scry build-file-refs --index DIR` for fast lookup. \
                     Falling back to a per-file linear scan (slow).",
                );
                r.iter_refs().enumerate()
                    .filter_map(|(i, rr)| (rr.file_id == def.file_id).then_some(i as u32))
                    .collect()
            }
        };
        for ref_idx in refs_in_file {
            let Some(rr) = r.get_ref(ref_idx) else { continue };
            if rr.byte_start < def.byte_start || rr.byte_start >= body_end { continue; }
            if let Some(k) = kind_filter {
                if !rr.kind.short().eq_ignore_ascii_case(k) { continue; }
            }
            // Dedupe by (file_id, byte_start, name) — same ref site
            // appearing across multiple defs (overloads) collapses.
            let key = ((rr.file_id as u64) << 32) | (rr.byte_start as u64);
            if seen_idx.insert(key) {
                out_refs.push(rr);
            }
        }
    }

    // v0.1.49 — --strict drops outgoing edges the resolver couldn't
    // attribute to a specific def. Useful for "show me only the
    // outgoing calls whose target we KNOW", filtering out the noise
    // of unresolved heuristic-only matches.
    if strict {
        let before = out_refs.len();
        out_refs.retain(|rr| rr.resolved_to.is_some());
        eprintln!(
            "[scry] uses --strict: {} → {} edges (unresolved dropped)",
            before, out_refs.len(),
        );
    }

    // v0.1.57 — --format paths / count on uses (symmetric with ref/callers).
    match format.as_deref() {
        Some("count") => {
            println!("{} edges", out_refs.len());
        }
        Some("paths") => {
            print_refs_paths(&r, &out_refs, limit, json);
            eprintln!("[scry] cmd=uses q={:?} defs={} hits={} elapsed={}ms",
                name, defs.len(), out_refs.len(), t.elapsed().as_millis());
        }
        _ => {
            if json {
                for rr in out_refs.iter().take(limit) {
                    println!("{}", ref_to_json(&r, rr));
                }
            } else {
                let shown = out_refs.len().min(limit);
                for rr in out_refs.iter().take(shown) {
                    let path = r.display_path_cached(rr.file_id).unwrap_or("<unknown>");
                    println!("{path}:{}:{}  ({} {})  {}",
                        rr.line, rr.col, rr.kind.short(), rr.lang.as_str(), rr.name);
                }
                println!("\n{} use{} (showing {})",
                    out_refs.len(),
                    if out_refs.len() == 1 { "" } else { "s" },
                    shown);
                eprintln!("[scry] cmd=uses q={:?} defs={} hits={} elapsed={}ms",
                    name, defs.len(), out_refs.len(), t.elapsed().as_millis());
            }
        }
    }
    Ok(())
}

/// Body-end heuristic: in `file_id`, find the next function-like
/// symbol whose byte_start is > `def_start`. Returns its
/// byte_start. None if there's no such symbol (def is the last
/// function in the file → body runs to EOF).
fn next_function_byte_start(
    r: &StoreReader,
    file_id: u32,
    def_start: u32,
) -> Option<u32> {
    let idxs = r.symbols_for_file(file_id)?;
    let mut starts: Vec<u32> = idxs.into_iter()
        .filter_map(|i| r.get_symbol(i))
        .filter(|s| matches!(s.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor))
        .map(|s| s.byte_start)
        .filter(|bs| *bs > def_start)
        .collect();
    starts.sort_unstable();
    starts.first().copied()
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
// ---------------------------------------------------------------------------
// build-digests (per-file blake3 content digest sidecar)
// ---------------------------------------------------------------------------
//
// The packed format is exactly `32 * n_files` bytes:
// `file_digests[file_id * 32 .. (file_id+1) * 32]` is the blake3 digest of
// that file's bytes. Files that can't be opened (deleted between index
// build and digest build, permission errors) get a zero digest — the
// incremental walker treats zero as "unknown" and rehashes.
//
// Hashing is parallel via rayon. Throughput on this corpus is ~3 GB/s
// per core; the full AOSP+Linux corpus (70 GB) hashes in ~25 s with
// 16 workers.
fn cmd_build_digests(index: Option<PathBuf>, workers: usize) -> Result<()> {
    use rayon::prelude::*;
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[digests] target index: {}", index_dir.display());
    if workers > 0 {
        // Best-effort: ignored if a global pool already exists.
        let _ = rayon::ThreadPoolBuilder::new().num_threads(workers).build_global();
    }
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    let n_files = r.file_count();
    eprintln!("[digests] {} files to hash", n_files);

    let t = Instant::now();
    // Compute (file_id, digest) in parallel and collect into a dense
    // Vec<[u8; 32]> sized to the file count.
    let mut digests: Vec<[u8; 32]> = vec![[0u8; 32]; n_files];
    let pairs: Vec<(u32, [u8; 32])> = r.par_iter_files().map(|fe| {
        let path = fe.display_path();
        match std::fs::read(&path) {
            Ok(bytes) => (fe.id, *blake3::hash(&bytes).as_bytes()),
            Err(_) => (fe.id, [0u8; 32]),  // unreadable → zero digest
        }
    }).collect();
    for (id, d) in pairs {
        if (id as usize) < digests.len() {
            digests[id as usize] = d;
        }
    }
    eprintln!("[digests] hashed in {} ms", t.elapsed().as_millis());

    // Write to .tmp then atomic-rename — same pattern as
    // build-file-symbols / build-resolutions.
    let tmp = paths.file_digests().with_extension("bin.tmp");
    {
        use std::io::{BufWriter, Write};
        let mut w = BufWriter::with_capacity(8 << 20, std::fs::File::create(&tmp)?);
        for d in &digests {
            w.write_all(d)?;
        }
        w.flush()?;
    }
    std::fs::rename(&tmp, paths.file_digests())?;
    eprintln!("[digests] DONE. {} bytes written → {}",
        32 * n_files, paths.file_digests().display());
    Ok(())
}

// ---------------------------------------------------------------------------
// index-diff (preview an incremental reindex without writing anything)
// ---------------------------------------------------------------------------
//
// Walks `roots`, hashes every file, compares against the existing
// index's `file_digests.bin`. Reports four sets:
//   - unchanged : same (root_id, relpath) → same digest
//   - changed   : same (root_id, relpath) → different digest
//   - added     : new (root_id, relpath)
//   - removed   : in old index, not in new walk
//
// This is the validation pre-step for `scry index --incremental` —
// if the diff doesn't match expectations, the full incremental
// commit would have done the wrong thing too.
fn cmd_index_diff(
    roots: Vec<PathBuf>,
    index: Option<PathBuf>,
    profile: String,
    verbose: bool,
    workers: usize,
    json: bool,
) -> Result<()> {
    use rayon::prelude::*;
    let index_dir = index.unwrap_or_else(default_index_dir);
    if workers > 0 {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(workers).build_global();
    }
    let roots = if roots.is_empty() { default_roots() } else { roots };
    if roots.is_empty() {
        anyhow::bail!("no source roots provided and no default available");
    }
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    if r.file_digests_mmap.is_none() {
        anyhow::bail!(
            "index at {} has no file_digests sidecar — run `scry build-digests` first",
            index_dir.display()
        );
    }

    // Build a map: (root_id, relpath) → (old file_id, old digest) from
    // the existing index.
    let mut old_map: std::collections::HashMap<(u8, String), (u32, [u8; 32])> =
        std::collections::HashMap::with_capacity(r.file_count());
    for fe in r.iter_files() {
        if let Some(d) = r.file_digest(fe.id) {
            old_map.insert((fe.root_id, fe.relpath.to_string()), (fe.id, d));
        }
    }
    let t = Instant::now();

    // Walk the roots and hash. Reuse the walker's classification so
    // the diff respects the same skiplist as a real index build.
    let mut all_new: Vec<(u8, String, [u8; 32])> = Vec::new();
    for (root_idx, root_path) in roots.iter().enumerate() {
        // Auto-detect profile per root unless the user pinned one.
        let prof = match profile.as_str() {
            "aosp" => Profile::Aosp,
            "linux" => Profile::Linux,
            "generic" => Profile::Generic,
            _ => Profile::auto_detect(root_path),
        };
        let collected = collect_files(root_path, prof)
            .with_context(|| format!("walk {}", root_path.display()))?;
        // Hash in parallel.
        let hashed: Vec<(String, [u8; 32])> = collected.files.par_iter().map(|rf| {
            let rel = rf.relpath.to_string_lossy().to_string();
            let bytes = std::fs::read(&rf.path).unwrap_or_default();
            let d = *blake3::hash(&bytes).as_bytes();
            (rel, d)
        }).collect();
        for (rel, d) in hashed {
            all_new.push((root_idx as u8, rel, d));
        }
    }

    // Compare.
    let mut unchanged = 0usize;
    let mut changed: Vec<(u8, String)> = Vec::new();
    let mut added: Vec<(u8, String)> = Vec::new();
    let mut seen_new: std::collections::HashSet<(u8, String)> =
        std::collections::HashSet::with_capacity(all_new.len());
    for (root_id, rel, d) in &all_new {
        let key = (*root_id, rel.clone());
        seen_new.insert(key.clone());
        match old_map.get(&key) {
            Some((_, old_d)) if old_d == d => unchanged += 1,
            Some(_) => changed.push(key),
            None => added.push(key),
        }
    }
    let mut removed: Vec<(u8, String)> = Vec::new();
    for k in old_map.keys() {
        if !seen_new.contains(k) {
            removed.push(k.clone());
        }
    }

    let elapsed_ms = t.elapsed().as_millis();
    if json {
        let entry = serde_json::json!({
            "unchanged": unchanged,
            "changed":   changed.len(),
            "added":     added.len(),
            "removed":   removed.len(),
            "elapsed_ms": elapsed_ms,
            "changed_files": if verbose { Some(changed.iter().map(|(_,r)| r).collect::<Vec<_>>()) } else { None },
            "added_files":   if verbose { Some(added.iter().map(|(_,r)| r).collect::<Vec<_>>()) } else { None },
            "removed_files": if verbose { Some(removed.iter().map(|(_,r)| r).collect::<Vec<_>>()) } else { None },
        });
        println!("{}", entry);
    } else {
        eprintln!("[index-diff] walked {} files in {} ms",
            all_new.len(), elapsed_ms);
        println!("unchanged: {unchanged}");
        println!("changed:   {} {}", changed.len(),
            if changed.is_empty() { "" } else { "(would re-parse)" });
        println!("added:     {} {}", added.len(),
            if added.is_empty() { "" } else { "(would parse fresh)" });
        println!("removed:   {} {}", removed.len(),
            if removed.is_empty() { "" } else { "(would tombstone)" });
        if verbose {
            for set in [("changed", &changed), ("added", &added), ("removed", &removed)] {
                if set.1.is_empty() { continue; }
                println!("\n--- {} ---", set.0);
                for (root_id, rel) in set.1 {
                    let root = roots.get(*root_id as usize)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| format!("<root {root_id}>"));
                    println!("  {}/{}", root, rel);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// scry index --incremental
// ---------------------------------------------------------------------------
//
// Selective-reparse + full-rebuild incremental. Skips the tree-sitter
// parse for files whose content digest matches the prior index;
// replays their existing records from the old index into a fresh
// writer; parses only the added + changed files; finalizes and
// atomically swaps the staging dir into place.
//
// Correctness pattern: we never mutate the old index. The new index
// is built fresh in `<out>.incr.tmp/` and only swapped in at the end
// via two rename calls. If the process dies mid-build, the old index
// stays queryable.
//
// Trigrams: re-extracted from disk for unchanged files (cheap with
// the kernel page cache; ~25 s on full corpus) plus produced by the
// regular parse path for changed files. Skipped unless
// --build-trigrams is also passed.
//
// What this version does NOT do:
//   - True append-only writes preserving old file_ids. The new index
//     reassigns file_ids 0..N sequentially; readers re-open and see
//     the new mapping. (No external state depends on file_id stability.)
//   - Re-run build-resolutions or build-file-symbols. The user can
//     invoke them post-incremental if they need the sidecars.
fn cmd_index_incremental(
    roots: Vec<PathBuf>,
    out: Option<PathBuf>,
    profile: Option<String>,
    workers: Option<usize>,
    max_file_bytes: u64,
    build_trigrams: bool,
) -> Result<()> {
    use rayon::prelude::*;
    let t_total = Instant::now();
    let out_dir = out.unwrap_or_else(default_index_dir);

    // === Phase 1: open old + verify prereqs ===
    if !out_dir.join("manifest.json").exists() {
        anyhow::bail!(
            "no existing index at {}; --incremental requires a prior `scry index` first",
            out_dir.display()
        );
    }
    let old = StoreReader::open(&out_dir)
        .with_context(|| format!("open existing index at {}", out_dir.display()))?;
    if old.file_digests_mmap.is_none() {
        anyhow::bail!(
            "index at {} has no file_digests.bin — run `scry build-digests` first \
             so --incremental can detect changes",
            out_dir.display()
        );
    }

    if let Some(n) = workers {
        if n > 0 {
            let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
        }
    }

    // === Phase 2: walk + hash + categorize ===
    // Resolve roots from CLI or fall back to defaults (matches non-
    // incremental cmd_index behavior).
    let roots = if roots.is_empty() { default_roots() } else { roots };
    if roots.is_empty() { anyhow::bail!("no source roots provided and no default available"); }

    // Map for fast diff: (root_id, relpath) -> (old_file_id, old_digest).
    // Tombstoned files are excluded — they're conceptually "already
    // deleted" and shouldn't influence the new index's contents.
    let old_map: std::collections::HashMap<(u8, String), (u32, [u8; 32])> =
        old.iter_files()
            .filter(|fe| !old.is_tombstoned(fe.id))
            .filter_map(|fe| old.file_digest(fe.id)
                .map(|d| ((fe.root_id, fe.relpath.to_string()), (fe.id, d))))
            .collect();

    eprintln!("[incremental] old index: {} files (post-tombstone)", old_map.len());

    // Walk + hash each root in turn. We need root_id stability between
    // old and new — assign by position in the roots vec. Where possible
    // (the typical case: same roots between runs) old_id == new_id.
    struct Categorized {
        root_id: u8,
        rf: RawFile,
        rel: String,
        digest: [u8; 32],
    }
    let mut all_files: Vec<Categorized> = Vec::new();
    for (root_idx, root_path) in roots.iter().enumerate() {
        let prof = match profile.as_deref() {
            Some("aosp") => Profile::Aosp,
            Some("linux") => Profile::Linux,
            Some("generic") => Profile::Generic,
            Some(other) => anyhow::bail!("unknown profile '{other}'"),
            None => Profile::auto_detect(root_path),
        };
        eprintln!("[incremental] walking root {} ({prof:?})", root_path.display());
        let collected = collect_files(root_path, prof)
            .with_context(|| format!("walk {}", root_path.display()))?;
        // Hash in parallel — blake3 is ~3 GB/s/core.
        let hashed: Vec<(RawFile, String, [u8; 32])> =
            collected.files.par_iter().map(|rf| {
                let rel = rf.relpath.to_string_lossy().to_string();
                let bytes = std::fs::read(&rf.path).unwrap_or_default();
                let digest = *blake3::hash(&bytes).as_bytes();
                (rf.clone(), rel, digest)
            }).collect();
        for (rf, rel, digest) in hashed {
            all_files.push(Categorized { root_id: root_idx as u8, rf, rel, digest });
        }
    }
    eprintln!("[incremental] walked + hashed {} files in {} ms",
        all_files.len(), t_total.elapsed().as_millis());

    // Split into unchanged vs needs_parse. Index by position in
    // all_files so we don't allocate parallel vectors of clones.
    let mut unchanged_idx: Vec<usize> = Vec::new();
    let mut needs_parse_idx: Vec<usize> = Vec::new();
    let mut seen_keys: std::collections::HashSet<(u8, String)> =
        std::collections::HashSet::with_capacity(all_files.len());
    for (i, c) in all_files.iter().enumerate() {
        let key = (c.root_id, c.rel.clone());
        seen_keys.insert(key.clone());
        match old_map.get(&key) {
            Some((_, old_d)) if *old_d == c.digest => unchanged_idx.push(i),
            _ => needs_parse_idx.push(i),
        }
    }
    let removed_count = old_map.keys().filter(|k| !seen_keys.contains(k)).count();
    let added_count = needs_parse_idx.iter()
        .filter(|i| !old_map.contains_key(&(all_files[**i].root_id, all_files[**i].rel.clone())))
        .count();
    let changed_count = needs_parse_idx.len() - added_count;

    eprintln!("[incremental] diff: {} unchanged, {} changed, {} added, {} removed",
        unchanged_idx.len(), changed_count, added_count, removed_count);

    if needs_parse_idx.is_empty() && removed_count == 0 {
        eprintln!("[incremental] no changes — index already current");
        return Ok(());
    }

    // === Phase 3: prepare staging writer ===
    // NOTE: Path extension MUST NOT be `.tmp`. StoreWriter::new_streaming
    // computes its own per-root staging path via `root.with_extension("tmp")`;
    // if we passed `out.incr.tmp` here the writer would resolve its tmp_dir
    // to the same path and delete our staging mid-build. `.incr` is safe.
    let staging = out_dir.with_extension("incr");
    // Best-effort cleanup of any stale staging dir from a crashed
    // prior run.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("create staging dir {}", staging.display()))?;
    let mut writer = StoreWriter::new_streaming(&staging)
        .with_context(|| "open streaming writer")?;
    if build_trigrams { writer.enable_trigrams(); }

    // Carry over root entries with their original profiles.
    for (i, root_path) in roots.iter().enumerate() {
        let prof = old.roots.iter()
            .find(|r| Path::new(&r.path) == root_path.as_path())
            .map(|r| r.profile)
            .unwrap_or_else(|| Profile::auto_detect(root_path));
        writer.roots.push(RootEntry {
            id: i as u8,
            path: root_path.display().to_string(),
            profile: prof,
        });
    }

    // === Phase 4: emit FileEntry records ===
    // Layout: unchanged files first (low new_file_ids), then needs_parse.
    // Build old_file_id -> new_file_id remap as we go.
    let mut new_fid: u32 = 0;
    let mut id_remap: std::collections::HashMap<u32, u32> =
        std::collections::HashMap::with_capacity(unchanged_idx.len());
    for &i in &unchanged_idx {
        let c = &all_files[i];
        let old_id = old_map[&(c.root_id, c.rel.clone())].0;
        id_remap.insert(old_id, new_fid);
        writer.files.push(FileEntry {
            id: new_fid, root_id: c.root_id,
            relpath: c.rel.clone(),
            kind: c.rf.kind, size: c.rf.size,
        });
        new_fid += 1;
    }
    let parse_id_start: u32 = new_fid;
    for &i in &needs_parse_idx {
        let c = &all_files[i];
        writer.files.push(FileEntry {
            id: new_fid, root_id: c.root_id,
            relpath: c.rel.clone(),
            kind: c.rf.kind, size: c.rf.size,
        });
        new_fid += 1;
    }
    // Per-file digest list (parallel to writer.files) so we can write
    // the new file_digests.bin sidecar after finalize.
    let mut new_digests: Vec<[u8; 32]> = Vec::with_capacity(writer.files.len());
    for &i in &unchanged_idx { new_digests.push(all_files[i].digest); }
    for &i in &needs_parse_idx { new_digests.push(all_files[i].digest); }

    // === Phase 5: replay unchanged records ===
    // Iterate ALL old symbols / refs once; keep only those whose
    // file_id is in the remap. Cheap relative to a parse pass.
    let t_replay = Instant::now();
    for s in old.iter_symbols() {
        if let Some(&new_fid) = id_remap.get(&s.file_id) {
            let mut s2 = s;
            s2.file_id = new_fid;
            writer.symbols.push(s2);
            if writer.symbols.len() >= 5_000_000 {
                writer.flush_symbols_chunk()
                    .with_context(|| "flush replayed symbols")?;
            }
        }
    }
    for r in old.iter_refs() {
        if let Some(&new_fid) = id_remap.get(&r.file_id) {
            let mut r2 = r;
            r2.file_id = new_fid;
            writer.refs.push(r2);
            if writer.refs.len() >= 5_000_000 {
                writer.flush_refs_chunk()
                    .with_context(|| "flush replayed refs")?;
            }
        }
    }
    eprintln!("[incremental] replayed {} symbols, {} refs from {} unchanged files in {} ms",
        writer.symbols.len() + writer.symbol_chunk_lens.iter().sum::<u64>() as usize,
        writer.refs.len() + writer.ref_chunk_lens.iter().sum::<u64>() as usize,
        unchanged_idx.len(), t_replay.elapsed().as_millis());

    // Replay trigrams from unchanged files. Re-read bytes (cheap;
    // kernel page cache is hot from the hash pass). Done in parallel
    // then merged single-threaded into the writer (push_trigrams
    // isn't Sync).
    if build_trigrams {
        let t_trig = Instant::now();
        let per_file: Vec<(u32, Vec<scry_store::trigram::Trigram>)> =
            unchanged_idx.par_iter().map(|&i| {
                let c = &all_files[i];
                let new_fid = id_remap[&old_map[&(c.root_id, c.rel.clone())].0];
                let bytes = std::fs::read(&c.rf.path).unwrap_or_default();
                let tgs = scry_store::trigram::extract_sorted(&bytes);
                (new_fid, tgs)
            }).collect();
        for (fid, tgs) in per_file {
            writer.push_trigrams(&tgs, fid);
        }
        if writer.trigrams.as_ref().is_some() {
            writer.flush_trigrams_chunk()
                .with_context(|| "flush replayed trigrams")?;
        }
        eprintln!("[incremental] replayed trigrams from {} files in {} ms",
            unchanged_idx.len(), t_trig.elapsed().as_millis());
    }

    // === Phase 6: parse the changed + added files ===
    let mut registry = FormatRegistry::new();
    for p in scry_lang::tree_sitter_parsers() { registry.register(p); }
    for p in scry_aosp::aosp_parsers() { registry.register(p); }
    let registry = Arc::new(registry);
    let t_parse = Instant::now();
    // No OOM skiplist for incremental — the set is small enough that
    // a problematic file's effects are obvious and the user can pin
    // a hard-skip explicitly.
    let skiplist: std::collections::HashSet<String> = std::collections::HashSet::new();
    let parsed: Vec<Option<(u32, Vec<SymbolRecord>, Vec<RefRecord>, Vec<scry_store::trigram::Trigram>)>> =
        needs_parse_idx.par_iter().enumerate().map(|(i, &cat_i)| {
            let c = &all_files[cat_i];
            let new_fid = parse_id_start + i as u32;
            let fe = FileEntry {
                id: new_fid, root_id: c.root_id,
                relpath: c.rel.clone(),
                kind: c.rf.kind, size: c.rf.size,
            };
            match parse_one(&c.rf, &fe, c.root_id, false, max_file_bytes,
                            &registry, &skiplist, None, build_trigrams) {
                Ok(t) => Some((new_fid, t.0, t.1, t.2)),
                Err(e) => {
                    eprintln!("[incremental] parse FAILED for {}: {e:#}", c.rf.path.display());
                    None
                }
            }
        }).collect();
    eprintln!("[incremental] parsed {} files in {} ms",
        needs_parse_idx.len(), t_parse.elapsed().as_millis());

    // Single-threaded merge of per-file results into the writer.
    for entry in parsed.into_iter().flatten() {
        let (fid, syms, refs, trigrams) = entry;
        writer.symbols.extend(syms);
        writer.refs.extend(refs);
        if build_trigrams && !trigrams.is_empty() {
            writer.push_trigrams(&trigrams, fid);
        }
    }

    // === Phase 7: finalize ===
    let final_files_total = writer.files.len();
    let stats = IndexStats {
        files_total: final_files_total as u64,
        files_parsed: needs_parse_idx.len() as u64,
        files_failed: 0,
        bytes_total: writer.files.iter().map(|f| f.size).sum(),
        symbols: 0,  // finalize_streaming computes the actual total
        refs: 0,
        elapsed_ms: t_total.elapsed().as_millis(),
    };
    writer.finalize_streaming(stats)
        .with_context(|| "finalize_streaming")?;

    // === Phase 8: write the file_digests sidecar for the new index ===
    // Use the same packed format as build-digests so future
    // --incremental runs see the fresh digests.
    let new_digests_path = staging.join("file_digests.bin");
    let tmp_digests = new_digests_path.with_extension("bin.tmp");
    {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp_digests)?);
        for d in &new_digests {
            f.write_all(d)?;
        }
        f.flush()?;
    }
    std::fs::rename(&tmp_digests, &new_digests_path)?;

    // === Phase 9: atomic swap ===
    // Two renames: old -> .bak, staging -> out. If either fails the
    // old index is recoverable from .bak; we clean it up on success.
    let bak = out_dir.with_extension("incr.bak");
    let _ = std::fs::remove_dir_all(&bak);  // clean any prior failed swap
    std::fs::rename(&out_dir, &bak)
        .with_context(|| format!("rename {} -> {}", out_dir.display(), bak.display()))?;
    if let Err(e) = std::fs::rename(&staging, &out_dir) {
        // Restore old on second-rename failure.
        let _ = std::fs::rename(&bak, &out_dir);
        return Err(anyhow::anyhow!("rename {} -> {} failed: {e}; old index restored",
            staging.display(), out_dir.display()));
    }
    // Cleanup the old index now that the new one is in place.
    let _ = std::fs::remove_dir_all(&bak);

    eprintln!(
        "[incremental] DONE in {} ms. {} files indexed ({} parsed, {} replayed)",
        t_total.elapsed().as_millis(),
        final_files_total,
        needs_parse_idx.len(),
        unchanged_idx.len(),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// tombstone (manually mark one file as deleted; next query skips it)
// ---------------------------------------------------------------------------
//
// Reads/creates `tombstones.bin`, sets the bit for the file_id matching
// `path`. The path can be either:
//   - absolute (matched against the file's display_path)
//   - root-relative substring (matched against display_path via contains)
fn cmd_tombstone(path: PathBuf, index: Option<PathBuf>) -> Result<()> {
    let index_dir = index.unwrap_or_else(default_index_dir);
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;

    let needle = path.to_string_lossy().to_string();
    let matches: Vec<u32> = r.iter_files()
        .filter(|fe| {
            let p = fe.display_path();
            p == needle || p.contains(&needle)
        })
        .map(|fe| fe.id)
        .collect();
    if matches.is_empty() {
        anyhow::bail!("no indexed file matches {}", needle);
    }

    // Read existing bitmap (or start empty) and grow as needed.
    let mut bitmap: Vec<u8> = if paths.tombstones().exists() {
        std::fs::read(paths.tombstones())?
    } else { Vec::new() };
    let max_id = matches.iter().copied().max().unwrap();
    let required_len = (max_id as usize) / 8 + 1;
    if bitmap.len() < required_len {
        bitmap.resize(required_len, 0u8);
    }
    let mut newly_marked = 0usize;
    for id in &matches {
        let byte = (*id as usize) / 8;
        let bit = (*id as usize) % 8;
        if (bitmap[byte] >> bit) & 1 == 0 {
            bitmap[byte] |= 1 << bit;
            newly_marked += 1;
        }
    }
    // Atomic write.
    let tmp = paths.tombstones().with_extension("bin.tmp");
    std::fs::write(&tmp, &bitmap)?;
    std::fs::rename(&tmp, paths.tombstones())?;
    eprintln!("[tombstone] marked {} file(s) ({} newly); bitmap is {} bytes",
        matches.len(), newly_marked, bitmap.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// callers --precise (clangd-driven, type-aware C++ references)
// ---------------------------------------------------------------------------
//
// Flow:
//   1. Look up NAME via scry's heuristic path to find a definition
//      site (file + line + column).
//   2. Spawn clangd, send initialize + didOpen for that file.
//   3. Send textDocument/references at the definition position.
//   4. Map LSP Locations back to scry file_ids via path.
//   5. Emit using the same format as `cmd_callers` so the caller
//      doesn't see a different shape just because they passed
//      --precise.
//
// Falls back to the heuristic path with a clear error when clangd
// or compile_commands.json is missing.
fn cmd_callers_precise(
    name: String,
    index: Option<PathBuf>,
    _lang: Option<String>,
    in_: Option<String>,
    not_in: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    if !clangd::clangd_available() {
        anyhow::bail!(
            "clangd not on PATH. --precise requires clangd for type-aware C++ \
             reference resolution.\n\
             Install: apt install clangd  (Debian/Ubuntu)\n\
             Or rerun without --precise for the heuristic path."
        );
    }
    let r = open_index(index)?;

    // 1. Pick the definition site to anchor the references query at.
    // We prefer a C++ Function/Method/Class definition matching the
    // name, since clangd is C++-shaped. If we can't find one, fall
    // back to the first match of any kind.
    let candidates = r.lookup_exact(&name);
    let def_site = candidates.iter()
        .find(|s| matches!(s.lang, FileKind::Cpp | FileKind::Header | FileKind::HeaderCpp))
        .or_else(|| candidates.first())
        .ok_or_else(|| anyhow::anyhow!("no definitions of '{name}' in the index"))?;
    let def_path = r.file_display_path(def_site.file_id)
        .ok_or_else(|| anyhow::anyhow!("def file_id {} out of range", def_site.file_id))?;
    let def_path = PathBuf::from(def_path);

    // 2. Discover compile_commands.json; clangd needs it for C++
    // resolution. If missing, error early — clangd would otherwise
    // run but every reference would come back wrong.
    let cc_dir = clangd::find_compile_commands(&def_path)
        .ok_or_else(|| anyhow::anyhow!(
            "no compile_commands.json found above {}\n\
             Generate one via `bear -- m` or your build system's equivalent.",
            def_path.display(),
        ))?;
    eprintln!("[precise] clangd OK; compile_commands.json under {}",
        cc_dir.display());

    // 3. Spawn clangd, didOpen the definition file, query references.
    let mut session = clangd::ClangdSession::start(Some(&cc_dir))
        .with_context(|| "starting clangd session")?;
    let lang_id = match def_site.lang {
        FileKind::C => "c",
        _ => "cpp",
    };
    session.did_open(&def_path, lang_id)?;
    // Clangd accepts 0-based char positions; def_site.col is 1-based.
    let char_0 = def_site.col.saturating_sub(1);
    let locs = session.references(&def_path, def_site.line, char_0, /* include_decl */ false)?;
    eprintln!("[precise] clangd returned {} locations in {} ms",
        locs.len(), t.elapsed().as_millis());

    // 4. Build a (path -> file_id) lookup so we can map LSP results
    // back to scry's path display + lang.
    let in_prefix = in_.as_deref();
    let not_in_prefix = not_in.as_deref();
    let by_path: std::collections::HashMap<String, FileKind> =
        r.iter_files().map(|fe| (fe.display_path(), fe.kind)).collect();
    let mut emitted = 0usize;
    if !json {
        for loc in &locs {
            let p = match loc.fs_path() { Some(p) => p, None => continue };
            let p_str = p.display().to_string();
            if !path_matches(&p_str, in_prefix, not_in_prefix) { continue; }
            let lang = by_path.get(&p_str).map(|k| k.as_str()).unwrap_or("?");
            println!("{}:{}:{}  (ref-precise {})  {}",
                p_str, loc.line, loc.character, lang, name);
            emitted += 1;
            if emitted >= limit { break; }
        }
    } else {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for loc in &locs {
            let p = match loc.fs_path() { Some(p) => p, None => continue };
            let p_str = p.display().to_string();
            if !path_matches(&p_str, in_prefix, not_in_prefix) { continue; }
            let lang = by_path.get(&p_str).map(|k| k.as_str()).unwrap_or("?");
            let obj = serde_json::json!({
                "name": name,
                "ref_kind": "call",
                "lang": lang,
                "path": p_str,
                "line": loc.line,
                "col": loc.character,
                "precise": true,
            });
            writeln!(out, "{}", obj)?;
            emitted += 1;
            if emitted >= limit { break; }
        }
    }
    log_query(&r, "callers-precise", &name, locs.len(), emitted, t);
    Ok(())
}

// ---------------------------------------------------------------------------
// build-embeddings (chunk every file, embed each chunk, write sidecars)
// ---------------------------------------------------------------------------
//
// Two outputs:
//   chunks.bin       bincode-encoded Vec<ChunkEntry> (file_id + line range)
//   embeddings.bin   8-byte header (dim u32 LE, count u32 LE) then
//                    `count` rows × `dim` × f32 LE
//
// The embedding model is the deterministic FNV-1a hashing trick in
// scry-store::embed — no model download, no external deps, identical
// across machines. Quality-wise: catches vocabulary overlap (the
// dominant signal for code search). A future feature-flagged
// transformer model can replace `embed_text` without changing the
// sidecar format.
fn cmd_build_embeddings(
    index: Option<PathBuf>,
    dim: usize,
    chunk_lines: usize,
    chunk_overlap: usize,
    workers: usize,
) -> Result<()> {
    use rayon::prelude::*;
    use scry_store::embed;
    let index_dir = index.unwrap_or_else(default_index_dir);
    if workers > 0 {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(workers).build_global();
    }
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    let n_files = r.file_count();
    eprintln!("[embed] {} files; dim={}, chunk={}+{}overlap",
        n_files, dim, chunk_lines, chunk_overlap);

    let t = Instant::now();
    // Per-file: read source, chunk, embed each chunk. Returns a Vec
    // of (ChunkEntry, embedding) which we'll flatten + sort by
    // file_id + start_line for stable on-disk ordering.
    let per_file: Vec<Vec<(embed::ChunkEntry, Vec<f32>)>> = r.par_iter_files().map(|fe| {
        let path = fe.display_path();
        let bytes = match std::fs::read(&path) { Ok(b) => b, Err(_) => return Vec::new() };
        let src = match std::str::from_utf8(&bytes) { Ok(s) => s, Err(_) => return Vec::new() };
        let mut out = Vec::new();
        for (start, end, body) in embed::chunk_lines(src, chunk_lines, chunk_overlap) {
            let v = embed::embed_text(body, dim);
            out.push((embed::ChunkEntry { file_id: fe.id, start_line: start, end_line: end }, v));
        }
        out
    }).collect();

    let mut all: Vec<(embed::ChunkEntry, Vec<f32>)> = per_file.into_iter().flatten().collect();
    // Stable sort: (file_id ASC, start_line ASC) so consumers can
    // binary-search by file_id or scan by file order.
    all.sort_by_key(|a| (a.0.file_id, a.0.start_line));
    eprintln!("[embed] computed {} chunks in {} ms", all.len(), t.elapsed().as_millis());

    // Write chunks.bin (bincode Vec<ChunkEntry>).
    {
        let chunks_only: Vec<embed::ChunkEntry> = all.iter().map(|(c, _)| c.clone()).collect();
        let tmp = paths.chunks().with_extension("bin.tmp");
        let f = std::fs::File::create(&tmp)?;
        bincode::serialize_into(std::io::BufWriter::new(f), &chunks_only)
            .map_err(|e| anyhow::anyhow!("bincode encode chunks: {e}"))?;
        std::fs::rename(&tmp, paths.chunks())?;
    }

    // Write embeddings.bin (header + packed f32).
    {
        use std::io::Write;
        let tmp = paths.embeddings().with_extension("bin.tmp");
        let mut f = std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(&tmp)?);
        f.write_all(&(dim as u32).to_le_bytes())?;
        f.write_all(&(all.len() as u32).to_le_bytes())?;
        for (_, v) in &all {
            for x in v {
                f.write_all(&x.to_le_bytes())?;
            }
        }
        f.flush()?;
        drop(f);
        std::fs::rename(&tmp, paths.embeddings())?;
    }
    let total_bytes = std::fs::metadata(paths.embeddings()).map(|m| m.len()).unwrap_or(0);
    eprintln!("[embed] DONE. {} chunks × {} dim → {} ({} ms)",
        all.len(), dim, human_bytes(total_bytes), t.elapsed().as_millis());
    Ok(())
}

// ---------------------------------------------------------------------------
// ask (semantic retrieval — embed query, cosine-rank chunks)
// ---------------------------------------------------------------------------

fn cmd_ask(
    query: String,
    index: Option<PathBuf>,
    in_: Option<String>,
    not_in: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let t = Instant::now();
    let r = open_index(index)?;
    if r.embeddings_mmap.is_none() || r.chunks.is_none() {
        anyhow::bail!(
            "index has no embedding sidecar — run `scry build-embeddings` first"
        );
    }
    let dim = r.embedding_dim as usize;
    // Embed query with the same kernel + dim used at build-time.
    let q_vec = scry_store::embed::embed_text(&query, dim);

    // Over-fetch from the ranker so the path filters can drop some
    // without starving the result count.
    let any_filter = in_.is_some() || not_in.is_some();
    let cap = limit.saturating_mul(if any_filter { 8 } else { 1 }).max(limit);
    let ranked = r.semantic_rank(&q_vec, cap);

    let mut shown = 0usize;
    let mut hits: Vec<serde_json::Value> = Vec::new();
    for (chunk_idx, sim) in ranked {
        let entry = match r.chunks.as_ref().and_then(|c| c.get(chunk_idx as usize)) {
            Some(e) => e, None => continue,
        };
        let fe = match r.file_view(entry.file_id) { Some(f) => f, None => continue };
        let path = fe.display_path();
        if !path_matches(&path, in_.as_deref(), not_in.as_deref()) { continue; }
        // Read a slice of the file to show context (best-effort).
        let snippet = chunk_snippet(&path, entry.start_line, entry.end_line);
        if json {
            hits.push(serde_json::json!({
                "path": path,
                "lang": fe.kind.as_str(),
                "start_line": entry.start_line,
                "end_line": entry.end_line,
                "score": sim,
                "snippet": snippet,
            }));
        } else {
            println!("{}:{}-{}  (score={:.3})  ({})",
                path, entry.start_line, entry.end_line, sim, fe.kind.as_str());
            if !snippet.is_empty() {
                // First 2 non-blank lines of the chunk as a tiny preview.
                for line in snippet.lines().filter(|l| !l.trim().is_empty()).take(2) {
                    println!("    {}", line.trim_end());
                }
            }
        }
        shown += 1;
        if shown >= limit { break; }
    }
    if json {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for h in &hits {
            writeln!(out, "{}", h)?;
        }
    }
    log_query(&r, "ask", &query, shown, shown, t);
    Ok(())
}

/// Read the chunk's byte range from disk for snippet display.
/// Best-effort: returns an empty string on any IO error so the
/// caller never crashes on a missing file.
fn chunk_snippet(path: &str, start_line: u32, end_line: u32) -> String {
    let bytes = match std::fs::read(path) { Ok(b) => b, Err(_) => return String::new() };
    let src = match std::str::from_utf8(&bytes) { Ok(s) => s, Err(_) => return String::new() };
    let lines: Vec<&str> = src.lines().collect();
    let s = (start_line as usize).saturating_sub(1).min(lines.len());
    let e = (end_line as usize).min(lines.len());
    if s >= e { return String::new(); }
    // Cap snippet size so large chunks don't blow out token budgets.
    let take = (e - s).min(8);
    lines[s..s + take].join("\n")
}

// ---------------------------------------------------------------------------
// compact (rewrite the index dropping tombstoned records)
// ---------------------------------------------------------------------------
//
// Reads the current index, builds a remap from old file_id → new file_id
// (compacted; tombstoned ids removed), streams every symbol/ref through,
// re-emits with the remapped file_id. The trigram FST + name FSTs need
// rebuilding from the surviving postings, which dominates the cost.
//
// For v1 this is a placeholder that errors clearly when there are
// tombstones; the full rebuild is a non-trivial standalone change.
// The expected use pattern is "incremental + occasional full rebuild
// via `scry index` overwriting the dir" — `compact` is the in-place
// alternative for very-large indexes where the full rebuild is
// expensive.
fn cmd_compact(index: Option<PathBuf>) -> Result<()> {
    let index_dir = index.unwrap_or_else(default_index_dir);
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let r = StoreReader::open(&index_dir)
        .with_context(|| format!("open index at {}", index_dir.display()))?;
    let n_files = r.file_count();
    let mut tombstoned = 0usize;
    for fe in r.iter_files() {
        if r.is_tombstoned(fe.id) { tombstoned += 1; }
    }
    if tombstoned == 0 {
        eprintln!("[compact] no tombstones in {} files — nothing to do", n_files);
        return Ok(());
    }
    eprintln!(
        "[compact] {} of {} files tombstoned ({:.1}%); rewriting would reclaim space.",
        tombstoned, n_files,
        (tombstoned as f64 / n_files as f64) * 100.0,
    );
    eprintln!("[compact] in-place rewrite is not yet implemented;");
    eprintln!("[compact] for now, run `scry index <roots> -o {} --workers N` for a clean rebuild.",
        index_dir.display());
    // Touch the tombstone sidecar so the user can see what's still
    // tombstoned via `xxd` etc.
    let _ = paths.tombstones();
    Ok(())
}

pub(crate) fn cmd_build_resolutions(index: Option<PathBuf>) -> Result<()> {
    use std::collections::HashMap;
    use std::io::{BufWriter, Write};
    let index_dir = index.unwrap_or_else(default_index_dir);
    eprintln!("[res] target index: {}", index_dir.display());
    let paths = scry_store::StorePaths::new(index_dir.clone());
    let r = StoreReader::open(&index_dir)?;
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
    let mut pass1 = |s: SymbolRecord| {
        // Java + Kotlin both emit one Package symbol per file (via the
        // package_declaration / package_header capture). Use it as the
        // per-file pkg key for the narrowing chain.
        if matches!(s.kind, SymbolKind::Package)
            && matches!(s.lang, FileKind::Java | FileKind::Kotlin)
        {
            per_file_pkg.insert(s.file_id, s.name.clone());
        }
        by_name.entry(s.name.clone()).or_default().push(ResolveDef {
            id: s.id, file_id: s.file_id, lang: s.lang,
            pkg: None,
            scope_path: s.scope_path.clone(),
        });
    };
    for s in r.iter_symbols() { pass1(s); }
    // Pass1b: stamp pkg on Java + Kotlin type-defs (Class/Interface/Enum/Object) —
    // small overhead, lets the resolver short-circuit by-package lookups
    // without re-resolving file_id → pkg on every ref.
    for entries in by_name.values_mut() {
        for e in entries.iter_mut() {
            if matches!(e.lang, FileKind::Java | FileKind::Kotlin) {
                if let Some(pkg) = per_file_pkg.get(&e.file_id) {
                    e.pkg = Some(pkg.clone());
                }
            }
        }
    }
    eprintln!("[res] pass 1 (by-name + per-file-pkg) in {} ms", t1.elapsed().as_millis());

    // --- Pass 2: per-file import lists (Java + Kotlin) + C++ using-directives
    //              + class-inheritance edges (v0.1.32). ---
    let t2 = Instant::now();
    let mut per_file_imports: HashMap<u32, Vec<(String, Option<String>)>> = HashMap::new();
    // Per-file C++ using-namespace directives: rr.name == "X" for
    // `using namespace X;`. Stored as Vec<String> per file.
    let mut per_file_using_ns: HashMap<u32, Vec<String>> = HashMap::new();
    // Class-inheritance edges (v0.1.32): child class simple name →
    // parent class simple names. Built from InheritFrom refs whose
    // ref name is the parent class and whose enclosing scope is the
    // child class. Lets resolve_one walk the chain to find methods
    // inherited from a parent.
    let mut child_to_parents: HashMap<String, Vec<String>> = HashMap::new();
    let mut process_import = |rr: &RefRecord| {
        match rr.kind {
            scry_store::RefKind::Import => {
                // Java + Kotlin: the importer emits the import ref with
                // name = the full qualified path. Split into pkg + simple.
                if !matches!(rr.lang, FileKind::Java | FileKind::Kotlin) { return; }
                let (pkg, simple) = match rr.name.rsplit_once('.') {
                    Some((p, s)) => (Some(p.to_string()), s.to_string()),
                    None => (None, rr.name.clone()),
                };
                per_file_imports.entry(rr.file_id).or_default().push((simple, pkg));
            }
            scry_store::RefKind::UsingNamespace => {
                // C++ `using namespace X;` — name is X (possibly with ::).
                if !matches!(rr.lang, FileKind::Cpp | FileKind::HeaderCpp | FileKind::Header) {
                    return;
                }
                per_file_using_ns.entry(rr.file_id).or_default().push(rr.name.clone());
            }
            scry_store::RefKind::InheritFrom => {
                if let Some(child) = rr.scope_path.last() {
                    // Avoid self-edges and noisy duplicates.
                    if child != &rr.name {
                        let parents = child_to_parents.entry(child.clone()).or_default();
                        if !parents.contains(&rr.name) {
                            parents.push(rr.name.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    };
    for rr in r.iter_refs() { process_import(&rr); }
    // Precompute the transitive ancestor set for each child class
    // ONCE (v0.1.33), so resolve_one doesn't BFS per-ref. Per-ref
    // cost drops from O(depth × pool) to O(pool) + 1 HashMap lookup.
    // Depth-capped at 8 levels — same as the previous BFS — to bound
    // pathological diamond hierarchies. Memory: ~139K classes × ~5
    // avg ancestors × ~20 B/string ≈ 14 MB, negligible.
    let class_to_ancestors: HashMap<String, std::collections::HashSet<String>> = {
        let mut out: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        for child in child_to_parents.keys() {
            let mut visited: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            visited.insert(child.clone());
            let mut queue: std::collections::VecDeque<String> = child_to_parents
                .get(child).map(|v| v.iter().cloned().collect()).unwrap_or_default();
            let mut ancestors: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut steps = 0usize;
            while let Some(cls) = queue.pop_front() {
                if !visited.insert(cls.clone()) { continue; }
                if steps > 8 { break; }
                steps += 1;
                ancestors.insert(cls.clone());
                if let Some(parents) = child_to_parents.get(&cls) {
                    for p in parents { queue.push_back(p.clone()); }
                }
            }
            if !ancestors.is_empty() {
                out.insert(child.clone(), ancestors);
            }
        }
        out
    };
    eprintln!("[res] pass 2 (per-file imports: {} files, cpp using-ns: {} files, \
               inheritance edges: {} children, ancestor sets: {}) in {} ms",
              per_file_imports.len(), per_file_using_ns.len(),
              child_to_parents.len(), class_to_ancestors.len(),
              t2.elapsed().as_millis());

    // --- Pass 3: resolve every ref, write sidecar. ---
    //
    // Parallel resolution with rayon: collect refs into chunks of
    // ~64K, resolve each chunk on a worker, then write chunks back
    // out in iteration order. The sidecar format is byte_offset =
    // ref_idx * 8, so ordering MUST be preserved — we collect chunks
    // sequentially from `iter_refs`, dispatch resolution in parallel,
    // and stream the results back in submission order.
    //
    // Memory: at peak we hold ~16 chunks × 64K refs × ~64B/ref ≈
    // ~64 MiB of input + ~16 chunks × 64K × 8B = ~8 MiB output.
    // Well within budget.
    let t3 = Instant::now();
    let tmp = paths.ref_resolutions().with_extension("bin.tmp");
    let mut ow = BufWriter::with_capacity(8 << 20, std::fs::File::create(&tmp)?);
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    const CHUNK_SIZE: usize = 64 * 1024;
    let resolved_count = AtomicU64::new(0);
    let narrowed_count = AtomicU64::new(0);
    let mut chunk: Vec<RefRecord> = Vec::with_capacity(CHUNK_SIZE);
    let process_chunk = |chunk: &mut Vec<RefRecord>, ow: &mut BufWriter<std::fs::File>| -> Result<()> {
        if chunk.is_empty() { return Ok(()); }
        let ids: Vec<u64> = chunk.par_iter().map(|rr| {
            let mut local_narrowed = 0u64;
            let id = resolve_one(rr, &by_name, &per_file_pkg, &per_file_imports,
                                 &per_file_using_ns, &class_to_ancestors, &mut local_narrowed);
            if local_narrowed > 0 { narrowed_count.fetch_add(local_narrowed, Ordering::Relaxed); }
            if id != 0 { resolved_count.fetch_add(1, Ordering::Relaxed); }
            id
        }).collect();
        for id in ids { ow.write_all(&id.to_le_bytes())?; }
        chunk.clear();
        Ok(())
    };
    for rr in r.iter_refs() {
        chunk.push(rr);
        if chunk.len() >= CHUNK_SIZE {
            process_chunk(&mut chunk, &mut ow)?;
        }
    }
    process_chunk(&mut chunk, &mut ow)?;
    ow.flush()?;
    drop(ow);
    std::fs::rename(&tmp, paths.ref_resolutions())?;
    let resolved_count = resolved_count.into_inner();
    let narrowed_count = narrowed_count.into_inner();
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
    rr: &RefRecord,
    by_name: &std::collections::HashMap<String, Vec<ResolveDef>>,
    per_file_pkg: &std::collections::HashMap<u32, String>,
    per_file_imports: &std::collections::HashMap<u32, Vec<(String, Option<String>)>>,
    per_file_using_ns: &std::collections::HashMap<u32, Vec<String>>,
    class_to_ancestors: &std::collections::HashMap<String, std::collections::HashSet<String>>,
    narrowed: &mut u64,
) -> u64 {
    let cands = match by_name.get(&rr.name) {
        Some(c) if !c.is_empty() => c,
        _ => return 0,
    };
    if cands.len() == 1 { return cands[0].id; }

    let same_lang: Vec<&ResolveDef> = cands.iter().filter(|c| c.lang == rr.lang).collect();
    let pool: &[&ResolveDef] = if !same_lang.is_empty() { &same_lang[..] } else {
        return cands[0].id;
    };
    if pool.len() == 1 { return pool[0].id; }

    // Same-file preference (all langs). A call to `foo()` inside file F
    // is much more likely to resolve to `foo` defined in F than to one
    // of the many `foo`s elsewhere in the corpus. Catches self-calls
    // and inner-class references without needing receiver inference.
    {
        let same_file: Vec<&&ResolveDef> = pool.iter()
            .filter(|c| c.file_id == rr.file_id).collect();
        if same_file.len() == 1 {
            *narrowed += 1;
            return same_file[0].id;
        }
    }

    // Same-class preference (v0.1.31). A method call inside class chain
    // [..., ClassC] should prefer a candidate defined in the same class
    // chain (cand.scope_path is a prefix of OR equal to ref.scope_path).
    // Catches sibling-method calls within a class even when the def and
    // call are in different files (partial classes, generated code,
    // implicit-this from inner classes). Complements same-file preference
    // by extending across files. The C++ same-namespace rule does the
    // same shape for C++; this generalizes it across all languages.
    if !rr.scope_path.is_empty() {
        let same_class: Vec<&&ResolveDef> = pool.iter()
            .filter(|c| !c.scope_path.is_empty()
                        && rr.scope_path.starts_with(&c.scope_path))
            .collect();
        if same_class.len() == 1 {
            *narrowed += 1;
            return same_class[0].id;
        }
    }

    // Inheritance walk (v0.1.32, optimized in v0.1.33). The
    // class_to_ancestors map is precomputed ONCE in pass 2, so
    // here we just check if any candidate's enclosing class is in
    // the ref's ancestor set. If exactly one candidate qualifies →
    // prefer it. Multiple matches (diamond / interface conflict) →
    // fall through (so an import-aware narrowing could still
    // disambiguate).
    if let Some(my_class) = rr.scope_path.last() {
        if let Some(ancestors) = class_to_ancestors.get(my_class) {
            let ancestor_matches: Vec<&&ResolveDef> = pool.iter()
                .filter(|c| c.scope_path.last()
                    .is_some_and(|cls| ancestors.contains(cls)))
                .collect();
            if ancestor_matches.len() == 1 {
                *narrowed += 1;
                return ancestor_matches[0].id;
            }
        }
    }

    // Java / Kotlin share the same package-narrowing shape: same package
    // → explicit import → wildcard import → implicit-import fallback.
    // The implicit-import fallback differs per language (`java.lang` for
    // Java; the kotlin.* / kotlin.collections.* / kotlin.io.* / kotlin.text.*
    // family for Kotlin).
    if matches!(rr.lang, FileKind::Java | FileKind::Kotlin) {
        let my_pkg = per_file_pkg.get(&rr.file_id);
        let imports = per_file_imports.get(&rr.file_id);

        if let Some(pkg) = my_pkg {
            for c in pool {
                if c.pkg.as_deref() == Some(pkg.as_str()) {
                    *narrowed += 1;
                    return c.id;
                }
            }
        }
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
            // Import-aware class-narrowing for method-call refs (v0.1.28).
            // Java imports are class-level, not method-level, so the rules
            // above (which match `simple == &rr.name`) only catch the
            // constructor case `new Foo()` where Foo is imported.
            // Regular method calls like `session.close()` need a different
            // angle: check whether any candidate's OWNING CLASS is imported
            // in the calling file. If exactly one such candidate exists,
            // prefer it — the file imports a single class with this method,
            // strong signal the call targets it. Multiple imported
            // candidates → still ambiguous (fall through to implicit_pkgs).
            let imported_class_matches: Vec<&&ResolveDef> = pool.iter().filter(|c| {
                let Some(c_pkg) = c.pkg.as_deref() else { return false };
                let Some(outer) = c.scope_path.first() else { return false };
                imps.iter().any(|(simple, ipkg)| {
                    let ipkg = match ipkg { Some(p) => p.as_str(), None => return false };
                    (simple == outer || simple == "*") && ipkg == c_pkg
                })
            }).collect();
            if imported_class_matches.len() == 1 {
                *narrowed += 1;
                return imported_class_matches[0].id;
            }
        }
        let implicit_pkgs: &[&str] = match rr.lang {
            FileKind::Java => &["java.lang"],
            FileKind::Kotlin => &[
                "kotlin",
                "kotlin.annotation",
                "kotlin.collections",
                "kotlin.comparisons",
                "kotlin.io",
                "kotlin.ranges",
                "kotlin.sequences",
                "kotlin.text",
                "kotlin.jvm",
            ],
            _ => &[],
        };
        for c in pool {
            if let Some(p) = &c.pkg {
                if implicit_pkgs.contains(&p.as_str()) {
                    *narrowed += 1;
                    return c.id;
                }
            }
        }
    }

    // C++ narrowing: prefer same-namespace > using-namespace > fallback.
    // Approximation: the namespace of a definition is its scope_path
    // (which compute_scope built from namespace_definition nodes). The
    // ref's "current namespace" is its own scope_path. A candidate is
    // "same namespace" if its scope_path equals OR is a prefix of the
    // ref's scope. "Using-namespace" candidates have scope_path starting
    // with one of the names in `using namespace X;` in the ref's file.
    if matches!(rr.lang, FileKind::Cpp | FileKind::HeaderCpp | FileKind::Header) {
        // 1. Same-namespace (or enclosing-namespace) match.
        if !rr.scope_path.is_empty() {
            for c in pool {
                if !c.scope_path.is_empty() && rr.scope_path.starts_with(&c.scope_path) {
                    *narrowed += 1;
                    return c.id;
                }
            }
        }
        // 2. `using namespace X;` directive match.
        if let Some(uses) = per_file_using_ns.get(&rr.file_id) {
            for u in uses {
                // `using namespace foo::bar;` → look for cand whose
                // scope starts with [foo, bar].
                let parts: Vec<&str> = u.split("::").collect();
                for c in pool {
                    if c.scope_path.len() >= parts.len()
                        && c.scope_path.iter().zip(parts.iter()).all(|(a, b)| a == b)
                    {
                        *narrowed += 1;
                        return c.id;
                    }
                }
            }
        }
    }

    // Truthful unresolved for ambiguous method/ctor/field calls.
    // Without receiver-type inference we cannot pick the right method
    // from a sea of same-named candidates. Returning 0 here (rather
    // than the misleading pool[0]) lets `--def-in PATH`'s permissive
    // branch include the ref instead of incorrectly excluding it.
    // Other ref kinds (type-use, import, inherit) keep the pool[0]
    // fallback — they tend to refer to types, which are far less
    // ambiguous than methods sharing a common name.
    use scry_store::RefKind;
    if matches!(rr.kind, RefKind::Call | RefKind::FieldAccess | RefKind::Ctor) {
        return 0;
    }

    pool[0].id
}

/// One candidate definition for a ref. Loaded from the lazy symbols
/// vec during the resolution pre-pass; the resolver then narrows by
/// pkg (Java / Kotlin) or scope_path / using-namespace (C++), and
/// falls back to "first same-lang candidate" otherwise. `file_id` is
/// kept so pass1b can stamp each Java/Kotlin type-def with the package
/// its file declares (resolver short-circuit: by-package lookup
/// without re-resolving file_id → pkg on every ref). `scope_path` is
/// the def's namespace nesting, used by the C++ narrowing path.
#[derive(Clone)]
struct ResolveDef {
    id: u64,
    file_id: u32,
    lang: FileKind,
    pkg: Option<String>,
    scope_path: Vec<String>,
}

// ---------------------------------------------------------------------------
// build-trigrams (standalone — add trigram index to an existing index)
// ---------------------------------------------------------------------------

pub(crate) fn cmd_build_trigrams(
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
    eprintln!("[trigrams] {} files in index", r.file_count());

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
    let n_files = r.file_count();
    let total_batches = (n_files + batch_size - 1) / batch_size.max(1);
    let mut chunk_count: u32 = 0;
    let t_total = Instant::now();
    let total_failed = AtomicU64::new(0);
    let total_skipped = AtomicU64::new(0);
    let total_trigram_pushes = AtomicU64::new(0);

    let mut start = 0usize;
    let mut batch_no = 0usize;
    while start < n_files {
        let end = (start + batch_size).min(n_files);
        batch_no += 1;
        // Materialise borrowed views for this batch into a Vec we can
        // hand to rayon; `FileView` is Copy so this is cheap.
        let slice: Vec<scry_store::FileView<'_>> =
            (start as u32 .. end as u32).filter_map(|i| r.file_view(i)).collect();
        let sink: parking_lot::Mutex<Vec<(scry_store::trigram::Trigram, u32)>> =
            parking_lot::Mutex::new(Vec::with_capacity(slice.len() * 4096));
        let t_batch = Instant::now();
        slice.par_iter().for_each(|fe| {
            if fe.size > max_file_bytes {
                total_skipped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let path = fe.display_path();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => { total_failed.fetch_add(1, Ordering::Relaxed); return; }
            };
            let trigrams = scry_store::trigram::extract_sorted(&bytes);
            if trigrams.is_empty() { return; }
            let mut s = sink.lock();
            s.reserve(trigrams.len());
            for t in &trigrams { s.push((*t, fe.id)); }
            total_trigram_pushes.fetch_add(trigrams.len() as u64, Ordering::Relaxed);
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
        total_failed.load(Ordering::Relaxed),
        total_skipped.load(Ordering::Relaxed),
        total_trigram_pushes.load(Ordering::Relaxed),
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
    let result = match (prefix_cands, suffix_cands) {
        (Some(p), Some(s)) => {
            // AND-intersect smaller-into-larger to minimize hash lookups.
            let (mut keep, drop_) = if p.len() <= s.len() { (p, s) } else { (s, p) };
            keep.retain(|f| drop_.contains(f));
            Some(keep)
        }
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    };
    // Safety net: a regex whose literal-extraction yields an empty
    // candidate set is almost always lossy literal extraction
    // (`[Bb]` split into pieces too short to be useful trigrams,
    // alternation imploding to nothing, etc.) — not a confident "no
    // file matches". Fall back to full scan rather than silently
    // returning zero hits. Eval-agent regression:
    //   scry grep 'Trace\.traceBegin.*[Bb]roadcast' → 0 hits in v0.1.25
    //   same query                                  → real hits in v0.1.26
    match result {
        Some(s) if s.is_empty() => None,
        other => other,
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
    let pb = Path::new(&path);
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
        let bp_path = r.display_path_cached(rr.file_id).unwrap_or("");
        let module_name = rr.scope_path.get(1).cloned().unwrap_or_default();
        let module_type = rr.scope_path.first().cloned().unwrap_or_default();
        println!("{} ({})  declared in {}", module_name, module_type, bp_path);
    }
    eprintln!("\n{} module(s)", out.len());
    Ok(())
}

// `cmd_health` lives in crate::health.

// ---------------------------------------------------------------------------
// owner (walk OWNERS chain)
// ---------------------------------------------------------------------------
//
// Walks up from PATH collecting OWNERS entries from each enclosing
// OWNERS file. Returns the nearest non-empty owner set by default;
// --include-deep returns every level so the caller can see the
// inheritance chain explicitly. Matches Gerrit's evaluation order
// (nearest first).
fn cmd_owner(
    path: PathBuf,
    index: Option<PathBuf>,
    include_deep: bool,
    accumulate: bool,
    json: bool,
) -> Result<()> {
    let r = open_index(index)?;
    // Resolve the queried path against the indexed roots; we accept
    // both absolute paths and root-relative substrings.
    let needle = path.to_string_lossy().to_string();
    let target_fe = r.iter_files().find(|fe| {
        let p = fe.display_path();
        p == needle || p.contains(&needle)
    });
    let target_path = match target_fe {
        Some(fe) => PathBuf::from(fe.display_path()),
        None => path.clone(),  // not in the index; still walk fs path
    };

    // Collect OWNERS files by walking up from the target. Gerrit's
    // semantics: walk continues until either the filesystem root or
    // an OWNERS file that declares `set noparent`. The set-noparent
    // file itself IS visited and its emails count; only the inherited
    // chain above it is cut.
    let mut layers: Vec<OwnersLayer> = Vec::new();
    let mut cur = if target_path.is_file() {
        target_path.parent().map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"))
    } else {
        target_path.clone()
    };
    loop {
        let owners_path = cur.join("OWNERS");
        if owners_path.is_file() {
            let parsed = parse_owners_file(&owners_path);
            // Always record set-noparent layers (the user wants to know
            // a boundary was reached) and any layer with content.
            let has_content = !parsed.emails.is_empty() || !parsed.per_file.is_empty();
            if include_deep || accumulate || has_content || parsed.noparent {
                layers.push(OwnersLayer {
                    file: owners_path.clone(),
                    emails: parsed.emails.clone(),
                    per_file: parsed.per_file,
                    noparent: parsed.noparent,
                });
            }
            // Nearest-non-empty: legacy default. Stop after first
            // direct-email hit. Does NOT trigger when accumulate or
            // include-deep is set — those want the full walk.
            if !include_deep && !accumulate && !parsed.emails.is_empty() {
                break;
            }
            // set noparent always stops the walk after visiting this
            // file, regardless of mode.
            if parsed.noparent {
                break;
            }
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => break,
        }
    }

    if json {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let arr: Vec<serde_json::Value> = layers.iter().map(|l| serde_json::json!({
            "owners_file":    l.file.display().to_string(),
            "emails":         l.emails,
            "per_file_rules": l.per_file,
            "set_noparent":   l.noparent,
        })).collect();
        let mut envelope = serde_json::json!({
            "path":   target_path.display().to_string(),
            "layers": arr,
        });
        if accumulate {
            let mut all: Vec<String> = layers.iter()
                .flat_map(|l| l.emails.iter().cloned()).collect();
            all.sort();
            all.dedup();
            envelope["approvers"] = serde_json::json!(all);
        }
        writeln!(out, "{}", envelope)?;
    } else if layers.is_empty() {
        println!("(no OWNERS file above {})", target_path.display());
    } else {
        println!("owners for {}:", target_path.display());
        for l in &layers {
            let np = if l.noparent { " [set noparent]" } else { "" };
            println!("  via {}{}", l.file.display(), np);
            for e in &l.emails { println!("    {e}"); }
            for pf in &l.per_file { println!("    per-file: {pf}"); }
        }
        if accumulate {
            let mut all: Vec<String> = layers.iter()
                .flat_map(|l| l.emails.iter().cloned()).collect();
            all.sort();
            all.dedup();
            println!();
            println!("approvers ({}):", all.len());
            for e in &all { println!("  {e}"); }
        }
    }
    Ok(())
}

struct OwnersLayer {
    file: PathBuf,
    emails: Vec<String>,
    per_file: Vec<String>,
    noparent: bool,
}

struct OwnersParsed {
    emails: Vec<String>,
    per_file: Vec<String>,
    noparent: bool,
}

/// Parse an OWNERS file: collect direct email entries, per-file rules,
/// and the `set noparent` flag.
///
/// Direct emails are lines containing `@` with no spaces — the most
/// common form (`alice@example.com`). `per-file PATTERN = EMAILS_OR_FILE`
/// rules are common in AOSP and we keep them as-is for display so
/// the caller can see "this dir has no direct owners but ActivityManager*
/// is owned by file:/ACTIVITY_MANAGER_OWNERS".
///
/// `set noparent` is a Gerrit-defined directive meaning "do not inherit
/// from any OWNERS file above this one". cmd_owner uses this to halt
/// the walk. `include …` directives still aren't followed (out of
/// scope; would require recursive resolution against the indexed tree).
fn parse_owners_file(path: &Path) -> OwnersParsed {
    let txt = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return OwnersParsed { emails: Vec::new(), per_file: Vec::new(), noparent: false },
    };
    let mut emails = Vec::new();
    let mut per_file = Vec::new();
    let mut noparent = false;
    for line in txt.lines() {
        let trim = line.trim();
        if trim.is_empty() || trim.starts_with('#') { continue; }
        // Gerrit accepts both "set noparent" and the bare "noparent" form
        // in practice; honor both.
        if trim == "set noparent" || trim == "noparent" {
            noparent = true;
            continue;
        }
        if trim.starts_with("set ") || trim.starts_with("include ") { continue; }
        if let Some(rest) = trim.strip_prefix("per-file") {
            per_file.push(rest.trim().to_string());
            continue;
        }
        if trim.starts_with("file:") {
            per_file.push(trim.to_string());
            continue;
        }
        if trim.contains('@') && !trim.contains(' ') {
            emails.push(trim.to_string());
        }
    }
    OwnersParsed { emails, per_file, noparent }
}

/// `scry diff --since COMMITISH` — surface symbols/refs in files
/// that have changed since a git revision. Per-root: shell out to
/// `git -C ROOT diff --name-only COMMITISH..HEAD`, intersect the
/// resulting paths with the file table, emit per-file summaries.
///
/// Roots that aren't git repos are skipped with a one-line warning;
/// we don't try to be clever about repo discovery (the user knows
/// their tree better than we do; if they want per-project AOSP
/// behavior they can `scry diff --in PROJECT/`).
fn cmd_diff(
    since: String,
    in_: Option<String>,
    verbose: bool,
    limit: usize,
    index: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let reader = open_index(index)?;
    let in_filter = in_.unwrap_or_default();

    // Collect changed (root_id, relpath) pairs from every git root.
    let mut changed: std::collections::HashSet<(u8, String)> = std::collections::HashSet::new();
    for root in &reader.roots {
        let root_path = Path::new(&root.path);
        if !root_path.join(".git").exists() {
            eprintln!("[scry diff] skipping non-git root: {}", root.path);
            continue;
        }
        match git_changed_files(root_path, &since) {
            Ok(paths) => {
                for p in paths {
                    changed.insert((root.id, p));
                }
            }
            Err(e) => {
                eprintln!("[scry diff] git failed in {}: {e}", root.path);
            }
        }
    }
    if changed.is_empty() {
        if !json {
            eprintln!("(no changed files in any indexed git root since {})", since);
        }
        return Ok(());
    }

    // Intersect with the file table. We index files by (root_id, relpath)
    // so the lookup is O(N) in the changed set, not O(N×M).
    let mut hits: Vec<scry_store::FileView<'_>> = reader.iter_files()
        .filter(|fe| changed.contains(&(fe.root_id, fe.relpath.to_string())))
        .filter(|fe| in_filter.is_empty() || fe.display_path().contains(&in_filter))
        .collect();
    // Sort by path for a deterministic output ordering.
    hits.sort_by(|a, b| a.relpath.cmp(b.relpath));
    hits.truncate(limit);

    if json {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        use std::io::Write;
        for fe in &hits {
            let symbols: Vec<u32> = reader.symbols_for_file(fe.id).unwrap_or_default();
            let entry = serde_json::json!({
                "path": fe.display_path(),
                "lang": fe.kind.as_str(),
                "symbol_count": symbols.len(),
                "symbols": if verbose {
                    Some(symbols.iter()
                        .filter_map(|i| reader.get_symbol(*i))
                        .map(|s| symbol_to_json(&reader, &s))
                        .collect::<Vec<_>>())
                } else {
                    None
                },
            });
            writeln!(out, "{}", entry)?;
        }
    } else {
        println!("{} changed file{} since {} (showing {})",
                 changed.len(),
                 if changed.len() == 1 { "" } else { "s" },
                 since,
                 hits.len());
        for fe in &hits {
            let symbols: Vec<u32> = reader.symbols_for_file(fe.id).unwrap_or_default();
            println!("  {} ({}) — {} symbol{}",
                     fe.display_path(),
                     fe.kind.as_str(),
                     symbols.len(),
                     if symbols.len() == 1 { "" } else { "s" });
            if verbose {
                for sym_id in symbols.iter().take(20) {
                    if let Some(s) = reader.get_symbol(*sym_id) {
                        println!("      {}:{}  ({})  {}", s.line, s.col, s.kind.short(), s.name);
                    }
                }
                if symbols.len() > 20 {
                    println!("      … {} more", symbols.len() - 20);
                }
            }
        }
    }
    Ok(())
}

/// Run `git -C ROOT diff --name-only SINCE..HEAD` and parse the output
/// as a list of repo-relative paths. Returns `Err` with stderr context
/// if git fails (bad revision, not a repo, etc.) so the caller can
/// report a clear message per-root.
fn git_changed_files(root: &Path, since: &str) -> Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C").arg(root)
        .arg("diff").arg("--name-only")
        .arg(format!("{since}..HEAD"))
        .output()
        .with_context(|| format!("spawn git in {}", root.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow::anyhow!("git diff exit {:?}: {}",
                                   out.status.code(), stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout.lines()
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect())
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
    for line in rd.lines().map_while(std::result::Result::ok) {
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
    let resolved = index.clone().unwrap_or_else(default_index_dir);
    let reader = open_index(index)?;
    // Auto-warm: same rationale as cmd_serve — the first agent /
    // Claude tool call shouldn't pay cold-mmap latency.
    let _ = warm_index_dir(&resolved);
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
        "initialize" => Ok(mcp_initialize_result(params)),
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

/// MCP protocol versions this server supports. Listed newest first so
/// `MCP_SUPPORTED_VERSIONS[0]` is our preferred latest. Our wire shape
/// (initialize / tools/list / tools/call with text content parts) has
/// been stable since 2024-11-05; the newer revisions add optional
/// features (tasks, elicitation, output schemas) we don't yet emit, so
/// declaring support is safe — we just don't use the new fields.
const MCP_SUPPORTED_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Reply to MCP `initialize`. Per spec §lifecycle/Version Negotiation:
/// if the server supports the version the client requested, it MUST
/// respond with the same version; otherwise it MUST respond with its
/// latest supported version (the client then decides whether to
/// continue or disconnect).
///
/// `tools` is the only capability we advertise — we don't (yet)
/// implement prompts, resources, sampling, logging, or tasks, so we
/// don't claim them.
fn mcp_initialize_result(params: &serde_json::Value) -> serde_json::Value {
    let requested = params.get("protocolVersion").and_then(|v| v.as_str());
    let agreed = match requested {
        Some(v) if MCP_SUPPORTED_VERSIONS.contains(&v) => v,
        _ => MCP_SUPPORTED_VERSIONS[0],
    };
    serde_json::json!({
        "protocolVersion": agreed,
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
    // Shared property fragments. Each description is one short
    // sentence written for a 1B-class model: it has to land the hint
    // without depending on the model "knowing what to do" from
    // surrounding context.
    let lang_prop = serde_json::json!({"type": "string",
        "description": "Language filter — narrow results to one of: java, kotlin, cpp, rust, go, python, soong, aidl. Always pass this when you know the language; cuts noise on cross-language names."});
    let in_prop = serde_json::json!({"type": "string",
        "description": "Path substring filter (e.g. 'frameworks/base/' to scope to a subtree). Useful when one name lives in many directories."});
    let not_in_prop = serde_json::json!({"type": "string",
        "description": "Negative path substring filter — drop results whose file path contains this substring. Symmetric to `in`. Common use: `not_in: '/tests/'` to exclude test files. Combined with `in` to scope + exclude in one call."});
    let limit_prop = serde_json::json!({"type": "integer", "default": 20,
        "description": "Max records returned. Pass a small integer (5-20). Do NOT pass placeholders like 'N' — they fail to parse."});
    let format_count_prop = serde_json::json!({"type": "string",
        "description": "Optional. Set to 'count' to get just `N callers` / `N hits` instead of per-record output — ~50× cheaper in tokens when you only need to know IF or HOW MANY. Mutually exclusive with json output mode."});

    let tools = vec![
        tool(
            "def",
            "Find exact-name symbol definitions. If a name is common \
             (e.g. 'Activity', 'Binder', 'Buffer'), you will get hits \
             from multiple kinds and languages — ALWAYS pass `kind` \
             (e.g. 'class') and/or `lang` to narrow the search; \
             otherwise the top hits may be Python test files or \
             unrelated structs.",
            obj(&["name"], serde_json::json!({
                "name": {"type": "string", "description": "Exact symbol name (case-sensitive)."},
                "lang": lang_prop,
                "kind": {"type": "string",
                    "description": "Kind filter. Common values: class, method, fn (Rust/Go function), interface, struct, enum, aidl.iface, soong (Soong module), init.svc, sepolicy. Strongly recommended when name is ambiguous."},
                "in":   in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
            })),
        ),
        tool(
            "ref",
            "Find all references to a name (any ref kind — call, ctor, \
             type-use, import, inherit). Use `callers` for the common \
             call-only case. Pass `format: 'count'` if you only need \
             the total. Set `reachable: true` to drop refs in modules \
             that can't actually link to a definition of `name` per \
             the build graph — eliminates cross-module false positives \
             on AOSP / kernel / GN-based projects when the index was \
             built with `scry index --build <system>`.",
            obj(&["name"], serde_json::json!({
                "name": {"type": "string"},
                "lang": lang_prop,
                "kind": {"type": "string",
                    "description": "Ref-kind filter. Common: call, ctor, inherit, import, type-use, field-access."},
                "in":   in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
                "format": format_count_prop,
                "reachable": {"type": "boolean", "default": false,
                    "description": "Filter refs by Soong/GN/kernel module-graph reachability. No-op if the index has no module_graph.json sidecar. Opt-in (not default) because the module graph is ~256MB and adds ~50-500ms to first query in a process."},
                "lexical": {"type": "boolean", "default": false,
                    "description": "Use lexical (tree-sitter) name match only. Default behavior auto-engages clang USR + SCIP symbol identity filters whenever their sidecars are present, dropping name-collision false positives across C/C++/ObjC + Java/Kotlin/Rust/Go/TS/Python. Set true to see raw name-match results (debugging / want-everything mode)."},
                "clang_precise": {"type": "boolean", "default": true,
                    "description": "Filter refs by clang USR identity (C/C++/ObjC). DEFAULTS TO TRUE: auto-engages whenever the clang_usrs.bin sidecar is present (`scry clang-index ...` produces it). No-op when the sidecar is absent. Set false (or pass `lexical: true`) to suppress."},
                "scip_precise": {"type": "boolean", "default": true,
                    "description": "Filter refs by SCIP symbol identity (any language with a SCIP indexer: Java / Kotlin / Rust / Go / TS / Python). DEFAULTS TO TRUE: auto-engages whenever the scip_index.bin sidecar is present (`scry scip-import ...` produces it). No-op when the sidecar is absent. Set false (or pass `lexical: true`) to suppress."},
                "scope": {"type": "string",
                    "description": "Keep only refs whose enclosing scope_path contains this class/namespace as an exact segment (e.g. \"BroadcastQueueImpl\")."},
                "def_in": {"type": "string",
                    "description": "Substring of the def-site file path. Keeps only refs whose Layer 2 resolution (resolved_to) points at a def in a file containing this path — e.g. `def_in: \"PerfettoTrace.java\"` disambiguates `close` when many unrelated classes also define `close`. Refs whose resolved_to is None pass through (over-include rather than silently drop). No-op without a build-resolutions sidecar."},
                "strict": {"type": "boolean", "default": false,
                    "description": "Drop refs whose Layer 2 resolution didn't land on a specific def (resolved_to=None). With `def_in`, also drops the permissive over-include — only refs the resolver confidently attributed to the target survive. Without `def_in`, shows only refs that resolved to some specific def."},
                "format": {"type": "string", "enum": ["by-def", "paths"],
                    "description": "Output mode. `by-def` returns a histogram array `[{count, def: {path, line, col, scope, kind, id}}, ..., {count, def: null}]` sorted descending by count; the unresolved bucket is last. `paths` returns a deduped sorted array of file path strings — cheapest way to ask `which files contain refs to X?`. Without `format`, returns the per-ref JSONL stream."},
            })),
        ),
        tool(
            "callers",
            "Find call sites of NAME (shorthand for `ref` with \
             kind=call). For 'does X get called anywhere?' or 'how \
             many?', pass `format: 'count'` — it returns just `N \
             callers` and costs almost nothing. Build-symbol \
             precision filters (clang USRs / SCIP symbols) \
             auto-engage when their sidecars are present; pass \
             `lexical: true` for raw name-match. Set `reachable: \
             true` for build-graph pruning (extra ~50-500ms cost).",
            obj(&["name"], serde_json::json!({
                "name": {"type": "string"},
                "lang": lang_prop,
                "in":   in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
                "format": format_count_prop,
                "reachable": {"type": "boolean", "default": false,
                    "description": "Same as on `ref` — filters by build-graph reachability when the module_graph.json sidecar is present. Opt-in."},
                "lexical": {"type": "boolean", "default": false,
                    "description": "Same as on `ref` — opt out of all auto-engaged precision filters and return raw name-match callers."},
                "clang_precise": {"type": "boolean", "default": true,
                    "description": "Same as on `ref` — clang USR identity filter; auto-engages when sidecar present."},
                "scip_precise": {"type": "boolean", "default": true,
                    "description": "Same as on `ref` — SCIP symbol identity filter; auto-engages when sidecar present."},
                "scope": {"type": "string",
                    "description": "Keep only callers whose enclosing scope_path contains this class as an exact segment. Big win on hub functions."},
                "def_in": {"type": "string",
                    "description": "Substring of the def-site file path. Keeps only callers whose Layer 2 resolution (resolved_to) points at a def in a file containing this path — e.g. `def_in: \"PerfettoTrace.java\"` cuts through cases where many classes share a method name like `close`. Callers whose resolved_to is None pass through. No-op without a build-resolutions sidecar."},
                "strict": {"type": "boolean", "default": false,
                    "description": "Drop callers whose Layer 2 resolution didn't land on a specific def. With `def_in`, also drops the permissive over-include — only callers the resolver confidently attributed survive. Trades recall for precision."},
                "format": {"type": "string", "enum": ["by-def", "paths"],
                    "description": "Output mode. `by-def` returns a histogram array `[{count, def: {...}}, ..., {count, def: null}]` sorted descending by count; the unresolved bucket is last — invaluable for polymorphic names like `close`, `onCreate`, `transact`. `paths` returns a deduped sorted array of file path strings — cheapest way to ask `which files call X?`."},
            })),
        ),
        tool(
            "subclasses",
            "Direct (or transitive) subclasses of a type. LSP \
             typeHierarchy/subtypes. Set `depth: N` to walk the \
             hierarchy N levels (default 0 = direct children only).",
            obj(&["name"], serde_json::json!({
                "name":  {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
                "depth": {"type": "integer", "minimum": 0, "default": 0,
                    "description": "BFS depth. 0 = direct subclasses; 1 = grandchildren too; etc."},
                "format": {"type": "string", "enum": ["count", "paths"],
                    "description": "Optional. `count` returns `{count: N}`. `paths` returns a deduped sorted array of file paths — `which files define a subtype of X?`. Without `format`, returns per-symbol records."},
            })),
        ),
        tool(
            "implementations",
            "Implementations of an interface — alias for `subclasses` \
             with Java/Kotlin-flavored naming. LSP \
             implementationProvider/implementationsForType.",
            obj(&["name"], serde_json::json!({
                "name":  {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
                "depth": {"type": "integer", "minimum": 0, "default": 0},
                "format": {"type": "string", "enum": ["count", "paths"],
                    "description": "See `subclasses.format`."},
            })),
        ),
        tool(
            "uses",
            "Outgoing edges from NAME's body — what does NAME call \
             or reference? Symmetric to `callers`. Returns up to \
             `limit` refs inside NAME's function body. Use \
             `kind: \"call\"` to restrict to call sites. Requires \
             the file_refs sidecar (`scry build-file-refs`) for \
             O(1) per-file lookup.",
            obj(&["name"], serde_json::json!({
                "name":  {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "kind":  {"type": "string",
                    "description": "Filter by ref kind: call, type, field, import, inherit, using-ns. Default: all."},
                "limit": limit_prop,
                "strict": {"type": "boolean", "default": false,
                    "description": "Drop outgoing edges whose Layer 2 resolution didn't pin a target. Use for `what does NAME call that we KNOW the target of?` — strips heuristic-only matches the resolver couldn't attribute."},
                "format": {"type": "string", "enum": ["count", "paths"],
                    "description": "Optional. `count` returns `{count: N}` — cheapest probe for `how many edges`. `paths` returns a deduped sorted array of file paths — `which files does NAME touch?`. Without `format`, returns per-ref JSONL stream."},
            })),
        ),
        tool(
            "callgraph",
            "Recursive callers tree for NAME — \"how does control \
             flow reach this function?\". Walks call refs upward N \
             levels via `enclosing_function` resolution (more \
             accurate than scope_path for Java/Kotlin where the \
             scope is the class). `--max-nodes` caps total expansion \
             on hub functions (logger, assert, etc.). Root-level \
             callers are auto-filtered by clang USR + SCIP symbol \
             identity when those sidecars are present; pass \
             `lexical: true` to opt out and see the raw name-match \
             tree.",
            obj(&["name"], serde_json::json!({
                "name":  {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "depth": {"type": "integer", "minimum": 1, "default": 3},
                "max_nodes": {"type": "integer", "minimum": 1, "default": 200},
                "reachable": {"type": "boolean", "default": false},
                "lexical": {"type": "boolean", "default": false,
                    "description": "Use lexical (tree-sitter) name match only. Default behavior auto-engages clang USR + SCIP symbol identity filters on the ROOT level whenever the sidecars are present. Deeper recursion stays lexical (walker doesn't track per-name precision context)."},
                "clang_precise": {"type": "boolean", "default": true,
                    "description": "Filter root-level callers by clang USR identity (C/C++/ObjC). Auto-on when sidecar present; pass `lexical: true` to suppress."},
                "scip_precise": {"type": "boolean", "default": true,
                    "description": "Filter root-level callers by SCIP symbol identity. Auto-on when sidecar present; pass `lexical: true` to suppress."},
                "def_in": {"type": "string",
                    "description": "Substring of the def-site file path for the ROOT callee. Same shape as `ref --def-in`. Narrows ONLY the topmost level — deeper recursive levels are not filtered because the walker doesn't track per-frame def context."},
                "strict": {"type": "boolean", "default": false,
                    "description": "Drop root-level callers whose Layer 2 resolution didn't land on a specific def. With `def_in`, also drops the permissive over-include."},
            })),
        ),
        tool(
            "impact",
            "\"What breaks if I change NAME?\" — composes callers + \
             transitive subclasses into one deduped impact set. \
             Returns counts (callers, subclasses, files_touched) plus \
             the up-to-`limit` instances of each. Use this as a \
             pre-flight check before refactors: small counts → safe \
             rename; large counts → split the change. The callers \
             leg auto-engages clang USR + SCIP symbol identity \
             filters (sidecar-gated); pass `lexical: true` to opt \
             out. Subclasses leg is identity-based already \
             (type-hierarchy lookup).",
            obj(&["name"], serde_json::json!({
                "name":  {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
                "subclass_depth": {"type": "integer", "minimum": 0, "default": 2,
                    "description": "BFS depth for the subclass leg of the impact set."},
                "reachable": {"type": "boolean", "default": false,
                    "description": "Build-graph reachability filter on callers leg only."},
                "lexical": {"type": "boolean", "default": false,
                    "description": "Use lexical (tree-sitter) name match only for the callers leg. Default behavior auto-engages clang USR + SCIP symbol identity filters whenever the sidecars are present."},
                "clang_precise": {"type": "boolean", "default": true,
                    "description": "Filter callers by clang USR identity (C/C++/ObjC). Auto-on when sidecar present; pass `lexical: true` to suppress."},
                "scip_precise": {"type": "boolean", "default": true,
                    "description": "Filter callers by SCIP symbol identity. Auto-on when sidecar present; pass `lexical: true` to suppress."},
                "def_in": {"type": "string",
                    "description": "Narrow the callers portion by def-site path (same as `ref --def-in`). Doesn't affect the subclass walk."},
                "strict": {"type": "boolean", "default": false,
                    "description": "Drop callers whose Layer 2 resolution didn't land on a specific def."},
            })),
        ),
        tool(
            "prefix",
            "Symbols whose name starts with PREFIX (FST-backed \
             completion). Useful for 'what's everything starting \
             with Activity?'.",
            obj(&["prefix"], serde_json::json!({
                "prefix": {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
            })),
        ),
        tool(
            "fuzzy",
            "Typo-tolerant symbol search, ranked by edit distance. \
             Use when you're not sure of the exact spelling. If you \
             ARE sure, use `def` (exact match, cheaper).",
            obj(&["substr"], serde_json::json!({
                "substr": {"type": "string"},
                "in":    in_prop,
                "not_in": not_in_prop,
                "distance": {"type": "integer", "default": 2,
                    "description": "Levenshtein bound for typo tolerance (1-3 is sensible; higher = noisier results)."},
                "limit": limit_prop,
            })),
        ),
        tool(
            "grep",
            "Content search across indexed source. Literal pattern \
             unless `regex: true`. Set `case_insensitive: true` for \
             case-folded match (e.g. 'bindservice' finds 'bindService') \
             — trigram pre-filter expands across case variants so this \
             stays fast on big indexes. For 'is X mentioned at all?' \
             prefer `format: 'count'`; for 'list all hits' use \
             `format: 'lines'` (rg-shape, much cheaper in tokens \
             than the default JSON envelope).",
            obj(&["pattern"], serde_json::json!({
                "pattern": {"type": "string"},
                "regex":   {"type": "boolean", "default": false,
                    "description": "Treat pattern as a regex. Default is literal substring."},
                "case_insensitive": {"type": "boolean", "default": false,
                    "description": "Match case-insensitively. Works for literal and regex patterns. Trigram pre-filter expands each query trigram across ASCII case variants so this stays fast."},
                "lang":    lang_prop,
                "in":      in_prop,
                "not_in":  not_in_prop,
                "limit":   limit_prop,
                "format":  {"type": "string",
                    "description": "Optional. 'lines' = rg-shape `path:line:col\\tsnippet` per hit (cheapest list form). 'count' = just `N hits across M files`. Mutually exclusive with json output."},
            })),
        ),
        tool(
            "outline",
            "Every symbol defined in one file, ordered by line. PATH \
             matches by suffix (e.g. 'Activity.java' works if \
             unambiguous). Set `with_snippets: N` to also include \
             the first N source lines of each symbol — saves a \
             round-trip when you'd otherwise call `def` per name.",
            obj(&["path"], serde_json::json!({
                "path":  {"type": "string",
                    "description": "Full or suffix-style path (e.g. 'app_main.cpp' if no ambiguity, or the full /home/... path)."},
                "limit": limit_prop,
                "with_snippets": {"type": "integer", "default": 0,
                    "description": "If > 0, attach the first N source lines of each symbol as a `snippet` field. Lines clip at 200 chars."},
            })),
        ),
        tool(
            "tldr",
            "One-call file summary: language, total symbol count, \
             per-kind breakdown, top 3 ranked symbols, and the file's \
             first non-blank line. Use this FIRST when the question \
             is 'what does this file do?' — saves ~70% of the tokens \
             vs `outline + N×def`.",
            obj(&["path"], serde_json::json!({
                "path": {"type": "string",
                    "description": "Full or suffix-style path (same matching rules as `outline`)."},
            })),
        ),
        tool(
            "coverage",
            "Subtree stats: files / bytes / symbols per language for \
             any directory inside the index. Useful for 'what \
             fraction of $repo did scry actually parse?'.",
            obj(&["path"], serde_json::json!({
                "path":    {"type": "string",
                    "description": "Path prefix to scope (empty string = whole index)."},
                "by_kind": {"type": "boolean", "default": false,
                    "description": "Also break down per SymbolKind within each language."},
            })),
        ),
        tool(
            "stats",
            "Index metadata: size, file/symbol/ref counts, freshness, \
             scry_version, and Layer 2 resolution coverage \
             (`refs_resolved` + `refs_resolved_pct`, both null when \
             the build-resolutions sidecar isn't present). No \
             arguments. Useful as a first probe before harder \
             queries — the resolution percentage tells you how much \
             of the call-graph the resolver has nailed down.",
            serde_json::json!({
                "type": "object", "properties": serde_json::json!({}),
            }),
        ),
        tool(
            "ask",
            "Semantic retrieval: find code chunks whose content is \
             most similar to a natural-language query. Use when you \
             DON'T know an identifier name to grep / def for (e.g. \
             'how is process priority computed?'). Requires `scry \
             build-embeddings` to have run on the index; returns a \
             tool-level error otherwise.",
            obj(&["query"], serde_json::json!({
                "query": {"type": "string",
                    "description": "Natural-language description of what you're looking for."},
                "in":    in_prop,
                "not_in": not_in_prop,
                "limit": limit_prop,
            })),
        ),
    ];
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
    let name = params.get("name").and_then(|v| v.as_str())
        .ok_or_else(|| "missing required parameter: 'name'".to_string())?;
    let arguments = params.get("arguments").cloned().unwrap_or(serde_json::json!({}));

    // Unknown tool: surface as a tool-level error (isError: true) per
    // MCP convention rather than a JSON-RPC -32601 error. A
    // JSON-RPC error would tell the client "the protocol failed";
    // we want "the tool call failed; the call shape was valid".
    if mcp_required_args_for(name).is_none() {
        return Ok(mcp_tool_error(format!("unknown tool: '{}'. \
            Call tools/list to see available tools.", name)));
    }

    // Required-arg validation. The tool schemas advertise required
    // fields; the wrapper must enforce them so a malformed call from
    // a client (or LLM) doesn't silently coerce to an empty-string
    // query and return garbage hits.
    if let Some(missing) = mcp_validate_required_args(name, &arguments) {
        return Ok(mcp_tool_error(format!(
            "missing or empty required argument '{}' for tool '{}'",
            missing, name,
        )));
    }

    // Route through serve_one_request so any future change to the
    // serve surface (new args, ranking tweaks) is picked up here too.
    let req = serde_json::json!({
        "id": 1, "cmd": name, "args": arguments,
    });
    let line = req.to_string();
    let mut buf: Vec<u8> = Vec::new();
    serve_one_request(reader, &line, &mut buf)
        .map_err(|e| format!("internal error invoking serve: {e:#}"))?;
    let resp_line = String::from_utf8(buf).map_err(|e| format!("non-utf8 serve output: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(resp_line.trim())
        .map_err(|e| format!("serve response parse error: {e}"))?;

    // Envelope-level error (e.g. unknown cmd that slipped past
    // mcp_required_args_for — shouldn't happen but defensive).
    // serve returns {"id": N, "error": "msg"} for these.
    if let Some(err) = resp.get("error") {
        let msg = err.as_str().map(String::from).unwrap_or_else(|| err.to_string());
        return Ok(mcp_tool_error(msg));
    }

    let result = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);

    // Tool-level error: the call protocol succeeded but the tool
    // couldn't satisfy the request (the canonical case: `ask` against
    // an index without an embedding sidecar). serve emits these as
    // `{"error": "..."}` in the result. MCP spec: set isError: true
    // so the client can branch correctly. Unwrap the bare message
    // string from the {error: "..."} envelope before placing it in
    // the text content — otherwise the LLM sees a JSON literal it
    // has to re-parse to find the human-readable hint.
    if let Some(err_val) = result.as_object().and_then(|m| m.get("error")) {
        let msg = err_val.as_str().map(String::from)
            .unwrap_or_else(|| err_val.to_string());
        return Ok(mcp_tool_error(msg));
    }

    let text = serde_json::to_string(&result).map_err(|e| format!("encode: {e}"))?;
    Ok(serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
}

/// Build an MCP "tool-level error" response: well-formed result with
/// `isError: true` and a human-readable text content part. Used for
/// all tool-call failures that aren't protocol-level (unknown tool,
/// missing required arg). Distinguishes from JSON-RPC errors which
/// indicate the *call* failed at the protocol level.
fn mcp_tool_error(msg: String) -> serde_json::Value {
    serde_json::json!({
        "content": [{"type": "text", "text": msg}],
        "isError": true,
    })
}

/// Required-args lookup. Keep in sync with `mcp_tools_list_result`'s
/// inputSchema declarations — these two functions must agree or the
/// MCP server lies about what it accepts. Returns `None` if the tool
/// name is unknown (callers treat that as "no such tool").
fn mcp_required_args_for(tool: &str) -> Option<&'static [&'static str]> {
    Some(match tool {
        "def"      => &["name"],
        "ref"      => &["name"],
        "callers"  => &["name"],
        "subclasses"      => &["name"],
        "implementations" => &["name"],
        "impact"          => &["name"],
        "callgraph"       => &["name"],
        "uses"            => &["name"],
        "prefix"   => &["prefix"],
        "fuzzy"    => &["substr"],
        "grep"     => &["pattern"],
        "outline"  => &["path"],
        "tldr"     => &["path"],
        "coverage" => &["path"],
        "stats"    => &[],
        "ask"      => &["query"],
        _ => return None,
    })
}

/// Validate that every required arg for `tool` is present in `args`
/// AND non-empty (an empty string would coerce to a meaningless
/// "match anything" query that returns garbage). Returns the name of
/// the first missing/empty arg, or `None` if all present.
fn mcp_validate_required_args(tool: &str, args: &serde_json::Value) -> Option<String> {
    let required = mcp_required_args_for(tool)?;
    for name in required {
        let v = args.get(name);
        let ok = match v {
            Some(serde_json::Value::String(s)) => !s.is_empty(),
            Some(serde_json::Value::Number(_) | serde_json::Value::Bool(_)) => true,
            Some(serde_json::Value::Array(a)) => !a.is_empty(),
            Some(serde_json::Value::Object(o)) => !o.is_empty(),
            Some(serde_json::Value::Null) | None => false,
        };
        if !ok {
            return Some((*name).to_string());
        }
    }
    None
}

/// Entry point for the `serve` subcommand. Dispatches to the requested
/// transport — stdin/stdout (default) or a bound listener (unix / tcp).
/// The shared `StoreReader` lives for the whole process and is borrowed
/// by every connection; mmap-backed and immutable, so no synchronization
/// is needed across concurrent clients.
fn cmd_serve(index: Option<PathBuf>, listen: Option<String>, max_conns: u32) -> Result<()> {
    let resolved = index.clone().unwrap_or_else(default_index_dir);
    let reader = open_index(index)?;
    // Daemon auto-warm: prefault every sidecar into the OS page cache
    // BEFORE accepting connections. Without this the first agent to
    // query pays cold-mmap latency (tens to hundreds of ms per file
    // depending on which sidecar gets touched first). The warm pass
    // runs once per daemon process and is a fixed cost; per-request
    // latency stays sub-10 ms warm for the daemon's lifetime.
    let _ = warm_index_dir(&resolved);
    match listen.as_deref() {
        None => serve_stdio(&reader),
        Some(spec) => serve_listener(&reader, spec, max_conns),
    }
}

/// `scry build-modgraph` entrypoint. Reads native build metadata
/// (Cargo.toml workspace, Soong module-graph.json output, etc.),
/// emits scry's canonical v1 module_graph.json at `output`. After
/// this, `scry callers X --reachable` honors the build-graph
/// reachability filter automatically.
pub(crate) fn cmd_build_modgraph(kind: &str, root: &Path, output: &Path) -> Result<()> {
    let t = Instant::now();
    let g = build_adapter::build_modgraph(kind, root)?;
    let json = serde_json::to_string_pretty(&g)
        .context("serialize module-graph as JSON")?;
    // Write to <output>.tmp then rename so a partial write doesn't
    // leave a broken sidecar in the index dir.
    let tmp = output.with_extension("json.tmp");
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&tmp, &json)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, output)
        .with_context(|| format!("rename {} -> {}", tmp.display(), output.display()))?;
    eprintln!(
        "[modgraph] {}: {} modules, {} dep edges, {} files attributed; wrote {} ({} bytes) in {} ms",
        kind, g.modules.len(), g.deps.len(), g.files.len(),
        output.display(), json.len(), t.elapsed().as_millis(),
    );
    Ok(())
}

// `cmd_clang_*` and `cmd_scip_*` live in crate::precision_subcmds.

/// Run the warm pass and print a one-line summary. Standalone
/// `scry warm --index DIR` entrypoint; the daemon paths call
/// `warm_index_dir` directly without the summary print.
fn cmd_warm(index: Option<PathBuf>) -> Result<()> {
    let dir = index.unwrap_or_else(default_index_dir);
    if !dir.is_dir() {
        anyhow::bail!("not an index directory: {}", dir.display());
    }
    let t = Instant::now();
    let stats = warm_index_dir(&dir)?;
    eprintln!(
        "[warm] {} files, {} read in {} ms — page cache primed",
        stats.files, human_bytes(stats.bytes), t.elapsed().as_millis(),
    );
    Ok(())
}

#[derive(Default)]
struct WarmStats { files: u64, bytes: u64 }

/// Prefault every regular file in the index directory into the OS
/// page cache. Parallel one-file-per-rayon-task; each file gets a
/// posix_fadvise(WILLNEED) hint followed by a sequential read pass
/// (the actual fault-in — fadvise alone is just a kernel scheduler
/// hint and doesn't guarantee residency). Errors are swallowed
/// (warming is best-effort; a partial warmup is still useful).
fn warm_index_dir(dir: &Path) -> Result<WarmStats> {
    use rayon::prelude::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    let entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => return Ok(WarmStats::default()),
    };
    let bytes = AtomicU64::new(0);
    entries.par_iter().for_each(|p| {
        scry_store::prefault_path(p);
        if let Ok(mut f) = std::fs::File::open(p) {
            let mut buf = vec![0u8; 1 << 20];
            let mut n: u64 = 0;
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(k) => n += k as u64,
                    Err(_) => break,
                }
            }
            bytes.fetch_add(n, Ordering::Relaxed);
        }
    });
    Ok(WarmStats {
        files: entries.len() as u64,
        bytes: bytes.load(Ordering::Relaxed),
    })
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
fn serve_listener(reader: &StoreReader, spec: &str, max_conns: u32) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    let reader = Arc::new(reader_clone_for_share(reader)?);
    // Live count of in-flight connections. Each accepted connection
    // increments before spawning the worker; the worker decrements
    // in its drop guard so a panic still releases the slot.
    let inflight = Arc::new(AtomicU32::new(0));
    let cap = max_conns;
    if cap > 0 {
        eprintln!("[scry serve] max_conns={cap}; over-cap accepts will be dropped");
    }

    // RAII guard: increment on construction, decrement on drop.
    // Prevents a panic in serve_connection from leaking a slot.
    struct ConnSlot(Arc<AtomicU32>);
    impl Drop for ConnSlot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Release);
        }
    }

    // Reply written when a connection is rejected for hitting the
    // cap. JSON-RPC-style shape so MCP-aware clients can branch on
    // `error.code` without ambiguity (-32004 is the canonical
    // "server busy" range in JSON-RPC custom-code space; we use it
    // here to mean "scry serve at capacity"). Non-MCP clients see
    // the human-readable `message`. Single line, newline-terminated
    // so any line-based client picks it up cleanly.
    let make_cap_reply = |cap: u32| -> Vec<u8> {
        let v = serde_json::json!({
            "jsonrpc": "2.0",
            "id": serde_json::Value::Null,
            "error": {
                "code": -32004,
                "message": format!(
                    "scry serve at capacity (max_conns={cap}); \
                     retry after current requests complete"
                ),
                "data": {"max_conns": cap, "retryable": true},
            },
        });
        format!("{v}\n").into_bytes()
    };

    match spec.split_once(':') {
        Some(("unix", path)) => {
            use std::io::Write;
            use std::os::unix::net::UnixListener;
            // Best-effort cleanup of a stale socket from a prior crashed
            // run. If the file isn't actually a socket we'll fail to bind
            // below with a clear error — safer than silently overwriting.
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)
                .with_context(|| format!("bind unix:{path}"))?;
            eprintln!("[scry serve] listening on unix:{path}");
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[scry serve] accept: {e}"); continue; }
                };
                // Reserve a slot if a cap is in effect. fetch_add returns
                // the PREVIOUS value, so >= cap means we just pushed
                // over and must back out + tell the client why.
                if cap > 0 {
                    let prev = inflight.fetch_add(1, Ordering::AcqRel);
                    if prev >= cap {
                        inflight.fetch_sub(1, Ordering::Release);
                        eprintln!("[scry serve] over cap ({cap}); rejecting conn");
                        // Best-effort write; if the client already
                        // hung up we don't care.
                        let _ = stream.write_all(&make_cap_reply(cap));
                        let _ = stream.flush();
                        drop(stream);
                        continue;
                    }
                }
                let r = Arc::clone(&reader);
                let slot = if cap > 0 { Some(ConnSlot(Arc::clone(&inflight))) } else { None };
                thread::spawn(move || {
                    let _slot = slot;
                    if let Err(e) = serve_connection(&r, &stream, &stream) {
                        eprintln!("[scry serve] connection: {e:#}");
                    }
                });
            }
            Ok(())
        }
        Some(("tcp", addr)) => {
            use std::io::Write;
            use std::net::TcpListener;
            let listener = TcpListener::bind(addr)
                .with_context(|| format!("bind tcp:{addr}"))?;
            // Print the actually-bound address — this matters when
            // the caller passes port 0 ("OS picks one"): without
            // logging the resolved port the user has no way to
            // connect. Tests rely on parsing this line to discover
            // the port.
            let bound = listener.local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| addr.to_string());
            eprintln!("[scry serve] listening on tcp:{bound}");
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(e) => { eprintln!("[scry serve] accept: {e}"); continue; }
                };
                if cap > 0 {
                    let prev = inflight.fetch_add(1, Ordering::AcqRel);
                    if prev >= cap {
                        inflight.fetch_sub(1, Ordering::Release);
                        eprintln!("[scry serve] over cap ({cap}); rejecting conn");
                        let _ = stream.write_all(&make_cap_reply(cap));
                        let _ = stream.flush();
                        drop(stream);
                        continue;
                    }
                }
                let r = Arc::clone(&reader);
                let slot = if cap > 0 { Some(ConnSlot(Arc::clone(&inflight))) } else { None };
                thread::spawn(move || {
                    let _slot = slot;
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
    let limit = args.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(50) as usize;
    let lang = args.get("lang").and_then(|v| v.as_str());
    let kind = args.get("kind").and_then(|v| v.as_str());
    let in_ = args.get("in").and_then(|v| v.as_str());
    let not_in = args.get("not_in").and_then(|v| v.as_str());
    let stream = req.get("stream").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let budget = req.get("budget").and_then(serde_json::Value::as_u64).map(|n| n as usize);

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
        "def"     => serve_def(reader, arg_str("name"), lang, kind, in_, not_in, limit),
        "prefix"  => serve_prefix(reader, arg_str("prefix"), in_, not_in, limit),
        "fuzzy"   => {
            let dist = args.get("distance").and_then(serde_json::Value::as_u64)
                .map(|n| n as u32).unwrap_or(2);
            serve_fuzzy_with_distance(reader, arg_str("substr"), in_, not_in, dist, limit)
        }
        "ref"     => {
            let lexical = args.get("lexical")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_reachable = args.get("reachable")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_clang = args.get("clang_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_scip = args.get("scip_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let (reachable, clang_precise, scip_precise) =
                resolve_precision(lexical, explicit_reachable, explicit_clang, explicit_scip);
            let scope = args.get("scope").and_then(serde_json::Value::as_str);
            let def_in = args.get("def_in").and_then(serde_json::Value::as_str);
            let strict = args.get("strict").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let format = args.get("format").and_then(serde_json::Value::as_str);
            serve_ref(reader, arg_str("name"), lang, kind, in_, not_in, limit, reachable, clang_precise, scip_precise, scope, def_in, strict, format)
        }
        "callers" => {
            let lexical = args.get("lexical")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_reachable = args.get("reachable")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_clang = args.get("clang_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_scip = args.get("scip_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let (reachable, clang_precise, scip_precise) =
                resolve_precision(lexical, explicit_reachable, explicit_clang, explicit_scip);
            let scope = args.get("scope").and_then(serde_json::Value::as_str);
            let def_in = args.get("def_in").and_then(serde_json::Value::as_str);
            let strict = args.get("strict").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let format = args.get("format").and_then(serde_json::Value::as_str);
            serve_ref(reader, arg_str("name"), lang, Some("call"), in_, not_in, limit, reachable, clang_precise, scip_precise, scope, def_in, strict, format)
        }
        "subclasses" | "implementations" => {
            let depth = args.get("depth")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize).unwrap_or(0);
            let format = args.get("format").and_then(serde_json::Value::as_str);
            serve_subclasses(reader, arg_str("name"), in_, not_in, depth, format, limit)
        }
        "impact" => {
            let depth = args.get("subclass_depth")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize).unwrap_or(2);
            let lexical = args.get("lexical")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_reachable = args.get("reachable")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_clang = args.get("clang_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_scip = args.get("scip_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let (reachable, clang_precise, scip_precise) =
                resolve_precision(lexical, explicit_reachable, explicit_clang, explicit_scip);
            let def_in = args.get("def_in").and_then(serde_json::Value::as_str);
            let strict = args.get("strict").and_then(serde_json::Value::as_bool).unwrap_or(false);
            serve_impact(reader, arg_str("name"), in_, not_in, depth, reachable,
                         clang_precise, scip_precise, def_in, strict, limit)
        }
        "callgraph" => {
            let depth = args.get("depth")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize).unwrap_or(3);
            let max_nodes = args.get("max_nodes")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize).unwrap_or(200);
            let lexical = args.get("lexical")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_reachable = args.get("reachable")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_clang = args.get("clang_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let explicit_scip = args.get("scip_precise")
                .and_then(serde_json::Value::as_bool).unwrap_or(false);
            let (reachable, clang_precise, scip_precise) =
                resolve_precision(lexical, explicit_reachable, explicit_clang, explicit_scip);
            let def_in = args.get("def_in").and_then(serde_json::Value::as_str);
            let strict = args.get("strict").and_then(serde_json::Value::as_bool).unwrap_or(false);
            serve_callgraph(reader, arg_str("name"), in_, not_in, depth, max_nodes,
                            reachable, clang_precise, scip_precise, def_in, strict)
        }
        "uses" => {
            let strict = args.get("strict").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let format = args.get("format").and_then(serde_json::Value::as_str);
            serve_uses(reader, arg_str("name"), in_, not_in, kind, strict, format, limit)
        }
        "grep"    => {
            let ci = args.get("case_insensitive")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            serve_grep(reader, arg_str("pattern"), lang, in_, not_in, limit, ci)
        }
        "outline" => serve_outline(reader, arg_str("path"), limit),
        "tldr"    => serve_tldr(reader, arg_str("path")),
        "coverage" => serve_coverage(reader, arg_str("path"),
            args.get("by_kind").and_then(serde_json::Value::as_bool).unwrap_or(false)),
        "stats"   => serve_stats(reader),
        "ask"     => serve_ask(reader, arg_str("query"), in_, not_in, limit),
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

/// Daemon-path file-id wrapper around `path_matches`. Combines
/// `--in` (must contain) and `--not-in` (must NOT contain) substring
/// filters in one lookup. Empty / None on either side skips that
/// filter. `display_path` returns the full absolute path
/// (root.path + relpath), so substrings like `frameworks/base/` —
/// repo-root-relative — match via `contains`, not `starts_with`.
fn file_path_matches(r: &StoreReader, file_id: u32, in_: Option<&str>, not_in: Option<&str>) -> bool {
    match r.display_path_cached(file_id) {
        Some(p) => path_matches(p, in_, not_in),
        None => in_.is_none() && not_in.is_none(),
    }
}

fn serve_def(
    r: &StoreReader,
    name: &str,
    lang: Option<&str>,
    kind: Option<&str>,
    in_: Option<&str>,
    not_in: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let mut filtered: Vec<SymbolRecord> = r.lookup_exact(name).into_iter()
        .filter(|s| {
            if let Some(l) = lang {
                if !s.lang.as_str().eq_ignore_ascii_case(l) { return false; }
            }
            if let Some(k) = kind {
                if !s.kind.short().eq_ignore_ascii_case(k) { return false; }
            }
            file_path_matches(r, s.file_id, in_, not_in)
        })
        .collect();
    rank_symbols(&mut filtered, r);
    let out: Vec<_> = filtered.iter().take(limit).map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(out)
}

fn serve_prefix(
    r: &StoreReader,
    prefix: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    // Over-fetch then rank+filter — the limit should land on the BEST
    // matches, not just the first ones the FST happens to return.
    let cap = limit.saturating_mul(8).max(limit);
    let mut filtered: Vec<SymbolRecord> = r.lookup_prefix(prefix, cap).into_iter()
        .filter(|s| file_path_matches(r, s.file_id, in_, not_in))
        .collect();
    rank_symbols(&mut filtered, r);
    let v: Vec<_> = filtered.iter().take(limit).map(|s| symbol_to_json(r, s)).collect();
    serde_json::Value::Array(v)
}

/// JSON-RPC fuzzy: typo-tolerant + edit-distance ranked. Each emitted
/// hit carries a `distance` field so callers know how close the match
/// is to their query. The request can pass `args.distance: N` to
/// override the default Levenshtein bound (2).
fn serve_fuzzy_with_distance(
    r: &StoreReader,
    substr: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    distance: u32,
    limit: usize,
) -> serde_json::Value {
    let cap = limit.saturating_mul(8).max(limit);
    // Over-fetch from the ranked path; filter by --in / --not-in
    // *after* ranking so a tight subdir filter doesn't kick out
    // closer matches.
    let scored: Vec<(SymbolRecord, u32)> = r.lookup_fuzzy_ranked(substr, distance, cap);
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(limit);
    for (s, d) in scored {
        if !file_path_matches(r, s.file_id, in_, not_in) { continue; }
        let mut j = symbol_to_json(r, &s);
        j.as_object_mut().unwrap()
            .insert("distance".to_string(), serde_json::json!(d));
        out.push(j);
        if out.len() >= limit { break; }
    }
    serde_json::Value::Array(out)
}

#[allow(clippy::too_many_arguments)]
fn serve_ref(
    r: &StoreReader,
    name: &str,
    lang: Option<&str>,
    kind: Option<&str>,
    in_: Option<&str>,
    not_in: Option<&str>,
    limit: usize,
    reachable: bool,
    clang_precise: bool,
    scip_precise: bool,
    scope: Option<&str>,
    def_in: Option<&str>,
    strict: bool,
    format: Option<&str>,
) -> serde_json::Value {
    // --def-in PATH: precompute the target def id set. Empty target
    // set ⇒ no filtering (matches cmd_ref's "over-include rather than
    // silently drop" policy; daemon stays quiet on diagnostics).
    let def_target_ids: Option<std::collections::HashSet<u64>> = def_in.map(|p| {
        r.lookup_exact(name).iter()
            .filter(|s| r.display_path_cached(s.file_id)
                .is_some_and(|dp| dp.contains(p)))
            .map(|s| s.id)
            .collect()
    });
    // Precompute the callee module set ONCE if --reachable + sidecar.
    // Same shape as cmd_ref's filter. No graph or no defs → no filter
    // (callers pass through unchanged; the daemon doesn't emit a
    // diagnostic line per request to keep the wire-stream clean).
    let callee_modules: Option<std::collections::HashSet<u32>> = if reachable {
        r.module_graph().map(|mg| {
            r.lookup_exact(name)
                .iter()
                .filter_map(|s| mg.module_of_file(s.file_id))
                .collect()
        })
    } else {
        None
    };
    // Cached lazy accessors. The StoreReader OnceLock caches keep
    // both sidecars in RAM after the first daemon query, so per-
    // request decode cost is paid once at startup.
    const PRECISE_WINDOW: u32 = 64;
    let cusr: Option<&scry_store::clang_usrs::ClangUsrIndex> = if clang_precise {
        r.clang_usrs()
    } else {
        None
    };
    let def_usrs: Option<std::collections::HashSet<String>> = cusr.map(|c| {
        r.lookup_exact(name).iter().filter_map(|s| {
            let p = r.display_path_cached(s.file_id)?;
            c.usr_for_window(p, s.byte_start, PRECISE_WINDOW).map(str::to_string)
        }).collect()
    });
    let sidx: Option<&scry_store::scip_index::ScipIndex> = if scip_precise {
        r.scip_index()
    } else {
        None
    };
    let def_scip: Option<std::collections::HashSet<String>> = sidx.map(|c| {
        r.lookup_exact(name).iter().filter_map(|s| {
            let p = r.display_path_cached(s.file_id)?;
            c.symbol_for_window(p, s.byte_start, PRECISE_WINDOW).map(str::to_string)
        }).collect()
    });
    let by_def = format == Some("by-def");
    let paths_only = format == Some("paths");
    // by-def + paths both need to see ALL surviving refs to build their
    // shape correctly (histogram / dedup respectively); the default JSONL
    // path caps at `limit` for cost.
    let mut out = Vec::new();
    let mut by_def_keep: Vec<RefRecord> = Vec::new();
    let mut paths_keep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rr in r.lookup_refs_exact(name).into_iter() {
        if !by_def && !paths_only && out.len() >= limit { break; }
        if let Some(l) = lang {
            if !rr.lang.as_str().eq_ignore_ascii_case(l) { continue; }
        }
        if let Some(k) = kind {
            if !rr.kind.short().eq_ignore_ascii_case(k) { continue; }
        }
        if !file_path_matches(r, rr.file_id, in_, not_in) { continue; }
        if let Some(sc) = scope {
            if !rr.scope_path.iter().any(|seg| seg == sc) { continue; }
        }
        // --def-in filter: keep refs whose resolved_to lands in the
        // target def set. Permissive mode keeps unresolved (None) too;
        // --strict (v0.1.34) drops them.
        // Empty target set ⇒ no narrowing (target def list was empty).
        if let Some(tids) = def_target_ids.as_ref() {
            if !tids.is_empty() {
                match rr.resolved_to {
                    Some(id) if !tids.contains(&id) => continue,
                    None if strict => continue,
                    _ => {}
                }
            }
        }
        // --strict without --def-in: drop unresolved refs entirely
        // (only keep refs the resolver could attribute to some def).
        if strict && def_target_ids.is_none() && rr.resolved_to.is_none() {
            continue;
        }
        // Reachability filter: keep only refs whose owning module can
        // reach at least one module that defines `name`.
        if let (Some(mg), Some(cms)) = (r.module_graph(), callee_modules.as_ref()) {
            if !cms.is_empty() {
                if let Some(caller_mod) = mg.module_of_file(rr.file_id) {
                    if !cms.iter().any(|cm| mg.is_reachable(caller_mod, *cm)) {
                        continue;
                    }
                }
                // Unattributed caller: pass through (same as CLI).
            }
        }
        // Clang USR identity filter (Path B). Sites without a clang
        // record pass through (non-C/C++ or uncovered TU).
        if let (Some(c), Some(usrs)) = (cusr, def_usrs.as_ref()) {
            if !usrs.is_empty() {
                if let Some(p) = r.display_path_cached(rr.file_id) {
                    if let Some(u) = c.usr_for_window(p, rr.byte_start, PRECISE_WINDOW) {
                        if !usrs.contains(u) { continue; }
                    }
                }
            }
        }
        // SCIP symbol identity filter (Path C).
        if let (Some(c), Some(syms)) = (sidx, def_scip.as_ref()) {
            if !syms.is_empty() {
                if let Some(p) = r.display_path_cached(rr.file_id) {
                    if let Some(s) = c.symbol_for_window(p, rr.byte_start, PRECISE_WINDOW) {
                        if !syms.contains(s) { continue; }
                    }
                }
            }
        }
        if by_def {
            by_def_keep.push(rr);
        } else if paths_only {
            if let Some(p) = r.display_path_cached(rr.file_id) {
                paths_keep.insert(p.to_string());
                if paths_keep.len() >= limit { break; }
            }
        } else {
            out.push(ref_to_json(r, &rr));
        }
    }
    if by_def {
        return serve_ref_by_def_histogram(r, name, &by_def_keep, limit);
    }
    if paths_only {
        let arr: Vec<serde_json::Value> = paths_keep.into_iter()
            .map(serde_json::Value::String).collect();
        return serde_json::Value::Array(arr);
    }
    serde_json::Value::Array(out)
}

/// JSON-RPC histogram for `format: "by-def"` — same data layout as
/// the CLI's `print_refs_by_def` (v0.1.35) with the v0.1.37 JSON
/// shape. Sorted descending by count, capped at `limit` resolved
/// groups + a final unresolved bucket if any.
fn serve_ref_by_def_histogram(
    reader: &StoreReader,
    name: &str,
    refs: &[RefRecord],
    limit: usize,
) -> serde_json::Value {
    use std::collections::HashMap;
    let mut by_id: HashMap<Option<u64>, usize> = HashMap::new();
    for r in refs {
        *by_id.entry(r.resolved_to).or_insert(0) += 1;
    }
    let unresolved = by_id.remove(&None).unwrap_or(0);
    let mut groups: Vec<(u64, usize)> = by_id.into_iter()
        .map(|(k, v)| (k.unwrap(), v))
        .collect();
    groups.sort_unstable_by_key(|g| std::cmp::Reverse(g.1));
    let candidates = reader.lookup_exact(name);
    let by_def_id: HashMap<u64, &SymbolRecord> = candidates.iter()
        .map(|s| (s.id, s)).collect();
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(
        groups.len().min(limit) + if unresolved > 0 { 1 } else { 0 });
    for (def_id, count) in groups.iter().take(limit) {
        let def_val = by_def_id.get(def_id).map(|s| {
            let path = reader.display_path_cached(s.file_id).unwrap_or("");
            serde_json::json!({
                "path": path,
                "line": s.line,
                "col": s.col,
                "scope": s.scope_path,
                "kind": s.kind.short(),
                "id": format!("{:x}", def_id),
            })
        }).unwrap_or_else(|| serde_json::json!({
            "id": format!("{:x}", def_id),
        }));
        out.push(serde_json::json!({"count": count, "def": def_val}));
    }
    if unresolved > 0 {
        out.push(serde_json::json!({"count": unresolved, "def": null}));
    }
    serde_json::Value::Array(out)
}

/// `uses` JSON-RPC handler — outgoing edges. Same algorithm as
/// [`cmd_uses`]: locate def(s) of NAME, compute body byte range
/// via next-function heuristic, intersect with refs in that file.
fn serve_uses(
    r: &StoreReader,
    name: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    kind: Option<&str>,
    strict: bool,
    format: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let defs: Vec<SymbolRecord> = r.lookup_exact(name).into_iter()
        .filter(|s| file_path_matches(r, s.file_id, in_, not_in))
        .collect();
    if defs.is_empty() {
        return serde_json::json!([]);
    }
    let paths_only = format == Some("paths");
    let count_only = format == Some("count");
    // paths/count both need every surviving edge to compute the right
    // shape; default JSONL still caps at `limit` for cost.
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut paths_keep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut count_total: usize = 0;
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    'outer: for def in &defs {
        let body_end = next_function_byte_start(r, def.file_id, def.byte_start)
            .unwrap_or(u32::MAX);
        let refs_in_file = match r.refs_for_file(def.file_id) {
            Some(v) => v,
            // Daemon path stays quiet; no stderr spam.
            None => continue,
        };
        for ref_idx in refs_in_file {
            if !paths_only && !count_only && out.len() >= limit { break 'outer; }
            if paths_only && paths_keep.len() >= limit { break 'outer; }
            let Some(rr) = r.get_ref(ref_idx) else { continue };
            if rr.byte_start < def.byte_start || rr.byte_start >= body_end { continue; }
            if let Some(k) = kind {
                if !rr.kind.short().eq_ignore_ascii_case(k) { continue; }
            }
            // v0.1.49 — --strict drops unresolved outgoing edges.
            if strict && rr.resolved_to.is_none() { continue; }
            let key = ((rr.file_id as u64) << 32) | (rr.byte_start as u64);
            if seen.insert(key) {
                if paths_only {
                    if let Some(p) = r.display_path_cached(rr.file_id) {
                        paths_keep.insert(p.to_string());
                    }
                } else if count_only {
                    count_total += 1;
                } else {
                    out.push(ref_to_json(r, &rr));
                }
            }
        }
    }
    if paths_only {
        let arr: Vec<serde_json::Value> = paths_keep.into_iter()
            .map(serde_json::Value::String).collect();
        return serde_json::Value::Array(arr);
    }
    if count_only {
        return serde_json::json!({"count": count_total});
    }
    serde_json::Value::Array(out)
}

/// `callgraph` JSON-RPC handler — returns the recursive callers
/// tree. Same algorithm as [`cmd_callgraph`]; result shape mirrors
/// the CLI's `--json` payload.
#[allow(clippy::too_many_arguments)]
fn serve_callgraph(
    r: &StoreReader,
    name: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    depth: usize,
    max_nodes: usize,
    reachable: bool,
    clang_precise: bool,
    scip_precise: bool,
    def_in: Option<&str>,
    strict: bool,
) -> serde_json::Value {
    let prefix = in_.unwrap_or("");
    let neg_prefix = not_in.unwrap_or("");
    let callee_modules: Option<std::collections::HashSet<u32>> = if reachable {
        r.module_graph().map(|mg| {
            r.lookup_exact(name).iter()
                .filter_map(|s| mg.module_of_file(s.file_id)).collect()
        })
    } else { None };

    // Root-level --def-in target def-ids (v0.1.44). Daemon stays
    // quiet on diagnostics; empty target set ⇒ no narrowing.
    let root_def_target_ids: Option<std::collections::HashSet<u64>> =
        def_in.map(|p| {
            r.lookup_exact(name).iter()
                .filter(|s| r.display_path_cached(s.file_id)
                    .is_some_and(|dp| dp.contains(p)))
                .map(|s| s.id)
                .collect()
        });

    // Root-level precision filter — same shape as cmd_callgraph.
    // Soft-fail on sidecar errors: a daemon shouldn't crash a single
    // RPC because of a broken precision sidecar.
    let root_precise_sites: Option<std::collections::HashSet<(u32, u32)>> =
        if clang_precise || scip_precise {
            let raw_root = r.lookup_refs_exact(name);
            match apply_precision_filter(r, name, raw_root, clang_precise, scip_precise) {
                Ok(kept) => Some(
                    kept.into_iter().map(|rr| (rr.file_id, rr.byte_start)).collect()
                ),
                Err(_) => None,
            }
        } else {
            None
        };

    #[derive(Debug, Default, serde::Serialize)]
    struct Node {
        call_sites: usize,
        first_site: Option<(String, u32, u32)>,
        callers: std::collections::BTreeMap<String, Node>,
    }

    #[allow(clippy::too_many_arguments)]
    fn expand(
        r: &StoreReader,
        callee: &str,
        depth_left: usize,
        in_prefix: &str,
        not_in_prefix: &str,
        callee_modules: Option<&std::collections::HashSet<u32>>,
        root_def_target_ids: Option<&std::collections::HashSet<u64>>,
        root_precise_sites: Option<&std::collections::HashSet<(u32, u32)>>,
        strict: bool,
        visited: &mut std::collections::HashSet<String>,
        budget: &mut usize,
    ) -> std::collections::BTreeMap<String, Node> {
        if depth_left == 0 || *budget == 0 { return Default::default(); }
        if !visited.insert(callee.to_string()) { return Default::default(); }
        let mut out: std::collections::BTreeMap<String, Node> = std::collections::BTreeMap::new();
        for rr in r.lookup_refs_exact(callee).into_iter() {
            if rr.kind != scry_store::RefKind::Call { continue; }
            if !in_prefix.is_empty() || !not_in_prefix.is_empty() {
                let Some(p) = r.display_path_cached(rr.file_id) else { continue };
                if !in_prefix.is_empty() && !p.contains(in_prefix) { continue; }
                if !not_in_prefix.is_empty() && p.contains(not_in_prefix) { continue; }
            }
            if let (Some(mg), Some(cms)) = (r.module_graph(), callee_modules) {
                if !cms.is_empty() {
                    if let Some(caller_mod) = mg.module_of_file(rr.file_id) {
                        if !cms.iter().any(|cm| mg.is_reachable(caller_mod, *cm)) {
                            continue;
                        }
                    }
                }
            }
            // Root-level --def-in / --strict filter (v0.1.44).
            if let Some(tids) = root_def_target_ids {
                if !tids.is_empty() {
                    match rr.resolved_to {
                        Some(id) if !tids.contains(&id) => continue,
                        None if strict => continue,
                        _ => {}
                    }
                }
            }
            if root_def_target_ids.is_none() && strict && rr.resolved_to.is_none() {
                continue;
            }
            // Root-level precision (clang USR / SCIP). Only Some at
            // the topmost call; deeper recursion stays lexical.
            if let Some(sites) = root_precise_sites {
                if !sites.contains(&(rr.file_id, rr.byte_start)) {
                    continue;
                }
            }
            let caller_name = r.enclosing_function(rr.file_id, rr.byte_start)
                .map(|s| s.name)
                .or_else(|| rr.scope_path.last().cloned());
            let Some(caller_name) = caller_name else { continue };
            let entry = out.entry(caller_name.clone()).or_default();
            entry.call_sites += 1;
            if entry.first_site.is_none() {
                let path = r.file_display_path(rr.file_id).unwrap_or_default();
                entry.first_site = Some((path, rr.line, rr.col));
            }
            *budget = budget.saturating_sub(1);
            if *budget == 0 { break; }
        }
        // Recurse without root filter — narrowing only applies at the
        // top level (same limitation as cmd_callgraph).
        for (caller_name, node) in &mut out {
            node.callers = expand(r, caller_name, depth_left - 1, in_prefix, not_in_prefix,
                callee_modules, None, None, strict, visited, budget);
        }
        visited.remove(callee);
        out
    }

    let mut budget = max_nodes;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let tree = expand(r, name, depth, prefix, neg_prefix, callee_modules.as_ref(),
                      root_def_target_ids.as_ref(), root_precise_sites.as_ref(),
                      strict, &mut visited, &mut budget);
    serde_json::json!({
        "callee": name,
        "depth": depth,
        "max_nodes": max_nodes,
        "callers": tree,
    })
}

/// `impact` JSON-RPC handler — returns the composed callers +
/// subclasses + files_touched summary. Same algorithm as
/// [`cmd_impact`]; result shape mirrors the CLI's `--json` output
/// so an LLM can consume either uniformly.
#[allow(clippy::too_many_arguments)]
fn serve_impact(
    r: &StoreReader,
    name: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    subclass_depth: usize,
    reachable: bool,
    clang_precise: bool,
    scip_precise: bool,
    def_in: Option<&str>,
    strict: bool,
    limit: usize,
) -> serde_json::Value {
    // Apply precision (clang USR + SCIP) on the callers leg only.
    // Subclasses use type-hierarchy lookup; no symbol-identity index
    // applies there. Soft-fail on sidecar errors so a broken sidecar
    // doesn't take down the daemon RPC.
    let raw_callers: Vec<RefRecord> = r.lookup_refs_exact(name).into_iter()
        .filter(|rr| rr.kind == scry_store::RefKind::Call)
        .collect();
    let callers_precise = if clang_precise || scip_precise {
        apply_precision_filter(r, name, raw_callers, clang_precise, scip_precise)
            .unwrap_or_else(|_| r.lookup_refs_exact(name).into_iter()
                .filter(|rr| rr.kind == scry_store::RefKind::Call).collect())
    } else {
        raw_callers
    };
    let mut callers: Vec<RefRecord> = callers_precise.into_iter()
        .filter(|rr| file_path_matches(r, rr.file_id, in_, not_in))
        .collect();
    // v0.1.46 — same root-level narrowing as cmd_impact.
    if let Some(def_path) = def_in {
        let target_ids: std::collections::HashSet<u64> = r.lookup_exact(name)
            .iter()
            .filter(|s| r.display_path_cached(s.file_id)
                .is_some_and(|dp| dp.contains(def_path)))
            .map(|s| s.id)
            .collect();
        if !target_ids.is_empty() {
            callers.retain(|rr| match rr.resolved_to {
                Some(id) => target_ids.contains(&id),
                None => !strict,
            });
        }
    } else if strict {
        callers.retain(|rr| rr.resolved_to.is_some());
    }
    if reachable {
        if let Some(mg) = r.module_graph() {
            let defs = r.lookup_exact(name);
            let callee_modules: std::collections::HashSet<u32> = defs.iter()
                .filter_map(|s| mg.module_of_file(s.file_id)).collect();
            if !callee_modules.is_empty() {
                callers.retain(|rr| match mg.module_of_file(rr.file_id) {
                    Some(cm) => callee_modules.iter().any(|m| mg.is_reachable(cm, *m)),
                    None => true,
                });
            }
        }
    }
    let subclasses: Vec<SymbolRecord> = r.subclasses_transitive(name, subclass_depth)
        .into_iter()
        .filter(|s| file_path_matches(r, s.file_id, in_, not_in))
        .collect();
    let mut files_touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rr in &callers {
        if let Some(p) = r.display_path_cached(rr.file_id) {
            files_touched.insert(p.to_string());
        }
    }
    for s in &subclasses {
        if let Some(p) = r.display_path_cached(s.file_id) {
            files_touched.insert(p.to_string());
        }
    }
    serde_json::json!({
        "name": name,
        "callers": callers.iter().take(limit).map(|rr| ref_to_json(r, rr))
            .collect::<Vec<_>>(),
        "subclasses": subclasses.iter().take(limit).map(|s| symbol_to_json(r, s))
            .collect::<Vec<_>>(),
        "files_touched": files_touched.iter().take(limit).collect::<Vec<_>>(),
        "totals": {
            "callers": callers.len(),
            "subclasses": subclasses.len(),
            "files_touched": files_touched.len(),
        },
    })
}

/// `subclasses` / `implementations` JSON-RPC handler. Direct (depth=0)
/// or transitive (depth>0) subtypes; --in prefix narrows to a subtree.
fn serve_subclasses(
    r: &StoreReader,
    name: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    depth: usize,
    format: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    let results = if depth == 0 {
        r.subclasses(name)
    } else {
        r.subclasses_transitive(name, depth)
    };
    let paths_only = format == Some("paths");
    let count_only = format == Some("count");
    let mut out = Vec::new();
    let mut paths_keep: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut count_total: usize = 0;
    for s in results.into_iter() {
        if !paths_only && !count_only && out.len() >= limit { break; }
        if paths_only && paths_keep.len() >= limit { break; }
        if !file_path_matches(r, s.file_id, in_, not_in) { continue; }
        if paths_only {
            if let Some(p) = r.display_path_cached(s.file_id) {
                paths_keep.insert(p.to_string());
            }
        } else if count_only {
            count_total += 1;
        } else {
            out.push(symbol_to_json(r, &s));
        }
    }
    if paths_only {
        let arr: Vec<serde_json::Value> = paths_keep.into_iter()
            .map(serde_json::Value::String).collect();
        return serde_json::Value::Array(arr);
    }
    if count_only {
        return serde_json::json!({"count": count_total});
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
    not_in: Option<&str>,
    limit: usize,
    case_insensitive: bool,
) -> serde_json::Value {
    if pattern.is_empty() {
        return serde_json::json!({"error": "empty pattern"});
    }
    let needle = pattern.as_bytes();
    let candidates: Option<std::collections::HashSet<u32>> = if case_insensitive {
        r.grep_candidates_ci(needle)
    } else {
        r.grep_candidates(needle)
    };
    // CI matches are compiled once as `regex::bytes` with case_insensitive(true);
    // the literal pattern is escaped first so meta-characters stay literal.
    let ci_re: Option<regex::bytes::Regex> = if case_insensitive {
        match regex::bytes::RegexBuilder::new(&regex::escape(pattern))
            .case_insensitive(true)
            .build()
        {
            Ok(re) => Some(re),
            // Escaped-literal compiles deterministically, but guard anyway.
            Err(e) => return serde_json::json!({"error": format!("regex build: {e}")}),
        }
    } else {
        None
    };
    let mut out: Vec<serde_json::Value> = Vec::new();
    // Soft cap on files scanned even when trigram returns many — keeps a
    // bad query (e.g. "the") from blocking the serve loop for seconds.
    const MAX_FILES_SCANNED: usize = 5000;
    let mut scanned = 0usize;
    for fe in r.iter_files() {
        if out.len() >= limit { break; }
        if scanned >= MAX_FILES_SCANNED { break; }
        if let Some(ref tg) = candidates {
            if !tg.contains(&fe.id) { continue; }
        }
        if let Some(l) = lang {
            if !fe.kind.as_str().eq_ignore_ascii_case(l) { continue; }
        }
        if in_.is_some() || not_in.is_some() {
            // Substring match — same semantics as file_path_matches and
            // CLI cmd_grep; absolute paths never start with a root-
            // relative subdir.
            let p = fe.display_path();
            if !path_matches(&p, in_, not_in) { continue; }
        }
        scanned += 1;
        let path = fe.display_path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut per_file = 0usize;
        if let Some(re) = &ci_re {
            for m in re.find_iter(&bytes) {
                let (line, col, snippet) = locate_match(&bytes, m.start(), m.end());
                out.push(serde_json::json!({
                    "path": path,
                    "line": line,
                    "col": col,
                    "snippet": snippet,
                    "lang": fe.kind.as_str(),
                }));
                per_file += 1;
                if out.len() >= limit || per_file >= 16 { break; }
            }
        } else {
            // memmem search through the file; cap matches per-file to avoid
            // pathological hits (e.g. every line) eating the limit.
            let mut start_at = 0usize;
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
    let fe = match r.file_view(file_id) {
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
        "path": fe.display_path(),
        "lang": fe.kind.as_str(),
        "symbols_total": found.len(),
        "symbols_shown": take,
        "symbols": arr,
    })
}

/// JSON-RPC `tldr`: one-call file summary. Mirrors `cmd_tldr`'s
/// shape — `{path, lang, symbols_total, by_kind:[{kind,count}],
/// top:[{name,kind,line,col,scope}], first_line}`. Same per-file
/// symbol set as `outline` but compressed for the "what does this
/// file do?" question.
fn serve_tldr(r: &StoreReader, path: &str) -> serde_json::Value {
    if path.is_empty() {
        return serde_json::json!({"error": "missing 'path' arg"});
    }
    let file_id = match resolve_file_id(r, path) {
        Some(id) => id,
        None => return serde_json::json!({"error": format!("no indexed file matches '{}'", path)}),
    };
    let fe = match r.file_view(file_id) {
        Some(f) => f,
        None => return serde_json::json!({"error": "file_id out of range"}),
    };
    let mut syms: Vec<SymbolRecord> = match r.symbols_for_file(file_id) {
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
    let mut by_kind: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for s in &syms {
        *by_kind.entry(s.kind.short()).or_default() += 1;
    }
    syms.sort_by_key(|s| std::cmp::Reverse(s.rank_score()));
    let top: Vec<_> = syms.iter().take(3)
        .map(|s| serde_json::json!({
            "name": s.name, "kind": s.kind.short(),
            "line": s.line, "col": s.col, "scope": s.scope_path,
        }))
        .collect();
    let display = fe.display_path();
    let first_line = std::fs::read_to_string(&display).ok()
        .and_then(|src| src.lines().find(|l| !l.trim().is_empty())
            .map(|l| if l.len() > 200 { format!("{}…", &l[..200]) }
                    else { l.to_string() }));
    let kinds: Vec<_> = by_kind.iter()
        .map(|(k, n)| serde_json::json!({"kind": k, "count": n}))
        .collect();
    serde_json::json!({
        "path": display,
        "lang": fe.kind.as_str(),
        "symbols_total": syms.len(),
        "by_kind": kinds,
        "top": top,
        "first_line": first_line,
    })
}

/// JSON-RPC coverage: subtree stats. Same shape as the CLI's
/// `scry coverage --json`. by_kind=true includes per-symbol-kind
/// counts inside each language; default false to keep responses
/// compact (typical agent use case is "what's in this dir" not
/// "how many ctors").
fn serve_coverage(r: &StoreReader, path: &str, by_kind: bool) -> serde_json::Value {
    use std::collections::HashMap;
    let matching: Vec<(u32, FileKind, u64)> = r.iter_files()
        .filter(|fe| path.is_empty() || fe.display_path().contains(path))
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

/// JSON-RPC semantic-retrieval handler. Returns an empty array (not
/// an error) when the index lacks the embedding sidecar — agents can
/// detect by length zero + a `stats` query that reports the dim is 0.
fn serve_ask(
    r: &StoreReader,
    query: &str,
    in_: Option<&str>,
    not_in: Option<&str>,
    limit: usize,
) -> serde_json::Value {
    if r.embeddings_mmap.is_none() || r.chunks.is_none() {
        return serde_json::json!({"error": "no embedding sidecar — run `scry build-embeddings`"});
    }
    let dim = r.embedding_dim as usize;
    let q_vec = scry_store::embed::embed_text(query, dim);
    let any_filter = in_.is_some() || not_in.is_some();
    let cap = limit.saturating_mul(if any_filter { 8 } else { 1 }).max(limit);
    let ranked = r.semantic_rank(&q_vec, cap);
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(limit);
    for (chunk_idx, sim) in ranked {
        let entry = match r.chunks.as_ref().and_then(|c| c.get(chunk_idx as usize)) {
            Some(e) => e, None => continue,
        };
        let fe = match r.file_view(entry.file_id) { Some(f) => f, None => continue };
        let path = fe.display_path();
        if !path_matches(&path, in_, not_in) { continue; }
        let snippet = chunk_snippet(&path, entry.start_line, entry.end_line);
        out.push(serde_json::json!({
            "path": path,
            "lang": fe.kind.as_str(),
            "start_line": entry.start_line,
            "end_line": entry.end_line,
            "score": sim,
            "snippet": snippet,
        }));
        if out.len() >= limit { break; }
    }
    serde_json::Value::Array(out)
}

fn serve_stats(r: &StoreReader) -> serde_json::Value {
    // Mirror cmd_stats's v0.1.41 additions so MCP clients and CLI
    // see the same resolution-coverage fields.
    let refs_resolved = r.count_resolved_refs();
    let refs_resolved_pct = refs_resolved.map(|n| {
        if r.manifest.stats.refs == 0 { 0.0 }
        else { (n as f64) * 100.0 / (r.manifest.stats.refs as f64) }
    });
    serde_json::json!({
        "scry_version": r.manifest.scry_version,
        "indexed_at": r.manifest.indexed_at,
        "roots": r.roots.iter().map(|x| serde_json::json!({
            "path": x.path, "profile": x.profile,
        })).collect::<Vec<_>>(),
        "files_total": r.manifest.stats.files_total,
        "symbols": r.manifest.stats.symbols,
        "refs": r.manifest.stats.refs,
        "refs_resolved": refs_resolved,
        "refs_resolved_pct": refs_resolved_pct,
        "bytes_total": r.manifest.stats.bytes_total,
        "elapsed_ms": r.manifest.stats.elapsed_ms,
    })
}

fn symbol_to_json(r: &StoreReader, s: &SymbolRecord) -> serde_json::Value {
    let path = r.display_path_cached(s.file_id).unwrap_or("");
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
    let path = r.display_path_cached(rr.file_id).unwrap_or("");
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
        TypeScript => "ts",
        Html => "html",
        Css => "css",
        Scss => "scss",
        Markdown => "md",
        Toml => "toml",
        Yaml => "yaml",
        Proto => "proto",
        Aidl => "aidl",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.1.39 — disambiguates same-basename files (the many
    /// MainActivity.java in AOSP) by showing the parent dir too,
    /// while staying short for users who want a glance.
    #[test]
    fn short_path_suffix_renders_last_two_components() {
        assert_eq!(
            short_path_suffix("/home/zim/dev/aosp/samples/MainActivity.java"),
            "samples/MainActivity.java",
        );
        assert_eq!(
            short_path_suffix("/home/zim/dev/aosp/test/MainActivity.java"),
            "test/MainActivity.java",
        );
        // Already short: a single-component path is returned verbatim.
        assert_eq!(short_path_suffix("MainActivity.java"), "MainActivity.java");
        // Two-component path: returned verbatim.
        assert_eq!(short_path_suffix("src/MainActivity.java"), "src/MainActivity.java");
        // Trailing slash (directory) — degenerate; just return as-is
        // since the basename is empty.
        assert_eq!(short_path_suffix("a/b/c/"), "c/");
        // Leading slash absolute path with only one segment under root.
        assert_eq!(short_path_suffix("/foo.txt"), "/foo.txt");
    }

    /// `format_eta` contract used by the `[progress]` line. Crosses the
    /// 60s and 1h boundaries cleanly; bad inputs (NaN, negative) render
    /// as `—` so the progress line stays readable instead of printing
    /// `NaN` or a negative duration.
    #[test]
    fn format_eta_renders_short_medium_long() {
        assert_eq!(format_eta(0.0), "0s");
        assert_eq!(format_eta(45.0), "45s");
        assert_eq!(format_eta(59.4), "59s");
        assert_eq!(format_eta(60.0), "1m00s");
        assert_eq!(format_eta(750.0), "12m30s");
        assert_eq!(format_eta(3600.0), "1h00m");
        assert_eq!(format_eta(7530.0), "2h05m");
        assert_eq!(format_eta(f64::NAN), "—");
        assert_eq!(format_eta(-5.0), "—");
    }

    /// Rotation: writing a 200-byte file with cap=100 → rename to
    /// {path}.1 on the next rotate_log_if_oversized call; the
    /// active path becomes empty, the backup contains the old
    /// content.
    #[test]
    fn rotate_log_renames_on_overflow() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-rotlog-{}.log", std::process::id())
        );
        let backup = tmp.with_extension("log.1");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&backup);
        std::fs::write(&tmp, vec![b'x'; 200]).unwrap();
        rotate_log_if_oversized(&tmp, 100);
        assert!(!tmp.exists(),
                "active log must be rotated out when oversized");
        assert!(backup.exists(),
                "backup must exist at {}", backup.display());
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&backup);
    }

    /// Cap = 0 → no rotation regardless of size.
    #[test]
    fn rotate_log_cap_zero_disables() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-norot-{}.log", std::process::id())
        );
        std::fs::write(&tmp, vec![b'x'; 10_000]).unwrap();
        rotate_log_if_oversized(&tmp, 0);
        assert!(tmp.exists(),
                "cap=0 must leave the log in place even when oversized");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Under cap → no rotation.
    #[test]
    fn rotate_log_under_cap_leaves_file() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-undercap-{}.log", std::process::id())
        );
        std::fs::write(&tmp, vec![b'x'; 50]).unwrap();
        rotate_log_if_oversized(&tmp, 100);
        assert!(tmp.exists(),
                "under-cap files must not be rotated");
        let meta = std::fs::metadata(&tmp).unwrap();
        assert_eq!(meta.len(), 50);
        let _ = std::fs::remove_file(&tmp);
    }

    /// Missing file → no panic, no rotation attempted.
    #[test]
    fn rotate_log_missing_file_is_noop() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-missing-{}.log", std::process::id())
        );
        // Pre-condition: doesn't exist.
        let _ = std::fs::remove_file(&tmp);
        rotate_log_if_oversized(&tmp, 100);
        assert!(!tmp.exists());
    }

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

    fn mk_def(id: u64, lang: FileKind, pkg: Option<&str>) -> ResolveDef {
        ResolveDef { id, file_id: 0, lang, pkg: pkg.map(String::from), scope_path: vec![] }
    }
    fn mk_def_scoped(id: u64, lang: FileKind, scope: &[&str]) -> ResolveDef {
        ResolveDef {
            id, file_id: 0, lang, pkg: None,
            scope_path: scope.iter().copied().map(String::from).collect(),
        }
    }
    fn mk_ref(name: &str, lang: FileKind, file_id: u32) -> RefRecord {
        RefRecord {
            name: name.into(), kind: RefKind::Call, file_id,
            byte_start: 0, byte_end: 0, line: 1, col: 1,
            scope_path: vec![], lang, resolved_to: None,
        }
    }
    fn mk_ref_scoped(name: &str, lang: FileKind, file_id: u32, scope: &[&str]) -> RefRecord {
        RefRecord {
            name: name.into(), kind: RefKind::Call, file_id,
            byte_start: 0, byte_end: 0, line: 1, col: 1,
            scope_path: scope.iter().copied().map(String::from).collect(),
            lang, resolved_to: None,
        }
    }
    fn empty_ns() -> HashMap<u32, Vec<String>> { HashMap::new() }

    #[test]
    fn resolve_one_single_candidate_trivial() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Foo".into(), vec![mk_def(42, FileKind::Java, None)]);
        let r = mk_ref("Foo", FileKind::Java, 0);
        let mut n = 0u64;
        let chosen = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(chosen, 42);
        assert_eq!(n, 0);
    }

    #[test]
    fn resolve_one_no_match_returns_zero() {
        let by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        let r = mk_ref("Foo", FileKind::Java, 0);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 0);
    }

    #[test]
    fn resolve_one_same_lang_preference_wins_over_cross_lang() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Foo".into(), vec![
            mk_def(100, FileKind::Cpp, None),
            mk_def(200, FileKind::Java, None),
            mk_def(300, FileKind::Python, None),
        ]);
        let r = mk_ref("Foo", FileKind::Java, 0);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 200);
    }

    #[test]
    fn resolve_one_java_same_package_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Activity".into(), vec![
            mk_def(11, FileKind::Java, Some("com.other")),
            mk_def(22, FileKind::Java, Some("android.app")),
            mk_def(33, FileKind::Java, Some("com.third")),
        ]);
        let mut pkg = HashMap::new();
        pkg.insert(5u32, "android.app".to_string());
        let r = mk_ref("Activity", FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &pkg, &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    #[test]
    fn resolve_one_java_import_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Binder".into(), vec![
            mk_def(11, FileKind::Java, Some("com.other")),
            mk_def(22, FileKind::Java, Some("android.os")),
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![("Binder".to_string(), Some("android.os".to_string()))]);
        let r = mk_ref("Binder", FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &imports, &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    #[test]
    fn resolve_one_java_wildcard_import_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Binder".into(), vec![
            mk_def(11, FileKind::Java, Some("com.other")),
            mk_def(22, FileKind::Java, Some("android.os")),
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![("*".to_string(), Some("android.os".to_string()))]);
        let r = mk_ref("Binder", FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &imports, &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    #[test]
    fn resolve_one_java_lang_fallback() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("String".into(), vec![
            mk_def(11, FileKind::Java, Some("com.other")),
            mk_def(22, FileKind::Java, Some("java.lang")),
        ]);
        let r = mk_ref("String", FileKind::Java, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    /// Ambiguous Java *method call* with no narrowing context →
    /// unresolved (returns 0). v0.1.27 dropped the misleading
    /// pool[0] fallback for Call / Ctor / FieldAccess refs: without
    /// receiver-type inference we cannot pick the right method from
    /// a sea of same-named candidates, so the resolver is honest
    /// about ambiguity. `--def-in PATH`'s permissive branch then
    /// includes these refs (over-include rather than mis-include).
    #[test]
    fn resolve_one_java_call_ambiguous_returns_unresolved() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("close".into(), vec![
            mk_def(11, FileKind::Java, Some("com.other")),
            mk_def(22, FileKind::Java, Some("com.third")),
        ]);
        let r = mk_ref("close", FileKind::Java, 5);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 0, "ambiguous Java method call should resolve to 0 (unresolved), not arbitrary pool[0]");
        assert_eq!(n, 0);
    }

    /// Type-use refs (and other non-call kinds) DO keep the pool[0]
    /// fallback — types referenced unqualified are far less ambiguous
    /// than methods, and the resolver was already doing the right
    /// thing for those.
    #[test]
    fn resolve_one_java_typeuse_ambiguous_keeps_pool0_fallback() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Foo".into(), vec![
            mk_def(11, FileKind::Java, Some("com.other")),
            mk_def(22, FileKind::Java, Some("com.third")),
        ]);
        let mut r = mk_ref("Foo", FileKind::Java, 5);
        r.kind = RefKind::TypeUse;
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 11);
        assert_eq!(n, 0);
    }

    /// Same-file preference: a call to `foo()` inside file F that
    /// has a uniquely-named `foo` def in F wins, even when other
    /// `foo` defs exist in the corpus. v0.1.27 narrowing rule.
    #[test]
    fn resolve_one_same_file_preference() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("close".into(), vec![
            // Same-file (file_id=7) candidate — should win.
            ResolveDef { id: 99, file_id: 7, lang: FileKind::Java,
                         pkg: Some("com.foo".into()), scope_path: vec![] },
            // Two other candidates elsewhere.
            ResolveDef { id: 11, file_id: 0, lang: FileKind::Java,
                         pkg: Some("com.other".into()), scope_path: vec![] },
            ResolveDef { id: 22, file_id: 0, lang: FileKind::Java,
                         pkg: Some("com.third".into()), scope_path: vec![] },
        ]);
        let r = mk_ref("close", FileKind::Java, 7);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 99, "same-file def should win over corpus-wide ambiguity");
        assert_eq!(n, 1, "same-file pick counts as narrowed");
    }

    // ---- Kotlin Layer 2 ----

    /// Kotlin same-package narrowing mirrors Java: a file's package
    /// matches one candidate → that one wins.
    /// Inheritance walk (v0.1.32). A call inside class Inner (which
    /// extends Outer) to a method defined only on Outer should
    /// resolve to Outer's method via the inheritance chain.
    #[test]
    fn resolve_one_inheritance_walks_parent_class() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("inherited".into(), vec![
            // Defined only on the parent class.
            ResolveDef { id: 42, file_id: 9, lang: FileKind::Java,
                         pkg: Some("com.example".into()),
                         scope_path: vec!["Outer".into()] },
            // Unrelated class with same method name.
            ResolveDef { id: 99, file_id: 8, lang: FileKind::Java,
                         pkg: Some("com.other".into()),
                         scope_path: vec!["Unrelated".into()] },
        ]);
        // Inner extends Outer; call site is inside Inner.
        let mut ancestors: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        ancestors.insert("Inner".to_string(),
            ["Outer".to_string()].into_iter().collect());
        let r = mk_ref_scoped("inherited", FileKind::Java, 5, &["Inner"]);
        let mut n = 0u64;
        let got = resolve_one(
            &r, &by_name, &HashMap::new(), &HashMap::new(),
            &empty_ns(), &ancestors, &mut n,
        );
        assert_eq!(got, 42, "should walk Inner → Outer and find inherited method");
        assert_eq!(n, 1);
    }

    /// Inheritance walk handles multi-level chains (grandparent).
    #[test]
    fn resolve_one_inheritance_walks_grandparent() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("inherited".into(), vec![
            ResolveDef { id: 7, file_id: 9, lang: FileKind::Java,
                         pkg: Some("com.example".into()),
                         scope_path: vec!["Grandparent".into()] },
            ResolveDef { id: 99, file_id: 8, lang: FileKind::Java,
                         pkg: Some("com.other".into()),
                         scope_path: vec!["Unrelated".into()] },
        ]);
        // Child → Parent → Grandparent chain — precomputed transitive set.
        let mut ancestors: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        ancestors.insert("Child".to_string(),
            ["Parent".to_string(), "Grandparent".to_string()].into_iter().collect());
        let r = mk_ref_scoped("inherited", FileKind::Java, 5, &["Child"]);
        let mut n = 0u64;
        let got = resolve_one(
            &r, &by_name, &HashMap::new(), &HashMap::new(),
            &empty_ns(), &ancestors, &mut n,
        );
        assert_eq!(got, 7);
        assert_eq!(n, 1);
    }

    /// Inheritance walk falls through when multiple ancestors define
    /// the same method (e.g. diamond / interface conflict).
    #[test]
    fn resolve_one_inheritance_diamond_stays_unresolved() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("close".into(), vec![
            ResolveDef { id: 1, file_id: 7, lang: FileKind::Java,
                         pkg: Some("com.a".into()),
                         scope_path: vec!["IA".into()] },
            ResolveDef { id: 2, file_id: 8, lang: FileKind::Java,
                         pkg: Some("com.b".into()),
                         scope_path: vec!["IB".into()] },
        ]);
        // Child implements both IA and IB — both define close.
        let mut ancestors: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        ancestors.insert("Child".to_string(),
            ["IA".to_string(), "IB".to_string()].into_iter().collect());
        let r = mk_ref_scoped("close", FileKind::Java, 5, &["Child"]);
        let mut n = 0u64;
        let got = resolve_one(
            &r, &by_name, &HashMap::new(), &HashMap::new(),
            &empty_ns(), &ancestors, &mut n,
        );
        assert_eq!(got, 0,
            "ambiguous inheritance (two ancestors define close) → unresolved");
    }

    /// Same-class preference (v0.1.31). A call to `foo()` inside
    /// class chain [Outer, Inner] should prefer a candidate defined
    /// in [Outer, Inner] (or any prefix like [Outer]) even when in a
    /// different file. Generalizes the C++ same-namespace rule to
    /// all languages, covering partial classes / generated code /
    /// implicit-this from inner classes.
    #[test]
    fn resolve_one_same_class_preference_across_files() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("helper".into(), vec![
            // Same class but different file (file_id 7 vs ref file 5).
            ResolveDef { id: 99, file_id: 7, lang: FileKind::Java,
                         pkg: Some("com.example".into()),
                         scope_path: vec!["Foo".into()] },
            // Unrelated class.
            ResolveDef { id: 11, file_id: 8, lang: FileKind::Java,
                         pkg: Some("com.other".into()),
                         scope_path: vec!["Bar".into()] },
        ]);
        let r = mk_ref_scoped("helper", FileKind::Java, 5, &["Foo"]);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 99, "should prefer same-class def even in different file");
        assert_eq!(n, 1);
    }

    /// Same-class preference must also fire for implicit-this calls
    /// from an inner class: ref scope [Outer, Inner] should find a
    /// candidate in [Outer] (parent class) when the inner class doesn't
    /// define the method itself.
    #[test]
    fn resolve_one_same_class_prefix_fires_for_inner_class() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("parentMethod".into(), vec![
            // Outer class method — should be visible to its inner class.
            ResolveDef { id: 42, file_id: 5, lang: FileKind::Java,
                         pkg: Some("com.example".into()),
                         scope_path: vec!["Outer".into()] },
            // Different class.
            ResolveDef { id: 99, file_id: 9, lang: FileKind::Java,
                         pkg: Some("com.other".into()),
                         scope_path: vec!["Unrelated".into()] },
        ]);
        // ref scope = [Outer, Inner] — call from inside the inner class.
        let r = mk_ref_scoped("parentMethod", FileKind::Java, 5, &["Outer", "Inner"]);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 42, "implicit-this from inner class should reach parent's methods");
        assert_eq!(n, 1);
    }

    /// Import-aware class narrowing (v0.1.28). Java file imports
    /// `android.os.PerfettoTrace`; an ambiguous `close()` call there
    /// should resolve to PerfettoTrace.Session.close because that's
    /// the only candidate whose owning class is imported.
    #[test]
    fn resolve_one_java_method_call_via_class_import() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("close".into(), vec![
            // PerfettoTrace.Session.close — outer class imported.
            ResolveDef { id: 99, file_id: 0, lang: FileKind::Java,
                         pkg: Some("android.os".into()),
                         scope_path: vec!["PerfettoTrace".into(), "Session".into()] },
            // Closeable.close — not imported.
            ResolveDef { id: 11, file_id: 0, lang: FileKind::Java,
                         pkg: Some("java.io".into()),
                         scope_path: vec!["Closeable".into()] },
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![
            ("PerfettoTrace".to_string(), Some("android.os".to_string())),
        ]);
        let r = mk_ref("close", FileKind::Java, 5);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &imports, &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 99, "should resolve to the imported class's method");
        assert_eq!(n, 1, "import-aware narrowing counts as narrowed");
    }

    /// Wildcard import variant: `import android.os.*` should also
    /// trigger the class-import narrowing rule.
    #[test]
    fn resolve_one_java_method_call_via_wildcard_class_import() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("close".into(), vec![
            ResolveDef { id: 99, file_id: 0, lang: FileKind::Java,
                         pkg: Some("android.os".into()),
                         scope_path: vec!["PerfettoTrace".into(), "Session".into()] },
            ResolveDef { id: 11, file_id: 0, lang: FileKind::Java,
                         pkg: Some("java.io".into()),
                         scope_path: vec!["Closeable".into()] },
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![
            ("*".to_string(), Some("android.os".to_string())),
        ]);
        let r = mk_ref("close", FileKind::Java, 5);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &imports, &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 99);
        assert_eq!(n, 1);
    }

    /// When multiple imported classes both define the method, the
    /// import-aware rule must NOT pick one — fall through to the
    /// truthful-unresolved branch for method calls.
    #[test]
    fn resolve_one_java_method_call_ambiguous_imports_stay_unresolved() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("close".into(), vec![
            ResolveDef { id: 99, file_id: 0, lang: FileKind::Java,
                         pkg: Some("android.os".into()),
                         scope_path: vec!["PerfettoTrace".into(), "Session".into()] },
            ResolveDef { id: 88, file_id: 0, lang: FileKind::Java,
                         pkg: Some("com.x".into()),
                         scope_path: vec!["OtherClass".into()] },
        ]);
        let mut imports = HashMap::new();
        // Both classes imported → ambiguous, must stay unresolved.
        imports.insert(5u32, vec![
            ("PerfettoTrace".to_string(), Some("android.os".to_string())),
            ("OtherClass".to_string(), Some("com.x".to_string())),
        ]);
        let r = mk_ref("close", FileKind::Java, 5);
        let mut n = 0u64;
        let got = resolve_one(&r, &by_name, &HashMap::new(), &imports, &empty_ns(), &HashMap::new(), &mut n);
        assert_eq!(got, 0, "two viable imported candidates → unresolved (truthful)");
        assert_eq!(n, 0);
    }

    #[test]
    fn resolve_one_kotlin_same_package_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Activity".into(), vec![
            mk_def(11, FileKind::Kotlin, Some("com.other")),
            mk_def(22, FileKind::Kotlin, Some("android.app")),
        ]);
        let mut pkg = HashMap::new();
        pkg.insert(5u32, "android.app".to_string());
        let r = mk_ref("Activity", FileKind::Kotlin, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &pkg, &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    /// Kotlin `kotlin.collections` fallback: `List` lands on the
    /// kotlin.collections candidate when no same-package / explicit-import
    /// hit applies.
    #[test]
    fn resolve_one_kotlin_implicit_collections_fallback() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("List".into(), vec![
            mk_def(11, FileKind::Kotlin, Some("com.other")),
            mk_def(22, FileKind::Kotlin, Some("kotlin.collections")),
        ]);
        let r = mk_ref("List", FileKind::Kotlin, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    /// Kotlin explicit import wins over both same-package and implicit.
    #[test]
    fn resolve_one_kotlin_explicit_import_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Bundle".into(), vec![
            mk_def(11, FileKind::Kotlin, Some("com.other")),
            mk_def(22, FileKind::Kotlin, Some("android.os")),
        ]);
        let mut imports = HashMap::new();
        imports.insert(5u32, vec![("Bundle".to_string(), Some("android.os".to_string()))]);
        let r = mk_ref("Bundle", FileKind::Kotlin, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &imports, &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    // ---- C++ Layer 2 ----

    /// C++ same/enclosing-namespace match: a ref in namespace `android::os`
    /// prefers a candidate scoped to `android::os` or one of its
    /// prefixes (`android`).
    #[test]
    fn resolve_one_cpp_same_namespace_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("Parcel".into(), vec![
            mk_def_scoped(11, FileKind::Cpp, &["other", "ns"]),
            mk_def_scoped(22, FileKind::Cpp, &["android", "os"]),
            mk_def_scoped(33, FileKind::Cpp, &["unrelated"]),
        ]);
        let r = mk_ref_scoped("Parcel", FileKind::Cpp, 5, &["android", "os"]);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &empty_ns(), &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
    }

    /// C++ `using namespace android::base;` directive: pick the
    /// candidate whose namespace begins with `android::base`.
    #[test]
    fn resolve_one_cpp_using_namespace_narrowing() {
        let mut by_name: HashMap<String, Vec<ResolveDef>> = HashMap::new();
        by_name.insert("StringPrintf".into(), vec![
            mk_def_scoped(11, FileKind::Cpp, &["other"]),
            mk_def_scoped(22, FileKind::Cpp, &["android", "base"]),
        ]);
        let mut using = HashMap::new();
        using.insert(5u32, vec!["android::base".to_string()]);
        let r = mk_ref("StringPrintf", FileKind::Cpp, 5);
        let mut n = 0u64;
        assert_eq!(resolve_one(&r, &by_name, &HashMap::new(), &HashMap::new(), &using, &HashMap::new(), &mut n), 22);
        assert_eq!(n, 1);
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

    // ------------------------------------------------------------------
    // MCP arg validation — pin the per-tool required-arg map and the
    // empty-string-rejection rule. These tests run without a real
    // StoreReader because mcp_required_args_for + mcp_validate_required_args
    // are pure functions of the tool name + JSON arguments.
    // ------------------------------------------------------------------

    /// Every tool advertised by tools/list must have an entry in
    /// mcp_required_args_for. Catches schema/validator drift at test
    /// time so a future "add a new tool" change has to update both
    /// in the same diff.
    #[test]
    fn mcp_required_args_covers_every_advertised_tool() {
        let v = mcp_tools_list_result();
        let tools = v.pointer("/tools").and_then(|x| x.as_array())
            .expect("tools array");
        assert!(!tools.is_empty(), "tools list must not be empty");
        for t in tools {
            let name = t.get("name").and_then(|x| x.as_str())
                .expect("tool entry has name");
            assert!(mcp_required_args_for(name).is_some(),
                "advertised tool '{name}' missing from mcp_required_args_for; \
                 add it to keep schema + validator in sync");
        }
    }

    /// The required set per tool. If this changes, USAGE / MCP docs
    /// must change too — that's the point of pinning it.
    #[test]
    fn mcp_required_args_match_documented_shape() {
        assert_eq!(mcp_required_args_for("def"),      Some(&["name"][..]));
        assert_eq!(mcp_required_args_for("ref"),      Some(&["name"][..]));
        assert_eq!(mcp_required_args_for("callers"),  Some(&["name"][..]));
        assert_eq!(mcp_required_args_for("prefix"),   Some(&["prefix"][..]));
        assert_eq!(mcp_required_args_for("fuzzy"),    Some(&["substr"][..]));
        assert_eq!(mcp_required_args_for("grep"),     Some(&["pattern"][..]));
        assert_eq!(mcp_required_args_for("outline"),  Some(&["path"][..]));
        assert_eq!(mcp_required_args_for("coverage"), Some(&["path"][..]));
        assert_eq!(mcp_required_args_for("stats"),    Some(&[][..]));
        assert_eq!(mcp_required_args_for("ask"),      Some(&["query"][..]));
        assert_eq!(mcp_required_args_for("nonexistent"), None);
    }

    /// Missing arg → returns the arg's name.
    #[test]
    fn mcp_validate_flags_missing_arg() {
        let args = serde_json::json!({});
        assert_eq!(mcp_validate_required_args("def", &args),
                   Some("name".to_string()));
    }

    /// Empty-string arg → also flagged. The original bug:
    /// `{"name": ""}` silently coerced to "match all" and returned
    /// garbage anonymous-enum hits from C++ code where the FST has
    /// thousands of empty-name entries.
    #[test]
    fn mcp_validate_flags_empty_string_arg() {
        let args = serde_json::json!({"name": ""});
        assert_eq!(mcp_validate_required_args("def", &args),
                   Some("name".to_string()));
    }

    /// Null arg → flagged. JSON null is the explicit "I deliberately
    /// don't have a value" — treat the same as missing.
    #[test]
    fn mcp_validate_flags_null_arg() {
        let args = serde_json::json!({"prefix": null});
        assert_eq!(mcp_validate_required_args("prefix", &args),
                   Some("prefix".to_string()));
    }

    /// Valid non-empty string → no error.
    #[test]
    fn mcp_validate_accepts_non_empty_arg() {
        let args = serde_json::json!({"name": "ActivityManagerService", "limit": 5});
        assert!(mcp_validate_required_args("def", &args).is_none());
    }

    /// A tool with no required args (stats) always passes validation
    /// regardless of what arguments object the caller sends.
    #[test]
    fn mcp_validate_zero_required_args_always_passes() {
        assert!(mcp_validate_required_args("stats", &serde_json::json!({})).is_none());
        assert!(mcp_validate_required_args("stats", &serde_json::json!({"junk": 1})).is_none());
    }

    /// mcp_tool_error wraps a message in the correct MCP envelope
    /// shape (content[] of text part, isError: true). Pinned because
    /// MCP clients rely on this exact field layout.
    #[test]
    fn mcp_tool_error_shape() {
        let err = mcp_tool_error("kaboom".to_string());
        assert_eq!(err.pointer("/content/0/type").and_then(|v| v.as_str()), Some("text"));
        assert_eq!(err.pointer("/content/0/text").and_then(|v| v.as_str()), Some("kaboom"));
        assert_eq!(err.get("isError").and_then(serde_json::Value::as_bool), Some(true));
    }

    /// Regression test for the MCP tool-error unwrap. When `serve`
    /// returns `{"error": "..."}` inside the result, the MCP wrapper
    /// must place the BARE message string into content[0].text — NOT
    /// the JSON-stringified `{"error": "..."}`. An LLM consuming the
    /// content shouldn't have to json.parse again to read the hint.
    ///
    /// This pins the fix for the double-encoding bug found during
    /// the v0.1.1 LLM-self-test against `ask` on an embedding-less
    /// index.
    #[test]
    fn mcp_tool_error_unwraps_serve_error_envelope() {
        // mcp_tool_error itself produces the envelope shape that
        // mcp_tools_call uses; the call path additionally unwraps
        // `{"error": "..."}` from the serve response before placing
        // the bare message into text. Test the contract via the
        // public helper + a constructed Value matching what serve
        // emits.
        let serve_result = serde_json::json!({"error": "no embedding sidecar — run `scry build-embeddings`"});
        // Simulate the unwrap that mcp_tools_call performs.
        let err_val = serve_result.as_object().and_then(|m| m.get("error"))
            .expect("serve emits {error: <string>}");
        let bare = err_val.as_str().map(String::from).expect("bare string");
        let envelope = mcp_tool_error(bare);
        let text = envelope.pointer("/content/0/text").and_then(|v| v.as_str())
            .expect("text part");
        // The bare message: no leading {"error":, no escaped quotes.
        assert!(!text.starts_with('{'),
                "tool-error text must be the bare message, not a JSON literal; got: {text}");
        assert!(text.contains("embedding sidecar"),
                "tool-error text must preserve the hint; got: {text}");
        assert!(text.contains("build-embeddings"),
                "tool-error text must include the actionable hint; got: {text}");
    }

    // ------------------------------------------------------------------
    // MCP version negotiation — per spec §lifecycle/Version Negotiation.
    // ------------------------------------------------------------------

    /// Client requests a version we support → server MUST echo it.
    /// (Forward-compatibility: a 2024 client connecting to a 2025 server
    /// still gets 2024 back so it can continue talking to us.)
    #[test]
    fn mcp_initialize_echoes_supported_version() {
        for v in MCP_SUPPORTED_VERSIONS {
            let req = serde_json::json!({"protocolVersion": v});
            let r = mcp_initialize_result(&req);
            assert_eq!(r.pointer("/protocolVersion").and_then(|x| x.as_str()), Some(*v),
                       "must echo client-requested version '{v}' verbatim");
        }
    }

    /// Client requests an unsupported version → server returns its
    /// latest. (The client is then free to disconnect per spec; that's
    /// not our problem.)
    #[test]
    fn mcp_initialize_returns_latest_on_unsupported_version() {
        let req = serde_json::json!({"protocolVersion": "1999-01-01"});
        let r = mcp_initialize_result(&req);
        assert_eq!(r.pointer("/protocolVersion").and_then(|x| x.as_str()),
                   Some(MCP_SUPPORTED_VERSIONS[0]),
                   "unsupported version must fall back to our latest");
    }

    /// Missing protocolVersion field → also falls back to latest.
    /// Some lightweight clients omit it entirely.
    #[test]
    fn mcp_initialize_handles_missing_version() {
        let r = mcp_initialize_result(&serde_json::json!({}));
        assert_eq!(r.pointer("/protocolVersion").and_then(|x| x.as_str()),
                   Some(MCP_SUPPORTED_VERSIONS[0]));
    }

    /// Newest-first ordering on MCP_SUPPORTED_VERSIONS is load-bearing
    /// — `[0]` is treated as "our latest" in the fallback path. Pin it.
    #[test]
    fn mcp_supported_versions_newest_first() {
        let v = MCP_SUPPORTED_VERSIONS;
        assert!(!v.is_empty(), "must support at least one MCP version");
        let mut sorted: Vec<&&str> = v.iter().collect();
        sorted.sort_by(|a, b| b.cmp(a));
        let sorted_owned: Vec<&str> = sorted.iter().map(|s| **s).collect();
        let actual: Vec<&str> = v.to_vec();
        assert_eq!(actual, sorted_owned, "MCP_SUPPORTED_VERSIONS must be newest-first");
    }

    // ------------------------------------------------------------------
    // OWNERS parser — small focused tests on the line classifier.
    // ------------------------------------------------------------------

    #[test]
    fn parse_owners_collects_emails_and_per_file() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-owners-test-{}", std::process::id())
        );
        std::fs::write(&tmp,
            "# AOSP OWNERS-style fixture\n\
             alice@example.com\n\
             bob@example.com  \n\
             set noparent\n\
             include /COMMON_OWNERS\n\
             per-file ActivityManager* = file:/AM_OWNERS\n\
             per-file *.java = carol@example.com\n\
             file:/TOP_LEVEL_REF\n\
             not_an_email_just_text\n"
        ).unwrap();
        let parsed = parse_owners_file(&tmp);
        // Two emails, in file order; comments + plain `set <other>` + include skipped.
        assert_eq!(parsed.emails, vec![
            "alice@example.com".to_string(),
            "bob@example.com".to_string(),
        ]);
        // Two per-file rules + the top-level file: reference (= 3 entries).
        assert_eq!(parsed.per_file.len(), 3);
        assert!(parsed.per_file.iter().any(|p| p.contains("ActivityManager*")));
        assert!(parsed.per_file.iter().any(|p| p.contains("carol@example.com")));
        assert!(parsed.per_file.iter().any(|p| p.starts_with("file:")));
        // The `set noparent` line in the fixture sets the flag.
        assert!(parsed.noparent, "set noparent must be recognized");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Missing file → empty parsed record, never panics.
    #[test]
    fn parse_owners_missing_file_returns_empty() {
        let parsed = parse_owners_file(Path::new("/no/such/OWNERS"));
        assert!(parsed.emails.is_empty());
        assert!(parsed.per_file.is_empty());
        assert!(!parsed.noparent);
    }

    /// Comments-only file → empty parsed record.
    #[test]
    fn parse_owners_comments_only() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-owners-comments-{}", std::process::id())
        );
        std::fs::write(&tmp, "# just\n# comments\n\n").unwrap();
        let parsed = parse_owners_file(&tmp);
        assert!(parsed.emails.is_empty());
        assert!(parsed.per_file.is_empty());
        assert!(!parsed.noparent);
        let _ = std::fs::remove_file(&tmp);
    }

    /// Bare `noparent` (without the `set ` prefix) is also accepted —
    /// some AOSP OWNERS files use the shorter form.
    #[test]
    fn parse_owners_bare_noparent_recognized() {
        let tmp = scry_store::scry_tmp_dir().join(
            format!("scry-owners-bare-noparent-{}", std::process::id())
        );
        std::fs::write(&tmp, "noparent\nlocal@example.com\n").unwrap();
        let parsed = parse_owners_file(&tmp);
        assert!(parsed.noparent, "bare `noparent` must set the flag");
        assert_eq!(parsed.emails, vec!["local@example.com".to_string()]);
        let _ = std::fs::remove_file(&tmp);
    }
}
