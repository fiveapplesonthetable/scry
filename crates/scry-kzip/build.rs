//! Compile `proto/analysis.proto` + `proto/storage.proto` into Rust
//! types at build time.
//!
//! - `analysis.proto` carries `IndexedCompilation` / `CompilationUnit`
//!   for the kzip walker.
//! - `storage.proto` carries `Entry` for the indexer-stdout decoder.
//!
//! Output lands in `$OUT_DIR/protos/` and is `include!`d from
//! `src/proto.rs`. We use the `protobuf-codegen` crate (pure Rust;
//! no `protoc` binary required) so the workspace stays free of system
//! tooling deps.

fn main() {
    let inputs = ["proto/analysis.proto", "proto/storage.proto"];
    for p in &inputs {
        println!("cargo:rerun-if-changed={p}");
    }
    protobuf_codegen::Codegen::new()
        .pure()
        .includes(["proto"])
        .inputs(inputs)
        .cargo_out_dir("protos")
        .run_from_script();
}
