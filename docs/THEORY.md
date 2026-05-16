# THEORY — building a system like scry from scratch

This document is a self-contained course on the computer science
behind scry. It is organized so that an upper-division undergraduate
can read straight through, derive every claim, and finish with
enough understanding to reimplement scry on a different language
stack. A graduate reader can skim the undergrad framings and focus
on the *Tradeoffs* and *Open questions* sections at the end of each
chapter. A reader at the research frontier should treat each
*Open questions* section as honest unsettled territory in the
literature, not finished work scry has solved.

Every chapter follows the same shape:

1. **The problem** — what would you write if you sat down with a
   blank editor.
2. **Where the naïve solution breaks** — derived from measurements
   on the real corpus, not folklore.
3. **The theory that fixes it** — the standard CS result that maps
   onto the problem, with the math worked out.
4. **What scry actually does** — pointer to the code, with the
   parameters chosen and why.
5. **Tradeoffs** — what we gave up to make that choice, and the
   alternative designs that are still defensible.
6. **Open questions** — what's genuinely unsolved, where the
   literature disagrees, and what the next version of scry might
   try.

The chapters are independent enough that you can read out of
order, but the cross-references assume you've at least skimmed
chapters 1–3 (the workload, the memory hierarchy, the EM model)
because every later chapter rests on them.

---

## Table of contents

- [Chapter 0 — prerequisites and notation](#chapter-0--prerequisites-and-notation)
- [Chapter 0.5 — Rust, the language and how scry uses it](#chapter-05--rust-the-language-and-how-scry-uses-it)
- [Chapter 0.6 — the toolbox (every dependency, what it does, why we picked it)](#chapter-06--the-toolbox-every-dependency-what-it-does-why-we-picked-it)
- [Chapter 1 — the workload, in numbers](#chapter-1--the-workload-in-numbers)
- [Chapter 1.5 — a brief history of code indexing](#chapter-15--a-brief-history-of-code-indexing)
- [Chapter 2 — the memory hierarchy and the external-memory model](#chapter-2--the-memory-hierarchy-and-the-external-memory-model)
- [Chapter 3 — virtual memory, mmap, and the page cache](#chapter-3--virtual-memory-mmap-and-the-page-cache)
- [Chapter 4 — trigram inverted indices](#chapter-4--trigram-inverted-indices)
- [Chapter 5 — from regex to trigrams (literal extraction)](#chapter-5--from-regex-to-trigrams-literal-extraction)
- [Chapter 6 — finite automata and the FST](#chapter-6--finite-automata-and-the-fst)
- [Chapter 7 — fuzzy match as automaton intersection](#chapter-7--fuzzy-match-as-automaton-intersection)
- [Chapter 8 — columnar layout + byte-offset sidecars](#chapter-8--columnar-layout--byte-offset-sidecars)
- [Chapter 9 — parallel pipelines and work-stealing](#chapter-9--parallel-pipelines-and-work-stealing)
- [Chapter 10 — incremental parsing with tree-sitter](#chapter-10--incremental-parsing-with-tree-sitter)
- [Chapter 11 — resilience under load (cgroups, jemalloc, OOM)](#chapter-11--resilience-under-load-cgroups-jemalloc-oom)
- [Chapter 12 — putting it together](#chapter-12--putting-it-together)
- [Chapter 13 — the tradeoffs scry made](#chapter-13--the-tradeoffs-scry-made)

---

## Chapter 0 — prerequisites and notation

You should be comfortable with:

- Big-O and Big-Θ in the comparison model (Cormen, Leiserson,
  Rivest, Stein chapters 2–3).
- Basic regular language theory: regexes, DFAs/NFAs, closure
  properties (Sipser, *Introduction to the Theory of
  Computation*, chapter 1).
- The Unix file abstraction: file descriptors, `read(2)`, `mmap(2)`,
  and what "page cache" means at a hand-wavy level.

You do *not* need:

- Compiler theory beyond what's in chapter 10.
- Distributed systems — scry is one host.
- ML — no model anywhere.

Notation:

- `N` = number of indexed files (1 M on the production corpus).
- `S` = number of symbols (~22 M).
- `R` = number of references (~63 M).
- `B` = page size or block size in the EM model (4 KiB unless
  stated otherwise).
- `|q|` = length of a query string in bytes.
- `posting(t)` = sorted list of file IDs that contain trigram `t`.
- `T(P)` = the set of trigrams of a string `P`.

When this doc says "the corpus", it means AOSP master + Linux
7.0-rc7 indexed together: 1,009,166 files, 70.4 GB source. The
production index lives at `/mnt/agent/scry-index` and every number
in the doc is reproducible by running the scripts in
`scripts/bench_grep.sh` against it.

---

## Chapter 0.5 — Rust, the language and how scry uses it

scry is ~7500 lines of Rust across five workspace crates. This
chapter is a short tour of what Rust gives you and how each
feature lands in scry's code, oriented at someone who has written
C++/Go/Java but never written serious Rust. If you've written
Rust day-to-day you can skim straight to §0.5.6, where the
scry-specific idioms start.

### 0.5.1 Memory and ownership in one minute

The Rust **ownership** rule: every value has exactly one owner;
when the owner goes out of scope, the value is dropped (destructor
runs, memory freed). You can pass ownership (`move`) or you can
*borrow* — temporarily lend a reference to another piece of code.
Borrows come in two flavors enforced by the compiler:

- `&T` — a *shared* immutable borrow. You can have many at once.
- `&mut T` — an *exclusive* mutable borrow. You can have exactly
  one at a time, and *no shared borrows simultaneously*.

The compiler tracks the lifetime of every borrow ("how long must
this reference stay valid") and refuses programs where a borrow
outlives the owner or where shared and mutable borrows coexist.
That refusal is what makes Rust safe without a garbage collector:
no use-after-free (the owner outlives the borrow), no iterator
invalidation (you can't `&mut` the container while iterating it),
no data races (a `&mut` is exclusive, so two threads can't write
the same value at once unless they explicitly synchronize).

The cost: some patterns easy in GC'd languages (cyclic graphs, two
threads incrementally building a shared structure) require either
explicit reference counting (`Rc<T>` single-threaded, `Arc<T>`
multi-threaded), interior mutability (`Cell<T>`, `RefCell<T>`,
`Mutex<T>`), or rethinking the data flow so the ownership tree is
acyclic.

In scry: the indexer's data flow *is* acyclic by construction
(walker → parsers → writer → finalize). We never reach for `Arc<Mutex<_>>`
inside the hot path; we use it only at the supervisor level
(progress counters, the OOM heartbeat), where atomic operations
or coarse locks are appropriate.

### 0.5.2 Errors as values: `Result<T, E>` and `?`

Rust has no exceptions. Fallible functions return
`Result<T, E>` — an enum with `Ok(T)` and `Err(E)` variants. The
`?` operator unwraps `Ok` or returns `Err` to the caller, so error
propagation looks like normal sequencing:

```rust
fn read_index(dir: &Path) -> Result<StoreReader> {
    let manifest: Manifest = read_bincode(&dir.join("manifest.bin"))?;
    let files: Vec<FileEntry> = read_bincode(&dir.join("files.bin"))?;
    Ok(StoreReader::new(manifest, files))
}
```

Two crates dominate error handling in scry:

- **`anyhow`** — for top-level/glue code where you just want a
  context-rich error type. `Result<T> = anyhow::Result<T>` is
  used throughout `scry-cli` and most internal APIs.
- **`thiserror`** — for crate-public error types where you want
  callers to match on specific variants. We use this sparingly;
  the indexer is mostly "if anything goes wrong, log it and skip
  the file or abort the run" which is exactly the `anyhow`
  shape.

In scry: read the error chain in `crates/scry-cli/src/main.rs`.
Every command returns `Result<()>`. Errors bubble up to `main`,
which prints them with `{:#}` to get the full `anyhow::Context`
chain. There is no `try/catch`; there are no exceptions; nothing
can panic across a function boundary unless we made it panic
explicitly. Panics that *do* happen (`expect`, `unwrap`, array
index out of bounds) are bugs and get fixed; we don't catch them.

### 0.5.3 Traits — Rust's interfaces

A **trait** is Rust's interface mechanism. You declare what a
type must do; implementers fill in the methods. Two flavors:

- **Static dispatch** (monomorphization). Calling a trait method
  on a generic type — `fn foo<T: Read>(r: T)` — compiles to a
  specialized copy of `foo` per concrete `T`. Zero overhead at
  runtime; some binary bloat at compile time.
- **Dynamic dispatch** (vtable). Calling a trait method through
  a `dyn` reference — `fn foo(r: &dyn Read)` — uses a vtable
  pointer. One indirection per call; one copy of `foo` in the
  binary.

scry leans heavily on monomorphization in hot paths: the lazy
reader's `LazyVec<T>` parameterized over record type, the k-way
merge over an `Iterator<Item=...>`. Cold paths (CLI plumbing,
output formatting) use `Box<dyn Trait>` where convenient.

The standard traits worth knowing for scry:

- `Read`, `Write`, `Seek` — IO interfaces. `BufReader<File>`,
  `BufWriter<File>` everywhere we touch disk.
- `Iterator` — the standard combinator surface (`.map`, `.filter`,
  `.collect`, etc.). The k-way merge takes an `Iterator` of
  per-chunk iterators.
- `Send`, `Sync` — marker traits for thread safety. `T: Send`
  means a value of `T` can be transferred to another thread;
  `T: Sync` means `&T` can be. rayon's `par_iter` requires the
  iterator items to be `Send`. The compiler enforces this; you
  can't accidentally share a non-thread-safe type across threads.
- `Drop` — destructor. Files close, locks release, mmaps unmap
  automatically when the value drops.

### 0.5.4 The `unsafe` keyword

Some operations the compiler can't prove safe are legal in
`unsafe` blocks: raw pointer dereferences, FFI calls, transmuting
between types, calling functions marked `unsafe fn`. `unsafe`
doesn't disable the borrow checker; it just unlocks a small set
of additional operations.

The cultural contract: every `unsafe` block needs a comment
explaining *why* the operation is sound, what invariants it
requires, and what could go wrong if those invariants are
violated. The Rust standard library has hundreds of `unsafe`
blocks; each one carries a `// SAFETY:` comment.

scry's posture: four of the five crates declare
`#![forbid(unsafe_code)]` at the crate root, which is a hard
compile-time refusal of `unsafe` anywhere in the crate. The fifth
crate (`scry-store`) has *one* function with `unsafe` —
`safe_mmap` (`crates/scry-store/src/lib.rs:69`) — which wraps
`memmap2::Mmap::map`. The `unsafe` is required by `memmap2`
because Rust's memory safety can't track changes to a file made
by another process; the module-level "Unsafe policy" doc block
explains the invariants we assume (the index files are
read-only for the lifetime of the mmap, and any writer uses the
atomic-rename pattern that doesn't modify them in place).

The result: 4/5 crates are *provably memory-safe by construction*
(the compiler refuses code that would violate it), and the
remaining `unsafe` is contained to a 5-line helper with one
documented invariant.

### 0.5.5 Why Rust for this workload specifically

C++ could do everything scry does. So could Go. The reasons we
picked Rust:

| feature                          | Rust                            | C++                            | Go                              |
|----------------------------------|----------------------------------|--------------------------------|---------------------------------|
| memory safety without GC         | yes, compiler-enforced          | unsafe by default              | yes, but with GC                |
| zero-cost abstractions           | yes                             | yes                            | partial (interface dispatch)    |
| `mmap` ergonomics                | excellent (`memmap2`)           | manual                         | `golang.org/x/exp/mmap` ok      |
| SIMD strings                     | `memchr`, `regex` (compiles to PCRE-class) | manual                  | `regexp` ~5× slower             |
| work-stealing parallelism        | `rayon` (Blumofe-Leiserson)    | TBB, OpenMP                    | goroutines (good but heavier)   |
| FST library                      | `fst` (production-grade)        | port required                  | port required                   |
| tree-sitter bindings             | first-class                     | first-class (C native)         | second-class                    |
| ignore-aware walker              | `ignore` (extracted from ripgrep) | manual                       | manual                          |
| build / dependency story         | `cargo`                         | meson/bazel/cmake/...           | `go mod`                        |
| static binary                    | trivial                         | possible but painful           | trivial                         |

The single most important entry is the second-to-last row: every
ingredient (FST, trigram, memmap, work-stealing, ignore-walker)
is a maintained crate with a battle-tested implementation. ripgrep
and friends already built and proved the pieces; scry assembles
them. C++ could match, but only by porting two of those libraries
from Rust or accepting weaker substitutes. Go could match for the
glue but loses on `memchr`/regex throughput, which is exactly the
inner loop of the grep candidate scan.

The cost: Rust has a steep learning curve, and the borrow checker
*will* push back on patterns that are easy in GC'd languages.
That's mostly fine because scry's data flow is naturally
acyclic, but it's a real cost for new contributors.

### 0.5.6 scry-specific Rust idioms

A handful of patterns recur throughout the codebase. Recognizing
them makes the source much easier to read.

**Builder + finalize on owning structures**. `StoreWriter` owns
file handles for the duration of indexing; calling `.finalize()`
consumes the writer (takes `self`, not `&mut self`), closing
files and emitting the FSTs. After `.finalize()` the writer is
gone — the type system prevents you from accidentally writing
more after finalization.

**Newtype wrappers around primitives**. `Trigram = [u8; 3]`,
`FileId = u32`, `SymbolId = u32`. The wrappers give us
typed APIs (`get_symbol(SymbolId)` not `get_symbol(u32)`) and
prevent mixing.

**`for<'de> Deserialize<'de>` bounds on `LazyVec<T>`**. The
higher-ranked trait bound says "`T` is `Deserialize` for *any*
lifetime", which is what we need because the lazy lookup borrows
the mmap'd bytes for the duration of one decode call. The
syntax is ugly but the pattern is standard.

**`anyhow::Context::with_context` everywhere on the IO boundary**.
Every `?` that crosses an IO boundary is followed by
`.with_context(|| format!("opening {}", path.display()))?` so
errors carry their file path. The cost is a few extra string
allocations on the error path; the benefit is debuggable error
chains in the wild.

**Atomic primitives for cross-thread state**. The progress
counter is `AtomicU64` with `Relaxed` ordering. The OOM gate is
`AtomicBool` with `Acquire`/`Release`. No mutexes in the parser
pool's hot path.

**Tests in the same file as the code**. Rust's convention is
`#[cfg(test)] mod tests { ... }` at the bottom of each `.rs`
file. scry follows it strictly: every module that has logic has
unit tests in the same file. End-to-end tests live in
`crates/scry-cli/tests/e2e.rs` and exercise the actual built
binary.

### 0.5.7 The crate layout

scry is a Cargo workspace with five crates. Each has one job and
depends only on what it must:

```
scry-walker     ── classifies files (the 40 FileKind variants)
   ▲
   │
scry-lang       ── tree-sitter parsers + symbol extraction
   ▲
   │
scry-aosp       ── AOSP-specific custom parsers
   ▲                (Android.bp, AIDL, init.rc, sepolicy, ...)
   │
scry-store      ── on-disk format: writer + reader + FSTs
   ▲                (the one crate that touches `unsafe`)
   │
scry-cli        ── the binary; CLI parsing, JSON-RPC, output
```

The arrows are "depends on". The graph is a chain, not a web,
which keeps build times short and the design intelligible. Each
crate has its own `Cargo.toml` declaring its dependencies; the
workspace `Cargo.toml` pins the versions.

If you've read this far, the rest of the doc is essentially "what
algorithms run inside each box". The Rust glue isn't load-bearing
once you've internalized the four idioms above (Builder/finalize,
newtypes, atomic cross-thread state, error context).

### 0.5.8 Tradeoffs

- **Compile times.** A clean `cargo build --release` takes ~20 s
  on this host. Incremental builds are 1-3 s. C++ would be
  faster with PCH; Go would be far faster. We accept the cost
  because the runtime wins make up for it within a single
  iteration.
- **Binary size.** The release binary is ~25 MB stripped. C
  would be smaller; we don't care because we ship one binary
  per host.
- **Async vs sync.** scry is synchronous throughout — no `async
  fn`, no `tokio`. The work is CPU-bound (parsing) or
  filesystem-bound (mmap); async wouldn't help and would add
  significant complexity. If we ever add a long-running daemon
  with thousands of concurrent connections, that calculus
  changes.

### 0.5.9 Open questions

- **`gccrs` and the polonius borrow checker** are both in flux.
  Neither affects scry directly; both could make some patterns
  (cyclic graphs, two-phase borrows) easier in the future.
- **Polonius** would let us write certain iterators more
  naturally. Today we work around the NLL borrow checker's
  conservatism with manual index-based iteration in a couple
  of places. The pattern is well-understood.

---

## Chapter 0.6 — the toolbox (every dependency, what it does, why we picked it)

scry has ~22 direct dependencies. Each is doing genuine work; we
didn't pull in any "framework" or "utility belt" crates. This
chapter is a per-dependency tour: what the crate does, why scry
needs it, what we'd reach for if we couldn't use it, and which
chapter of this doc explains the theory the crate implements.

The list is organized by *role in the system*, not alphabetically.
The roles are: filesystem walking, parsing, storage, parallelism,
allocator, CLI plumbing, error handling, logging.

### Filesystem walking — `ignore`

[`ignore`][ignore] (Andrew Gallant, 2016) is the gitignore-aware
parallel directory walker extracted from `ripgrep`. It honors
`.gitignore`, `.ignore`, parent-directory ignore files, and
custom skiplists. Its parallel walker is what feeds scry's
indexer with file paths.

[ignore]: https://docs.rs/ignore/

Why this crate specifically:

- It's the reference implementation. `ripgrep` is the most-
  battle-tested tool that walks 100k-file trees daily; we
  inherit the same code.
- The parallel walker yields files to a callback as it finds
  them. We collect into a single `Vec<PathBuf>` first
  (cheap — paths are small), then sort by file size before
  handing to the parser pool (ch. 9).
- The ignore semantics match programmer intuition: a file the
  user has put in `.gitignore` is not "missing" or "extra" —
  it's correctly absent.

Alternative if it didn't exist: `walkdir` (simpler, no ignore
support) + manual gitignore parsing. ~1000 LOC of work for
worse semantics.

### Source parsing — `tree-sitter` family

[`tree-sitter`][ts-crate] (Max Brunsfeld, 2018) is the GLR
parser generator covered in ch. 10. scry depends on the runtime
crate (`tree-sitter`) plus one grammar per supported language:

[ts-crate]: https://docs.rs/tree-sitter/

| crate                       | language                            |
|-----------------------------|-------------------------------------|
| `tree-sitter-c`             | C                                   |
| `tree-sitter-cpp`           | C++                                 |
| `tree-sitter-java`          | Java                                |
| `tree-sitter-kotlin-ng`     | Kotlin (community grammar, the `-ng` fork is actively maintained) |
| `tree-sitter-rust`          | Rust                                |
| `tree-sitter-go`            | Go                                  |
| `tree-sitter-python`        | Python                              |
| `tree-sitter-language`      | shared loader API for the grammars  |
| `streaming-iterator`        | tree-sitter's query iterator returns this trait |

Why this set:

- All seven languages are first-class in AOSP (Java framework,
  C/C++ native, Kotlin services, Rust safety code, Go build
  tooling, Python build scripts) or the Linux kernel (C +
  shell + a smattering of Python). Adding bash is on the
  roadmap (tree-sitter-bash exists; we haven't wired it).
- Kotlin's grammar is the weakest — tree-sitter-kotlin-ng is a
  fork of the original tree-sitter-kotlin because the upstream
  maintainer went inactive. The fork compiles cleanly and
  handles modern Kotlin (sealed classes, smart casts) better.
- `streaming-iterator` is a small crate whose only job is to
  provide the `StreamingIterator` trait that tree-sitter's
  `QueryCursor` needs. It's the kind of pure-interface crate
  that exists because Rust's `Iterator` doesn't allow yielding
  references to internal state, and `StreamingIterator` does.

Alternative if it didn't exist: per-language hand-written
parsers. ~10 000+ LOC of work for worse error recovery and no
shared API.

### AOSP-specific parsers — written in scry

Soong (`Android.bp`), Android.mk, AIDL, HIDL, Bazel BUILD, GN,
CMake, Kconfig, init.rc, sepolicy `.te`, AndroidManifest.xml,
aconfig, OWNERS, `api/*.txt` — these don't have good
tree-sitter grammars (or none at all), so scry has 12 custom
parsers in `crates/scry-aosp/src/`. Each is ~200-500 LOC of
hand-written recursive-descent.

The one external dep here is **`quick-xml`** for the XML
formats (AndroidManifest.xml in scry-aosp; some resource files
later). It's the fastest pull-parser in the Rust ecosystem and
handles the XML quirks correctly.

Why hand-written rather than a parser generator: the grammars
are small (200-500 production rules each); error recovery
needs to be lenient (an `Android.bp` with a single broken
module should still index the rest); and the parsers don't
need to round-trip the source — they only need to extract
symbols.

### Storage primitives — `bincode`, `memmap2`, `fst`, `blake3`, `libc`

#### `bincode`

[`bincode`][bincode] is a binary serialization format for Rust.
It encodes `Serialize`-derived types into a compact binary
representation (smaller than JSON, no field names, length-
prefixed strings, native endian). scry uses it for every on-
disk record (`SymbolRecord`, `RefRecord`, `FileEntry`, ...).

[bincode]: https://docs.rs/bincode/

Why this crate:

- Zero-overhead with `Serialize`-derived types. We write the
  same struct that we read; no IDL, no schema file.
- Stable on-disk format across point releases (we pin
  `bincode = "1.3"` rather than the 2.x major because the
  format is locked).
- Fast: ~hundreds of MB/s decode on modern hardware. The
  byte-offset sidecar pattern (ch. 8) makes the decode-cost
  irrelevant for the warm path, but it still matters for the
  finalize step that decodes 22 M symbols once.

Tradeoff: bincode encodes language-internal types, so the
format is *language-coupled*. A non-Rust reader would have to
reimplement the bincode format spec. We accept this because
scry's reader and writer are both Rust.

#### `memmap2`

[`memmap2`][memmap2] is the maintained successor to `memmap`
(deprecated). It wraps `mmap(2)` and `munmap(2)` in safe-ish
Rust. The "ish" is because the OS allows other processes to
modify the file while it's mapped, and Rust's borrow checker
can't see this — so the public API has `unsafe fn map`. We
contain that `unsafe` to one helper (`safe_mmap` in
`crates/scry-store/src/lib.rs`).

[memmap2]: https://docs.rs/memmap2/

Why this crate:

- The de-facto standard. `ripgrep`, `tantivy`, `fst` all use it.
- Clean RAII: when the `Mmap` drops, the mapping is released.
  No need to remember `munmap`.
- Supports `Madvise` and `MAP_HUGETLB` hints (we don't use
  them yet; possible future tuning).

Alternative: raw `libc::mmap` + manual lifetime management.
~50 LOC of unsafe per call site. memmap2 saves us from
writing that 11 times.

#### `fst`

[`fst`][fst-crate-toolbox] (Andrew Gallant, 2017) is the
minimized FST library covered in ch. 6. We use the `Set` and
`Map` builders for symbol-name dictionaries and the trigram
dictionary, and the `Automaton` trait for fuzzy and prefix
search.

[fst-crate-toolbox]: https://docs.rs/fst/

Why this crate:

- Production-grade implementation of the FST data structure.
  The minimization is correct; the on-disk format is stable;
  the build API supports streaming construction from a sorted
  iterator.
- Levenshtein automaton intersection built in (the
  `fst::automaton::Levenshtein` type), which is what
  `scry fuzzy` runs on.
- Same author as `ripgrep` and `memchr` — consistent quality
  bar across the Rust text-processing ecosystem.

Alternative: a B-tree or hash map per dictionary. Loses prefix
walk, loses fuzzy, loses minimization. The fst crate is doing
real work that would be ~2000 LOC of careful code to replace.

#### `blake3`

[`blake3`][blake3] is a cryptographic hash function (BLAKE3,
2020) optimized for SIMD and parallel hashing. scry uses it
*non-cryptographically* — for deterministic symbol IDs. The
input is the symbol's fully-qualified name + kind; the output is
a 128-bit ID that's stable across rebuilds.

[blake3]: https://docs.rs/blake3/

Why a crypto hash rather than `fnv` or `xxhash`:

- Determinism across machines. blake3 produces the same bytes
  on any host; `fnv` and `xxhash` are non-cryptographic but
  *also* deterministic across platforms, so this isn't a
  blake3-specific win.
- Collision resistance. With 22 M symbols and 128 bits of ID,
  the birthday-paradox collision probability is ~2^-44 — well
  below "ever happens in practice". `fnv` or 64-bit `xxhash`
  would have measurable collisions at this scale.
- Speed. blake3 hashes at ~3 GB/s per core with SIMD; we hash a
  few bytes per symbol so this isn't a bottleneck either way.

Tradeoff: we're using a crypto hash for non-crypto purposes,
which sometimes triggers code-review eyebrows. The honest answer
is "we picked the strongest of the fast hashes because it's
free", not "we need crypto properties".

#### `libc`

[`libc`][libc-crate] gives us raw FFI bindings to the C standard
library and POSIX. scry uses exactly one symbol: `posix_fadvise`
with `POSIX_FADV_WILLNEED`, for the grep candidate prefetch
(commit `014b061`).

[libc-crate]: https://docs.rs/libc/

Why a 3rd-party crate just for one syscall: there's no stdlib
wrapper for `posix_fadvise`. The alternative is `nix` (a richer
POSIX wrapper) which would pull in more code than we use.

### Parallelism — `rayon`, `parking_lot`, `crossbeam`

#### `rayon`

[`rayon`][rayon] is the work-stealing parallel iterator covered
in ch. 9. scry uses it for the parser pool and for parallel
post-finalize passes.

[rayon]: https://docs.rs/rayon/

Why this crate:

- Implements Blumofe-Leiserson work-stealing with strong
  empirical performance.
- The `par_iter()` API turns a sequential iterator into a
  parallel one with a one-character change. The user-facing
  API is what makes it usable.
- Configurable global pool (`ThreadPoolBuilder::num_threads`)
  for tuning vs. core count.

Alternative: raw `std::thread::scope` + a hand-rolled
work-queue. We'd write ~300 LOC and lose the work-stealing
properties.

#### `parking_lot`

[`parking_lot`][parking-lot] provides drop-in replacements for
`std::sync::Mutex`, `RwLock`, and `Condvar` that are faster
under contention and *not* poisoned by panics. scry uses the
`Mutex` in the writer's append paths.

[parking-lot]: https://docs.rs/parking_lot/

Why:

- 2-5× faster than std under contention (the std `Mutex` is
  pthread mutex; parking_lot uses a parking-lot algorithm).
- No `PoisonError` to unwrap. If a thread panics holding the
  lock, parking_lot's lock is still usable; std's `Mutex`
  permanently fails.
- The std `Mutex` improvements in recent Rust releases close
  some of the gap. We could probably switch back; we haven't
  bothered because the difference doesn't show up in our
  profile.

#### `crossbeam`

[`crossbeam`][crossbeam] is referenced in the workspace `Cargo.toml`
for the channel and atomic primitives, though our actual usage is
light (most cross-thread coordination is via rayon and atomics).
It's there for future use; specifically, the OOM heartbeat thread
could move from a polled atomic to a crossbeam channel if we
needed back-pressure signaling.

[crossbeam]: https://docs.rs/crossbeam/

### String / regex — `memchr`, `regex`, `regex-syntax`

#### `memchr`

[`memchr`][memchr-crate] (Andrew Gallant) is SIMD-accelerated
byte searching. `memchr(b, slice)` finds the first byte equal to
`b` in `slice`. Throughput is ~50 GB/s on AVX2; falls back to
SSE2/scalar where needed. scry uses it for every per-file scan
in `cmd_grep`.

[memchr-crate]: https://docs.rs/memchr/

Why:

- Single-byte search is the inner loop of literal grep.
  `memchr` is the fastest implementation in the Rust ecosystem
  by a meaningful margin (5-20× over naive `slice.iter().position(|&b| b == n)`).
- Multi-byte (`memmem`) is included for substring search.

Alternative: hand-roll SIMD. We won't.

#### `regex`

[`regex`][regex-crate] is the regex engine from the ripgrep
project. Compiles patterns to a state machine; provides both
"is there a match" and "where are the matches" APIs. Bounded
worst-case (no catastrophic backtracking like PCRE).

[regex-crate]: https://docs.rs/regex/

scry uses it for the regex-mode grep (`scry grep PATTERN
--regex`) and for the literal-extractor's regex parsing.

Why this crate:

- Same lineage as everything else in this ecosystem
  (Gallant, ripgrep).
- Bounded worst case is required. We can't have a user's regex
  hang the indexer.

#### `regex-syntax`

[`regex-syntax`][regex-syntax-crate] is the *parser* for regex
patterns; produces an AST (the HIR — High-level Intermediate
Representation). scry's literal extractor (ch. 5) walks the
HIR to find prefix and suffix literal anchors.

[regex-syntax-crate]: https://docs.rs/regex-syntax/

Why we pull this in directly (rather than going through the
`regex` crate): the `regex` crate hides the HIR; we need
direct access to the AST to walk it.

### Allocator — `tikv-jemallocator` + `tikv-jemalloc-ctl`

[`tikv-jemallocator`][jemalloc-crate] is the TiKV team's
maintained binding to jemalloc. scry uses it as the global
allocator, with `MALLOC_CONF=dirty_decay_ms:100,muzzy_decay_ms:100`
to aggressively return freed pages to the kernel (covered in
ch. 11).

[jemalloc-crate]: https://docs.rs/tikv-jemallocator/

`tikv-jemalloc-ctl` is the API for reading jemalloc's runtime
statistics (`stats.allocated`, `stats.resident`, etc.). The OOM
heartbeat thread polls `stats.allocated` every 100 ms.

Why jemalloc rather than glibc malloc:

- Better thread-local arenas → lower contention under parallel
  workloads.
- The `decay_ms` knobs that let us bound RSS for the cgroup
  envelope. glibc malloc doesn't expose this.
- Runtime stats. We can ask jemalloc "how much memory are you
  holding?" and act on the answer; glibc malloc has nothing
  comparable.

Tradeoff: jemalloc adds ~1 MB to the binary and 1-2 MB of
metadata RSS at startup. Both are noise at our scale.

### CLI — `clap`

[`clap`][clap-crate] is the de-facto CLI parser for Rust. We
use the `derive` feature: declare the CLI as a struct, clap
generates the parser, help text, and error messages.

[clap-crate]: https://docs.rs/clap/

Why:

- The derive API is genuinely good. Struct definitions read
  almost like the CLI docs they generate.
- Subcommands, flags, argument parsing, default values, env
  var fallback — all handled.

Alternative: `argh` is smaller; `pico-args` is tiny. Neither
generates the help text we want without manual work.

### Error handling — `anyhow`

[`anyhow`][anyhow-crate] is the trait-object error type for
contexts where the caller doesn't need to match on specific
error variants. scry uses `anyhow::Result<T>` throughout.

[anyhow-crate]: https://docs.rs/anyhow/

Why:

- `?` propagation just works.
- `.with_context(|| format!("opening {}", path.display()))`
  attaches context as the error unwinds. Final error message
  is a chain of contexts back to the original cause —
  invaluable for debugging.
- We don't have any callers that need to match on specific
  variants. If we did, we'd use `thiserror`.

### Logging — `tracing` + `tracing-subscriber`

[`tracing`][tracing-crate] is the structured-logging successor
to `log`. `tracing-subscriber` is the renderer (formats
messages, applies env-filter, etc.).

[tracing-crate]: https://docs.rs/tracing/

scry uses `tracing::info!` / `warn!` / `error!` macros for
status messages from the indexer and reader. The subscriber is
configured with `EnvFilter` so `RUST_LOG=debug` selects
verbosity per crate.

Why:

- Tree-structured spans let us attribute time to phases ("walk",
  "parse", "finalize") with `tracing::info_span!`.
- Compatible with `tracing-flame` for flamegraphs (we haven't
  needed this; left as a tool for future profiling).

### Serialization — `serde` + `serde_json`

`serde` is the *trait* for "this type can be serialized". `bincode`
implements it for binary; `serde_json` implements it for JSON.

scry uses `serde_json` for the JSON output formats (`--json`,
`scry serve` requests/responses) and `bincode` for the on-disk
columnar files.

The split (`serde` as trait, separate impl crates) is what
lets us use the same `#[derive(Serialize, Deserialize)]` on a
struct and get both encodings for free.

### Putting the toolbox into perspective

The dependency count looks high (~22 direct) but every entry is
doing one specific job and almost all are by ~2 authors (Andrew
Gallant for the text-processing stack, dtolnay for the
serialization + error stack). The "two main authors" pattern is
the standard one in the Rust text-tooling community; we're not
assembling a fragile pile of weakly-maintained crates.

If we had to replace each crate with our own implementation
the work would be roughly:

| crate                    | replacement effort  |
|--------------------------|---------------------|
| `ignore`                 | ~1 000 LOC          |
| `tree-sitter` + grammars | ~50 000 LOC         |
| `bincode`                | ~300 LOC            |
| `memmap2`                | ~150 LOC            |
| `fst`                    | ~2 000 LOC          |
| `blake3`                 | ~3 000 LOC          |
| `rayon`                  | ~1 500 LOC          |
| `memchr`                 | ~2 000 LOC (SIMD)   |
| `regex`                  | ~10 000 LOC         |
| `clap`                   | ~500 LOC (minimal)  |
| `serde` + `serde_json`   | ~3 000 LOC          |
| `tikv-jemallocator`      | (allocator port)    |
| `anyhow` / `tracing` / etc. | ~500 LOC each    |

Total ~75 000 LOC of *just the libraries* before we'd have a
working scry. Compare: scry itself is ~7 500 LOC. The
ecosystem leverage is the design's biggest hidden lever.

---

## Chapter 1 — the workload, in numbers

### 1.1 The problem

A developer at a terminal, or an LLM agent in a tool loop, wants
to answer questions about a source corpus too large to keep in any
single tool's memory. Three example queries — all real, all
common:

```
Q1: Where is `ActivityManagerService` defined?
Q2: Who calls `Binder.transact` from inside `frameworks/base`?
Q3: Show me every file containing the literal `ZygoteInit`.
```

A correct answer must be returned in the time it takes a human to
look up from the keyboard — say, ≤ 1 second — and the index that
produces it must fit on one host's storage and rebuild in the time
a coffee takes to brew, say ≤ 15 minutes.

### 1.2 The naïve approach and what it costs

For Q3 (the simplest), the naïve approach is `grep -rF
'ZygoteInit' /path/to/aosp`. On the production corpus this:

- Walks 1,009,166 files (filesystem traversal).
- Opens, reads, and scans each one.
- Wall time: **> 5 minutes**, killed before completion.

The work is `O(total bytes) = O(70 GB)` regardless of how rare the
pattern is. The disk is fast (3 GB/s sequential) so the lower
bound is ~25 s just for the IO; the rest is the cost of the
per-file open/close syscall and the random access pattern across
a directory tree.

For Q1 and Q2 there is no naïve tool. `grep` can't answer them at
all; `ctags` produces tag tables but doesn't understand scope or
references. The space of plausible answers expands when you have
any kind of pre-built index.

### 1.3 What "fast" actually requires

For all three queries to finish in ≤ 1 s on a 70 GB corpus, the
query must touch *at most some small fraction of the corpus*. The
arithmetic is forced: 1 GB of disk read takes ~330 ms on NVMe; a
query that has to scan the whole tree is permanently > 20 s. So
every query type needs a precomputed structure that lets us go
straight to the answer.

This is the central design constraint, and it determines almost
everything below:

- A *content* query (Q3) needs a structure mapping bytes → files
  containing them. The trigram inverted index, ch. 4.
- A *name* query (Q1) needs a structure mapping names → defining
  symbols, with prefix and fuzzy support. The FST, ch. 6.
- A *reference* query (Q2) needs the def→callers reverse index,
  which falls out of the same pipeline that builds names (ch. 8).

The rest of the course is about how to build those structures so
that (a) the index fits, (b) the index rebuild fits in the time
budget, and (c) the queries are fast at the *right* level of
parallelism for one host.

### 1.4 Tradeoffs at this level

scry is **one host, one user, one repo lineage**. Sourcegraph and
GitHub Code Search both attack the same problem with very different
constraints (many users, many repos, distributed indexers). They
get to amortize index build over many readers; they pay for the
network hop on every query; they have to deal with version
identity. scry skips all of that. The tradeoff is that scry's
design assumes one well-known corpus to index against, and would
need significant rework to host arbitrary user repos at scale.

### 1.5 Open questions

- **Streaming index updates.** scry rebuilds. Sourcegraph
  incrementally re-indexes per commit. The literature on
  incremental inverted indices (Lester et al., 2005; Asadi &
  Lin, 2013) shows the per-commit cost is small but the
  consistency model is delicate. We've left this for v2; the
  rebuild is fast enough for now.

---

## Chapter 1.5 — a brief history of code indexing

Nothing in scry is new. Every load-bearing decision below was made
by some predecessor 5, 15, or 50 years ago, and the field has
been refining the same handful of ideas the whole time. Reading
the lineage is the fastest way to internalize *which decisions
are forced by the problem* and *which are scry's pick from
several defensible options* — because the ones that are forced
keep showing up across the decades. This chapter walks the
history straight through. Each section names the tool, what it
shipped, what it left for its successor, and which chapter of
this doc revisits the technique.

### 1960s — the inverted index, before code

The inverted index was a library-science idea before it was a
computer-science one. Mooers (1949) used the term "information
retrieval" for the first time in a paper on punched-card systems.
By the time SMART (Salton, Cornell, 1960s) was running, the
shape was settled: a sorted dictionary of terms, a posting list
per term holding document IDs, set intersection for AND queries.
Salton's *Information Retrieval Service Center* paper (1968) and
his book *Automatic Information Organization and Retrieval*
(1968) are the canonical references; everything since rearranges
the same three primitives.

What was true then is still true now: the bottleneck is the
posting list, not the dictionary. A useful indexing system spends
its bytes on postings. (We'll see scry repeat this exactly in
ch. 4.)

### 1970s — `ctags` and `grep`: the first generation that actually shipped

In 1979 Ken Arnold wrote `ctags` for BSD Unix. The format was
trivial — one line per tag, three space-separated fields: `name
path pattern`. `vi` and its descendants read the file and let you
jump to a definition with `^]`. There was no scope, no
references, no fuzzy match; just *exact identifier → location*.

Two things make `ctags` historically pivotal:

1. **It established the indexing/querying split.** Run `ctags`
   once over your tree to produce a tag file; thereafter, every
   editor uses it instantly. This is the same split scry uses:
   `scry index` produces the artifacts, `scry def` reads them.
   Every code-search tool since has followed the pattern.
2. **It established the "one file per identifier" failure
   mode.** When two unrelated codebases both define `init`, the
   `ctags` file has two lines and the editor jumps to whichever
   came first. Disambiguation is the user's problem. Every
   modern tool — including scry — has to answer the question
   "which definition is most relevant" that ctags ducked.

`grep` had been around since 1973 (Thompson, Unix V4) and
remained the answer to "show me every file containing this
literal". Throughput was filesystem-bound; selectivity was the
user's problem. `grep` is what every text-search tool has been
trying to beat ever since by *not reading the whole tree on every
query*. (Ch. 4: the trigram index is the canonical answer.)

### 1980s — `cscope` and the first cross-reference databases

`cscope`, written at Bell Labs in 1979 and widely deployed by
the mid-80s, was the first tool to index *references* in addition
to definitions. The database was a custom flat-file format — file
table, line table, symbol table, cross-reference table — that
covered C only. Querying offered eight predicates: callers,
callees, references, file-includes-this, etc. The same shape
scry's query model has, 40 years later.

What `cscope` got right:

- The query model with composable predicates (callers / callees
  / refs) maps onto what programmers actually want to know.
- A single host index, queried interactively, is the right
  scope. Distributed code search wasn't even an idea yet.
- Per-line precision in the output is non-negotiable. The
  format records byte offsets.

What `cscope` left for successors:

- C only. No notion of "language plugin"; every supported
  language meant a custom parser inside the tool.
- No scope or type awareness. `transact` would return every
  `transact` regardless of which class.
- Re-indexing was full-corpus from scratch. Incremental updates
  came much later.

The two big architectural choices we've seen so far —
indexing/querying split, composable predicates over a custom
on-disk format — are *forced* by the problem. Tools that drop
either fail. Tools that keep them both are still in the lineage
that scry sits in.

### 1990s — Glimpse, agrep, and approximate matching

Manber & Wu's `agrep` (1991) introduced fast approximate
matching: edit-distance search over a corpus in `O(n)` rather
than `O(n*k)` for small `k`. Glimpse (1994) layered an inverted
index on top so the approximate scan only ran on *candidate*
files — exactly the trigram / scan two-step that everyone uses
now. The Glimpse paper ("GLIMPSE: A Tool to Search Through
Entire File Systems", Manber & Wu, Usenix 1994) is essentially a
preview of what livegrep and Zoekt would re-derive 20 years
later.

Two ideas Glimpse pioneered that scry uses verbatim:

- **Sound over-approximation as the design pattern**. The index
  returns a superset; the scan filters to the exact set. This is
  the same shape as a Bloom filter, but the architectural insight
  predates the Bloom-filter-as-a-tool-for-grep framing.
- **Multi-pattern indexing**. Glimpse indexed file blocks, not
  full files, to keep posting lists small for blocks that
  appeared in many files. The same kind of granularity choice
  shows up in modern systems (Zoekt's "shards", Sourcegraph's
  "subindexes").

(Ch. 4 picks up the trigram thread. Ch. 7 — Levenshtein
automaton — is the modern descendant of `agrep`'s approximate
matching.)

### Late 1990s — Excite, AltaVista, and the web-scale shock

The web search engines of the late 90s scaled the inverted index
to billions of documents. The shape stayed the same; the
engineering changed:

- Posting lists got *huge*, so compression mattered for the
  first time. The variable-byte (varint), Simple9, PForDelta
  families of integer encodings all came out of this era.
- Distribution became unavoidable. AltaVista famously had one
  giant machine; Google made the case that many small ones
  beat one big one (the cluster-as-computer thesis).
- Query latency budgets dropped to single-digit milliseconds at
  P99. This forced caching and warm-up strategies.

For code search this period contributed exactly two things:
(a) the realization that compressed postings were table-stakes
for any inverted index above ~10^6 documents, and (b) the
ranking-as-first-class-citizen mindset. PageRank doesn't
directly apply to code, but the *idea* that the index returns a
*ranked* set (not just an unordered match set) is what makes a
code-search UI actually usable. Every modern code search ranks;
scry's `rank_score` (`crates/scry-store/src/lib.rs:303`)
combines exactness, depth, and path penalties for the same
reason AltaVista ranked by term frequency.

### 2000s — OpenGrok and the first wave of web-shaped code search

OpenGrok (Sun Microsystems, 2007) was the first widely-deployed
web-shaped code search. Built on Lucene, it indexed many
languages syntactically (one tokenizer per language) and
exposed a browser UI. The architectural template is what
Sourcegraph and others picked up:

```
indexer (per-repo, per-language)
  → Lucene segments
    → web frontend over HTTP
```

OpenGrok's contribution wasn't algorithmic; it was social. It
proved that "code search across many repos, by many users, in a
browser" was a workflow people wanted, and that Lucene's
inverted-index machinery could be repurposed for it. The
limitations of Lucene for code (token boundaries are wrong for
identifiers; no n-gram indexing in the default tokenizer)
became the explicit problem that Zoekt and Sourcegraph would
solve.

### 2012 — Google Code Search and the trigram revolution

In 2006 Google launched Code Search, a public service for
searching open-source code. It worked. It got shut down in 2012
("Code Search" the product, not the technology). Russ Cox, who
worked on it, wrote up the design in a four-part essay series
that is still required reading for anyone in this field:

- [Part 1][cox1]: Brute Force — why grep over a tree is slow.
- [Part 2][cox2]: Thompson's 1968 NFA-to-DFA construction —
  the regex theory that any practical implementation rests on.
- [Part 3][cox3]: Implementation — building a regex engine
  whose worst case is bounded.
- [Part 4][cox4]: **Trigram indices** — the algorithm scry's
  ch. 4 derives from first principles.

[cox1]: https://swtch.com/~rsc/regexp/regexp1.html
[cox2]: https://swtch.com/~rsc/regexp/regexp2.html
[cox3]: https://swtch.com/~rsc/regexp/regexp3.html
[cox4]: https://swtch.com/~rsc/regexp/regexp4.html

The Cox 2012 essay codified what Glimpse had implemented:

- For a literal regex `R`, extract the trigrams that any
  matching file must contain.
- Intersect the posting lists; scan the candidates with the
  full regex.
- For regexes without literal anchors, fall back to full scan.

This is the design that livegrep, Zoekt, Hound, Sourcegraph, and
scry have all reimplemented, varying only in the literal-
extraction strategy and the on-disk encoding. (Ch. 5 picks up
the literal-extraction question; scry uses the `regex-syntax`
HIR walker, which is essentially the algorithm in part 4 of
Cox's essay.)

The deeper contribution: Cox 2012 was the moment "code search"
acquired its own canonical algorithm. Before, it was "Lucene
applied to code". After, it was "trigram-narrowed grep with
language-aware extraction". The field consolidated around this
pretty quickly.

### 2010s — Sourcegraph, Zoekt, Hound, livegrep

Four open-source-ish code search engines reached production in
this decade:

| year | tool       | what it added                                                    |
|------|------------|------------------------------------------------------------------|
| 2014 | livegrep   | regex-to-trigram with `\b`-boundary-aware extraction; web UI    |
| 2014 | Hound      | Etsy-internal; multi-repo aggregation across a fleet            |
| 2015 | Zoekt      | Sourcegraph-internal; sharded trigram index; very fast warm queries |
| 2016 | Sourcegraph (public) | Multi-tenant code search-as-a-service                  |

Architecturally they all share the trigram-pre-filter + scan
shape. The differences are operational:

- **Zoekt** is a single-process indexer + searcher. Each "shard"
  is one file group's index; searches fan out across shards in
  parallel; results are merged. The shard format is essentially
  trigram FST + posting lists + record sidecar — the same three
  components scry has. (The shape is genuinely forced once you
  pick trigram-narrowed grep; everyone arrives at the same
  three-part layout independently.)
- **Sourcegraph** wraps Zoekt with a web frontend, multi-repo
  routing, and authn/authz. The query path goes browser → API
  → Zoekt shards → result merge. The core code search is still
  Zoekt; the rest is web plumbing.
- **livegrep** focuses on *interactive* search — every keystroke
  triggers a new query. The trigram index has to support sub-
  100ms warm queries to feel live, which forces aggressive
  in-memory caching of postings.
- **Hound** is the simplest of the four; a single Go binary that
  re-shells `grep` against a candidate set. Useful as a
  reference implementation, not as a fast indexer.

The two things scry takes from this generation:

1. **Lazy mmap of the entire index** (Zoekt's "open shard ==
   mmap the file" pattern). Cold open of a Zoekt shard is one
   `mmap` and a manifest read; the rest is demand-paged. scry
   does the same.
2. **One process, one index** as a forcing function for
   simplicity. Zoekt's no-network-no-RPC default is what made it
   embeddable.

What this generation left for the next:

- **No symbol model.** Zoekt, livegrep, and Hound are all
  content-only. They can find a string; they can't find a
  definition. ctags-style "where is `Foo` defined" requires a
  separate index entirely. (This is half of what scry adds —
  the FST over symbol names from ch. 6.)
- **No cross-language references.** AIDL → Java/C++/Rust
  generated bindings are invisible to a trigram index;
  generated code lives in `out/` and is ignored, but the
  *reference* from a Java caller to the AIDL source isn't
  recoverable without semantic parsing.
- **Build-system blindness.** "Show me callers of `transact`
  only inside `frameworks/base/services`" requires
  understanding the Soong module graph, which content search
  doesn't see.

### Late 2010s — SCIP, LSIF, and the precision uplift

The Language Server Protocol (Microsoft, 2016) gave IDEs a
language-agnostic interface to per-language semantic engines:
`clangd`, `rust-analyzer`, `gopls`, `pylsp`, etc. The "semantic"
side of code intelligence consolidated around LSP.

LSIF (Language Server Index Format, 2019) was the first attempt
to *serialize* LSP responses to disk so a code-search system
could replay them. SCIP (Source Code Intelligence Protocol,
Sourcegraph, 2022) is the second-generation cleaner protobuf
form. The idea: run the real per-language indexer (`scip-clang`,
`scip-java`) once, emit a SCIP file, and load it as an *override*
for the heuristic resolution scry / Zoekt / etc. would otherwise
do.

The tradeoff is sharp:

- **Heuristic + tree-sitter** (scry's default): 80-90% accurate
  on the queries that matter, ~13 min for a 1 M-file index,
  zero per-language toolchain dependencies.
- **SCIP**: 99% accurate, requires the real per-language
  compiler to run (clangd needs `compile_commands.json`;
  scip-java needs a working JVM build), and the index emit
  takes longer than the actual build for most projects.

scry leaves SCIP as a phase-5 opt-in path (DESIGN.md §13). The
implementation is straightforward: a SCIP record wins over a
tree-sitter heuristic where they overlap; everything else stays
unchanged. We haven't shipped it because AOSP's build is large
and the SCIP indexers aren't currently configured for it.

### 2020s — Sourcegraph Code Intelligence, GitHub Code Search v2, embeddings

Three threads in the current decade:

1. **Github Code Search v2 (2021-23)** is a complete rebuild
   over a sparse n-gram index that's claimed to outperform
   trigrams for short queries. The technical writeup
   ("Github Code Search Architecture", 2023) suggests the
   underlying ideas are familiar: inverted index over content,
   with engineering tuning for the GitHub-scale corpus (200 M
   repos). The core algorithm is the same lineage.
2. **Embedding-based code search** (Sourcegraph Cody, GitHub
   Copilot retrieval, etc.) builds dense vector indexes over
   chunks of code and answers semantic queries ("how do I
   parse a TOML file in this codebase?") via nearest-neighbor
   search. This is genuinely orthogonal to trigram/FST
   indexing; the two are complementary. scry doesn't do it.
   The state of the art in 2026 has retrieval-augmented LLMs
   using *both* lexical (trigram-narrowed) and semantic
   (embedding-narrowed) retrieval, each catching what the
   other misses.
3. **Streaming/incremental indexers** (Sourcegraph zoekt-mirror,
   GitHub's per-commit re-index). Per-commit updates are
   tractable when the index format supports tombstones and
   compaction (LSM-tree shape rather than the fully-sorted-FST
   shape scry uses). scry's rebuild-per-hour cadence sidesteps
   this.

### 2026 — where scry fits

scry is **2010s lineage** (trigram-narrowed grep, mmap'd shards,
language-aware tokenization) **with a 2020s LLM-friendly
interface layer** (stable symbol IDs, JSON-RPC, stats footer
per query for token accounting). The core algorithms are 2014-
2016 Zoekt-shape; the things that make scry *new* are:

- **AOSP-specific coverage.** aconfig flags, init.rc services,
  SELinux types, AIDL interfaces with cross-language linking,
  Soong module graph as a first-class filter. None of the
  general code search tools index these; an AOSP engineer
  asking "who reads this aconfig flag" can't get an answer
  from Sourcegraph today.
- **LLM-shaped output.** Every result has a stable ID, a scope
  path, a snippet, a path. Every query emits stats so an
  agent can budget tokens. The JSON-RPC server reads one
  request per line and writes one response per line — designed
  to be driven from a tool-using LLM loop, not a browser.
- **Single host, one user, opinionated defaults.** Sourcegraph
  ships for the multi-tenant case; scry ships for the
  "one engineer on one Skylake host" case, and the design
  drops everything the multi-tenant case requires (auth,
  sharding, replication, network). The drop simplifies
  enough that the codebase is ~7500 lines.

### What history tells you about scry's tradeoffs

Reading the 60-year arc gives a hard test for any new
decision: **has someone in this lineage tried this before, and
what happened?** The answers for scry's main decisions:

| scry decision                | precedent                       | outcome of precedent              |
|------------------------------|----------------------------------|------------------------------------|
| trigram-narrowed grep        | Glimpse 1994, Cox 2012, Zoekt 2015 | standard pattern; works           |
| FST for symbol names         | `fst` crate (Gallant), rooted in Daciuk 2000 | works; production-grade       |
| mmap'd on-disk format        | Zoekt 2015, Lucene segments      | works; standard                    |
| heuristic resolution + opt-in SCIP | Sourcegraph default, kythe at Google | works; SCIP is the precision-when-you-need-it lever |
| single host, no daemon       | ctags 1979, cscope 1979, livegrep 2014 (mostly) | works for one user, fails at scale |
| LLM-shaped JSON-RPC          | new — no precedent              | unknown but designed conservatively |

The first four entries are "stand on the shoulders of giants".
The fifth is "the small-scale design is forced by the one-user
assumption". The sixth is the only genuinely new thing scry
ships, and it's a thin interface layer over a well-trodden
core.

The lesson: when you build a system in a 60-year-old field, the
*algorithms* are mostly settled and the *engineering decisions*
are mostly forced. The novelty, if any, is in **what you
choose to index and what shape you serve the answer in**. Scry
indexes AOSP idioms that nobody else covers and serves answers
LLM-shaped. The rest is the same trigram-narrowed inverted-
index shape Glimpse shipped in 1994.

---

## Chapter 2 — the memory hierarchy and the external-memory model

### 2.1 The thing every undergrad RAM model gets wrong

Introductory algorithms classes use the **RAM model**: every
memory access is unit cost; locality doesn't matter; sequential
and random reads are equally fast. This is wrong by five orders of
magnitude on real hardware. The production host has:

```
tier            size           latency       throughput
L1d             32 KiB         1 ns          ~1 TB/s
L2              256 KiB        3 ns          ~500 GB/s
L3 (shared)     36 MiB         12 ns         ~200 GB/s
DRAM            240 GiB        80 ns         ~30 GB/s
page cache      (=DRAM)        80 ns         (same)
NVMe disk       2 TiB          80 µs        ~3 GB/s sequential
                                              ~250 MB/s random 4 KiB
```

The latency gap between DRAM and disk is 1000×. The throughput gap
between sequential and 4 KiB-random disk reads is 10×. Every
algorithm in scry has to be designed against these numbers,
not against "memory access is constant time".

### 2.2 The external-memory model (Aggarwal-Vitter)

Aggarwal and Vitter (1988, *"The Input/Output Complexity of
Sorting and Related Problems"*) introduced the **external-memory
model** to formalize this:

- Memory is divided into two tiers: *fast* (size `M`) and *slow*
  (effectively unbounded). The slow tier is divided into *blocks*
  of size `B`.
- The CPU can compute freely on data in fast memory.
- Moving one block between tiers costs 1.
- Algorithm cost = number of block transfers.

A simple example: sorting `N` elements. In the RAM model, optimal
sort is `O(N log N)` comparisons. In the EM model, optimal sort is
`O((N/B) log_{M/B}(N/B))` block transfers, achieved by external
merge sort. The latter dominates the former for `N` larger than
fast memory — *every* well-engineered sort over data that doesn't
fit in RAM is some flavor of EM merge sort, including the one
inside the database you used last week.

For scry, the model applies twice: between **DRAM and NVMe** (the
big jump), and between **L3 cache and DRAM** (the smaller one,
relevant to inner loops). The block sizes are 4 KiB and 64 bytes
respectively. An algorithm that's not optimal in this model
either:

- Issues too many random disk reads (NVMe's 4 KiB random IOPS is
  the bottleneck — 250 MB/s = ~60 k reads/s).
- Has poor cache line utilization (reading 8 bytes when the line
  is 64 means we're wasting 87% of every DRAM fetch).

Both are easy to do by accident. Both show up in `perf stat` as
high cache-miss rate or high `iowait`.

### 2.3 Three EM-optimal patterns scry uses

There are three classical EM patterns and scry uses all three:

1. **B-tree-shaped descent** — height `O(log_B N)`. Each level
   reads one block, so a lookup costs `~log_B N` block transfers.
   For `N = 22 M` symbols and `B = 4 KiB`, this is ~3 transfers,
   meaning ~240 µs to find any symbol on cold cache.
   The FST (ch. 6) realizes this shape.
2. **Sorted run + offset index** — one block transfer to the
   offset, one to the record. Sized at 2 transfers per lookup
   regardless of `N`. The byte-offset sidecar (ch. 8) is exactly
   this.
3. **Inverted index intersection** — the cost is dominated by
   the smallest posting list's block count. The intersection of
   `k` sorted lists in `N_1, N_2, ..., N_k` blocks costs
   `O(min(N_i))` block transfers, which is the information-
   theoretic minimum (you can't beat reading at least the
   smallest input). The trigram index (ch. 4) is this.

### 2.4 What it would look like to pick wrong

Counterexample 1: store the symbol table as a `Vec<SymbolRecord>`
serialized with bincode and load it with `deserialize::<Vec<_>>`.
Every lookup forces reading `N · sizeof(record)` ≈ 4 GB of disk,
then allocating a 4 GB Vec, then doing the lookup. The naïve
choice costs `O(N/B)` transfers per lookup; the EM-optimal choice
costs `O(1)` transfers. The naïve choice is what scry shipped in
v1 (before the lazy sidecar); cold open was ~4 seconds.

Counterexample 2: store grep as `for file in files {
mmap_and_scan(file) }`. The pattern is *random* reads across
files: each file's first page is a random 4 KiB read. NVMe random-
4 KiB read is ~60 k IOPS; 1 M files takes ~17 s in random IO
alone, before any scanning happens. The trigram index narrows to
~1400 files; the random-IO floor drops to ~25 ms.

### 2.5 Tradeoffs

The EM model is a *lower bound on transfers*. It says nothing
about constants, CPU time, or development cost. An algorithm that
issues 1.5× the optimal block transfers but uses 100× less code
is often the right pick.

Example: the trigram index has a perfectly EM-optimal alternative
(suffix array on the concatenated corpus). Suffix arrays give
substring search in `O(|q| log N)` block transfers and don't
require pattern decomposition. We rejected them because (a) the
suffix array for 70 GB of source is ~280 GB on disk, (b) building
it is itself an EM-bounded operation that takes hours, and (c) the
trigram index gets us within 2× of optimal for the queries we
actually run. The "perfect" algorithm is wrong here.

### 2.6 Open questions

- **The right metric for code-search latency** is genuinely
  unsettled. Production query distributions are heavy-tailed: a
  few common-substring queries (`void`, `error`) cost as much as
  thousands of selective queries. Optimizing for median latency
  vs P99 vs total throughput gives different designs. scry
  optimizes for selective queries because the LLM workload skews
  that way; a human-only tool might pick differently.

---

## Chapter 3 — virtual memory, mmap, and the page cache

### 3.1 The kernel as a buffer manager

Every process has its own virtual address space; the MMU maps
virtual pages to physical pages on the fly. The kernel's **page
cache** is the unified pool of physical pages used for all file
data the kernel has touched recently, evicted by LRU under memory
pressure, shared across all processes that map the same file.

When you `read(2)` from a file, the bytes are copied through the
page cache into your user buffer. When you `mmap(2)` a file, the
kernel maps the file's pages *directly* into your address space:
the bytes you see at `mmap_ptr[offset]` are the same physical
pages the page cache holds. No copy. Reading `mmap_ptr[offset]`
the first time triggers a page fault that the kernel resolves by
reading the page from disk; subsequent reads of the same page are
free (already mapped, no syscall, no copy).

The consequences for an index:

- A 9.5 GB index with `mmap` has a 9.5 GB *virtual* footprint and
  a *physical* footprint equal only to the pages actually touched
  by queries.
- Cold reads pay one page fault per touched page (~80 µs each on
  NVMe). Warm reads are L1/L2/L3 cache hits — single nanoseconds.
- Two `scry` processes share the same page cache pages
  automatically.
- LRU eviction is global: the kernel will reclaim pages from idle
  files first.

The page cache *is* scry's buffer pool. We never wrote one.

### 3.2 What you give up vs an in-memory structure

`mmap` is not free:

- The first access to a page is a hard fault; cold latency is
  bounded by disk seek time.
- Writes through `mmap` are tricky (page-aligned, dirty tracking,
  flushing). scry sidesteps this by being read-mostly: writes use
  `BufWriter<File>` and only readers `mmap`.
- The mapped region counts against the process's virtual address
  space (irrelevant on 64-bit, but not on 32-bit hosts).
- Random access patterns within an `mmap` can defeat the
  prefetcher; the kernel's readahead algorithm only kicks in for
  sequential streams. We work around this with explicit
  `posix_fadvise(WILLNEED)` for the grep candidate scan.

### 3.3 Cache-oblivious algorithms

Frigo, Leiserson, Prokop, and Ramachandran (1999, *"Cache-Oblivious
Algorithms"*) proved that an algorithm with the right recursive
structure can achieve EM-optimality at *every* block size
simultaneously — *without ever knowing the block size or memory
size*. The standard examples are recursive matrix multiply (van
Emde Boas layout) and funnelsort.

Why this matters for scry: the algorithm doesn't need to know
whether it's running against L1, L2, L3, or NVMe. The page cache
will work it out. By picking layouts that have good asymptotic
behavior at every B (sequential record streams, sorted offset
arrays, FST byte arrays), we automatically get the right cache
behavior at every tier.

We don't do anything explicitly cache-oblivious in our own code;
we just *inherit* the property by laying out files in the obvious
sequential way and letting the kernel manage everything.

### 3.4 Working sets and thrashing

Denning's 1968 working-set model says a program's wall-time
performance is governed by `W(t, τ)` — the set of distinct pages
referenced in the last `τ` time units — and the available physical
memory. When `|W| < memory`, page faults are rare; when `|W| >
memory`, every reference can fault and performance collapses
non-linearly. The transition is sharp.

This is the *real* reason the trigram pre-filter is load-bearing:

- Naïve grep: working set = the whole corpus, 70 GB on a 240 GB
  host (large but fits) — except the page cache is *shared* with
  every other workload, so in practice the index pages get evicted
  between queries and every cold query pays disk price.
- Trigram-narrowed grep: working set per query = trigram FST +
  postings for the query's trigrams + the candidate files. ~50 MB
  per query, easily resident across many queries, no thrashing.

The trigram index gives us a 100× speedup on paper. The
working-set argument tells us we keep that speedup in practice
because we no longer compete with other workloads for the page
cache.

### 3.5 What scry does (concrete)

`crates/scry-store/src/lib.rs::safe_mmap` is the single audited
entry point for all `mmap` calls. The other crates declare
`#![forbid(unsafe_code)]` and route through it. The reader's
`StoreReader::open()` `mmap`s symbols.bin, refs.bin, the offset
sidecars, the FST byte arrays, the trigram FST, and the trigram
postings — all read-only, all eagerly mapped, none eagerly faulted.

Cold open of the reader = `O(number of files)` syscalls (just the
`mmap` calls, no IO); first query against the FST pays `~log_B N`
faults to walk down to the relevant FST node. Subsequent queries
that touch the same FST node pay nothing.

### 3.6 Tradeoffs

The page cache is opaque. We can't say "evict everything except
the trigram FST" — the kernel decides. Under memory pressure from
unrelated workloads, the index can get evicted and the next query
pays cold-cache latency. This is almost always fine; in a
multi-tenant environment we might want explicit `mlock(2)` on the
hot pages, at the cost of giving up the *graceful degradation* the
kernel currently provides.

### 3.7 Open questions

- **DAX (direct access)** on byte-addressable persistent memory
  (Intel Optane, until it was discontinued; possibly NVDIMM-N in
  some setups) lets `mmap` skip the page cache entirely — the
  mapped pages live in the persistent medium and the CPU loads
  directly. scry's design would inherit this with zero changes;
  whether it's worth deploying on is unsettled because the medium
  itself isn't widely available.
- **io_uring** changes the IO model fundamentally — submission
  queues, registered buffers, kernel-side polling. The published
  numbers (Axboe, 2019) suggest 1.5-2× throughput improvements
  for random-IO-heavy workloads. scry hasn't migrated; the
  speedup over `mmap + read` is real but not transformative on
  this workload.

---

## Chapter 4 — trigram inverted indices

### 4.1 The setup

We want: given a literal string `P`, return all files containing
`P`, faster than scanning every file.

The data structure: for each 3-byte sequence `t`, store
`posting(t) = sorted list of file IDs that contain t somewhere`.

This is an **inverted index** (van Rijsbergen, 1979) restricted to
*n-grams* (overlapping byte windows) rather than *words*. Word-
based inverted indices are what search engines use for natural
language; n-gram indices are what code search uses because code
doesn't have clean word boundaries (`ZygoteInit` is one identifier;
splitting on case boundaries is a lossy heuristic).

### 4.2 The fundamental identity

For `|P| ≥ 3`, define `T(P) = { P[i..i+3] : 0 ≤ i ≤ |P|-3 }`.

```
candidates(P) = ⋂  posting(t)
              t∈T(P)
```

This is a **sound over-approximation**: every file that contains
`P` is in `candidates(P)`, but some files in `candidates(P)` don't
actually contain `P` (they have all the trigrams in the wrong
order, or in pieces that don't concatenate). The exact filter is
a `memchr` scan of each candidate.

Why sound? If file `f` contains `P` then `f` contains every
trigram of `P` (they all live within `P`'s bytes inside `f`).
Therefore `f ∈ posting(t)` for every `t ∈ T(P)`. Therefore `f ∈ ⋂
posting(t)`. The intersection is a superset of the true match
set. The converse fails — `f` could contain `Zyg`, `ygo`, ...,
`nit` in completely unrelated places — which is why we scan.

### 4.3 Choosing n

`n = 3` is conventional. The choice is a discrete Pareto frontier
between:

| problem with too-small n | problem with too-large n |
|--------------------------|--------------------------|
| Posting lists are huge   | Dictionary explodes      |
| Intersections still big  | Short queries can't index |
| Index is no better than scan | Storage cost dominates |

The arithmetic:

- `n = 1`: dictionary = 256 keys, every posting is ~all files.
  Useless.
- `n = 2`: dictionary = 65 k keys; common bigrams (`er`, `in`,
  `()`) cover > 50% of files; intersection rarely narrows.
- `n = 3`: dictionary ≤ 16.7 M keys, in practice ~3 M on real
  source; selective patterns narrow to << 1% of files; any literal
  ≥ 3 bytes can be indexed.
- `n = 4`: dictionary ≤ 4.3 B keys, in practice ~30 M; postings
  shrink more but the dictionary FST grows to GBs; 3-byte queries
  can't use the index.

Russ Cox's 2012 essay [*Regular Expression Matching with a Trigram
Index*][cox] picks `n = 3` for exactly these reasons; Zoekt,
livegrep, Hound all converged on the same choice.

[cox]: https://swtch.com/~rsc/regexp/regexp4.html

### 4.4 Encoding posting lists

A posting list is a sorted, strictly-increasing `Vec<u32>` of file
IDs. Two compressions, both standard:

1. **Delta encoding**: store `d_i = id_i - id_{i-1}` instead of
   `id_i`. The deltas are small for trigrams that appear in many
   adjacent files (alphabetical sort of paths means similar
   paths are nearby).
2. **Varint (LEB128)**: each delta uses `⌈log₂(d_i + 1) / 7⌉`
   bytes. Bytes ≥ 128 mean "continued"; bytes < 128 mean "end".
   Encoding of small integers is 1 byte, large is 5 (for u32).

Combined, postings shrink from `8N` bytes (raw u64) to roughly
`1.5N` bytes on the production corpus — a ~5× shrink that gets
the on-disk trigram payload from ~7 GB down to ~3 GB. See
`crates/scry-store/src/lib.rs::read_trigram_posting` for the
decoder (15 lines of varint + delta-undelta).

### 4.5 Intersection algorithm

The k-way merge intersection is the classical algorithm — given
`k` sorted iterators, sweep them in lockstep:

```
sort iterators by ascending current head;
loop:
  m = min of current heads;
  if all heads == m: emit m; advance all;
  else: advance any iterator whose head < m;
```

Cost: `O(Σ |posting_i|)` block transfers in the EM model. The
constant matters: with `k` iterators in a binary heap, each step
is `O(log k)` comparisons; with `k` linear iterators, each step
is `O(k)`. For trigram intersection `k ≤ |P| - 2`, so for
`|P| ≤ 32` either approach is fine. scry uses the linear sweep
because the typical query has ≤ 20 trigrams.

The crucial micro-optimization: **sort iterators by posting length
ascending before starting**. The intersection size is bounded by
the smallest posting, so processing the smallest first prunes the
working set the fastest. Processing the largest first means
carrying a 1 M-entry running intersection through the inner loop
unnecessarily.

### 4.6 What scry actually does

`crates/scry-store/src/trigram.rs::extract_sorted` walks a file's
bytes and returns the deduplicated, sorted vec of trigrams.
`crates/scry-store/src/trigram.rs::trigrams_of_query` does the
same for a query needle. The intersection happens in
`crates/scry-cli/src/main.rs::grep_candidates_for_regex` (see ch. 5
for how regex queries feed into this).

Per-file extraction uses a `HashSet<[u8; 3]>` rather than `Vec`
because the per-file dedup matters — `"abababab"` has 2 unique
trigrams, not 7. NUL bytes are filtered (`if t[0] != 0 && ...`)
because a NUL is a strong signal of binary content that slipped
past the walker's classification.

### 4.7 Tradeoffs

- **No regex bytes in the index.** We only index *what files
  contain what 3-byte sequences*, not "what regex matches what
  files". The price: regex queries have to pass through the
  literal-extraction step (ch. 5) to get the candidate set; some
  regexes (`[a-z]+`) have no literal anchors and fall back to
  full scan.
- **No position info per posting.** A posting says "file 42 has
  trigram `Zyg` somewhere", not "at byte 12345". Storing
  positions would let us skip the `memchr` scan, but blows up the
  index by ~10× (positions are u32 per occurrence, not per
  file). The `memchr` scan over 1400 files is fast enough that
  the tradeoff favors the smaller index.
- **No incremental update.** If a file changes, every posting
  list it appears in needs editing. scry rebuilds. An online
  algorithm (B-tree-shaped postings + tombstones) exists in the
  literature (Lester et al., 2005) and would be necessary if
  scry ever wanted per-commit refresh.

### 4.8 Open questions

- **Bigram + trigram hybrid.** Lin and Yan (2016) show that
  storing bigrams in addition to trigrams cuts intersection cost
  on 5+ character patterns by combining bigrams with their
  preceding/following bigrams, at ~30% extra storage cost. scry
  hasn't tried it; the win on our query mix is probably <2×.
- **Field-aware n-grams.** Indexing trigrams separately for
  identifiers vs string literals vs comments would let queries
  scope to "only in identifiers". The Zoekt design supports this;
  scry doesn't. The win for code search is real but the storage
  doubles.

---

## Chapter 5 — from regex to trigrams (literal extraction)

### 5.1 The problem

`scry grep` accepts regex (`scry grep "TODO\(.*\):"`). The trigram
index only indexes literals. How do we use the index for a regex
query?

### 5.2 The Cox/livegrep insight

Walk the regex's syntax tree (HIR — high-level intermediate
representation, in `regex-syntax` crate terms) and extract the
maximal literal substrings that *every* match must contain. For
the regex `TODO\(.*\):`:

- Maximal prefix literal: `TODO(`
- Maximal suffix literal: `):`

A match must start with `TODO(` and end with `):` (with arbitrary
bytes in between). Therefore any matching file must contain both
literals — somewhere. Trigrammify each, AND-intersect, scan.

For `ActivityMgr.*Service`:

- Prefix: `ActivityMgr`
- Suffix: `Service`

Same pattern.

For an empty case like `[a-z]+`:

- No literal anchors exist. The extractor returns empty, and the
  query falls back to full scan over the lang/in-filtered files.

The general algorithm is in Russ Cox's [regexp4 essay][cox]
(linked above). The key invariants are:

1. **Soundness**: every literal returned must be required by every
   match. If you return a literal that *isn't* required (e.g.,
   one branch of an alternation), you'll miss matches.
2. **Completeness is optional**: returning *no* literals when
   anchors exist makes the query slow but not wrong. Returning
   *too many* literals is unsound — if a regex matches files
   without `bar`, you can't require `bar` in the index lookup.

For complex regexes (alternations, character classes, lazy
quantifiers) the extractor has to walk the HIR carefully. scry's
implementation in
`crates/scry-cli/src/main.rs::regex_literals_for_trigram` handles
the common cases:

- Literal patterns → return the whole literal.
- Concat of `Literal` and `Repeat(Any)` → return the literal
  prefix (and similarly for suffix).
- `Repeat(Literal)` → return the literal.
- Alternation → return only literals required by *every* branch
  (intersection of per-branch literal sets).
- Character classes, lookarounds, anchors → return nothing for
  that node; fall back to scan if the entire regex has no usable
  literals.

### 5.3 What scry actually does

The seven unit tests on `regex_literals_for_trigram` cover the
adversarial corners:

| test                               | what it pins down                       |
|------------------------------------|------------------------------------------|
| literal anchor                     | `TODO(.*):` → `["TODO(", "):"]`         |
| prefix-only                        | `foo.*` → `["foo"]`                     |
| suffix-only                        | `.*foo` → `["foo"]`                     |
| no-literal (correct fallback)      | `[a-z]+` → `[]`                         |
| nested alternation                 | `(foo|bar)x` → `["x"]` (only x is required) |
| character class without literals   | `[A-Z][a-z]+` → `[]`                    |
| empty pattern                      | `""` → `[]`                             |

The "correct fallback" is the load-bearing one. A buggy extractor
that returns `["foo"]` for `(foo|bar)x` is *unsound* — files
matching `barx` would be excluded from the candidate set, and the
query would silently miss results. The test exists because earlier
drafts had exactly that bug.

### 5.4 Tradeoffs

- **We don't try to extract from character classes.** `[a-z]+`
  has no usable literals; `[Ff]oo` could in principle be turned
  into "files containing `foo` or `Foo`" (union of postings, not
  intersection), but our extractor returns nothing. The win is
  case-sensitive regex on small classes; the cost is more HIR
  walking and the risk of unsound merging. The tradeoff favors
  staying simple.
- **No memoization.** Each regex is extracted fresh per query.
  Memoization wouldn't help meaningfully because extracted-
  literals strings are small (kilobytes) and queries rarely
  repeat verbatim.

### 5.5 Open questions

- **Optimal literal extraction is NP-hard** in the general case
  (decomposing into the *smallest* literal set whose intersection
  is the candidate superset). Cox 2012 notes this and uses a
  greedy heuristic; nothing in the literature has moved past
  greedy. There may be wins for adversarial patterns but they're
  bounded by the gap between greedy and optimal, which is small
  in practice.

---

## Chapter 6 — finite automata and the FST

### 6.1 The problem

Given 22 M symbol names, build a data structure that supports:

- Exact lookup: "does `ActivityManagerService` exist?" in O(|key|).
- Prefix walk: "every symbol starting with `Acti`" in time
  proportional to the result set, not the dictionary.
- Fuzzy match: "every symbol within edit distance 2 of `Pacelfile`"
  in time proportional to the result set.

And, in scry's case, the structure has to fit in < 1 GB on disk
and < 300 MB resident.

### 6.2 The wrong choices

| structure          | exact   | prefix       | fuzzy     | size (22M keys) |
|--------------------|---------|--------------|-----------|------------------|
| `HashMap<String, _>` | O(1)  | impossible   | impossible | ~3 GB resident  |
| sorted `Vec<&str>` | O(log N) | O(log N + k) | impossible | ~1.5 GB         |
| trie (uncompressed) | O(|key|) | O(|prefix| + k) | (with patches) ~ | ~10 GB |
| B-tree (LMDB)      | O(log_B N) | O(log_B N + k) | impossible | ~2 GB |
| **minimized FST**  | **O(|key|)** | **O(|prefix| + k)** | **O(|q|·k·|out|)** | **~280 MB** |

`k` here is the number of results. The FST wins on every axis at
once *because* of minimization — the structure compresses suffixes
that are shared across unrelated keys.

### 6.3 What an FST is

A **finite-state transducer** (FST) is a finite-state automaton
where each transition is labeled with both an input symbol and an
output value. For our purposes, the input is a byte of a key, and
the output is the value associated with that key (we use it as a
"set", so the output is just "accept/reject"). The crate is
[`fst`][fst-crate] by Andrew Gallant, the same author as `ripgrep`.

[fst-crate]: https://docs.rs/fst/

```
                 (a)            (c)            (t)
   start ─────► s1 ─────► s2 ─────► s3 ─────► (accept "act")
                  \
                   \      (n)         (d)
                    └───► s4 ─────► (accept "and")
```

Reading "act": start → s1 → s2 → s3 → accept. Reading "ant":
start → s1 → s4 → reject (no `t` transition from s4 after seeing
`an`). Each transition is a single byte read.

The trick that makes FSTs small is **minimization**: any two
states whose entire forward language is identical get merged. A
trie containing {`Activity`, `Service`, `Provider`} has three
disjoint chains; a minimized FST containing the same plus
{`ServiceConnection`, `ActivityManager`, ...} fuses the shared
`...Manager`, `...Connection` suffixes into single chains
referenced from multiple parents.

### 6.4 Myhill-Nerode and why minimization is optimal

The Myhill-Nerode theorem (1958) says: for any regular language
`L`, the minimum DFA recognizing `L` has exactly as many states
as Myhill-Nerode equivalence classes of `Σ*` under `L`. Two
prefixes `x` and `y` are equivalent if `∀z: xz ∈ L ⇔ yz ∈ L`.

The theorem has two consequences:

1. The minimum DFA is **unique** (up to renaming states).
2. Hopcroft's algorithm (1971) constructs it in `O(n log n)`.

For our use, this means: the FST the `fst` crate builds is
provably optimal in state count. We can't make it smaller without
giving up the language. Suffix sharing across unrelated keys
(`...Manager` after `Activity` and after `Window`) is exactly the
Myhill-Nerode classes merging.

The empirical effect: a trie over 22 M symbol names would take
~3-10 GB (each key is its own chain of nodes). The minimized FST
takes ~280 MB. Most of the savings come from suffix sharing across
the long tail of `*Manager`, `*Service`, `*Connection`, `*Factory`,
`*Builder`, `*Listener` identifiers that pervade Java/Kotlin/C++
codebases.

### 6.5 Construction and the sorted-input requirement

The `fst` crate requires keys to be inserted in **sorted byte
order**. This isn't an arbitrary limitation — it's what lets the
builder use bounded memory and write streamingly. The
construction algorithm (Daciuk, Mihov, Watson, Watson, 2000) walks
the input keys in order, comparing each to the previous one to
find the shared prefix, then *finalizing* the diverging suffix of
the previous key (compressing it and emitting it to disk).

In scry, this means: the symbol-name pipeline has to produce a
sorted stream. The path:

1. Parsing produces `(name, symbol_id)` tuples per chunk,
   unsorted.
2. Each chunk gets sorted in memory and written to a chunk file.
3. Finalize does a **k-way merge** over chunk files (binary heap
   keyed on the next unread name from each chunk) and feeds the
   merged stream into `fst::SetBuilder` / `fst::MapBuilder`.

See `crates/scry-store/src/lib.rs::kway_merge_names_to_fst` for
the implementation. The k-way merge is itself EM-optimal: each
input block is read sequentially, the output is written
sequentially, the heap operations are in fast memory. This is the
standard external merge sort pattern.

### 6.6 Why prefix walk is free

Once you've walked the input prefix `p` into some state `s`, the
set of completions is exactly the set of strings accepted starting
from `s`. BFS from `s` enumerates them in time proportional to
the *output*, not the dictionary size. This is the structural
reason `scry prefix Acti` stays sub-millisecond regardless of how
big the dictionary grows.

### 6.7 What scry actually does

`crates/scry-store/src/lib.rs` builds three FSTs at finalize:

- `names.fst`: symbol name → list of symbol IDs (a `Map` whose
  value is an offset into a sidecar postings file when multiple
  symbols share a name).
- `refnames.fst`: reference name → list of reference IDs.
- `trigrams.fst`: trigram → offset into trigram postings.

All three use the same k-way merge machinery. All three are
`mmap`'d at read time. The FST keys live on disk in their
canonical byte order; lookup walks the byte representation
directly without ever materializing a deserialized data structure.

### 6.8 Tradeoffs

- **No in-place updates.** Adding a new symbol means rebuilding.
  Online FST construction exists (Daciuk 1998) but is more
  complex and would still need to flush periodically. scry's
  whole-corpus rebuild is fast enough that this hasn't been
  worth the complexity.
- **Strings only.** FSTs are great for keys with shared
  structure (suffix-shared identifiers). Random byte strings
  compress less well; the FST representation degrades toward
  trie behavior. For symbol names this is a perfect fit; for
  arbitrary content it wouldn't be.

### 6.9 Open questions

- **Lossy FSTs.** Storing a *probabilistic* FST that can have
  false positives (like a Bloom filter for set membership) would
  let us shrink further. The literature (Belazzougui et al.,
  2011) has constructions; scry hasn't tried any.
- **Compressed-domain operations.** Doing prefix walks directly
  on the compressed representation, without decompression, is a
  research direction. `fst` does this; the question is whether
  more aggressive compression schemes (FSA + Huffman on labels)
  pay off.

---

## Chapter 7 — fuzzy match as automaton intersection

### 7.1 The construction

Given a query `q` and an edit distance `k`, the **Levenshtein
automaton** `L_k(q)` is a DFA that accepts exactly the strings
within edit distance `k` of `q`. Schulz and Mihov (2002) gave a
direct construction that produces the automaton in `O(|q|)` time
and space.

Regular languages are closed under intersection, and the
intersection automaton has at most `|A| × |B|` states. The
intersection of `L_k(q)` with the symbol FST is itself an FST
whose accepted language is exactly *the set of symbols within
edit distance `k` of `q`*.

Walk that intersection FST → emit accepted keys. Cost is
`O(|q| · k · |output|)`: linear in the query, linear in the edit
distance, linear in the number of results. Critically: keys not
within distance `k` *never get visited* — wrong branches die at
the automaton level before contributing to runtime.

### 7.2 Why this is better than the naïve "compute edit distance to every key"

Naïve: iterate all 22 M keys, compute Levenshtein distance, keep
the ones ≤ k. Cost is `O(N · |q| · k)` — ~400 ms even at modest
constants, before any IO.

Automaton: only visit FST nodes that are still "alive" relative to
some path in `L_k(q)`. The branching factor of the FST is bounded;
the live frontier in the intersection collapses quickly for any
realistic `q`. Result: 150-250 ms on the production corpus for
`scry fuzzy ParcelFile --limit 10`, which is the FST walk time,
not the keys-considered time.

### 7.3 What scry actually does

The `fst` crate has `Automaton` trait implementations for
`Levenshtein`, `Subsequence`, and other useful queries.
`StoreReader::fuzzy_symbols` runs the Levenshtein automaton over
the names FST and returns matches. The substring fallback
(`Subsequence`) runs when the user explicitly wants a substring
match rather than edit-distance.

### 7.4 Tradeoffs

- **Cost grows quickly in k.** `k = 1` is fast; `k = 3` is
  noticeably slower because the live frontier expands. scry
  defaults to a small `k` and lets the user override.
- **No ranking by edit distance.** The current implementation
  returns matches in FST traversal order, not sorted by edit
  distance. Adding a ranking pass over the candidates costs an
  extra Levenshtein computation per result; we haven't measured
  the latency hit.

### 7.5 Open questions

- **Approximate substring with bounded edit distance.** Combining
  Levenshtein with subsequence (substring of distance k) is
  open; the cleanest construction we know is to build a different
  automaton per (substring length, edit distance) pair, which
  doesn't compose well.

---

## Chapter 8 — columnar layout + byte-offset sidecars

### 8.1 The problem

22 M symbol records, ~50-200 bytes each. The user query "give me
symbol #4,318,927" must not require reading the previous 4 M
records or allocating a 10 GB `Vec`.

### 8.2 The naïve approach and what it costs

The naïve bincode pattern is
`deserialize::<Vec<SymbolRecord>>(&bytes)`:

- Allocates `Vec<SymbolRecord>` of length 22 M → ~4 GB resident.
- Walks every byte of the input → 4 GB read.
- Wall time: ~400 ms warm, ~4 s cold.

This is what scry v1 shipped. Removing it was a 10-30× cold-query
speedup with no algorithmic change — just a layout fix.

### 8.3 The byte-offset sidecar

For a sequence of variable-length records, store the records
themselves in one file (concatenated, no length prefixes needed if
the decoder is self-delimiting) and the *byte offsets* of each
record in a parallel file.

```
symbols.bin:     [rec0][rec1][rec2]...[recN-1]
symbols.offs:    [off0=0][off1][off2]...[offN-1] (u64 LE, fixed-width)
```

To look up record `i`:

1. Read `off_i = u64::from_le_bytes(&offs[8i..8(i+1)])`. One
   memory access into the offset mmap. Kernel demand-pages the
   offsets page once; thereafter it's an L1/L2 hit.
2. Decode `&records_mmap[off_i..]`. Bincode reads exactly one
   record's bytes. Kernel demand-pages the record's page.

Per-lookup cost: 2 page faults cold, ~10 µs warm. Compared with
the naïve approach: ~30 000× speedup warm, ~400 000× cold.

The sidecar costs `8 · N` bytes total — 150 MB for 22 M records,
1.5% of the records file. The win is overwhelming.

### 8.4 Why this is more than "lazy loading"

You might ask: "couldn't we just lazily decode the Vec on demand?"
The answer is no — the obvious scheme ("scan forward through the
length-prefixed records until you've skipped i-1 of them") is
`O(i)` per lookup. The byte-offset array makes the location
*O(1)*; the records file stays sequentially layed out (good for
prefetch when you do read multiple); the page cache handles all
the caching.

It's also worth saying what this *isn't*: it's not a B-tree, not
an LSM tree, not a skiplist. It's a flat sorted file with a
parallel index. The same construction underpins:

- SSTable (Bigtable, LevelDB, RocksDB) — sorted strings + sparse
  block index.
- Filesystem extents — sorted blocks + extent tree.
- Parquet / ORC / Arrow IPC — columnar files + page footer with
  per-page offsets.

Every large columnar format does some flavor of this, for the
same reason: it's the simplest construction that gives O(1)
record fetch with O(N) storage and zero buffer-management code.

### 8.5 What scry actually does

The writer in `crates/scry-store/src/lib.rs::StoreWriter` keeps a
`current_offset` counter while concatenating records to
`symbols.bin`; before each record it appends the current offset
to `symbols.offs`. The reader uses `LazyVec<T>`
(`crates/scry-store/src/lib.rs:92`) which mmaps both files and
implements `get(i)` via the two-step lookup above.

Same construction for `refs.bin / refs.offs`, `file_symbols.bin /
file_symbols.offs`, and `ref_resolutions.bin / ref_resolutions.offs`.

### 8.6 Tradeoffs

- **Records can't change size without rebuilding.** An in-place
  update to a record might exceed the slot's bytes; the simple
  approach is to copy-on-write to a new file with new offsets.
  We rebuild, so this never comes up.
- **No record-level compression.** Each record is stored
  uncompressed. Page-level compression (zstd over 64 KiB blocks)
  is a standard option for SSTable formats; we haven't found it
  necessary because the records are small and the on-disk size
  is fine.

### 8.7 Open questions

- **Vectorized batch decode.** When a query reads 1000 contiguous
  records, the offset reads are sequential and the record reads
  are sequential. A specialized batch decoder that does both in
  tight loops (rather than per-record `deserialize`) could be
  faster. We haven't measured the win; the suspicion is it's
  small because bincode is already very fast.

---

## Chapter 9 — parallel pipelines and work-stealing

### 9.1 The problem

The indexer's per-file work is variable: a 200-byte OWNERS file
parses in microseconds, a 5 MB tree-sitter parse takes ~5 seconds.
On 72 cores, the question is how to schedule files across workers
without (a) leaving cores idle while one worker chews on the long-
tail outlier, or (b) paying so much per-task overhead that the
small files become serial.

### 9.2 The wrong choices

| approach                  | failure mode                              |
|---------------------------|-------------------------------------------|
| one thread per file (`thread::spawn`) | OS thread overhead > per-file work for small files |
| round-robin to N workers   | one worker stuck on the largest file; others starve |
| fixed-size batches to N workers | last batch contains 71 small + 1 huge file → 1 core blocks the rest |

The standard solution is **work-stealing** — keep a per-worker
deque of tasks; idle workers steal from busy workers' deques.
Originally due to Burton & Sleep (1981); the canonical analysis is
Blumofe & Leiserson, 1999 ([*Scheduling Multithreaded
Computations by Work Stealing*][bl-ws]).

[bl-ws]: https://supertech.csail.mit.edu/papers/steal.pdf

### 9.3 The Blumofe-Leiserson bound

For a "fully strict" computation with total work `T_total` and
critical-path length `T_∞`, work-stealing on `P` processors runs
in time

```
T_P ≤ T_total/P + O(T_∞)
```

with high probability. The interpretation:

- The first term is "perfectly parallel" — what you'd get if every
  worker stayed busy for the whole computation.
- The second term is "tax for the longest dependency chain" — the
  critical path is serial no matter how many processors you have.

So the speedup approaches `P` only when `T_total / T_∞ >> P`. In
practice: when the average task is short relative to the longest
task and there are many tasks, work-stealing tops out near linear.

### 9.4 How scry uses it

scry uses [`rayon`][rayon] for the parser pool:

```rust
files.par_iter().for_each(|file| {
    parse_and_emit(file);
});
```

[rayon]: https://docs.rs/rayon/

rayon implements Blumofe-Leiserson work-stealing under the hood:
each worker has a deque, idle workers steal from busy workers,
the scheduling overhead per task is ~hundreds of nanoseconds.

The `T_∞` term is the single largest file's parse time. We bound
it from above two ways:

1. **`--big-file-bytes` serial bucket.** Files > 64 KiB get
   routed through a single serial worker rather than the
   parallel pool. This prevents two 5 MB files from landing on
   different cores at the same time and competing for memory.
   The bound is loose (we'd rather a 200 KiB file go through the
   parallel pool) but it caps the parallel-pool variance.
2. **`SCRY_PARSE_TIMEOUT_MS=60000` per-file budget.** No single
   parse runs longer than 60 s; ts-TIMEOUT skips it. The
   critical path is bounded by the time of the longest parsed
   file plus 60 s for the longest *attempted* parse.

The empirical sweet spot is `workers=16` on a 72-core host (see
the BENCHMARKS.md matrix). Why not 72? Three reasons:

1. **Memory.** Each parser holds a per-file working set; with 72
   simultaneous parses we'd be holding 72 × max-file-AST-bytes
   resident, which can be many GB.
2. **Parser-state contention.** Tree-sitter parsers aren't
   thread-safe, so we keep one per worker per language — at 72
   workers × 7 languages, the per-thread cache footprint
   competes with the corpus content for the page cache.
3. **jemalloc arena collisions.** jemalloc creates one arena per
   thread by default; at 72 arenas the metadata + fragmentation
   start to dominate.

### 9.5 Why no shared mutable state

Worker A doesn't update a shared symbol table while Worker B is
reading it. The pipeline is single-direction:

```
walker (immutable file list)
  → parsers (independent per-file output)
  → writer (single-threaded append)
  → finalize (single-threaded sort + merge + emit)
```

This is Hoare's [*Communicating Sequential Processes*][csp] in
the small: each stage owns its data, communication is via
ownership transfer (rayon's `for_each` results, the channel into
the writer), and there is no shared mutable state to lock.

[csp]: https://www.cs.cmu.edu/~crary/819-f09/Hoare78.pdf

When sharing *is* unavoidable (the OOM heartbeat thread reading
jemalloc stats and pausing workers, the progress counter), it's
done with atomics, not mutexes. The memory-ordering choice
(`Relaxed` for the counter, `Acquire`/`Release` for the OOM gate)
follows from Lamport's memory-consistency framework: the OOM gate
needs *visibility ordering* (a paused worker must see the writes
that justified pausing); the progress counter does not.

### 9.6 Tradeoffs

- **No fork-join parallelism within a parse.** A tree-sitter
  parse of a large file uses one core. Spawning sub-tasks for
  query-against-AST would give some intra-file parallelism but
  the per-task overhead would dominate at typical file sizes.
- **No streaming output from a parser.** A parser holds the
  whole per-file output until it's done, then emits to the
  writer. Streaming partial output would let the writer overlap
  IO with parsing, but the writer is rarely the bottleneck.

### 9.7 Open questions

- **GPU-accelerated parsing** is a research direction (Henke et
  al., 2022, parallel LR parsing on GPUs). For tree-sitter
  workloads it would be hard to win because tree-sitter parses
  are already very fast on CPU; GPU offload pays its overhead
  in the host↔device copies.
- **Heterogeneous task scheduling.** Tasks of very different
  cost (an OWNERS file vs a 5 MB C++ header) might benefit from
  task-cost-aware scheduling rather than the cost-oblivious
  rayon scheduler. The work-stealing bound is asymptotically
  tight regardless, so it'd be a constant-factor win.

---

## Chapter 10 — incremental parsing with tree-sitter

### 10.1 The problem

We want one parser API across C, C++, Java, Kotlin, Rust, Go,
Python, bash. We want it to tolerate syntax errors (a half-edited
file should still produce useful output). We want it to be
incremental (an edit shouldn't reparse the whole file).

### 10.2 What tree-sitter is

[tree-sitter][ts] (Brunsfeld, 2018) is a parser generator that
produces incremental, error-tolerant parsers from a grammar
specification. The output is a C library + a tree.

[ts]: https://tree-sitter.github.io/tree-sitter/

The interesting design choices:

- **GLR with backtracking.** A generalized LR algorithm with
  bounded ambiguity. Lets it handle real-world grammars
  (C++ template syntax, Ruby's `do...end` vs `{...}` block
  ambiguity) without exponential blowup.
- **Error recovery.** When parsing fails at a token, the parser
  inserts an `ERROR` node and continues. The downstream consumer
  gets a partial tree that's usable for symbol extraction.
- **Incremental reparse.** Given the old tree, a list of edits,
  and the new source, the parser reuses unchanged subtrees and
  reparses only changed regions. Cost is roughly proportional
  to the edit size, not the file size.

### 10.3 Symbol extraction via tree queries

A tree-sitter **query** is a pattern over the syntax tree, written
in S-expression syntax. Example for Java method definitions:

```scheme
(method_declaration
  name: (identifier) @name
  parameters: (formal_parameters) @params)
```

Run the query against a parse tree → get every match with
`@name` and `@params` capture nodes. scry uses this for symbol
and reference extraction; the queries live in `.scm` files
per-language under the tree-sitter grammar's bindings.

### 10.4 Per-file timeouts and the progress callback

A pathological input can make tree-sitter's GLR backtracking
explode. We saw a generated Java test fixture take > 1 hour
before being killed.

The right fix is a *parse-time budget* — abort if the parse
exceeds N seconds. tree-sitter has two APIs for this:

1. **`set_timeout_micros(usize)`** — deprecated as of
   tree-sitter 0.22. Sets a wallclock deadline; the parser
   checks it periodically. We discovered the hard way that
   "periodically" can mean "never" on certain pathological
   inputs.
2. **`parse_with_options(..., progress_callback: ...)`** — the
   replacement. The callback is invoked at every "interruption
   point" in the parser; returning `true` aborts the parse.

scry uses the callback (`crates/scry-lang/src/lib.rs::
parse_with_timeout`). The callback reads the elapsed time via
`Instant` and returns `true` if it exceeds the budget. The parser
exits cleanly; the file is logged with `[ts-TIMEOUT]` and
skipped.

### 10.5 Tradeoffs

- **No type inference.** tree-sitter is a parser, not a type
  checker. Symbol resolution within a file uses scope rules;
  cross-file resolution uses imports + heuristics; type-level
  questions (which overload, which type parameter) are out of
  reach. The opt-in SCIP path (ch. 13) covers this when needed.
- **Per-language quality varies.** tree-sitter-c, tree-sitter-cpp,
  tree-sitter-java, tree-sitter-rust are excellent.
  tree-sitter-kotlin is weaker — Kotlin's grammar is genuinely
  hard (extension functions, smart casts, lambda receivers) and
  the community grammar has gaps. We patched the receiver-type
  extraction in `crates/scry-lang/src/lib.rs::
  kotlin_receiver_for_decl`.

### 10.6 Open questions

- **Semantic vs syntactic indexing.** scry's symbol model is
  syntactic — it knows there's a definition called `foo`, but it
  doesn't know if `foo()` resolves to it without scope+import
  reasoning. SCIP files from real compilers give precise answers
  but require building the code. Whether it's worth the cost is
  workload-dependent; we made it opt-in.

---

## Chapter 11 — resilience under load (cgroups, jemalloc, OOM)

### 11.1 The problem

The full corpus has long-tail files that parse to gigabyte-scale
ASTs. A naive indexer OOM-kills its host once a week. An indexer
that runs unattended in production has to defend against this in
depth.

### 11.2 The eight layers

In order, outermost (host-level) to innermost (per-parse):

1. **systemd cgroup `MemoryMax=60G`.** Kernel-enforced hard
   ceiling. Crossing it triggers the OOM killer; the systemd
   unit restarts (via `Restart=on-failure`) and resumes from the
   last batch checkpoint. Worst-case redo is one batch
   (~5 k files, ~20-30 s).
2. **`MemorySwapMax=0`.** Refusing swap means the OOM kill is
   fast. With swap allowed, the kernel will thrash for minutes
   before giving up, making the recovery worse than the
   original problem.
3. **`--mem-cap 40G` jemalloc soft backpressure.** A heartbeat
   thread polls `jemalloc::stats::allocated` every 100 ms. At
   > 80% of the soft cap, new file pickups wait. Catches the
   memory growth before it can trip the cgroup.
4. **`--big-file-bytes 65536` serial routing.** Files larger
   than 64 KiB go into a single serial worker. Prevents two
   pathological parses landing on different workers in the same
   batch and compounding.
5. **`--max-file-bytes 5242880` hard skip.** Files > 5 MiB are
   skipped entirely; logged as `[skip-large]`. Above this size,
   the file is almost certainly machine-generated and parsing it
   won't add useful symbols.
6. **`SCRY_PARSE_TIMEOUT_MS=60000` per-file budget.** The
   tree-sitter progress callback aborts any parse > 60 s.
   Skipped with `[ts-TIMEOUT]`.
7. **Auto OOM skiplist.** Each parse writes its file path to
   `last_attempted.txt`. On resume, if the prior run's last-
   attempted file matches what we're about to reparse, the file
   goes onto `oom_skiplist.txt` and is skipped permanently.
   Self-healing: a file that reliably OOMs gets quarantined on
   the second try.
8. **`MALLOC_CONF=dirty_decay_ms:100,muzzy_decay_ms:100`.**
   jemalloc returns freed pages to the kernel within 100 ms.
   Without this, jemalloc retains the high-water-mark
   allocation, the kernel sees high RSS, the cgroup gets close
   to its limit, and the soft backpressure kicks in
   unnecessarily.

In practice (live indexer logs at `/mnt/agent/scry-index.log`):

- Steady-state RSS: 600 MB – 1 GB across the full AOSP+Linux run.
- Per-OOM cost: ~3 OOMs total per full reindex, each redoing one
  ~5 k-file batch (cumulative ~90 s of redo over 13 min).
- ts-TIMEOUTs: 4-10 files per run, all in the long tail.

### 11.3 The principle: defense in depth

No single layer is sufficient. Layer 1 alone (cgroup OOM) works
but loses minutes per OOM. Layer 3 alone (soft backpressure) only
defends against gradual growth; sudden spikes leak through. Layer
6 alone (per-file timeout) doesn't help for *memory* explosions
that happen quickly under the timeout.

Combining them gives graceful degradation: a pathological file
hits the layer 3 soft cap → pauses workers → if a single parse
exceeds layer 6 timeout → aborted; if memory spikes faster than
the 100 ms heartbeat → layer 1 catches it; on restart, layer 7
quarantines the offender.

This is the same logic that informs production systems generally
([Hamilton 2007, *On Designing and Deploying Internet-Scale
Services*][hamilton]): every dependency must fail safely, and the
failure modes must be visible enough that operators can
diagnose.

[hamilton]: https://www.usenix.org/legacy/event/lisa07/tech/full_papers/hamilton/hamilton.pdf

### 11.4 Tradeoffs

- **Static caps over adaptive ones.** A more sophisticated system
  would tune `--mem-cap` based on observed file sizes. We chose
  static caps because they're predictable and easy to reason
  about. The cost is a small amount of throughput on memory-rich
  hosts.
- **Stop the world rather than throttle.** When the soft cap is
  hit, all workers pause until the heap drains. We could throttle
  individual workers, but the bookkeeping is complex and the
  global pause is short (typically < 1 s).

### 11.5 Open questions

- **Predictive admission control.** Could we predict, from file
  size + extension + a few content bytes, the parse-memory cost
  and admit only as many parallel parses as fit? Yes in
  principle (linear regression on observed parses works for
  ~70% of variance); the residual is exactly the adversarial
  cases we built layers 5-7 for.

---

## Chapter 12 — putting it together

### 12.1 The indexer

```
walker (ignore crate)               parsers (rayon par_iter)
┌────────────┐                      ┌─────────────────────────┐
│ collect    │ ─── file list ────►  │ per-file:               │
│ all paths  │                      │   classify (40 kinds)   │
└────────────┘                      │   read (mmap)           │
                                    │   tree-sitter parse     │
                                    │     OR custom parser    │
                                    │   query → defs + refs   │
                                    │   trigram extract       │
                                    └────────┬────────────────┘
                                             │
                                             ▼
                              ┌──────────────────────────┐
                              │ writer (single-threaded) │
                              │   per-chunk sorted       │
                              │   names side-files       │
                              │   trigram side-files     │
                              │   records appended       │
                              │   offsets recorded       │
                              └──────────┬───────────────┘
                                         │
                                         ▼
                              ┌──────────────────────────┐
                              │ finalize                 │
                              │   k-way merge → FSTs     │
                              │   build file_symbols     │
                              │   build trigram postings │
                              │   build ref_resolutions  │
                              │   atomic rename .tmp → / │
                              └──────────────────────────┘
```

Every box above is something we've covered:
- walker = ch. 1's "collect all paths up front, sort by size,
  hand to a work-stealer".
- parsers = ch. 9 (work-stealing) + ch. 10 (tree-sitter) + ch. 4
  (trigram extraction).
- writer = ch. 8's byte-offset sidecar pattern, repeated for
  every artifact.
- finalize = ch. 6's k-way merge + FST build, plus the trigram
  posting-list build using the same merge machinery.

The pipeline is ~3000 lines of Rust across five crates; the
runtime is 13 minutes on the production corpus.

### 12.2 The query path

```
CLI args / JSON-RPC
       │
       ▼
StoreReader::open(index_dir)         ← only mmap calls, no IO
       │
       ▼
predicate dispatch (def/ref/grep/...)
       │
       ▼
either:                              or:                            or:
  names.fst lookup     (ch. 6)         trigrams.fst lookups  (ch. 4)   file_symbols sidecar (ch. 8)
  → symbol_ids                         + posting intersect             → symbol_ids for outline
       │                                 → candidate file_ids
       ▼                                       │
  LazyVec<SymbolRecord>::get             open + memchr scan
  (ch. 8 sidecar)                              │
       │                                       ▼
       ▼                                  emit hits
  emit records
```

Every query is some combination of: walk an FST to get an ID
list, look up records by ID, scan a small set of candidate files.
No query touches "the rest of the corpus".

### 12.3 If you wanted to build this from scratch

The reading list:

1. **CLRS, chapters 2-3, 6, 12, 22** — algorithms baseline.
2. **Bentley, *Programming Pearls*** — for the data-structures
   intuition.
3. **Cox 2012, *Regular Expression Matching with a Trigram
   Index*** ([essay][cox]) — ch. 4 and 5 above.
4. **Hopcroft & Ullman, *Introduction to Automata Theory,
   Languages, and Computation*** — Myhill-Nerode and the FST
   minimization theory.
5. **Aggarwal & Vitter 1988** — the EM model.
6. **Frigo et al. 1999** — cache-oblivious algorithms.
7. **Blumofe & Leiserson 1999** — work-stealing.
8. **Brunsfeld 2018, *tree-sitter*** — incremental parsing.

The implementation order I'd suggest:

1. Walker + classify + serial parse + bincode writer. **Goal**:
   build a complete index, however slowly. ~500 LOC.
2. Add the byte-offset sidecar pattern to writer + reader.
   **Goal**: O(1) record lookup. ~200 LOC.
3. Add FST construction over symbol names (sort, k-way merge,
   `fst::SetBuilder`). **Goal**: prefix + fuzzy queries. ~300 LOC.
4. Add trigram extraction + posting-list construction.
   **Goal**: literal grep faster than ripgrep. ~400 LOC.
5. Add rayon to the parser. **Goal**: indexing fast enough to
   iterate on the corpus. ~50 LOC of change.
6. Add the resilience envelope (cgroup, jemalloc backpressure,
   per-file timeout, OOM skiplist). **Goal**: unattended
   production. ~300 LOC.
7. Add the regex-to-literals extractor for grep regex queries.
   **Goal**: full `rg`-compat grep on the index. ~200 LOC.

Each step is independently useful. The total is ~2000 LOC for the
core (the other ~5000 in scry is language-specific custom parsers
for `Android.bp`, AIDL, init.rc, etc., which are scope-specific
add-ons, not the fundamental architecture).

---

## Chapter 13 — the tradeoffs scry made

This chapter is a single table summarizing the design decisions
the rest of the doc derived. Each row names a decision, what
scry chose, what we gave up, and when a different choice would
be right.

| decision                          | scry's choice                          | gave up                              | when different               |
|-----------------------------------|----------------------------------------|--------------------------------------|------------------------------|
| query model                       | precomputed index, query-time read     | per-edit freshness                   | live editor integration      |
| storage tier                      | mmap'd files, kernel page cache        | explicit cache control               | mlock'd hot path for QoS     |
| index update                      | full rebuild                           | incremental                          | per-commit re-index          |
| trigram n                         | 3                                      | 2-byte queries excluded              | wider corpus, larger n       |
| trigram posting position info     | file-level only                        | byte-level                           | exact-position queries       |
| string dictionary                 | minimized FST                          | online updates                       | streaming new symbols        |
| fuzzy search                      | Levenshtein automaton intersection     | ranked-by-distance output            | UX-quality fuzzy             |
| record layout                     | bincode + byte-offset sidecar          | record-level compression             | very large records           |
| parallelism                       | rayon work-stealing on whole files     | intra-file parallelism               | giant single files           |
| per-file parse timeout            | 60 s hard                              | parsing genuinely huge files         | extreme generated code       |
| memory enforcement                | cgroup MemoryMax=60G + restart         | running in unconstrained env         | embedded / non-systemd       |
| resolution layer                  | tree-sitter + scope heuristics         | type-level precision                 | refactoring tools (use SCIP) |
| regex grep                        | literal extract → trigram → scan       | non-literal-anchored regex           | full regex over scan-only    |
| language coverage                 | C/C++/Java/Kotlin/Rust/Go/Python/sh+asm | Swift, Dart, Haskell, etc.          | other tree's primary langs   |
| host count                        | one                                    | multi-host scale                     | Sourcegraph-scale corpus     |

The decisions all rest on the same workload assumption: **one
well-known corpus, indexed once per ~hour, queried many times
per minute, by a mix of human and LLM clients on one host**.
Change the workload and the table changes. Add real-time
freshness, scry needs incremental updates. Add multi-tenant
hosting, scry needs sharded indexes. Add intra-file precision,
scry needs SCIP integration.

Within the assumed workload, the decisions are tight: each one
saves a measurable amount of work or memory and gives up
something we've measured the cost of. The system is what it is
because the workload is what it is.

That's the L7 instinct in one paragraph: name the workload
honestly, derive the constraints, pick the smallest design that
satisfies them, and don't pay for capabilities the workload
doesn't ask for.
