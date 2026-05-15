# scry — design

Status: draft, pre-implementation. Comments and pushback welcome before code
lands.

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
scry mod     NAME
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

## 11. Performance budget

Targets on this host (72 cores, NVMe-backed `/mnt/agent`):

| Op                                     | Budget       |
|----------------------------------------|--------------|
| cold full index                        | < 10 min     |
| incremental (one file)                 | < 200 ms     |
| incremental (one Soong module)         | < 5 s        |
| `scry def NAME` (warm)                 | < 10 ms      |
| `scry ref NAME` (warm, 1k refs)        | < 100 ms     |
| `scry grep PATTERN`                    | within 2× rg |
| `scry fuzzy STR`                       | < 30 ms      |
| index size (no SCIP)                   | < 6 GB       |
| index size (with full SCIP-clang)      | < 30 GB      |
| RSS for `scry serve`                   | < 1 GB       |

These are aspirations; the milestone gates (§13) make them concrete.

## 12. Risks and known unknowns

1. **C++ resolution without compile commands is mediocre.** Headers,
   templates, overload sets, ADL — tree-sitter alone can't do this well.
   Mitigation: ship the SCIP-clang uplift path early; document the gap.
2. **Tree-sitter-kotlin is the weakest of the major grammars.** Kotlin is
   first-class in AOSP. We may need to invest in fixing/patching the
   grammar, or pair it with a custom symbol extractor.
3. **AIDL cross-language linkage is the killer feature but also subtle.**
   Generated names follow rules (`IFoo.Stub`, `BpFoo`, `BnFoo`, mangled C++
   names). We need to model these rules precisely or the linkage breaks.
4. **Incremental index correctness.** Tombstones, atomic swaps, watcher
   races. We need a `scry health` command that can detect drift and an
   automated reindex fallback.
5. **Memory pressure during cold index.** A naive design loads all
   symbols in memory before flushing. We must stream-flush to keep RSS
   bounded (target < 4 GB during indexing).
6. **Soong correctness.** Hand-written Blueprint parser will be wrong in
   edge cases. Fallback: shell out to a real Soong query if available, or
   accept the imprecision for the module graph.

## 13. Phases and milestones

Each phase ends with a measurable artifact and a runnable demo.

### Phase 0 — scaffold (1–2 days)

- Install rustup, set up workspace.
- Skeleton crates: `scry-cli`, `scry-index`, `scry-walker`, `scry-lang`,
  `scry-store`, `scry-query`.
- `scry index` walks the tree and prints file counts by language.
- `.gitignore`-aware walker excludes `out/`, `prebuilts/`, etc.
- **Exit gate**: walks the full AOSP tree in < 30 s, reports counts
  matching `notes/AOSP_SCALE.md`.

### Phase 1 — syntactic MVP (1 week)

- Tree-sitter integration for C, C++, Java, Kotlin, Rust, Go, Python.
- Per-language `.scm` query files extracting definitions only.
- LMDB-backed symbol store (defer custom format).
- `scry def NAME` works.
- `scry fuzzy STR` works via FST.
- **Exit gate**: full AOSP index < 30 min, `scry def Binder` returns the
  Java + native definitions in < 50 ms.

### Phase 2 — references and resolution (1 week)

- Reference extraction for the seven languages.
- Layer 1 resolver (imports + same-file scope + inheritance).
- `scry ref`, `scry callers`, `scry callees`, `scry overrides`, `scry
  impls`, `scry subtypes`, `scry members`.
- **Exit gate**: `scry callers Binder.transact --lang java` returns ≥ 95%
  of what IntelliJ finds in `frameworks/base`.

### Phase 3 — AOSP build awareness (1 week)

- Android.bp parser → module graph (with cflags, deps, visibility).
- Android.mk shallow parser; Bazel BUILD parser.
- Kconfig parser.
- AIDL parser → cross-language symbol linker.
- OWNERS parser → owner queries.
- Filters: `--module`, `--owner`, `--in`, `--lang`, `--kind`.
- `scry mod`, `scry owner`, `scry aidl-link`, `scry module-of`, `scry cflag`.
- **Exit gate**: `scry callers IBinder#transact --aidl-link` returns Java
  *and* native call sites; `scry mod services.core` returns srcs + deps;
  `scry module-of frameworks/base/services/core/.../ActivityManagerService.java`
  returns `services.core`.

### Phase 3.5 — AOSP platform configs (a few days, parallelizable with 3)

- aconfig parser + flag-read pattern matchers in Java/Kotlin/C++ for
  `Flags.FOO_BAR` / `aconfig_flags_FOO_BAR()` callsites.
- init.rc parser → services, `on` blocks, property triggers.
- sepolicy parser → types and rules.
- AndroidManifest.xml component extraction; resource XML id extraction.
- Bash/sh tree-sitter integration with `source` cross-file linkage.
- `scry flag`, `scry service`, `scry sepolicy`, `scry component`,
  `scry xml-id`.
- **Exit gate**: `scry flag <pick-a-flag>` returns the `.aconfig`
  definition and every Java + native read; `scry service zygote` finds
  init.rc decl + binary + start sites.

### Phase 4 — speed and ergonomics (1 week)

- Replace LMDB symbol/file tables with custom mmap columnar format.
- Zoekt-style trigram index for `scry grep`.
- `scry serve` JSON-RPC daemon.
- Output formatters: `--json`, `--jsonl`, `--md`, `--budget`.
- Incremental indexing via inotify.
- **Exit gate**: warm `scry def` < 10 ms; `scry grep` within 2× of
  ripgrep on the same corpus; `scry serve` survives 10k queries / minute
  from a loop.

### Phase 5 — precision uplift (opt-in, later)

- Ingest SCIP files from `scip-clang`, `scip-java`, `scip-typescript`, etc.
- Stack Graphs experiment for Kotlin/Python.
- Cross-language JNI binding inference (Java `native` ↔ C++
  `Java_pkg_Class_method`).

### Phase 6 — polish

- Ranking tuning.
- `scry health`, `scry stats`.
- Optional minimal web UI (single page that wraps `scry serve`).
- Packaging and install scripts.

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
