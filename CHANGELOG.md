# Changelog

All notable changes to scry are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/fiveapplesonthetable/scry/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/fiveapplesonthetable/scry/releases/tag/v0.1.0
