# scry

Semantic code search and cross-reference engine for AOSP.

**Status:** Design / research. No code yet — see `docs/DESIGN.md`.

## What it is

A single-binary, ripgrep-fast, build-aware code intelligence tool that indexes a
full AOSP checkout (~118 GB of source, ~735k files across C/C++, Java, Kotlin,
Rust, Go, Python, AIDL, .proto, Android.bp, Makefile, OWNERS) and answers
queries — definitions, references, callers, callees, hierarchies, fuzzy
symbol lookup, module-aware filters — in tens of milliseconds.

Built to be consumed by **both humans at a terminal and LLM agents over a
JSON/RPC protocol**. The index is read-only and mmapped, so many query clients
can share one warm index.

## Why not just $existing_tool

- **ctags / gtags / cscope**: tag-only, no real semantics, weak on Java/Kotlin,
  no build-graph awareness, single-threaded indexing.
- **ripgrep**: full-text grep only; no symbol model, no xrefs.
- **clangd / IntelliJ / Android Studio**: precise but per-language, slow to
  warm, can't scale to the whole AOSP tree at once, not LLM-shaped.
- **Sourcegraph / Zoekt**: closest in spirit, but heavyweight services not
  designed to be driven from a CLI loop in an LLM agent.
- **Kythe / Glean**: industrial-grade semantic graphs, but require deep
  per-language indexer integration and big infra.

`scry` aims for the **80% semantic precision of clangd, 80% speed of ripgrep,
100% breadth across AOSP languages** in one local binary.

## Constraints

- **Zero changes inside `~/dev/aosp/`** (no in-tree mutation, no symlink farm).
- Index lives off-tree on `/mnt/agent/scry-index/`.
- Static binary deliverable; no daemon required for one-shot CLI use, but a
  daemon mode is provided for hot indexes and LLM sessions.
