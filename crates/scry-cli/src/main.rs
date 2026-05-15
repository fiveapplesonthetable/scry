//! scry: semantic code search and cross-reference engine for AOSP and Linux.
//!
//! Phase 0 surface: only `scry index <ROOT>...` works, and it just reports
//! file counts. Later phases attach parsers and a real on-disk index.

use anyhow::Result;
use clap::{Parser, Subcommand};
use scry_walker::{walk_root, FileKind, Profile, WalkResult};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "scry", version, about = "Semantic code search for AOSP and Linux")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Walk source root(s) and report per-language file counts (Phase 0).
    Index {
        /// Source root(s). Default: ~/dev/aosp and /mnt/agent/dev/linux if present.
        roots: Vec<PathBuf>,
        /// Override profile (aosp / linux / generic). Default: auto-detect per root.
        #[arg(long)]
        profile: Option<String>,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

fn default_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let aosp = PathBuf::from(home).join("dev/aosp");
        if aosp.is_dir() {
            v.push(aosp);
        }
    }
    let linux = PathBuf::from("/mnt/agent/dev/linux");
    if linux.is_dir() {
        v.push(linux);
    }
    v
}

fn human_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut x = b as f64;
    let mut i = 0;
    while x >= 1024.0 && i + 1 < UNITS.len() {
        x /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", x, UNITS[i])
}

fn print_result(r: &WalkResult) {
    println!("\n=== {} ===", r.root.display());
    println!("  profile:       {:?}", r.profile);
    println!("  total files:   {}", r.total_files);
    println!("  unknown ext:   {}", r.unknown_files);
    println!("  bytes:         {}", human_bytes(r.total_bytes));
    println!("  elapsed:       {} ms", r.elapsed_ms);
    let mut entries: Vec<_> = r.counts.iter().map(|(k, v)| (*k, *v)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    println!("\n  source:");
    for (k, c) in entries.iter().filter(|(k, _)| k.is_source()) {
        println!("    {:>10}  {:?}", c, k);
    }
    println!("  build:");
    for (k, c) in entries.iter().filter(|(k, _)| k.is_build()) {
        println!("    {:>10}  {:?}", c, k);
    }
    println!("  android-config:");
    for (k, c) in entries.iter().filter(|(k, _)| k.is_android_config()) {
        println!("    {:>10}  {:?}", c, k);
    }
    println!("  other:");
    for (k, c) in entries.iter().filter(|(k, _)| {
        !k.is_source() && !k.is_build() && !k.is_android_config()
    }) {
        println!("    {:>10}  {:?}", c, k);
    }
}

#[derive(serde::Serialize)]
struct JsonResult<'a> {
    root: String,
    profile: Profile,
    total_files: u64,
    unknown_files: u64,
    total_bytes: u64,
    elapsed_ms: u128,
    counts: std::collections::BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    _phantom: Option<&'a ()>,
}

fn print_json(r: &WalkResult) {
    let counts: std::collections::BTreeMap<String, u64> = r
        .counts
        .iter()
        .map(|(k, v)| (format!("{:?}", k), *v))
        .collect();
    let j = JsonResult {
        root: r.root.display().to_string(),
        profile: r.profile,
        total_files: r.total_files,
        unknown_files: r.unknown_files,
        total_bytes: r.total_bytes,
        elapsed_ms: r.elapsed_ms,
        counts,
        _phantom: None,
    };
    println!("{}", serde_json::to_string(&j).unwrap());
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { roots, profile, json } => {
            let roots = if roots.is_empty() { default_roots() } else { roots };
            if roots.is_empty() {
                anyhow::bail!(
                    "no source roots: pass one or more paths, or ensure ~/dev/aosp / \
                    /mnt/agent/dev/linux exist"
                );
            }

            // total summary across all roots
            let mut grand_total = 0u64;
            let mut grand_bytes = 0u64;
            let mut grand_counts: std::collections::HashMap<FileKind, u64> =
                std::collections::HashMap::new();
            let t_all = std::time::Instant::now();

            for root in &roots {
                let prof = match &profile {
                    Some(s) => Profile::parse(s)?,
                    None => Profile::auto_detect(root),
                };
                eprintln!(
                    "scanning {} (profile: {:?})",
                    root.display(),
                    prof
                );
                let r = walk_root(root, prof)?;
                grand_total += r.total_files;
                grand_bytes += r.total_bytes;
                for (k, c) in &r.counts {
                    *grand_counts.entry(*k).or_insert(0) += c;
                }
                if json {
                    print_json(&r);
                } else {
                    print_result(&r);
                }
            }

            if !json && roots.len() > 1 {
                let elapsed_all = t_all.elapsed().as_millis();
                println!("\n=== TOTAL across {} roots ===", roots.len());
                println!("  total files:   {}", grand_total);
                println!("  bytes:         {}", human_bytes(grand_bytes));
                println!("  elapsed:       {} ms", elapsed_all);
                let mut entries: Vec<_> = grand_counts.into_iter().collect();
                entries.sort_by(|a, b| b.1.cmp(&a.1));
                for (k, c) in entries.iter().take(20) {
                    println!("  {:>10}  {:?}", c, k);
                }
            }
            Ok(())
        }
    }
}
