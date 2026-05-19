//! Look up the symbol that `clang_usrs.bin` / `scip_index.bin` carries
//! at a specific `(path, byte_offset)`. Diagnostic tool for the precision
//! pipeline — answer "what USR does cxx_indexer think is at the def
//! site of IPCThreadState::self at line 374 col 25?"
//!
//! Usage:
//!   cargo run --release -p scry-store --example lookup_usr -- \
//!     <sidecar.bin> <abs-path> <byte-offset> [window-bytes]

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let sidecar = args.next().ok_or("missing <sidecar.bin>")?;
    let path = args.next().ok_or("missing <abs-path>")?;
    let off: u32 = args.next().ok_or("missing <byte-offset>")?.parse()?;
    let window: u32 = args.next().as_deref().unwrap_or("64").parse()?;

    // Determine which sidecar (clang USR or SCIP) by the filename
    // — both share the precision_packed format but with different
    // MAGIC bytes. Try both magics.
    use scry_store::precision_packed::{PrecisionPacked, MAGIC_CLANG_USR, MAGIC_SCIP};
    let magics: &[(&str, &[u8; 8])] = &[
        ("clang USR", MAGIC_CLANG_USR),
        ("SCIP",      MAGIC_SCIP),
    ];
    let (kind, packed) = magics.iter()
        .find_map(|(k, m)| PrecisionPacked::open(Path::new(&sidecar), *m).ok().flatten().map(|p| (*k, p)))
        .ok_or("could not open sidecar with either MAGIC_CLANG_USR or MAGIC_SCIP")?;
    eprintln!("[lookup_usr] opened {sidecar} as {kind}");
    eprintln!("[lookup_usr] records: {}", packed.record_count());

    // Look up symbol via two paths: symbol_at (exact byte) + symbol_at_window
    if let Some(s) = packed.symbol_at(&path, off) {
        println!("symbol_at  ({path} @ {off})        = {s}");
    } else {
        println!("symbol_at  ({path} @ {off})        = (no anchor at exact offset)");
    }
    if let Some(s) = packed.symbol_for_window(&path, off, window) {
        println!("symbol_for_window ({path} @ {off} ±{window}) = {s}");
    } else {
        println!("symbol_for_window ({path} @ {off} ±{window}) = (no anchor in window)");
    }
    Ok(())
}
