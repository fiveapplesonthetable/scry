# Contributing to scry

scry welcomes patches that fix bugs, close a documented test gap,
add a new file-format parser, or sharpen an existing query path.

## Before you start

- Read [`docs/DESIGN.md`](docs/DESIGN.md) for the as-built design
  and [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) for the
  workspace layout, build / test recipe, and code-quality posture.
- For non-trivial changes (new subcommand, new sidecar, new parser,
  a refactor that crosses crates), open an issue first sketching
  the design. Avoids dead-end work.

## Setup

Follow the **Prerequisites** + **First-time setup** sections of
[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md). The short form:

```sh
git clone https://github.com/fiveapplesonthetable/scry
cd scry
. ./env.sh                                           # optional path pinning
cargo build --release
cargo test --release --workspace                     # all 129 tests pass
cargo clippy --release --workspace --all-targets     # must be clean
```

## Sending a change

1. **Branch from `master`.** Name it after the change
   (`incremental-tombstone-fix`, `parser-cmake-set-list`, etc.).
2. **Keep the diff focused.** One concern per commit; one merge
   per concern. A bug fix doesn't need a refactor riding along.
3. **Tests are mandatory.** Any new code path needs a test that
   would fail without it. Use the existing patterns:
   - Per-parser happy-path test inline as `&str` fixture →
     `extract` → assert on `RawSymbol` list. See
     `crates/scry-aosp/src/bp.rs`.
   - Per-CLI command end-to-end via the synthetic tree in
     `crates/scry-cli/tests/e2e.rs`.
   - Reader/writer round-trip in `crates/scry-store/src/lib.rs`.
4. **Commit message format:**
   ```
   area: one-line summary (≤ 65 chars)

   What changed and why. The why matters more than the what —
   the diff already shows the what. If the change fixes a
   specific bug or closes a numbered ROADMAP item, name it.

   Co-Authored-By: ... <noreply@anthropic.com>     # if pair-coded
   ```
5. **Run the full gate locally before pushing:**
   ```sh
   cargo build --release
   cargo clippy --release --workspace --all-targets   # 0 warnings
   cargo test --release --workspace                   # all green
   ```
   CI runs the same gate. Failing builds get rejected.
6. **Open a PR** against `master`. Include in the description:
   what changed, why, what you tested, and (for new commands or
   flags) an example invocation + expected output.

## Code-quality bar

- Match the existing style — no need for a `rustfmt.toml`; default
  formatting is fine. Run `cargo fmt --all` if your editor doesn't
  do it for you.
- `#![forbid(unsafe_code)]` is on every crate except `scry-store`.
  Don't add `unsafe` outside `scry-store` without discussion.
- Workspace lints in `Cargo.toml` are strict on purpose. If a lint
  fires on legitimate code, prefer `#[allow(clippy::lint_name)]`
  with a one-line comment explaining why over silencing the lint
  workspace-wide.
- New comments should explain the **why**, not the **what** —
  the code shows the what.
- No emoji in code or commit messages unless the file already uses
  them (a few docs do, deliberately).

## Reporting bugs

Open a GitHub issue with:

- scry version (`scry --version`)
- OS + Rust version (`rustc --version`)
- Minimal command + index state that reproduces the bug
- What you expected vs what happened
- The relevant section of `~/.scry/queries.log` if it's a
  query-result discrepancy

For security issues, see [`SECURITY.md`](SECURITY.md).

## License

By contributing you agree your patch is licensed under Apache-2.0,
the same license as the rest of the codebase. No CLA.
