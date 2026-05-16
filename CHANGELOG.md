# Changelog

All notable changes to scry are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.7] — 2026-05-16

The "works on non-Android repos too" drop. Wires six new tree-sitter
parsers — TypeScript, Proto (proto2 + proto3), HTML, CSS, SCSS,
Markdown — so a single scry binary covers the perfetto trace_viewer,
its own Rust source, scry-ui, and any web-shaped repo with the same
zero-config flow as AOSP. Pure addition: every existing AOSP
extractor stays where it was; the new parsers slot in via the
existing FormatRegistry trait. Zero Android regressions (164 tests
pass, was 158).

### Added — language support

- **TypeScript** (`tree-sitter-typescript 0.23`) wired for `.ts` /
  `.tsx`. Captures class / interface / function / method / enum /
  type-alias / top-level variable. Built for the perfetto
  trace_viewer's ~5500 .ts files; works for any TS project.
- **Proto** (`tree-sitter-proto 0.4`) — proto2 + proto3 in one
  grammar. Captures message / enum / service / rpc. Uses the
  existing `ProtoMessage` / `ProtoEnum` / `ProtoService`
  SymbolKinds that were defined but unused before this release.
- **HTML** (`tree-sitter-html 0.23`) — captures `id=` / `name=` /
  any `attribute_value` text as XmlId symbols so JS-side
  `getElementById("x")` calls have a definition to resolve to.
- **CSS** (`tree-sitter-css 0.25`) — class selectors → Class,
  id selectors → XmlId, `@keyframes` names → Type.
- **SCSS** (`tree-sitter-scss 1.0`) — superset of CSS captures
  plus `@mixin` and `@function` definitions as Function-kind.
  perfetto's UI uses SCSS as the primary stylesheet format.
- **Markdown** (`tree-sitter-md 0.5`) — atx + setext headings as
  Module-kind symbols. `scry def "Verification checklist"` jumps
  to that section of DEVELOPMENT.md without grepping.

### Added — corpora validated this release

| Corpus     | Files | Symbols | Notes                          |
|------------|------:|--------:|--------------------------------|
| perfetto   | 40478 | 1.26 M  | TS UI + proto + C++/Python + SCSS/CSS/HTML + GN — full coverage, 8 min on 12 workers |
| scry repo  |    53 |    1357 | Rust + Bash + Markdown headings; 591 ms cold |
| scry-ui    |    43 |     746 | TypeScript + SCSS + HTML; 100 ms cold |

All three exercised end-to-end this release. `scry def Track --lang
TypeScript` lands on the interface in `perfetto/ui/src/public/track.ts`;
`scry def TracePacket --kind proto.msg` lands on the proto definition;
`scry def "Quickstart"` finds every Markdown Quickstart heading in
the perfetto docs tree.

### Improved

- `short_lang()` (the display-side lang chip on result rows) now
  knows ts/html/css/scss/md so result lines say `(iface ts)` not
  `(iface ?)`.
- README "Also works on" section names the three non-AOSP corpora
  scry was exercised against this release, with the indexed
  file/symbol/time numbers.
- DEVELOPMENT.md's "Known coverage gaps" tightened to reflect
  reality — assembly stays a gap; bash / TS / proto / HTML / CSS
  / SCSS / Markdown have moved out of "gap" and into the
  positive-coverage table above it.

### Migration notes

None. The added grammars are pure additions to scry-lang; the
walker just learned five new file extensions (`.ts` / `.tsx` /
`.html` / `.htm` / `.css` / `.scss`) to classify (Markdown
classification was already in place). Existing AOSP indexes can
be reused; rebuild only if you want symbol coverage on web /
proto / docs files that were walked-but-not-symbolized before.

## [0.1.6] — 2026-05-16

The DEVELOPMENT.md sweep. Worked the "What's left" / "Concrete
pending" / "Experiments" backlog end-to-end: kept the ideas that
earned it, deleted the ones that didn't, measured the ones that
needed measuring. Every item now lives somewhere — implemented,
discarded with a rationale in DEVELOPMENT § "Decisions", or
sitting in BENCHMARKS § "Investigation findings" as a
result-of-record.

### Added

- `scry stats --json` for machine consumption. Stable shape:
  scry_version + manifest_version + indexed_at + roots +
  files_total/parsed/failed + bytes_total + symbols + refs +
  elapsed_ms + by_lang + by_kind histograms. Pinned by e2e.
- `scry grep --explain` query plan dumper. Lists every extracted
  trigram (smallest-first, with posting size), the final
  candidate count, and a rough scan-cost estimate. Short-circuits
  the scan; use to diagnose why a grep feels slow.
- `scry owner PATH --accumulate` emits the union of emails across
  every visited OWNERS layer (the Gerrit "potential approvers"
  set). OWNERS chain walk now respects `set noparent` (and the
  bare `noparent` form) per Gerrit semantics.
- AIDL frozen-version kind (`aidl.frozen`). Files under
  `aidl_api/<pkg>/<N>/` are promoted from `aidl.iface` so agents
  can filter `--kind aidl.frozen` to scope to a specific frozen
  surface version.
- JNI binding shadows (`SymbolKind::JniBinding`, "jni"). Every
  Java `native` method emits a synthetic symbol named after the
  standard JNI mangling (`Java_<pkg>_<class>_<method>` with
  `_`→`_1`, `$`→`_00024`, `;`→`_2`, `[`→`_3`). `scry def
  Java_android_os_Parcel_nativeWriteString` now lands on the
  Java declaration even when the C++ side is missing.
- Kotlin companion-object coverage. Anonymous `companion object
  { ... }` injects a synthetic Class symbol named "Companion"
  scoped to the enclosing class; members get `[Outer, Companion]`
  scope. Named companions captured directly.
- Layer 2 narrowing for Kotlin and C++. Kotlin mirrors Java
  (same-package → explicit import → wildcard → implicit-import
  fallback over kotlin / kotlin.collections / kotlin.io /
  kotlin.text / ...). C++ does same-namespace > using-namespace
  > fallback via a new `RefKind::UsingNamespace`.
- Bash tree-sitter wiring (`tree-sitter-bash 0.25`) — captures
  function definitions + top-level variable assignments. Surfaces
  AOSP envsetup.sh's `lunch / mm / mmm / croot` family.
- Auto-narrow hint: when a result set saturates `--limit` and
  shown paths share a 2+ segment common directory prefix, a
  stderr line suggests `--in <prefix>/` to narrow. Suppressed
  by `SCRY_QUIET=1`.
- Nightly rebuild systemd .timer recipe in OPERATIONS.md.

### Improved

- `Manifest::version` hoisted to `MANIFEST_VERSION` const with a
  documented bump policy.
- Coverage `--json` shape pinned by an explicit e2e shape assertion.
- `unbounded_parse_returns_tree` test strengthened: compares the
  `timeout=0` result to a reference `parse_with_options(_, None)`
  call (root kind, byte range, node count, no parse error).
- `validate.sh` hard-fails if `def Activity --kind class` returns
  an `api/*.txt` first hit instead of a `.java`/`.kt` source.
- Resolver tests grew from 7 (Java only) to 13 (Java + Kotlin +
  C++); resolve_one has line-by-line coverage of every narrowing
  path it can take.

### Removed

- `tracing` + `tracing-subscriber` deps. The subscriber was
  initialized in `main()` but no code emitted through it;
  eprintln! is the convention. -127 Cargo.lock entries.
- `crossbeam` + `toml` from workspace deps (declared but never
  imported by any crate).

### Investigated

Findings in BENCHMARKS § "Investigation findings":
- Cold-vs-warm `def` gap: ~300 ms (not 7 ms); page-fault dominated.
- Cold-grep cache-miss rate: 17.7 % (not 38 %); IO-bound.
- `lto=thin` payoff: sub-1 % perf delta on warm grep.
- ts-TIMEOUT recurrence: same two libwebsockets files every run.

### Documentation

- DEVELOPMENT.md collapsed 747 → 578 lines. "Known coverage
  gaps" / "What's left" / "Concrete pending" / "Experiments"
  merged into "Roadmap" + "Things worth investigating" +
  "Decisions: ideas considered and not pursued".
- BENCHMARKS.md gets an "Investigation findings" appendix.
- OPERATIONS.md documents the nightly-rebuild .timer recipe.

## [0.1.5] — 2026-05-16

The capacity-caps + agent-affordances drop. Address the
explicit ask "do the CPU/mem caps work for query, not just
indexing?" plus the long-deferred `scry tldr` and a
counter-intuitive finding from the small-model retest.

### Added
- **`scry serve --max-conns N`** — bound concurrent connections
  to the daemon. Each accepted connection runs grep with its
  own rayon pool, so unbounded fan-in × per-query fan-out can
  OOM a host. `0` (default) preserves prior unlimited
  behavior. Over-cap accepts receive a JSON-RPC error
  (`code: -32004`, `data.retryable: true`) before the server
  closes the connection — clients see an actionable hint, not
  silent EOF. An RAII `ConnSlot` guard releases the slot even
  on panic. USAGE.md "Index admin" gets a new subsection
  documenting the cap reply + the standard Unix tools for
  inspecting / killing the daemon (`ss`, `lsof`, `pkill`). New
  e2e regression `unix_serve_max_conns_drops_over_cap` asserts
  the error code, the `retryable: true` flag, and the stderr
  log line.
- **`scry tldr PATH`** — one-call file summary: language,
  total symbol count, per-kind histogram, top 3 ranked symbols
  (by `rank_score`), and the first non-blank line of the file
  (typically the package decl or leading docstring). Cuts ~70%
  of the tokens vs `outline + 3×def` for "what does this file
  do?" agent queries. Exposed as the `tldr` MCP tool. New e2e
  block exercises both JSON and plain output shapes.
- **Strengthened MCP tool descriptions.** Every tool's
  description now leads with the most common failure mode an
  agent will hit (e.g. `def` opens with "If a name is common
  (Activity, Binder), ALWAYS pass `kind` and/or `lang`";
  `limit` reminds "Do NOT pass placeholders like 'N'"). Helps
  ≥3B-class models meaningfully; see AGENT_NOTES §6.5 for the
  honest counter-finding on ≤1B models.
- **DESIGN §6.5 — Ranking and narrowing heuristics.** Full
  documentation of `rank_score` (kind tiers, lang penalty,
  scope penalty), grep candidate path-quality penalty, Layer 2
  resolver narrowing rules per language (Java's pkg → import →
  wildcard → fallback chain), trigram intersection ordering,
  and fuzzy ranking composition. Tied to the source files that
  implement each.
- **DEVELOPMENT.md toolchain install commands** for Ubuntu /
  Debian / Fedora / Arch / macOS. rustup one-liner + the
  optional clang / ripgrep packages.

### Tests
- 144 → **146 tests** across the workspace. New: serve
  `--max-conns` over-cap drop regression, `scry tldr` JSON +
  plain output assertions.

## [0.1.4] — 2026-05-16

The small-model-comparison drop. Ran Qwen 2.5 0.5B (Ollama, CPU)
against the same task I'd give myself (Claude) and captured the
interaction patterns. The comparison surfaced one real
consistency gap that small models hit hardest.

### Added
- **`--format count`** on `scry callers` and `scry ref`. Emits
  one short line (`N callers` / `N ref`) for the "how many
  references does X have?" agent query. Was only on `grep`
  before; small models reach for verb-only commands and
  shouldn't need to count lines themselves. Mutually exclusive
  with `--json`. New e2e regression block.
- **AGENT_NOTES §6.5** — a real Qwen 2.5 0.5B vs Claude
  side-by-side on the BatteryStats / noteAlarmStart task.
  Verbatim prompts, verbatim outputs, actual scry invocations,
  what Qwen got wrong and why (missed `--kind class`, missed
  `--lang Java`, used literal `N` instead of a number),
  measured timing (823 s for 200 tokens at 0.4 t/s on CPU).
- **AGENT_NOTES §6.6** — updated 8B-model recommendation,
  reflecting what the comparison taught: default `--format
  count` on first-invocation, expose `with_snippets` via
  outline, hint at `--kind` for ambiguous `def`.

### Tests
- 143 → 144 tests across the workspace (new `callers --format
  count` + `ref --format count` + `--format + --json` mutual-
  exclusion checks).

## [0.1.3] — 2026-05-16

The user-shouldn't-have-to-know drop. The stale-index version
skew check that v0.1.2 added to `scry health` was the right
diagnostic but the wrong UX — most users won't think to run
`scry health` before believing query results. Fixed: scry now
warns automatically.

### Added
- **Auto stale-index warning.** Every command that opens an
  index now emits a one-line stderr warning if the manifest's
  `scry_version` doesn't match the running binary. The warning
  is informational — queries still run — and includes the exact
  rebuild command. Catches the silent-bad-data class of bug
  (e.g. the pre-0.1.2 Java/C++ scope_path double-encoding) the
  moment it could mislead a result.
- **`SCRY_QUIET=1`** env var to suppress the warning. For CI,
  scripted use, or operators who've consciously decided to keep
  using a known-older index.
- New e2e regression test `stale_index_emits_warning_on_every_open`
  pinning: warning fires by default, `SCRY_QUIET=1` suppresses,
  matching versions stay silent.

## [0.1.2] — 2026-05-16

The LLM-self-test drop: drove `scry mcp` end-to-end as an agent
would, fixed every paper-cut it surfaced, hardened the queries.log
for long-running MCP sessions, and pruned one sugar command that
didn't earn its keep.

### Added
- **`scry grep --format=lines`** — `path:line:col\tsnippet` rg-shape,
  one hit per line. 5–10× cheaper in tokens vs `--json` for
  "list call sites of X" agent queries.
- **`scry grep --format=count`** — just `N hits across M files`,
  no per-hit rows. Cheapest possible "is X referenced AT ALL?"
  reply.
- **`scry outline --with-snippets N`** — inline the first N source
  lines of each symbol so the agent doesn't need a per-symbol
  `def` round-trip. JSON gets a `snippet` field; plain output
  shows snippet blocks with `│` separators. Lines clip at 200
  chars to bound the worst case.
- **`SCRY_LOG_MAX_BYTES`** env var (default `100 MiB`) — rotates
  `~/.scry/queries.log` to `<path>.1` when it crosses the cap,
  bounding total disk to 2 × cap. `0` disables rotation.
  **`SCRY_LOG=`** (empty) disables logging entirely for ephemeral
  MCP sessions. Matters at MCP scale where a tight loop can
  write ~6 M rows / week.
- **`queries.log` schema** gains `scry_version` and `pid` fields
  so usage analysis can disambiguate parallel callers and
  correlate latency with code versions. Documented schema +
  `jq` / DuckDB analysis recipes in USAGE.md "Ops log".
- **`scry health`** now surfaces the `scry_version` that built
  the index alongside the running binary's version. A mismatch
  is a soft warning (rebuild recommended), not a failure.
- **THEORY.md Chapter 14** — the LLM-agent surface (JSON-RPC,
  MCP, token economy, persistence).
- **THEORY.md Chapter 15** — scaling beyond the canonical
  corpus, with concrete knobs for 3 M-file / 200 GB+
  internal-master setups.

### Fixed
- **MCP tool-error envelope was double-encoded.** Found by
  LLM-self-test: an `ask` against an index without embeddings
  returned `content[0].text = "{\"error\":\"no embedding
  sidecar…\"}"` — an LLM had to `json.parse` twice to find the
  hint. Now unwraps to the bare message. Regression test
  pinned.
- **Java/C++ scope_path doubled the class's own name.** Pre-
  704d917, every top-level class had `scope: [ClassName]` and
  `fqn: "ClassName::ClassName"`. The parser fix shipped; this
  release adds three `scope_regression_tests` (Java top-level,
  Java nested, C++ top-level) pinning the contract so a
  tree-sitter upgrade can't re-introduce the bug silently. Plus
  the version-skew warning in `scry health` to surface stale-
  index data built with the buggy older scry.
- **Friendlier first-run error.** A user who runs `scry def Foo`
  before ever building an index got `No such file or directory
  (os error 2)`. Now they get a clear "no scry index at <path>"
  + an actionable command to build one.
- **TCP listener** now logs the actually-bound address
  (`listener.local_addr()`) rather than the user-supplied
  string. Matters when binding to port 0 — without this you
  have no way to discover the resolved port.

### Removed
- **`scry mod NAME`** — pure sugar for `def NAME --kind soong`,
  duplicated the API surface for marginal convenience. Use the
  uniform `--kind` spelling instead.

### Tests
- 134 → **143 tests** across the workspace. New: scope-
  regression suite (3 tests), MCP tool-error unwrap (1),
  log rotation pure-helper (4), grep `--format` + outline
  `--with-snippets` e2e blocks (4 assertions).

## [0.1.1] — 2026-05-16

The release-polish drop: everything that should have been in v0.1.0
but wasn't. No new query features; no on-disk format changes.

### Added
- `LICENSE` file at repo root (Apache-2.0 full text).
- `CONTRIBUTING.md` with the contribution workflow.
- `SECURITY.md` with responsible-disclosure instructions.
- `CHANGELOG.md` (this file).
- `.github/workflows/ci.yml` — runs `cargo build --release`,
  `cargo test --release --workspace`, and `cargo clippy --release
  --workspace --all-targets -- -D warnings` on every push and PR
  against `master`. PRs that introduce a warning or a test
  failure are rejected at the CI gate.
- `scry completions <shell>` — emit shell completions to stdout
  for bash / zsh / fish / powershell / elvish via `clap_complete`.
- `scry man` — emit a roff-formatted man page to stdout via
  `clap_mangen`.
- README "Install" section pointing at the GitHub release asset
  and documenting the `cargo install --git` fallback.
- Prebuilt release binary attached to the v0.1.1 release:
  `scry-x86_64-unknown-linux-gnu.tar.gz`.

### Tests
- TCP listener path (`scry serve --listen tcp:127.0.0.1:0`) —
  round-trip a `def` query over a real TCP connection.
- Concurrent serve under load — 32 client threads, each sending
  10 queries against the same Unix-socket server, all must
  receive consistent results without panic or hang.
- `scry callers --precise` against a malformed
  `compile_commands.json` — must fail gracefully (clean error,
  non-zero exit), not panic or hang.
- Parse-budget timeout — pathological tree-sitter input with a
  1 ms budget must abort cleanly and continue with the rest of
  the corpus.

### Fixed
- DEVELOPMENT.md line 399 said "all 80 tests pass" — was stale;
  now reads 129 (the actual number).
- MCP `initialize` now negotiates protocol version per spec
  (echoes client's version when supported, otherwise replies
  with our latest). Previously hard-coded `2024-11-05`.

## [0.1.0] — 2026-05-16

First tagged release. Full feature surface implemented and tested.

### Added
- **Indexing**: 1M-file AOSP+Linux corpus in 13.3 min on a
  72-core host. cgroup-enveloped, OOM-resumable, jemalloc-
  backpressured. 40 file categories. Tree-sitter for source
  languages; custom parsers for Soong, AIDL, HIDL, init.rc,
  SELinux, AndroidManifest.xml, Bazel, CMake, GN, Kconfig,
  Makefile, Gradle, OWNERS, aconfig, api/*.txt.
- **Querying**: `def`, `ref`, `callers`, `prefix`, `fuzzy`,
  `grep` (literal + regex), `outline`, `coverage`, `stats`,
  `ask`, `diff --since`, `recall`, `owner`, `module-of`.
- **Incremental**: `scry index --incremental` re-parses only
  changed + added files, replays unchanged records, atomically
  swaps the new index into place. Sub-second on small change
  sets.
- **Transports**: stdio CLI, JSON-RPC via `scry serve` (stdio /
  Unix-socket / TCP), MCP via `scry mcp` (Claude Desktop, Cursor,
  Continue, custom). MCP supports protocol versions 2024-11-05
  through 2025-11-25.
- **Precision uplift**: `scry callers NAME --precise` via
  clangd for type-aware C++ references.
- **Sidecars**: file_digests, file_symbols, ref_resolutions,
  trigrams, chunks/embeddings — all built by separate `build-*`
  subcommands or inline with `--build-*` flags on `scry index`.

### Engineering posture
- 129 tests across the workspace; ~3 s end-to-end.
- Zero clippy warnings under strict `[workspace.lints]` policy.
- Pre-release discipline: no backward-compat shims carried.

[Unreleased]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/fiveapplesonthetable/scry/releases/tag/v0.1.0
