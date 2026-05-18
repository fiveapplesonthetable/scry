//! `scry-kzip` — Kythe kzip ingest path.
//!
//! Turns a single `.kzip` produced by a Kythe extractor (AOSP via
//! `build/soong/build_kzip.bash`, Bazel via the equivalent, any custom
//! Kythe pipeline) into scry's mmap-packed precision sidecars
//! (`clang_usrs.bin` for C/C++/ObjC, `scip_index.bin` for everything
//! else with symbol identity).
//!
//! ## Pipeline
//!
//! ```text
//!   kzip
//!    +-- walker.rs    open kzip, iterate root/pbunits/*
//!    +-- dispatch.rs  per CU language → IndexerKind
//!    +-- indexer.rs   spawn the right Kythe indexer once per language
//!    +-- entries.rs   decode delimited Entry protos, aggregate
//!    +-- emit.rs      flush aggregated decls / refs into packed sidecars
//!    +-- driver.rs    orchestration + phase logging + env knobs
//! ```
//!
//! ## Per-language coverage (Kythe v0.0.75 public release)
//!
//! | Language        | Indexer            | Sidecar           |
//! |-----------------|--------------------|-------------------|
//! | C / C++ / ObjC  | `cxx_indexer`      | `clang_usrs.bin`  |
//! | Java            | `java_indexer.jar` | `scip_index.bin`  |
//! | Kotlin          | `jvm_indexer.jar`  | `scip_index.bin`  |
//! | Go              | `go_indexer`       | `scip_index.bin`  |
//! | proto           | `proto_indexer`    | `scip_index.bin`  |
//! | textproto       | `textproto_indexer`| `scip_index.bin`  |
//! | Rust            | not in v0.0.75     | skipped + logged  |
//!
//! Kotlin source-level indexing is Google-internal; we ingest the
//! post-compile bytecode shipped in the kzip via `jvm_indexer.jar`
//! instead — less precise than a source-level walker but produces real
//! Kythe entries with symbol identity.

pub mod dispatch;
pub mod driver;
pub mod emit;
pub mod entries;
pub mod indexer;
pub mod proto;
pub mod walker;

pub use driver::{build_packed_from_kzip, KzipBuildReport};
