//! Smoke-test driver: extract Soong compilations and report counts.
//! Run with `cargo run -p scry-bridge --example extract_soong -- \
//! /home/zim/dev/aosp /home/zim/dev/aosp/out/soong`.

use scry_bridge::{BuildSystem, soong::Soong};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        anyhow::bail!("usage: extract_soong <source_root> <build_dir>");
    }
    let source_root = std::path::PathBuf::from(&args[1]);
    let build_dir = std::path::PathBuf::from(&args[2]);
    let bridge = Soong::new(&source_root);
    let t = std::time::Instant::now();
    let comps = bridge.extract_compilations(&build_dir)?;
    let java = comps.iter().filter(|c| matches!(c.language, scry_bridge::Language::Java)).count();
    let kotlin = comps.iter().filter(|c| matches!(c.language, scry_bridge::Language::Kotlin)).count();
    let total_sources: usize = comps.iter().map(|c| c.sources.len()).sum();
    let total_cp_entries: usize = comps.iter().map(|c| c.classpath.len()).sum();
    eprintln!(
        "extracted {} compilations ({java} Java, {kotlin} Kotlin) in {:.2}s",
        comps.len(), t.elapsed().as_secs_f64(),
    );
    eprintln!("  total source files across all compilations: {total_sources}");
    eprintln!("  total classpath entries across all compilations: {total_cp_entries}");
    if let Some(sample) = comps.first() {
        eprintln!("\nfirst compilation: {}", sample.module);
        eprintln!("  language: {:?}", sample.language);
        eprintln!("  sources[0..3]: {:?}", &sample.sources[..sample.sources.len().min(3)]);
        eprintln!("  classpath entries: {}", sample.classpath.len());
        eprintln!("  bootclasspath entries: {}", sample.bootclasspath.len());
    }
    Ok(())
}
