# scry — design

**Status: implemented + in production.** This document is the as-designed
spec; everything below is shipped against `~/dev/aosp` + `/mnt/agent/dev/linux`
unless explicitly marked "(deferred)". For the as-built operator view see
`OPERATIONS.md`; for the fast-path (trigram + lazy mmap) details see
`FAST_PATH.md`. Both fast-path optimizations shipped on 2026-05-16
(commits `1041f2b`, `e96a4ee`, `d1e507a`). The deprecated
`set_timeout_micros` per-file budget was replaced by
`parse_with_options` + progress callback on 2026-05-16 (`c2e32fa`)
after several real AOSP Java files were observed running > 1 h with
the old mechanism silently failing to abort.

---

## 1. What we're trying to build

A local-first **semantic code search and cross-reference engine** for AOSP
**and the Linux kernel**, with the index keyed against arbitrary source
roots so additional trees can be added.

The interface is a single static binary `scry` that:

1. **Indexes** one or more source roots off-tree (zero modification of the
   trees themselves), in parallel, in roughly the time a build's `m nothing`
   takes. Default roots configured for this host: `~/dev/aosp/` (AOSP) and
   `/mnt/agent/dev/linux/` (Linux 7.0-rc7, 37 GB). Additional roots passed
   on the command line.
2. **Answers queries** — definitions, references, callers, callees,
   subtype/override hierarchies, fuzzy symbol lookup, build-module-scoped
   filters, owner-scoped filters, ripgrep-grade substring search — at
   interactive latency on a warm index.
3. **Streams results in formats LLM agents can consume**: line-delimited
   JSON, ranked snippets with surrounding scope, deterministic symbol IDs.

The target user is split:
- **Humans** at a terminal who want to navigate a 118 GB tree without booting
  Android Studio.
- **LLM agents** that need to fire hundreds of cheap, structured queries per
  task ("what calls `Binder.transact` in `frameworks/base`?", "show me every
  override of `IActivityManager#startActivity`", "what AIDL stubs depend on
  this interface?").

Both should get the same answers; only the formatter changes.

## 2. Goals and non-goals

### Goals

- **Multi-language, AOSP-first**: C, C++, Java, Kotlin, Rust, Go, Python,
  AIDL, .proto, Android.bp (Soong), Android.mk, OWNERS. First-class
  support — not afterthought.
- **Speed**: cold full index of AOSP in < 10 minutes on 72 cores;
  incremental update < 30 s for a single-module edit; symbol lookup
  < 10 ms warm; xref query < 100 ms warm; ripgrep-class substring search.
- **Compactness**: total on-disk index < 5% of source = < ~6 GB.
- **Build-aware**: parse `Android.bp` enough to know which files belong to
  which Soong module, which modules depend on which, who owns what. This
  unlocks scoped queries like `scry ref --in frameworks/base` or
  `scry callers --module services.core`.
- **LLM-friendly output**: stable symbol IDs, JSONL streams, snippet
  extraction with scope context, ranked relevance.
- **Zero AOSP mutation**: nothing under `~/dev/aosp/` is written to, ever.
- **One static binary**, no daemon required (daemon is an option for
  multi-query LLM sessions).
- **Resilient parsing**: bad/partial files do not stall a full reindex.

### Non-goals (initially)

- **Refactoring / writing**: scry is read-only. No edits, no quick-fixes.
- **Full type inference**: we will not reach `clangd`-level precision for
  C++ overload resolution; that requires `compile_commands.json` and a real
  compilation database, which we treat as **optional precision uplift**.
- **GUI**: terminal + JSON only. A web UI can come later but it's not the
  product.
- **Anything outside AOSP**: scry is tuned for this tree. Generalization to
  arbitrary repos is a side-effect, not a requirement.
- **Distributed indexing**: single host, big-CPU, big-disk. We have 72
  cores; we use them.

## 3. AOSP-specific characteristics that shape the design

Working assumption: the indexer should be designed *for* AOSP's quirks, not
in spite of them.

1. **Soong (`Android.bp`) is the ground truth for module structure.**
   Every source file in the platform belongs to one or more modules
   declared in some `Android.bp`. The module knows: source globs, language,
   target SDK, visibility, dependencies, generated outputs (e.g. AIDL →
   Java stubs). Knowing modules makes references meaningful (private vs
   exported) and queries scopable.

2. **AIDL is a cross-language pivot.** Every AIDL `interface IFoo` produces
   a Java stub, a C++ stub, an NDK stub, and a Rust stub. A reference to
   the Java side is meaningfully a reference to the C++ side. The index
   should *link* these together so "find all callers of `IFoo#bar`"
   returns Java *and* native callers.

3. **Java + Kotlin are mixed within the same module routinely.** Resolution
   must cross the boundary (Kotlin calling Java is the common case).

4. **OWNERS files matter.** Owner-scoped queries ("show me code I own that
   calls into Binder") are the kind of question only AOSP engineers ask,
   and `scry` should answer them natively.

5. **Generated code lives under `out/` and we ignore it.** But AIDL-,
   HIDL-, proto-generated symbols appear in many places via include paths;
   we resolve to the *source* `.aidl`/`.proto` definition, not the
   generated artifact.

6. **`prebuilts/` is mostly binary blobs + SDK jars.** We skip it for
   indexing. We may later parse `.jar` manifests for SDK symbol coverage,
   but that's a phase-3 task.

7. **The tree is huge (118 GB / 734k files) but the *changed* set per
   session is small.** Incremental indexing is not a nice-to-have; it's the
   common path.

## 3.5 Profiles (per-root configuration)

A **profile** is a small TOML config that tells scry what to expect at a
given root: which build-file parsers to enable, which directories to
ignore, what language mix to expect, and which language-specific resolver
rules apply.

| Profile   | Root example            | Build parsers enabled                    | Skipped dirs                          |
|-----------|-------------------------|------------------------------------------|---------------------------------------|
| `aosp`    | `~/dev/aosp/`           | Android.bp, Android.mk, BUILD.bazel, Kconfig, aconfig, init.rc, sepolicy, OWNERS | `out/`, `prebuilts/`, `.repo/` |
| `linux`   | `/mnt/agent/dev/linux/` | Kbuild, Makefile, Kconfig                | build artifacts, `.git/`, vmlinux* |
| `generic` | anything else           | Auto-detect: Cargo.toml, BUILD, CMakeLists.txt, package.json, Makefile, Kconfig | `.git/`, common build dirs |

Profiles are auto-selected by sniffing the root (presence of `build/soong/`
→ aosp; presence of `MAINTAINERS` + `Kbuild` → linux; otherwise generic),
overridable with `--profile`.

The on-disk index is *unified across roots*. Every result carries a
`root_id` so query output can show whether a hit came from AOSP or the
kernel, and filters like `--root linux` scope queries. Symbol IDs are
namespaced by root so a function named `init` in the kernel doesn't
collide with `init` in AOSP.

## 4. Prior art and why we're not just using it

| Tool        | Strength                          | Why not enough for us                                         |
|-------------|-----------------------------------|---------------------------------------------------------------|
| ripgrep     | World-class substring search      | No symbol model, no xrefs                                     |
| ctags / gtags | Symbol tags, callers (gtags)    | No real scope/type resolution; Java/Kotlin/Rust support weak  |
| cscope      | C/C++ xrefs                       | C/C++ only; slow on 100k+ files                               |
| clangd      | Precise C++ semantics             | Needs `compile_commands.json`, single language, slow warmup   |
| Sourcegraph | Web UI, multi-lang, SCIP-based    | Heavyweight service, not designed for CLI/LLM loops           |
| Zoekt       | Trigram index, very fast grep     | Substring only — no semantic xrefs                            |
| SCIP        | Precise multi-lang index *format* | Format, not a tool; needs per-language indexer (we'll use these as optional inputs) |
| Stack Graphs (GitHub) | Scope resolution from rules | Promising; we may adopt for Java/Kotlin/Python              |
| Kythe       | Semantic graph (used in Google3)  | Industrial scope, deep per-lang indexer integration           |

**Pragmatic synthesis**: we build a tree-sitter-based syntactic index as the
spine (covers all languages cheaply), then **optionally** ingest SCIP files
from `scip-clang` / `scip-java` to upgrade precision where the user has
generated them. The on-disk format is unified; clients don't care how a
fact arrived.

## 5. Technology choices

### Language: **Rust**

- Best-in-class ecosystem for this exact shape of problem: `ignore` (gitignore-
  respecting walker from ripgrep), `memchr` (SIMD byte search), `rayon`
  (work-stealing parallelism), `memmap2` (zero-copy index reads), `fst`
  (compact transducer for prefix/fuzzy symbol lookup), `tree-sitter` (multi-
  language parsing), `tantivy` (full-text if we want it), `serde` + `bincode`
  for stable on-disk encodings, `clap` for the CLI, `tokio` only if/when we
  add the daemon.
- One static binary out of the box (`cargo build --release`).
- Memory safety matters for a long-running daemon over a 6 GB mmap.
- We don't have rustup installed yet — first action of phase 0 is to install
  it.

C++ was considered. Tree-sitter's C API is native, and we have clang 18
locally. But the *infra around the parser* — gitignore-aware walking, SIMD
string search, parallel pipelines, FSTs, mmap — is all dramatically more
ergonomic in Rust, and `ripgrep`'s lineage already proved out the pattern
we want to copy.

### Parsing: **tree-sitter as the spine**

- One API across C, C++, Java, Kotlin, Rust, Go, Python, .proto, bash, etc.
- Tolerant of syntax errors: a half-edited file still yields a usable parse.
- Incremental: edits reuse most of the previous tree.
- We extract symbols and references via **tree-sitter query files** (`.scm`)
  per language. These are declarative S-expression patterns; adding a new
  language is "write a query file", not "write a parser".

For languages without (good) tree-sitter grammars we write **mini-parsers**:
- **Android.bp**: Blueprint syntax is small (assignments, module-type calls
  with named args, lists, strings, basic ops). A 300-line recursive-descent
  parser in Rust is plenty.
- **AIDL**: small grammar (interface, methods, in/out/inout, oneway,
  parcelables, enums). Custom parser, ~500 lines.
- **OWNERS**: line-based, trivial.
- **Android.mk**: we parse just enough to find `LOCAL_*` assignments and
  `include $(BUILD_*)`; not a full Make implementation.

### Storage: **custom mmap'd columnar format + FST + trigram**

Five physical artifacts under `/mnt/agent/scry-index/`:

| File              | Role                                            | Format                |
|-------------------|-------------------------------------------------|-----------------------|
| `files.col`       | file_id → path, lang, module_id, mtime, size    | columnar, mmap        |
| `symbols.col`     | symbol_id → name, kind, file_id, byte range, scope | columnar, mmap     |
| `refs.idx`        | symbol_id → list of (file_id, byte_offset, kind)| LMDB or our own KV    |
| `names.fst`       | identifier → symbol_id(s)                       | FST (sorted set)      |
| `trigrams.dat`    | trigram → posting list of file_ids              | zoekt-style, mmap     |
| `modules.col`     | module_id → name, type, srcs, deps, owners      | columnar, mmap        |
| `manifest.json`   | index version, source root, language versions   | JSON                  |

Indexes are written via **atomic snapshot swap**: indexer writes to
`index.tmp/`, renames to `index/` (POSIX rename is atomic on the same fs).
Readers hold the old mmap until they re-open.

A custom format is justified here because (a) we want mmap-friendly layouts
SQLite/RocksDB don't give us cheaply and (b) it lets us put fast paths for
the queries we know matter (prefix on names, posting-list intersection on
trigrams). We will fall back to **LMDB** for the symbol→refs adjacency only,
because writing a custom B-tree is not a fight worth picking.

### Resolution: **layered**

- **Layer 0 — lexical**: tree-sitter queries pull out definitions and
  candidate references. Reference is currently just "an identifier in this
  file at this position with this surrounding scope path".
- **Layer 1 — scope-aware (per-language rules)**: resolve a reference to
  a definition using:
  - same file's scope tree (block, class, function),
  - imports (Java/Kotlin `import`, C++ `#include`, Rust `use`, Python
    `import`, Go `import`),
  - package/namespace siblings,
  - inheritance graph (for `super`/`this` and method lookups).
  This is heuristic — we accept ambiguity (multiple candidates).
- **Layer 2 — precise (optional)**: if `compile_commands.json` exists, run
  `scip-clang` and merge SCIP facts. Same for `scip-java` if the user
  produced jars. SCIP records win over heuristics where they overlap.
- **Layer 3 — build-graph augmented**: use the Soong module graph to filter
  candidates by visibility ("module A can't see module B's internals").

Layer 0 + 1 + build-graph is the MVP. Layer 2 is a precision uplift, opt-in.

## 6. Indexing pipeline

```
                ┌──────────────────────────────────────────────┐
                │            scry index (cold path)            │
                └──────────────────────────────────────────────┘

  walker (ignore crate)                     parser pool (rayon)
  ┌────────────┐    file paths    ┌─────────────────────────────┐
  │ walk dirs  │ ───────────────► │  per-file:                  │
  │ skip out/, │                  │   - read (mmap)             │
  │ prebuilts, │                  │   - detect lang             │
  │ .git, .repo│                  │   - tree-sitter parse       │
  └────────────┘                  │   - run lang query (.scm)   │
                                  │   - emit defs + refs        │
                                  └─────────────┬───────────────┘
                                                │
                                                ▼
                                  ┌─────────────────────────────┐
                                  │ resolver (scope + imports)  │
                                  │   - resolve refs to defs    │
                                  │   - tag with module_id      │
                                  └─────────────┬───────────────┘
                                                │
                                                ▼
                                  ┌─────────────────────────────┐
                                  │ writers (sharded by id %N)  │
                                  │   - append columnar files   │
                                  │   - bucket trigrams         │
                                  │   - update LMDB refs        │
                                  └─────────────┬───────────────┘
                                                │
                                                ▼
                                  ┌─────────────────────────────┐
                                  │ finalize: build FST, sort   │
                                  │ trigrams, write manifest,   │
                                  │ atomic rename               │
                                  └─────────────────────────────┘
```

Key engineering choices:

- **Walker phase first, fully**: collect all paths up front (cheap — `find`
  takes seconds). This lets us load-balance the parser pool by file size,
  not directory order (otherwise one rayon worker gets stuck on
  `external/llvm-project/` while the others starve).
- **Parser pool is `rayon::par_iter`** over the file list, with each
  language's `tree_sitter::Parser` held in a thread-local. Tree-sitter
  parsers are not thread-safe, so we keep one per thread per language.
- **Resolver runs in a second pass** because Layer 1 needs the full
  symbol table to be visible. Pass 1 writes raw `(file, name, position,
  scope_path, kind)` tuples; pass 2 joins them.
- **Soong parse runs as a sidecar** (parses ~14k `Android.bp` files in
  parallel; cheap) and produces `modules.col` before the symbol resolver
  runs, so we can attach `module_id` to each file.
- **Trigram index** is built by streaming each file through a trigram
  shingler. We index *identifiers and comments* separately so that
  symbol-only searches don't have to wade through prose.

### Incremental path

- Watch `~/dev/aosp/` with `inotify` (Linux) — pluggable backend.
- On change: re-parse the file, regenerate its symbols/refs, delete the
  old file's rows by `file_id`, append new rows. Mark trigram postings as
  invalidated for the affected `file_id`; trigrams are filtered on query
  rather than rewritten eagerly (deferred compaction).
- Background compaction reclaims tombstoned postings periodically.

## 6.5 Ranking and narrowing heuristics

Most queries return more than one candidate. scry's heuristics
decide what surfaces first, how broad a search starts, and which
narrowing rules each language gets.

### Symbol ranking — `rank_score`

Implementation: `SymbolRecord::rank_score()` in
`crates/scry-store/src/lib.rs`. Composite integer score; callers
typically `sort_by_key(|s| Reverse(s.rank_score()))`. Three
components:

**1. Kind tier (base score).** Higher = surfaces earlier.

| Tier | Score | SymbolKinds |
|------|------:|-------------|
| Top  | 100   | Class · Interface · Trait · Struct · Enum · Union |
| Call | 90    | Method · Function · Constructor |
| IPC  | 85    | AidlInterface · AidlMethod · AidlParcelable · ProtoMessage · ProtoService · ProtoEnum |
| Build| 80    | SoongModule |
| Shadow| 78   | AidlShadow · HidlShadow (derived bindings — below the real .aidl) |
| Platform| 75 | InitService · SepolicyType |
| Module| 70   | Module · Namespace · Package |
| Config| 65   | AconfigFlag · ManifestComponent |
| Value | 50   | Field · Variable · Constant · EnumVariant · Type |
| Macro | 40   | Macro · Annotation · Decorator |
| Param | 20   | Parameter |
| Misc  | 10   | XmlId · OwnersEmail · Other |

**2. Language penalty.** `FileKind::ApiTxt` gets `-40` so SDK
surface declarations never crowd out the actual source
definition for "where is X?".

**3. Scope penalty.** `min(scope_depth, 10) * 3`. Top-level
symbols outrank deeply nested ones. Catches the common case
where you want the outer `Activity`, not an inner helper.

Pinned by `tests::rank_score_orders_tiers_correctly` in
`scry-store`. Bench-protected: a refactor that subtly reorders
tiers gets caught at `cargo test`.

### Grep-candidate ranking — path-quality penalty

Implementation: `score_path` in `crates/scry-cli/src/main.rs`.
Applied to grep hits to push noise paths down:

- **Generated paths.** `/generated/`, `/gen/`, `.pb.`, `_pb2.`
  in the path → `-PENALTY_GENERATED_PATH` (~50). Catches
  proto-generated stubs that would otherwise drown out the
  hand-written source.
- **Path depth.** `(depth - 3).clamp(0, 30)` — files more than
  3 directories deep get penalized linearly, capped at 30.
  Surfaces the root-level service over its 10-deep test
  variant.

### Layer 2 resolver — narrow callers / refs to one def

Implementation: `resolve_one` in `crates/scry-cli/src/main.rs`
(see `build-resolutions`). Per-language narrowing rules,
applied in order until one candidate remains:

**Java**:
  1. Same package as the caller.
  2. Explicit `import a.b.C;` in the caller's file.
  3. Wildcard `import a.b.*;` in the caller's file.
  4. `java.lang.*` implicit import.
  5. If still > 1, return all (the reader picks by `rank_score`).

**Kotlin / C++**: framework in place; today falls back to
"first same-lang candidate." Language-specific narrowing
queued in DEVELOPMENT.md "What's left."

**Cross-language** (e.g. AIDL → Java Stub): handled by the
AIDL/HIDL shadow-symbol pass at index time, not the resolver.
The shadow symbols (`AidlShadow`, `HidlShadow` kinds) live in
the same lookup table as their real declarations and rank
~78 — below the real `.aidl` but above plain fields, so a
search for `IFoo.Stub` lands on something useful even when
the agent doesn't know which file it's in.

### Trigram candidate selection — `grep_candidates`

Implementation: `crates/scry-store/src/trigram.rs` +
`grep_candidates` in `crates/scry-cli/src/main.rs`. Russ-Cox
style: extract every overlapping 3-byte trigram of the literal
needle, intersect their per-trigram file-id sets. The
smallest-first intersection order is bench-pinned (smaller set
× larger set = O(small)). Regex patterns route through HIR
literal extraction (`regex-syntax`) to find required substrings
before the trigram step.

Pinned by `tests::trigram_intersection_smallest_first` in
`scry-store`. A regression that reordered the intersection
would silently 5-10× the grep candidate set.

### Fuzzy ranking — composite

Implementation: `cmd_fuzzy` in `crates/scry-cli/src/main.rs`.
Two candidate sources merged (FST prefix substring + Levenshtein
automaton), re-ranked by Wagner-Fischer edit distance. Substring
matches outrank pure-typo matches at equal distance; closer
matches outrank farther ones. Pinned by ranking tests in
`scry-cli/src/main.rs::tests` (search for `fuzzy_ranks_substring`).

### What's deliberately NOT a heuristic

- **`def NAME` does NOT auto-narrow by `--kind`.** Without a
  filter, you see every kind. The principle: the heuristic
  ranks, but it doesn't hide. An agent that wants the class
  passes `--kind class`; the tool doesn't guess.
- **Symbol ID is content-hash, not heuristic-ranked.** Two
  symbols with the same `(root_id, relpath, kind, scope, name,
  line)` get the same `id`. Stable across rebuilds.

## 7. Per-file-type strategy

Coverage is **everything that drives a build, ships in an image, or
encodes platform behavior** — not just source. AOSP-specific config
formats (aconfig, init.rc, sepolicy) are first-class, not afterthoughts.

### Source code
| Language    | Parser       | Notes                                                       |
|-------------|--------------|-------------------------------------------------------------|
| C / C++     | tree-sitter-cpp | No preprocessor. We do *not* expand macros — we record macro names as symbols and treat macro call sites as refs. SCIP-clang (optional) handles precision. |
| Java        | tree-sitter-java | Imports resolved against package map; inner classes flattened in scope path. |
| Kotlin      | tree-sitter-kotlin | Extension functions tracked as defs on receiver type. Java interop resolved through shared FQN map. |
| Rust        | tree-sitter-rust | `use` tree expanded; `mod` files merged. Macro names recorded; macro expansion not attempted. |
| Go          | tree-sitter-go | Standard. |
| Python      | tree-sitter-python | `import` resolution best-effort; relative imports per project root. |
| AIDL        | custom       | Each `interface IFoo` produces synthetic symbols `IFoo`, `IFoo#method`. We *link* these to Java/Cpp/Rust generated names by deterministic ID, so refs in any language find the AIDL source. |
| .proto      | tree-sitter-proto | Message/enum/service as defs; generated-code refs link back. |

### Build files (first-class — drive every other resolution)
| Type         | Parser        | What we extract                                                       |
|--------------|---------------|-----------------------------------------------------------------------|
| Android.bp   | custom (~300 LOC) | Module name as a symbol; `srcs` glob → file→module mapping; `deps`, `static_libs`, `shared_libs`, `header_libs`, `defaults`, `cflags`, `cppflags`, `ldflags`, `required`, `visibility`, `apex_available` all as refs. **Soong is the source of truth for module structure.** |
| Android.mk   | custom (shallow) | `LOCAL_MODULE`, `LOCAL_SRC_FILES`, `LOCAL_CFLAGS`, `LOCAL_*_LIBRARIES`, `include $(BUILD_*)`. Best-effort — Make is not fully parsed. |
| BUILD / BUILD.bazel | tree-sitter-starlark or custom | Same pattern: rule name, srcs, deps, visibility. AOSP is partly on Bazel and growing. |
| Kconfig      | custom (small) | `config FOO`, `select`, `depends on`, `default` — for kernel feature queries. |
| `.flags`     | line-based    | Each line is a compiler flag fragment; we link to the .bp/.mk that consumes it. |
| `.jarjar`    | line-based    | Rename rules — link old FQN → new FQN, so refs follow through repackaging. |

### Android platform configuration (high-value AOSP-distinctive)
| Type             | Parser            | What we extract                                                                                  |
|------------------|-------------------|--------------------------------------------------------------------------------------------------|
| `.aconfig`       | custom (proto-shaped textproto) | **Feature-flag definitions**: name, namespace, description, default, is_fixed_read_only. Match them up with `Flags.FOO_BAR` symbol references in Java/Kotlin/C++ code so `scry flag foo.bar` returns the definition *and* all readers. |
| `.rc` (init)     | custom            | `service NAME`, `on EVENT`, `class`, `user`, `group`, property triggers. Resolves `start NAME`/`stop NAME` to service defs. |
| `.te` (sepolicy) | custom            | `type T`, `typeattribute`, `allow`/`neverallow`/`auditallow` rules — refs from rule sources to the types they touch. |
| `.policy`        | custom            | mac_permissions / seapp_contexts style records. |
| AndroidManifest.xml | XML + targeted extraction | `<activity>`, `<service>`, `<receiver>`, `<provider>`, `<permission>`, `<uses-permission>`, `<intent-filter>` — each declared component is a symbol; `android:name` values cross-reference into Java/Kotlin classes. |
| Layout/Resource XML | XML + selective | `android:id="@+id/foo"` becomes a symbol; references to `@id/foo` and `R.id.foo` resolve. We do *not* index attribute text in general — only IDs and `style`/`theme` names — to keep the trigram budget sane. |
| `.properties`    | line-based        | Key/value; keys become refs. |
| `.toml`          | tree-sitter-toml  | Cargo manifests for Rust; we read `[dependencies]` for Rust module deps. |
| `.cfg`           | best-effort       | Many are key=value; we index keys. |
| `.json`          | JSON              | Indexed selectively: `.json` files near build configs (e.g. `aconfig`, `compatibility_matrix.json`) get key extraction; arbitrary JSON elsewhere is skipped to control index size. |

### Scripts
| Type      | Parser              | Notes                                                                |
|-----------|---------------------|----------------------------------------------------------------------|
| `.sh` / `.bash` | tree-sitter-bash | Function defs, function calls, variable defs, `source`/`. file.sh` cross-file links, env-var refs. Shell is heavy in AOSP build/test infra — first-class. |
| `.py`     | tree-sitter-python  | Already in source table. |
| envsetup.sh family | bash + name table | `lunch`, `mm`, `mmm`, `croot` and friends recognized so `scry def lunch` finds the right function. |

### Ownership
| OWNERS    | trivial line parser | Emails / `include` directives / `per-file` rules. Powers `scry owner PATH` and `--owner EMAIL` filters. |

## 8. Query model

A query is `(predicate, filters, output)`. Predicates:

| Predicate          | Meaning                                                     |
|--------------------|-------------------------------------------------------------|
| `def NAME`         | Definitions of NAME (any kind), ranked by exactness.        |
| `ref NAME`         | References to NAME, with surrounding scope.                 |
| `callers NAME`     | Refs where NAME appears in call position.                   |
| `callees IN`       | Names in call position inside IN (a function/method def).   |
| `impls IFACE`      | Types/classes that implement IFACE.                         |
| `overrides METHOD` | Subclasses that override METHOD.                            |
| `subtypes TYPE`    | Direct + transitive subtypes.                               |
| `members TYPE`     | Fields/methods/inner types of TYPE.                         |
| `mod NAME`         | Soong module info: srcs, deps, type, owners.                |
| `owner PATH`       | Owners of PATH (walks up OWNERS chain).                     |
| `fuzzy STR`        | Fuzzy symbol search (FST-based).                            |
| `grep PATTERN`     | ripgrep-style substring/regex over indexed files only.      |
| `aidl-link NAME`   | All Java/Cpp/Rust shadows of an AIDL symbol.                |
| `flag NAME`        | aconfig flag definition + every read site (Java/Kotlin/C++). |
| `service NAME`     | init.rc service definition + start/stop sites + binary path. |
| `sepolicy TYPE`    | SELinux type definition + every rule that touches it.        |
| `component NAME`   | Android manifest component (activity/service/receiver/provider) → declaration + Java/Kotlin class. |
| `xml-id ID`        | `@+id/ID` definition + every `@id/ID` and `R.id.ID` read.    |
| `module-of PATH`   | Soong module a path belongs to (and which `.bp` declared it). |
| `cflag FLAG`       | Which modules use this compiler flag.                        |

Filters (composable, attach to any predicate):
- `--lang java,kotlin`
- `--root aosp|linux|<name>` (limit to one source root)
- `--in PATH` (path prefix, multiple OK)
- `--module NAME` (Soong module name)
- `--owner EMAIL` (matches OWNERS)
- `--kind class,method` (symbol kind)
- `--exclude PATH`
- `--since GIT_REV` (only files changed since this revision — uses git, not
  blame; falls back to full when AOSP source has no git history at the
  scope queried)

Output formats:
- default: human, colorized, `path:line:col  scope  snippet`
- `--json` / `--jsonl`: structured, suitable for piping into agents
- `--md`: markdown for paste-into-prompts, each result a section with
  fenced code snippet
- `--paths-only`: just paths (for piping to xargs)
- `--count`: tallies, not results

## 8.4 What a sidecar is, and why scry uses them

A scry index is a directory of files. The required core files
(produced by `scry index`) are the source-of-truth:

```
<index>/
  manifest.json         # version + roots + lang breakdown
  roots.bin             # list of indexed root paths
  files_packed.bin      # mmap'd packed file table (root_id, kind, size, relpath)
  symbols.bin           # per-symbol records (the def table)
  refs.bin              # per-ref records (the use table)
  names.fst             # FST: name → posting list in name_postings.bin
  name_postings.bin     # symbol-id postings keyed off names.fst
  ref_names.fst         # same shape for ref names
  ref_postings.bin
```

Everything else in `<index>/` is a **sidecar**: an optional,
independently-built, mmap'd file that adds capability without
changing the core. The convention is one sidecar per file (so
they can be regenerated and atomically swapped without touching
the core), each with its own `version: u32` header (so format
drift is detected at open time), each opened lazily (so the
filter no-ops gracefully when the sidecar is missing).

Why "sidecar" instead of "extend the main index":

- **Optional.** A C++-only project doesn't need `scip_index.bin`.
  An AOSP index without `m json-module-graph` run doesn't have
  `module_graph.json`. Making these required would force every
  user to install every toolchain.

- **Independently regeneratable.** Editing source flips file
  digests → next `scry index --incremental` reparses changed
  files and rewrites `symbols.bin` / `refs.bin`. The sidecars
  stay valid because they're keyed by `(abs_path, byte_offset)`
  — only the parts of them whose source moved need a rebuild,
  and that rebuild happens via the per-sidecar command (e.g.
  `scry clang-index` when `compile_commands.json` changes,
  `scry scip-import` when `*.scip` changes). The core never
  forces an across-the-board rebuild.

- **Independent versioning.** Each sidecar checks its own
  version header on open: mismatched version → "absent" status
  in `scry health`, query path skips the filter. No silent
  decode of stale data.

- **Bounded read cost.** mmap one sidecar; pay only the pages
  touched on lookup. The 256MB AOSP module graph never enters
  memory unless `--reachable` is set. The 800-byte synthetic
  C++ fixture sidecar pages in instantly.

Current sidecars (all optional; `scry health` reports each):

| File                       | Built by                          | Used by                          |
|----------------------------|-----------------------------------|----------------------------------|
| `trigrams.fst` + `trigram_postings.bin` | `scry build-trigrams`     | `scry grep` (Russ-Cox prefilter) |
| `symbols_offsets.bin`      | `scry build-offsets`              | O(1) symbol record lookup        |
| `refs_offsets.bin`         | `scry build-offsets`              | O(1) ref record lookup           |
| `file_symbols.bin` + `_offsets.bin` | `scry build-file-symbols` | `scry outline FILE` (file → syms)|
| `file_refs.bin` + `_offsets.bin` | `scry build-file-refs`        | `scry uses NAME` (refs-in-file)  |
| `ref_resolutions.bin`      | `scry build-resolutions`          | Layer-2 resolved_to per ref      |
| `module_graph.json`        | `scry build-modgraph`             | `--reachable` filter             |
| `clang_usrs.bin`           | `scry clang-index`                | `--clang-precise` (default-on)   |
| `scip_index.bin`           | `scry scip-import`                | `--scip-precise` (default-on)    |

`scry finalize` is the one-stop runner: it invokes each builder
in order, skipping the ones whose input isn't available
(`--build-soong /path`, `--build-out /path`, `--scip FILE`,
`--clang-compile-commands FILE`). After `scry finalize`, the
sidecars are all in place and every query auto-engages whatever
precision the data supports.

## 8.5 Build-symbol precision (Path B / Path C)

Tree-sitter gives scry name-level symbols cheaply across every
language. Name-level matching is fast but lossy: `transact()` in
AOSP has 1981 name-matched call sites; only ~166 actually
target `BBinder.transact`. The other ~1815 are false positives
from unrelated `transact` methods in unrelated classes.

Two compiler-backed precision layers sit on top of the tree-sitter
base, both optional and both consumed as separate on-disk
sidecars in the index dir:

- **Path B (`clang_usrs.bin`)** — per-translation-unit libclang
  parse driven by `scry clang-index <compile_commands.json>`.
  Emits one `UsrRecord{ abs_path, byte_offset, usr_id, kind }`
  per declaration / reference cursor in the TU. The clang USR
  is a globally unique mangled identifier for the symbol — same
  USR for the def of `strdup` and every call site to it across
  every translation unit, regardless of which Soong module the
  call sits in. Coverage: C, C++, Objective-C.

- **Path C (`scip_index.bin`)** — generic SCIP protobuf ingest
  driven by `scry scip-import <index.scip>`. SCIP
  (https://github.com/sourcegraph/scip) is the Sourcegraph
  successor to Kythe-the-format; one indexer per language
  emits a single .scip file. Same record shape as Path B
  (`ScipRecord{ abs_path, byte_offset, symbol_id, role }`)
  but the symbol IDs are SCIP-formatted strings. Coverage:
  every language with a SCIP producer — Java (`scip-java`),
  Kotlin (`scip-kotlin`), Rust (`rust-analyzer scip`),
  Go (`scip-go`), TypeScript (`scip-typescript`), Python
  (`scip-python`), and others.

Both sidecars are mmap'd into a `(path, byte_offset) → symbol_id`
index at query time. Lookup is O(1). When a `ref` / `callers`
query runs, scry asks the sidecar for the symbol at each
candidate ref's location and keeps only those whose symbol
matches one of the def's symbols. This is the Kythe-class
structured-identity narrowing — false positives drop because
the structured ID disagrees, even when the names match.

**Default-on:** both filters auto-engage whenever their sidecar
exists in the index dir. Users get the precise answer for free
on covered code, and graceful fallback to lexical name match on
uncovered code. `--lexical` is the explicit opt-out — useful
for "show me everything" mode or for measuring filter impact.
A third filter, `--reachable`, narrows by Soong/Bazel/Kernel
module-graph visibility; it stays explicit opt-in because the
256MB AOSP module graph + Warshall closure costs ~30s cold.

**Auto-discovery:** `scry finalize --index DIR --build-out PATH`
walks each indexed source root + each `--build-out PATH`
looking for `compile_commands.json` and `*.scip` artifacts.
Source roots honor `.gitignore` (vendored artifacts skip);
`--build-out` paths walk verbatim because build outputs
typically live in gitignored dirs (`out/soong`, `build/`,
`target/`). One cc.json and one *.scip per index — multiple
discovered candidates warn and skip, so user must pass
`--clang-compile-commands` / `--scip` explicitly to disambiguate.

## 9. CLI surface (concrete)

```
scry index [ROOT...] [--profile aosp|linux|generic] [--incremental] [--workers N]
                                   # accepts multiple roots; each can have a profile
                                   # incremental is manual — no inotify watcher
scry def    NAME [filters...]   [--json|--jsonl|--md]
scry ref    NAME [filters...]
scry callers NAME [filters...]
scry callees DEF  [filters...]
scry impls   IFACE
scry overrides METHOD
scry subtypes TYPE
scry members  TYPE
scry owner   PATH
scry fuzzy   STR
scry grep    PATTERN [filters...]   # rg-class speed
scry aidl-link NAME
scry flag     NAME                  # aconfig def + readers
scry service  NAME                  # init.rc service
scry sepolicy TYPE                  # SELinux type + rules
scry component NAME                 # AndroidManifest activity/service/...
scry xml-id   ID                    # @+id def + R.id readers
scry module-of PATH                 # which .bp declared this file
scry cflag    FLAG                  # who passes this compiler flag
scry serve  [--bind unix:/tmp/scry.sock|tcp:127.0.0.1:PORT]
scry stats                          # index size, freshness, lang breakdown
scry health                         # validates index integrity
```

`scry serve` exposes the same predicates over a JSON-RPC line protocol
(one JSON object per line, request/response keyed by `id`). This is the
intended LLM interface: an agent opens one connection at task start and
fires N queries against a warm mmap'd index, paying parse/load cost once.

## 10. LLM affordances (the part most tools get wrong)

Every result carries:
- **stable `symbol_id`** (deterministic hash of FQN + kind), so an agent
  can correlate across queries without re-resolving names.
- **`path:line:col` location**, always.
- **`scope`** as an ordered list (e.g. `["frameworks/base", "package com.android.server.am", "class ActivityManagerService", "method startActivity"]`).
- **`snippet`** — the enclosing definition, truncated to a configurable
  byte budget (default 1.5 KB), with `// …` elisions if it overflows.
- **`context`** — N lines around the match (configurable).
- **`module`** — Soong module name and type.
- **`owners`** — top OWNERS entries.
- **`lang`** and **`kind`**.

We provide a `--budget BYTES` flag for queries that caps the total response
size, dropping lowest-ranked results first. This matters because an agent
asking `scry ref Binder` should not get 200 KB of JSON.

Ranking inputs (in priority order):
1. exact name match > prefix > fuzzy
2. definition kind matches the predicate's expected kind
3. proximity to current working directory (if invoked from inside a
   subdirectory of AOSP, files there outrank others)
4. lang preference if `--lang` given
5. module fan-in (well-connected modules rank lower for breadth queries —
   you don't want `String` matches first)

## 11. Performance budget + how the indexer stays inside it

### Aspirational targets

| Op                                     | Budget       | Achieved (live AOSP+Linux index) |
|----------------------------------------|--------------|----------------------------------|
| cold full index                        | < 10 min     | 13.3 min (1.0M files, workers=16) |
| `scry def NAME` (warm)                 | < 10 ms      | 5–15 ms                          |
| `scry ref NAME` (warm, 1k refs)        | < 100 ms     | 80–150 ms (Layer 2 sidecar adds ~20ms) |
| `scry grep PATTERN`                    | within 2× rg | 30–45× FASTER than rg            |
| `scry fuzzy STR`                       | < 30 ms      | 150–250 ms (substring FST walk; over budget — see USAGE.md) |
| index size                             | < 6 GB       | 9.5 GB (refs + offsets + trigrams + file_symbols + resolutions; the columns are 4 GB total) |
| RSS for `scry serve`                   | < 1 GB       | 200–300 MB (lazy reader)         |

See `docs/BENCHMARKS.md` for the full measurement methodology and
per-pattern numbers; the indexing matrix and the perf-stat
decomposition (38% cache-miss rate = IO-bound, not CPU-bound) are
documented there.

### Resource envelope during indexing (the cgroup story)

Tree-sitter parses can transiently allocate gigabytes on adversarial
inputs. The full AOSP corpus has dozens of such files in the long
tail (large generated headers, machine-translated test fixtures,
proto-generated C++). A naive indexer OOMs the host once a week.

scry's defense in depth, outermost to innermost:

1. **systemd cgroup MemoryMax=60G.** Hard ceiling. The kernel OOM-
   kills the unit if RSS crosses this; `Restart=on-failure` brings
   it back and the `--resume` checkpoint picks up from the last
   completed batch. Worst case per OOM is one batch (≤ 5000 files,
   ~20-30 s) redone.

2. **MemorySwapMax=0.** Refusing swap means the OOM kill is fast
   (no thrash before the kernel gives up).

3. **`--mem-cap N` soft backpressure (default 40 GiB).** A heartbeat
   thread polls jemalloc's `stats.allocated` every 100 ms; at >80%
   of the soft cap, new file pickups pause via
   `await_memory_headroom()` until the heap drains. This is the
   first line of defense — bursts that *would* trip the cgroup get
   absorbed here.

4. **`--big-file-bytes 65536` serial routing.** Files larger than
   N bytes go into a serial bucket parsed one-at-a-time. Without
   this, two pathological large parses landing on different workers
   in the same batch can pile up gigabytes of in-flight ASTs.

5. **`--max-file-bytes 5242880` hard ceiling.** Files larger than
   5 MiB are skipped entirely with `[skip-large]` logged. Above
   this size, the file is almost certainly machine-generated and
   not worth parsing (a real AOSP source file > 1 MiB is rare
   enough to special-case if we ever hit one).

6. **`SCRY_PARSE_TIMEOUT_MS=60000` per-file parse budget.** The
   tree-sitter progress callback (post-2026-05-16; replaces the
   deprecated `set_timeout_micros` after observing >1h hangs on
   real AOSP Java) aborts any single parse exceeding 60 s. The
   file is skipped with `[ts-TIMEOUT]` logged — explicit, never
   silent.

7. **Auto OOM skiplist.** Each parse start writes its file path to
   `last_attempted.txt`. On resume, if the prior run's
   last_attempted is the SAME file as the one we'd reparse next,
   it's added to `oom_skiplist.txt`. Self-healing: a file that
   reliably OOMs gets skipped on the next attempt instead of looping.

8. **MALLOC_CONF aggressive return-to-OS.** jemalloc with
   `dirty_decay_ms:100,muzzy_decay_ms:100` releases freed pages
   back to the kernel within 100 ms, so RSS tracks current workload
   rather than accumulating a high-water mark across batches.

In practice (live indexer logs at `/mnt/agent/scry-index.log`):

- Steady-state RSS: 600 MB – 1 GB across the full AOSP+Linux run.
- Per-OOM cost in the worst-week run: ~3 OOMs total, each redoing
  one ~5 k-file batch (cumulative ~90 s of redo over 13 min).
- ts-TIMEOUTs per full run: 4–10 files, all in the long tail of
  machine-generated AOSP test fixtures.

The production wrapper that wires all of this is
`scripts/run_index.sh` + the `systemd-run --user --unit=scry-index`
invocation documented in `docs/OPERATIONS.md`. The post-finalize
chain (build-offsets → build-file-symbols → build-trigrams →
build-resolutions → validate → bench → email) runs automatically
via `scripts/await_finalize.sh`.

These are achieved, not aspirations; the milestone gates (§13) made
them concrete.

## 12. Risks and known unknowns

Status of each risk as of 2026-05-16. ✅ = mitigated/shipped,
⏳ = partial, 📋 = explicitly accepted, see linked follow-up.

1. ✅ **C++ resolution without compile commands is mediocre.**
   *Mitigation shipped.* `scry callers NAME --precise` routes
   through clangd via LSP (`crates/scry-cli/src/clangd.rs`,
   commit 6bf1b3d). Uses the real compiler's overload resolution.
   Requires `clangd` on PATH + compile_commands.json; clean error
   message when missing. Heuristic path stays default.

2. ⏳ **Tree-sitter-kotlin is the weakest of the major grammars.**
   *Partial mitigation.* We pinned `tree-sitter-kotlin-ng` (the
   actively maintained fork) and patched
   `kotlin_receiver_for_decl` to handle extension functions and
   extension properties correctly. Companion objects, sealed-class
   hierarchies, and `inline reified` fns are still tracked under
   "Known coverage gaps" in `docs/DEVELOPMENT.md`.

3. ✅ **AIDL cross-language linkage is the killer feature but
   also subtle.** *Mitigation shipped.* Every `interface IFoo` in
   an `.aidl` / `.hal` now emits synthetic shadow symbols
   (`IFoo.Stub`, `IFoo.Stub.Proxy`, `BpIFoo`, `BnIFoo`,
   `IFooAsyncServer`; HIDL: `BpIFoo`, `BnIFoo`, `BsIFoo`) via
   the new `SymbolKind::AidlShadow` / `HidlShadow` (commit
   f9a506f). `scry def IFoo.Stub` now lands on the AIDL source.
   The shadow-name set is pinned by tests so a toolchain rename
   is loud.

4. ✅ **Incremental index correctness.** *Shipped 2026-05-16.*
   `scry index --incremental` opens the existing index, diffs the
   source tree against `file_digests.bin`, re-parses only the
   changed + added files, replays unchanged records into a fresh
   staging dir, and atomically swaps it into place. The old index
   stays queryable for the whole rebuild; a mid-process crash
   leaves the old index intact. Foundation: `file_digests.bin` +
   tombstone bitmap + reader-side filter on every query path +
   `scry index-diff` + `scry tombstone` + `scry health` (commits
   c89ed40, e711c15, c2c8b9e). The remaining true append-only
   writer that mutates the index in place (preserving `file_id`s)
   is a perf-only optimization for huge corpora with tiny change
   rates — `docs/ROADMAP.md` § 2.

5. ✅ **Memory pressure during cold index.** *Mitigation
   shipped — 8-layer envelope.* cgroup `MemoryMax=60G` (outer),
   `--mem-cap` soft backpressure via jemalloc heartbeat, big-file
   serial routing, hard `--max-file-bytes` skip, per-file
   `parse_with_options` 60 s budget, auto OOM skiplist, jemalloc
   `dirty_decay_ms` aggressive return-to-OS, MemorySwapMax=0.
   See `§ 11` of this doc. Steady-state RSS on the live indexer
   stays 600 MB – 1 GB; target met with > 3× headroom.

6. ⏳ **Soong correctness.** *Hand-written parser ships with a
   broad test surface but doesn't fall back to a real Soong
   query.* `crates/scry-aosp/src/bp.rs` covers the common module
   shapes (module-type calls, named-arg lists, srcs/deps/cflags
   refs, anonymous assignments). Edge-case fallback to a real
   `b query` invocation is documented in DEVELOPMENT.md
   "Concrete pending items"; not blocking for the common case.

## 13. Phases and milestones

Status of each phase as of 2026-05-16. ✅ = exit-gate met,
⏳ = partial / deferred. Original design text preserved verbatim;
the status header reflects what shipped vs what was scoped down.

### Phase 0 — scaffold ✅ shipped

- Crates: `scry-walker`, `scry-lang`, `scry-store`, `scry-aosp`,
  `scry-cli` (we collapsed the planned `scry-index` and
  `scry-query` into the crates above; cleaner separation).
- `.gitignore`-aware walker excludes `out/`, `prebuilts/`, etc.
- **Exit gate met**: walks the full AOSP tree in ~25 s.

### Phase 1 — syntactic MVP ✅ shipped

- Tree-sitter integration for C, C++, Java, Kotlin, Rust, Go, Python.
- Per-language `.scm` query files extract definitions.
- Custom mmap'd columnar format (skipped LMDB intermediate per
  DESIGN-decision).
- `scry def NAME` works.
- `scry fuzzy STR` ships with Levenshtein automaton + Wagner-Fischer
  reranking (commit c6d4eab).
- **Exit gate met**: full AOSP+Linux index 13.3 min;
  `scry def Binder` returns 5–15 ms warm.

### Phase 2 — references and resolution ✅ shipped (core); ⏳ sugar commands

- Reference extraction for the seven languages.
- Layer 1 resolver (imports + same-file scope + inheritance).
- Layer 2 resolver via `scry build-resolutions` sidecar (89%
  of refs resolved on the live index).
- `scry ref`, `scry callers` shipped. `--precise` clangd routing for
  C++ shipped (commit 6bf1b3d).
- ⏳ Sugar commands `scry callees`, `scry overrides`, `scry impls`,
  `scry subtypes`, `scry members` not shipped as separate
  subcommands — achievable today via `def --kind X` / `ref --kind X`
  combinations. Documented as a follow-up in
  `docs/DEVELOPMENT.md` "Concrete pending items".
- **Exit gate met for the core**: `scry callers transact --lang Java`
  returns the right call sites in 80 ms warm.

### Phase 3 — AOSP build awareness ✅ shipped (core); ⏳ sugar commands

- Android.bp parser → module graph (deps, srcs, cflags, ldflags
  refs). Hand-written; 4 unit tests.
- Android.mk shallow parser; Bazel BUILD parser; CMake; GN; Kconfig.
- AIDL parser → cross-language linker via shadow symbols (commit
  f9a506f).
- OWNERS parser → `scry owner PATH` (this commit).
- Filters: `--in`, `--lang`, `--kind` shipped. `--module`,
  `--owner` not shipped as standalone filters; module info is
  reachable via `def NAME --kind soong` and `scry module-of`,
  ownership via `scry owner PATH`.
- `scry module-of`, `scry owner` shipped. The earlier `scry mod`
  sugar was removed in v0.1.2 — `def --kind soong` is the
  uniform spelling. ⏳ `scry aidl-link`, `scry cflag` not
  shipped — same `def --kind` / `ref --kind` story as Phase 2
  sugar.
- **Exit gate met for the core**.

### Phase 3.5 — AOSP platform configs ✅ shipped (parsers); ⏳ sugar commands

- aconfig parser (`crates/scry-aosp/src/aconfig.rs`); flags appear
  as `SymbolKind::AconfigFlag` queryable via
  `scry def NAME --kind aconfig`.
- init.rc parser → services as `SymbolKind::InitService`;
  query via `scry def NAME --kind init.svc`.
- sepolicy parser → types as `SymbolKind::SepolicyType`;
  `scry def NAME --kind sepolicy`.
- AndroidManifest.xml component extraction →
  `SymbolKind::ManifestComponent`.
- Resource XML id extraction → `SymbolKind::XmlId`.
- ⏳ Sugar commands `scry flag`, `scry service`, `scry sepolicy`,
  `scry component`, `scry xml-id` not shipped — all reachable as
  `def --kind X`. Sugar layer is a thin alias; queued for
  follow-up.
- Bash tree-sitter integration deferred (low corpus
  signal-to-noise).

### Phase 4 — speed and ergonomics ✅ shipped

- Custom mmap'd columnar format shipped (DESIGN § 5).
- Russ-Cox trigram index shipped (`FAST_PATH.md`).
- `scry serve` shipped with three transports: stdio,
  Unix-socket, TCP (commit 0025782).
- Streaming responses + `--budget BYTES` (commit e9784e1).
- Output formatters: `--json`, `--md` shipped; `--jsonl` is the
  default serve format.
- Incremental indexing shipped end-to-end: foundation
  (commit c89ed40) + selective-reparse rebuild (commit c2c8b9e).
  Atomic two-rename swap; old index stays queryable for the
  duration. True in-place append writer in `docs/ROADMAP.md` § 2.
- **Exit gate met**: warm `def` ~ 8 ms; grep is **30–45× faster
  than `rg`** (not within-2x — exceeded expectation).

### Phase 5 — precision uplift ✅ shipped (clangd path); ⏳ SCIP + Stack Graphs

- ✅ `scry callers NAME --precise` via clangd (commit 6bf1b3d).
  Covers the C++ overload-resolution use case SCIP-clang was
  originally planned for.
- ⏳ Direct SCIP file ingestion deferred — clangd-direct gives
  same precision without requiring users to manage SCIP files.
  If user demand for SCIP appears, the ingestion path is a
  contained add.
- ⏳ Stack Graphs experiment for Kotlin/Python and cross-language
  JNI binding inference not shipped. Both documented as future
  work in `docs/DEVELOPMENT.md`.

### Phase 6 — polish ⏳ partial

- Ranking shipped with composite `rank_score` (DESIGN.md § 5;
  4 unit tests on the tier ordering).
- ✅ `scry stats` shipped; ✅ `scry health` shipped this commit
  (validates every required + optional sidecar; non-zero exit
  on any required failure; spot-decodes the FST + lazy vec).
- ⏳ Web UI deferred (not blocking the LLM-agent or terminal use
  case; minimal effort to add if a browser-facing path becomes
  necessary).
- Packaging: cargo build produces a static binary; install
  scripts not yet shipped as a separate effort.

### Additional shipped beyond original Phase plan

- **MCP server** (`scry mcp`) — drop-in Model Context Protocol
  integration with required-arg validation and `isError`
  discipline. See `docs/MCP.md`.
- **Semantic retrieval** (`scry ask`) — embedding-based chunk
  search via the deterministic hashing trick; transformer model
  upgrade behind a future feature flag (ROADMAP § 1).
- **Memory primitives** (`scry recall`, `scry diff --since`) —
  thin readers over the ops log and git history for agent
  memory + PR-scoped exploration.

## 14. Decisions (resolved with user)

1. ~~**Name**~~ — `scry`. **Confirmed.**
2. ~~**Language**~~ — Rust. **Confirmed.**
3. ~~**Index location**~~ — `/mnt/agent/scry-index/`. **Confirmed.**
4. ~~**Source roots**~~ — multi-root. AOSP at `~/dev/aosp/`, Linux kernel
   at `/mnt/agent/dev/linux/`, plus any additional roots passed to
   `scry index`. Each root gets a **profile** (`aosp`, `linux`, `generic`)
   that controls which build-file parsers run and what gets ignored.
   **Confirmed.**
5. ~~**Incremental**~~ — manual only (`scry index --incremental`). No
   inotify watcher. **Confirmed.**
6. ~~**SCIP**~~ — Phase 5, opt-in precision uplift. SCIP = Sourcegraph
   Code Intelligence Protocol; per-language indexers like `scip-clang`
   that hook into the real compiler and emit precise references. We
   ingest `.scip` files when present and let them outvote tree-sitter
   answers. **Confirmed.**
7. **AIDL cross-linkage** — Phase 3 (AOSP-distinctive killer feature).
8. **LLM transport** — line-delimited JSON over a Unix socket from
   `scry serve`. Open to revisit if you want an MCP variant later.

---

When this is signed off (or revised), Phase 0 starts: install rustup,
create the cargo workspace, and ship the walker.

---

# Appendix A — Theory

This appendix exists because the three data structures that make scry
fast are not novel — they're the standard textbook structures —
but the *reason* each of them is the right shape for this workload
is easy to get wrong, and getting it wrong is the difference between
a 13-minute index and a 13-hour one, or a 600 ms grep and a 6-second
one. Each section below works from first principles, derives the
complexity, and then says what we actually picked and why.

## A.1 Inverted trigram indices: from a literal string to a candidate set

The query `scry grep ZygoteInit` ends in 600 ms over a 1 M-file
corpus. Naïve grep over the same corpus takes 5 minutes. Both touch
the same disk; the gap is entirely about *how many files each one
opens*. The trigram index is what closes that gap.

### A.1.1 The basic identity

For any string `P` of length `|P| ≥ 3`, define the set of trigrams

```
T(P) = { P[i..i+3]  :  0 ≤ i ≤ |P|-3 }
```

The set of files that *could* match `P` is exactly

```
candidates(P) = ⋂  posting(t)
              t∈T(P)
```

where `posting(t)` is the set of file IDs that contain the trigram
`t` anywhere. The identity is one-directional: a file in
`candidates(P)` may or may not contain `P` (it has all the trigrams,
maybe in the wrong order), but a file *not* in `candidates(P)` is
guaranteed not to contain `P`. So the candidate set is a *sound
over-approximation*. We still scan it with `memchr` to filter
false positives; the index turns "scan everything" into "scan a tiny
candidate set".

For `P = "ZygoteInit"` (10 bytes), `|T(P)| = 8`. Each posting
on the AOSP+Linux index is on the order of 10³–10⁶ files. The
intersection sequence

```
posting("Zyg") ∩ posting("ygo") ∩ posting("got") ∩ … ∩ posting("nit")
```

collapses to ~1400 candidate files. We open those 1400 files
instead of all 1 M, and the scan is over.

### A.1.2 Why `n = 3` specifically

Picking `n` is a tradeoff between two failure modes:

- **`n` too small** (e.g. `n = 1`): every posting list is enormous
  (most bytes appear in most files), every intersection still has
  ~all files, and the index is no better than full scan. With `n =
  2`, the dictionary is 65 536 keys and posting lists for common
  bigrams ("ing", " (", "()") still cover the bulk of the corpus.
- **`n` too large** (e.g. `n = 5`): the dictionary explodes (~2⁴⁰
  keys in the worst case), posting lists are short but storing them
  costs more than the saving, and *short queries can no longer be
  indexed at all* — `grep foo` (3 bytes) has zero 5-grams. The
  fallback to full scan eats the win.

`n = 3` puts the dictionary at most 2²⁴ ≈ 16.7 M keys (the actual
live AOSP dictionary is ~3.2 M keys; only a fraction of the 24-bit
space appears in real source), gives posting lists that intersect
down by 2–3 orders of magnitude on selective patterns, and lets
any literal `|P| ≥ 3` use the index. This is the same choice
Google Code Search made in 2012 ([Russ Cox][cox-trigram]) and
Zoekt, livegrep, and Hound have all converged on since. There is
no theoretical optimum — it's a discrete pareto frontier with `n=3`
near the knee for source-code workloads.

[cox-trigram]: https://swtch.com/~rsc/regexp/regexp4.html

### A.1.3 Posting-list encoding

Postings are stored sorted (file IDs ascending). Two encodings
matter:

1. **Delta encoding**: store `d_i = id_i − id_{i-1}` instead of
   `id_i`. Deltas are small for dense trigrams, and 0 is impossible
   (sorted set means strictly increasing), so we can use 1-based
   deltas without a sentinel.
2. **Varint** (LEB128): each `d_i` uses ⌈log₂(d_i+1)/7⌉ bytes.
   For a posting where the average delta is 10 (a trigram in ~10%
   of files), each entry costs ~1.5 bytes vs 8 bytes raw — a 5×
   shrink.

The combined shrink on the live index is roughly 8× over raw u64,
which is what gets the trigram payload from ~7 GB down to ~3 GB.

### A.1.4 Intersecting sorted posting lists in linear time

The classic algorithm — given `k` posting lists sorted ascending,
the merge is the obvious sweep:

```
heap of k iterators, sorted by current head;
loop:
  let m = min head;
  if all heads == m: emit m, advance all;
  else: advance the iterator with the min head;
```

Total cost is `O(N log k)` where `N = Σ |posting_i|`. For
selective queries the *smallest* posting dominates `N` (the
intersection cannot be larger than any input), so the work is
near-linear in the smallest posting — which is exactly the
workload we want.

scry's optimization is to **sort the trigrams by ascending posting
length before intersecting**. The smallest posting bounds the
candidate set, so picking the smallest two first prunes the working
set as fast as possible. Picking the largest first means carrying a
1 M-entry working set through the inner loop unnecessarily.

### A.1.5 From regex to trigrams (the livegrep trick)

`scry grep` accepts regex. For a regex `R`, what's the equivalent
of `T(P)`?

The Russ Cox / livegrep insight: walk the regex's syntax tree and
extract the longest prefix and suffix literals that every match
must contain. For `ActivityMgr.*Service`:

- Prefix literal: `ActivityMgr`
- Suffix literal: `Service`

Both must appear in any matching file. Trigrammify both, AND-intersect
their postings, scan the result with the full regex. For a regex
with no extractable literals (`.*foo.*` after the `.*` strip → just
`foo`, fine), the candidate set is the trigrams of `foo`. For a
regex genuinely without 3-byte literal anchors (`[a-z]+`), the
extractor returns empty and we fall back to full scan.

This is in `crates/scry-cli/src/main.rs::regex_literals_for_trigram`
with seven dedicated unit tests covering: literal anchor, prefix-
only, suffix-only, no-literal (correct fallback), nested alternation,
character class without literals, and the empty pattern edge case.

## A.2 Finite-state transducers for the symbol dictionary

`scry def Acti<TAB>` should return every symbol starting with
"Acti" in under 10 ms on a 22 M-symbol dictionary. The data
structure that makes prefix and fuzzy lookup that fast is the
*finite-state transducer* (FST), specifically a minimized,
sorted FST as implemented by Andrew Gallant's `fst` crate.

### A.2.1 Why not a hash map, a sorted vector, or a B-tree

| Structure          | Prefix? | Fuzzy?   | RAM for 22 M keys | Lookup latency |
|--------------------|---------|----------|-------------------|----------------|
| `HashMap`          | no      | no       | ~3 GB             | O(1) point     |
| sorted `Vec<&str>` | yes     | no       | ~1.5 GB           | O(log N) point |
| B-tree (LMDB)      | yes     | no       | ~2 GB on disk     | ~µs per node   |
| **FST (minimized)**| **yes** | **yes**  | **~280 MB mmap**  | **O(\|key\|)** |

The FST wins three ways at once:

1. **Sharing suffixes**: an automaton that accepts {`Activity`,
   `ActivityManager`, `ActivityThread`} shares the prefix `Activity`
   in the trie sense, but it also shares any *suffix* substructure
   between unrelated keys. Hopcroft-style minimization fuses
   equivalent subautomata, collapsing the structure from a trie
   (Σ|key|) to something much smaller — empirically ~12 bytes per
   key on real symbol dictionaries.
2. **mmap-friendly**: the FST serializes to a single byte array
   where every state's transitions live adjacent to it. Walking the
   automaton is sequential pointer-chasing in mapped memory; the
   page cache absorbs the working set and warm queries take
   microseconds.
3. **Prefix walk is free**: once you've walked the input prefix into
   some state `s`, enumerating completions is BFS from `s`. The
   cost is proportional to the *output set size*, not the
   dictionary size. This is the structural reason `scry prefix Acti`
   stays sub-millisecond regardless of how big the index grows.

### A.2.2 Fuzzy matching as automaton intersection

For fuzzy matching, the `fst` crate constructs a *Levenshtein
automaton* over the query at a fixed edit distance `k`, then
*intersects* it with the symbol FST. The intersection is itself an
FST — its accepted language is exactly "symbols within edit
distance `k` of the query". Walk it, emit accepted keys.

This is `O(|query| · k · |output|)` and crucially does *not*
materialize candidates that don't pass the edit distance — wrong
branches die at the automaton level before they reach the result
set. For `scry fuzzy ParcelFile --limit 10` on a 22 M-symbol index,
this runs in ~150–250 ms today, dominated by visiting the FST
states for matches not by enumerating the dictionary.

### A.2.3 Construction cost

Building a minimized FST requires the input to be **sorted**. scry
collects symbol names into per-batch sorted vectors during parsing,
then does a streaming k-way merge into a single sorted stream that
feeds the `fst::SetBuilder`. The merge dominates the build (it's
the only step that needs all keys in one place), but it's strictly
linear in the total input size and runs at the speed of sequential
disk reads. On the live index it takes ~25 s as part of the
finalize phase — small enough that we accept the constraint that
the FST cannot be updated in place. A reindex is the only way to
add new symbols today; the alternative (online FST construction)
costs more in code complexity than it saves in latency.

## A.3 The byte-offset sidecar: mmap + index beats deserialize-into-Vec

The third structure is so simple it's barely a structure: a packed
array of u64 byte offsets, one per record, written alongside the
records themselves. It exists because of a sharp asymmetry between
how bincode wants to be read and how the operating system wants to
serve data.

### A.3.1 The naïve approach and what it costs

bincode's natural API is `deserialize::<Vec<SymbolRecord>>(&bytes)`.
For a 10 GB columnar payload of 22 M symbol records, that:

- Allocates a `Vec` of 22 M `SymbolRecord`s in the heap (≈ 4 GB
  resident).
- Walks the entire byte slice from front to back, decoding every
  record, regardless of whether the query touches it.
- Burns ~400 ms of wall time on a warm page cache, ~4 s on cold.

The query, meanwhile, typically wants *one* record (a `def` lookup)
or a thousand (a `callers` query). The decode work and the
allocation are 99.9% waste.

### A.3.2 The sidecar trick

During finalize, while writing record `i` to `symbols.bin`, we also
write `byte_offset_of_record_i` as a fixed-width u64 little-endian to
`symbols_offsets.bin`. The sidecar is `8 · N` bytes for `N` records
— ~150 MB for the AOSP+Linux index, a 60× shrink over the payload.

To read record `i`:

1. `mmap` both files at startup (cheap — `mmap` is just a VM
   mapping; no IO yet).
2. Read `off = u64::from_le_bytes(&offsets_mmap[8i .. 8(i+1)])`.
   This is a single memory access into the offset sidecar; the
   kernel demand-pages the offset page on first touch.
3. `bincode::deserialize::<SymbolRecord>(&records_mmap[off..])`.
   Bincode reads exactly one record's bytes. The kernel demand-pages
   exactly the records page (and a few neighbors for prefetch).

Total: one `u64` read, one record decode, two minor page faults. On
the live index this runs in ~10 µs warm and ~100 µs cold. The RSS
footprint of the entire `StoreReader` is ~200 MB regardless of
index size — the records aren't *in* the process; they're in the
page cache, where the kernel manages them with global LRU across
the host.

### A.3.3 Why this is not just lazy loading

Lazy loading would still need to know where each record starts. The
naïve scheme — "scan forward until you've passed `i-1` length
prefixes" — is O(i) per lookup and pessimizes random access.
The offset sidecar makes record location *O(1)* without giving up
the page-cache benefit. The whole construction is a re-derivation of
the same trick used by sorted-string-table indexes (LevelDB,
RocksDB), file-system extents, and just about every other large-
scale columnar format. We chose to roll it by hand because the
payload format (bincode) was already fixed and we needed nothing
more than a sibling array.

### A.3.4 The page cache as a tier 1 cache

The deeper point: by sizing the index so that the *hot working set*
fits comfortably in the page cache and the *cold tail* lives on
NVMe, we get a two-tier cache for free, sized and managed by the
kernel. The `posix_fadvise(POSIX_FADV_WILLNEED)` prefetch in the
grep candidate scan (commit `014b061`) is the only place we hint
the cache manually — everywhere else, the kernel's default LRU
plus our access pattern (small offset reads → small record reads)
does the right thing.

This is why the perf-stat decomposition in `BENCHMARKS.md` shows
38 % cache-miss rate and ~70 % syscall time on a cold-cache grep:
not because the code is wrong, but because at that point the
remaining cost is the unavoidable IO to read the bytes the query
actually needs. The page cache is doing exactly what it's supposed
to.

## A.4 Why these three together

Each structure addresses a different cost:

- **Trigram index** turns a content query from "read every byte"
  into "read the bytes of the candidate files only".
- **FST** turns a name query from "scan a 22 M-row table" into
  "walk an automaton in time proportional to the answer".
- **Offset sidecar** turns a record fetch from "decode 10 GB" into
  "decode 128 bytes".

The three are independent — failing one degrades a single query
type to roughly the cost of `rg` or `grep -r`, not all of them.
Together they're what makes `scry` interactive on a corpus that's
two orders of magnitude larger than its working set.

## A.5 The computer-science scaffolding underneath

The previous four sections derive *what* scry does. This one names
*why* — the underlying CS concepts that the design rests on. None
of these are scry inventions; they're the standard apparatus from
the textbook chapters that justify each decision. The point of
collecting them here is that the design holds together only because
all of them are simultaneously true. Get any one wrong and a layer
above it collapses.

### A.5.1 The memory hierarchy and the external-memory model

A modern Skylake host has at least five tiers of storage, each
roughly 10× larger and 10× slower than the one above:

```
L1 cache        ~32 KiB         ~1 ns       per-core
L2 cache        ~256 KiB        ~3 ns       per-core
L3 cache        ~36 MiB         ~12 ns      per-socket
DRAM            240 GiB         ~80 ns      per-node
NVMe page cache 240 GiB(shared) ~80 ns      per-node, cached on demand
NVMe disk       ~2 TiB          ~80 µs      per-device
```

The conventional RAM model (every memory access is unit cost)
breaks at this scale; the right model is Aggarwal–Vitter's
**external-memory (EM) model** ([Aggarwal & Vitter,
1988][aggarwal-vitter]), where you count *block transfers* between
adjacent tiers, not individual loads. An EM-optimal algorithm
minimizes the number of times a tier-N block has to be fetched
from tier-N+1.

The three scry data structures are exactly the three classical EM
patterns:

- **B-tree-shaped lookup** (the FST): height `O(log_B N)` over
  blocks of size `B`, so a single lookup touches `O(log_B N)`
  blocks. For a minimized FST on 22 M keys with `B = 4 KiB` and
  realistic fanout, this is 3–4 page faults.
- **Sorted run + binary index** (the byte-offset sidecar over the
  records file): one block fetch to the offset, one to the record.
  This is the access pattern of every sorted-string-table file
  format ever shipped (SSTable, LevelDB, RocksDB), and the reason
  is EM-optimality, not aesthetics.
- **Inverted index with sorted postings** (the trigram index):
  intersection cost dominated by the smallest posting's block
  count, which is the *information-theoretic minimum* — the
  intersection can't be computed without reading at least one
  representation of the smallest input.

[aggarwal-vitter]: https://dl.acm.org/doi/10.1145/48529.48535

### A.5.2 The page cache as a cache-oblivious tier

Frigo, Leiserson, Prokop, and Ramachandran's **cache-oblivious**
result ([Frigo et al., 1999][cache-oblivious]) says: an algorithm
that has good EM behavior at *every* block size simultaneously
achieves EM-optimality at *every* tier — without knowing the tier
parameters. Reading a packed sequential record format via `mmap`
inherits this property for free: the kernel's page cache replaces
manually-tuned buffer pools, the prefetcher handles sequential
runs, and the working set is bounded by *the queries the user
actually runs* rather than by anything scry has to declare up
front.

The corollary is that scry has essentially no buffer management
code. We don't keep an LRU of decoded records; we don't size a
read cache; we don't even tune readahead. The kernel does all of
it, correctly, because `mmap` puts our reads on the same hot path
as every other file the OS has ever managed. We get LRU eviction
under memory pressure for free; we get prefetch of adjacent pages
for free; we get sharing across `scry` processes for free. The one
manual hint is `posix_fadvise(WILLNEED)` on grep candidate files,
and even that only matters because the access pattern is
*pseudo-random* across files — the kernel's sequential prefetcher
can't see it coming. Everywhere else, the cache-oblivious
guarantee holds.

[cache-oblivious]: https://erikdemaine.org/papers/FOCS1999b/

### A.5.3 Working-set theory and why the index size matters

Denning's **working-set model** ([Denning, 1968][denning]) says
that a program's performance is governed by the size of its
working set `W(t, τ)` — the distinct pages it references in the
last `τ` time units — relative to the available physical memory.
Below the threshold, page faults are rare and the program runs at
RAM speed; above it, every reference can fault and the program
runs at disk speed. The transition is sharp; this is what
"thrashing" actually is.

This is the *real* reason the trigram pre-filter is load-bearing.
A query that opens every file's pages drags the working set
across the threshold (the corpus is 70 GB; the page cache is
~150 GB but already populated by other workloads). A query that
opens 1400 files keeps the working set in the tens of MB — well
below the threshold, page faults stay rare, and the algorithmic
complexity actually translates to wall time. Without the
pre-filter, the "memchr is fast" claim is true on a microbenchmark
and meaningless on the real corpus.

[denning]: https://denninginstitute.com/pjd/PUBS/WSModel_1968.pdf

### A.5.4 Automata theory — Myhill–Nerode and why FSTs are minimal

The Myhill–Nerode theorem ([Hopcroft & Ullman, 1979][hop-ull])
says that the minimum DFA for a regular language `L` has exactly
one state per Myhill–Nerode equivalence class of `Σ*` under `L`.
Two prefixes are equivalent if every possible suffix produces the
same accept/reject decision. The minimum DFA is unique up to
state-renaming.

The `fst` crate's minimization step (Hopcroft's `O(n log n)`
algorithm) realizes this lower bound: the resulting automaton has
exactly as many states as Myhill–Nerode requires, no more. This
is *not* a 2× constant-factor win over a less-good representation;
it's the structural reason the FST shrinks from ~3 GB (trie) to
~280 MB (minimized) on the AOSP+Linux symbol set. Suffixes shared
across unrelated keys (`...Manager`, `...Service`, `...Activity`)
are stored once because their continuations are identical from
that point on, and Myhill–Nerode says you can't do better while
still recognizing the same language.

The fuzzy-search-as-intersection-with-a-Levenshtein-automaton
trick (§A.2.2) is the dual: regular languages are closed under
intersection, the intersection automaton has at most `|A| × |B|`
states, and walking it is the same algorithm as walking either
input. The whole construction is a single regular-languages
identity applied twice.

[hop-ull]: https://archive.org/details/introductiontoau0000hopc

### A.5.5 Sound over-approximations and the Bloom-filter analogy

The trigram filter is a sound over-approximation: it returns a
superset of the true matches, and the calling code verifies. This
is the same shape as a **Bloom filter** ([Bloom, 1970][bloom]) —
a probabilistic structure with one-sided error (false positives
allowed, false negatives forbidden) used to prune work before an
expensive exact check. The same pattern shows up in JIT compilers
(speculation + deoptimization), garbage collectors (card tables,
remembered sets), and database query planners (predicate pushdown).

Naming the pattern matters because it tells you the right
correctness argument: *false positives are tolerated* (they're
caught by the exact memchr scan; they cost a wasted file open but
not a wrong answer), but *false negatives must be impossible*
(any file that contains the pattern must be in the candidate
set). The trigram extractor in §A.1.5 satisfies this if and only
if every literal it returns is genuinely required by every match,
which is why the extractor has explicit unit tests for the
no-anchor case ("return empty, fall back to scan") rather than
ever silently returning a partial trigram set.

[bloom]: https://dl.acm.org/doi/10.1145/362686.362692

### A.5.6 Parallelism — work-stealing and the structure of the index pipeline

The indexer is a parallel pipeline: walker → parsers → resolver →
writers. The throughput on a 72-core host depends entirely on how
the work is distributed; naïve `for file in files { thread::spawn
}` is provably worse than nothing because the scheduling overhead
exceeds the per-file work.

scry uses **Blumofe-Leiserson work-stealing** ([Blumofe & Leiserson,
1999][bl-ws]) via rayon. The theorem: a fully-strict computation
with critical-path length `T₁` and total work `T_total` runs in
`O(T_total/p + T₁)` time on `p` processors with high probability.
For our pipeline, `T_total` is the sum of all per-file parse times
(~12 hours of single-threaded work) and `T₁` is the longest
individual file's parse (~60 s with the cap). Work-stealing makes
the actual wall time approach `T_total/p` once `p` exceeds the
critical path's reciprocal — which on this host means somewhere
between 8 and 16 workers, exactly the sweet spot the
`BENCHMARKS.md` matrix measured empirically.

Two practical consequences:

1. **Per-file work units must be small enough that critical path
   doesn't dominate.** This is why the `--big-file-bytes` serial
   bucket exists: a single 50 MiB generated header is its own
   critical path and would alone gate wall time, so we serialize
   the few of them away from the parallel pool rather than letting
   them sit in workers' deques starving the others.
2. **Workers shouldn't communicate.** Tree-sitter parsers aren't
   thread-safe, so each thread gets its own; the resolver runs in
   a second pass over collected output rather than locking a
   shared symbol table during parsing. Work-stealing is provably
   optimal only when the steal operation is cheap; if every steal
   contends on a lock, the bound degrades to serial.

[bl-ws]: https://supertech.csail.mit.edu/papers/steal.pdf

### A.5.7 Lock-free patterns where parallelism still has to share

Two places in the indexer have unavoidable cross-thread
communication: the OOM heartbeat thread (reads `jemalloc::epoch`
and pauses workers if the soft cap is exceeded) and the
progress-counter shared with the main thread for periodic reporting.

Both use **atomics** rather than mutexes. The memory-ordering
choices come from Lamport's **sequentially consistent** vs
**relaxed** memory models ([Lamport, 1979][lamport-sc]) — the
progress counter is `Relaxed` (we don't care about ordering with
other state, just that the count is eventually correct), while the
OOM gate is `Acquire`/`Release` (a worker that observes "paused"
must see the writes that justified pausing). These are not
optimizations; they are the *correctness specification* for the
data structure, and using `SeqCst` everywhere would silently
serialize the indexer through the global atomic store buffer on
x86, costing measurable throughput.

The deeper principle — **don't communicate, share state via
ownership** — is what most of the design follows. The walker
produces an immutable file list; the parsers consume it and
produce per-thread output; the writer consumes that and produces
the on-disk format. Each stage owns its data; there is no shared
mutable state between stages. This is the same idea as Hoare's
**Communicating Sequential Processes** ([CSP, 1978][csp]), realized
in rayon's API rather than channels, and it's the reason the
indexer scales linearly with cores up to the point where the EM
model says it can't.

[lamport-sc]: https://lamport.azurewebsites.net/pubs/multi.pdf
[csp]: https://www.cs.cmu.edu/~crary/819-f09/Hoare78.pdf

### A.5.8 Strings, automata, and why memchr beats KMP here

The classical string-search results — Knuth-Morris-Pratt (1977),
Boyer-Moore (1977), Aho-Corasick (1975) — give `O(n + m)` worst-
case bounds for pattern matching. They're all asymptotically
optimal in the comparison model.

scry uses none of them. The inner loop of the candidate scan is
SIMD `memchr` from the `memchr` crate, which is `O(n/w)` where
`w` is the SIMD word size (32 bytes on AVX2). Asymptotically
KMP/BM/AC are tied; concretely `memchr` is 5–20× faster because
modern CPUs reward straight-line vectorizable loops far more than
they reward fewer comparisons. This is the practical lesson of the
**RAM-with-vectors model** vs the comparison model: complexity
analysis tells you which algorithms are *competitive*; constant
factors and pipeline behavior pick the winner.

The trigram pre-filter changes the calculus: we've already
narrowed to ~1400 files, average ~5 KB each = ~7 MB of bytes to
scan. `memchr` does that in ~1 ms wall on cold cache. KMP would
do it in ~3–5 ms. Neither matters because the disk read is 470 ms.
This is the standard refrain — **once the algorithm is right,
the bottleneck moves to IO, and once IO dominates, micro-optimizing
the inner loop has zero leverage.** It's the reason the
perf-stat decomposition decomposes the way it does.

### A.5.9 Why all of this composes

Each chapter above gives a *local* correctness/efficiency
argument. The reason they compose into a working system rather
than fighting each other is the layering:

```
                            ┌──────────────────────────────────┐
                            │  query (CLI or JSON-RPC)         │
                            └──────────────────────────────────┘
                                          │
                                          ▼
       FST walk (A.2)                Offset sidecar (A.3)         Trigram intersect (A.1)
       O(|q|), 280 MB mmap     ───   O(1), 150 MB mmap     ───    O(smallest posting), ~3 GB mmap
                                          │
                                          ▼
                            ┌──────────────────────────────────┐
                            │       page cache (A.5.2)         │
                            │ cache-oblivious; manages all 3   │
                            └──────────────────────────────────┘
                                          │
                                          ▼
                            ┌──────────────────────────────────┐
                            │  EM model (A.5.1) / working-set  │
                            │  block transfers, LRU eviction   │
                            └──────────────────────────────────┘
                                          │
                                          ▼
                            ┌──────────────────────────────────┐
                            │  Skylake + NVMe — fixed costs    │
                            └──────────────────────────────────┘
```

The three index structures are independent (failing one degrades
one query type) but they all *rely on the same lower layers* —
they all assume the page cache will keep the hot pages resident,
they all assume EM-optimal access patterns, they all assume the
parallel pipeline above them produced consistent, sorted on-disk
formats. The reason scry is fast is not that any one of these is
clever; it's that they were chosen so the interfaces line up.

That's the L7 system-design instinct made concrete: design each
layer so the layer below it can stay generic, and so the layer
above it pays nothing for capabilities it doesn't use. The
texture of the design — Rust, mmap, FST, trigram, byte-offset
sidecar, work-stealing — is the boring downstream consequence of
that single instinct, applied consistently from the kernel page
cache up to the CLI.

