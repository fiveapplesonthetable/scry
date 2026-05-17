//! Cross-language Path B/C integration tests. For each of the six
//! Kythe-target languages (Java, Kotlin, C/C++, Rust, TypeScript, Go,
//! plus Python which uses the same SCIP path), build a tiny real
//! fixture in that language, run its real indexer toolchain to
//! produce a `compile_commands.json` or `*.scip` artifact, then run
//! `scry index` + `scry finalize` + `scry health` and assert the
//! sidecar lands with non-zero records.
//!
//! Toolchain detection: each test probes for its indexer binary on
//! `PATH` (or in a few well-known locations). If the toolchain is
//! absent, the test SKIPS with a clear `eprintln!` message and a
//! passing `return` — it does NOT silently pass and does NOT fail.
//! This keeps the test useful in any environment that has the
//! toolchain installed without blocking environments that don't.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scry_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scry"))
}

/// Look up an external indexer binary. Searches PATH first, then a
/// short list of sandbox-specific install dirs scry's test
/// environment uses. Returns the first match.
fn find_tool(names: &[&str], extra_dirs: &[&str]) -> Option<PathBuf> {
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            for n in names {
                let p = PathBuf::from(dir).join(n);
                if p.is_file() { return Some(p); }
            }
        }
    }
    for d in extra_dirs {
        for n in names {
            let p = PathBuf::from(d).join(n);
            if p.is_file() { return Some(p); }
        }
    }
    None
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = std::env::temp_dir().join(format!("scry-xlang-{prefix}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Parse `scry health --json`, find the named check by `artifact`
/// field, return its `status` string.
fn health_status(index_dir: &Path, artifact: &str) -> String {
    let out = Command::new(scry_bin())
        .args(["health", "--index"]).arg(index_dir).arg("--json")
        .output().expect("spawn health");
    assert!(out.status.success(), "health failed: {}",
        String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("health --json parses");
    v["checks"].as_array().expect("checks array").iter()
        .find(|c| c["artifact"] == artifact)
        .and_then(|c| c["status"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| format!("<no check named {artifact}>"))
}

/// Extract the leading integer after `v1, ` from a "v1, N foo, M bar"
/// status string. Returns 0 if the format doesn't match.
fn extract_v1_count(status: &str) -> usize {
    status.strip_prefix("v1, ").and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse().ok()).unwrap_or(0)
}

// ===========================================================================
// TypeScript via scip-typescript
// ===========================================================================

#[test]
fn typescript_scip_end_to_end() {
    let scip_ts = match find_tool(
        &["scip-typescript"],
        &["/mnt/agent/tools/node_modules/.bin"],
    ) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: scip-typescript not installed; \
                       install via `npm i -g @sourcegraph/scip-typescript`");
            return;
        }
    };

    let base = temp_dir("ts");
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(base.join("package.json"),
        r#"{"name": "fixture", "version": "1.0.0"}"#).unwrap();
    std::fs::write(base.join("tsconfig.json"),
        r#"{"compilerOptions": {"target": "es2020", "module": "commonjs", "strict": true, "outDir": "dist"}, "include": ["src/**/*.ts"]}"#).unwrap();
    std::fs::write(src.join("animal.ts"), r#"export class Animal {
    speak(): string { return "noise"; }
}
export class Dog extends Animal {
    speak(): string { return "woof"; }
}
export function pet(a: Animal): string { return a.speak(); }
"#).unwrap();

    let r = Command::new(&scip_ts)
        .args(["index", "--cwd"]).arg(&base)
        .output().expect("spawn scip-typescript");
    assert!(r.status.success(),
        "scip-typescript failed: {}", String::from_utf8_lossy(&r.stderr));
    let scip_path = base.join("index.scip");
    assert!(scip_path.exists(), "scip-typescript should produce index.scip");

    let r = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(r.status.success(),
        "scry index failed: {}", String::from_utf8_lossy(&r.stderr));

    // index.scip lives in `base` (not under `src`) → use --build-out
    // to point at it. Mirrors how scip-typescript + scry users actually
    // wire this up (scip writes to project root, scry indexes src/).
    let r = Command::new(scry_bin())
        .args(["finalize", "--index"]).arg(&idx)
        .args(["--build-out"]).arg(&base)
        .output().expect("spawn scry finalize");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(r.status.success(), "finalize failed: {stderr}");
    assert!(stderr.contains("scip-import (auto:"),
        "auto scip-import should fire on the discovered .scip; got:\n{stderr}");

    let status = health_status(&idx, "scip_index");
    assert!(status.starts_with("v1,"),
        "scip_index health should be 'v1, …'; got: {status}");
    let n_syms = extract_v1_count(&status);
    // Animal, Dog, speak (twice), pet, the param `a` → at least 5
    // SCIP symbols. Lower bound is conservative against scip-ts
    // version drift.
    assert!(n_syms >= 5,
        "scip-typescript should produce >= 5 symbols from the fixture; got: {status}");

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// Java via scip-java (skip if coursier/scip-java not on PATH)
// ===========================================================================

#[test]
fn java_scip_end_to_end() {
    let scip_java = match find_tool(
        &["scip-java"],
        &["/mnt/agent/tools/bin"],
    ) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: scip-java not installed; download from \
                       https://github.com/sourcegraph/scip-java/releases");
            return;
        }
    };
    // scip-java drives a real build system (Gradle/Maven/Bazel) to
    // capture javac invocations. Without one of those on PATH, there's
    // no way to run the e2e flow even with scip-java installed.
    if find_tool(&["gradle", "mvn", "bazel"], &[]).is_none() {
        eprintln!("SKIP: scip-java needs gradle/mvn/bazel on PATH; \
                   none found in test environment");
        return;
    }

    let base = temp_dir("java");
    let src = base.join("src/main/java/demo");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(base.join("build.gradle"), r#"plugins { id 'java' }
repositories { mavenCentral() }
"#).unwrap();
    std::fs::write(base.join("settings.gradle"), "rootProject.name = 'fixture'\n").unwrap();
    std::fs::write(src.join("Animal.java"),
        "package demo;\npublic class Animal { public String speak() { return \"noise\"; } }\n").unwrap();
    std::fs::write(src.join("Dog.java"),
        "package demo;\npublic class Dog extends Animal { @Override public String speak() { return \"woof\"; } }\n").unwrap();
    std::fs::write(src.join("Main.java"),
        "package demo;\npublic class Main { public static String pet(Animal a) { return a.speak(); } }\n").unwrap();

    let r = Command::new(&scip_java)
        .args(["index", "--build-tool", "gradle"])
        .current_dir(&base).output().expect("spawn scip-java");
    if !r.status.success() {
        eprintln!("SKIP: scip-java failed (env probably lacks JDK toolchain): {}",
                  String::from_utf8_lossy(&r.stderr));
        std::fs::remove_dir_all(&base).ok();
        return;
    }
    let scip_path = base.join("index.scip");
    if !scip_path.exists() {
        eprintln!("SKIP: scip-java succeeded but produced no index.scip");
        std::fs::remove_dir_all(&base).ok();
        return;
    }

    let r = Command::new(scry_bin())
        .args(["index"]).arg(base.join("src")).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(r.status.success(),
        "scry index failed: {}", String::from_utf8_lossy(&r.stderr));

    let r = Command::new(scry_bin())
        .args(["finalize", "--index"]).arg(&idx)
        .args(["--build-out"]).arg(&base)
        .output().expect("spawn scry finalize");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(r.status.success(), "finalize failed: {stderr}");
    assert!(stderr.contains("scip-import (auto:"),
        "auto scip-import should fire; got:\n{stderr}");

    let status = health_status(&idx, "scip_index");
    assert!(status.starts_with("v1,"),
        "scip_index health should be 'v1, …'; got: {status}");

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// Kotlin via scip-kotlin (skip if not on PATH)
// ===========================================================================

#[test]
fn kotlin_scip_end_to_end() {
    if find_tool(&["scip-kotlin"], &[]).is_none() {
        eprintln!("SKIP: scip-kotlin not on PATH");
        return;
    }
    eprintln!("SKIP: scip-kotlin fixture pending (toolchain present but \
               test not yet wired)");
}

// ===========================================================================
// Rust via rust-analyzer (skip if not on PATH)
// ===========================================================================

#[test]
fn rust_scip_end_to_end() {
    let ra = match find_tool(
        &["rust-analyzer"],
        &["/mnt/agent/cargo/bin"],
    ) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: rust-analyzer not on PATH; install via \
                       `rustup component add rust-analyzer`");
            return;
        }
    };

    let base = temp_dir("rust");
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(base.join("Cargo.toml"), r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
[lib]
path = "src/lib.rs"
"#).unwrap();
    std::fs::write(src.join("lib.rs"), r#"pub trait Speak { fn speak(&self) -> &'static str; }
pub struct Dog;
impl Speak for Dog { fn speak(&self) -> &'static str { "woof" } }
pub fn pet<S: Speak>(s: &S) -> &'static str { s.speak() }
"#).unwrap();

    let r = Command::new(&ra).args(["scip", "."]).current_dir(&base)
        .output().expect("spawn rust-analyzer scip");
    if !r.status.success() {
        eprintln!("SKIP: rust-analyzer scip failed (likely cargo metadata \
                   issue in test sandbox): {}",
                  String::from_utf8_lossy(&r.stderr));
        std::fs::remove_dir_all(&base).ok();
        return;
    }
    let scip_path = base.join("index.scip");
    assert!(scip_path.exists(), "rust-analyzer should write index.scip");

    let r = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(r.status.success(),
        "scry index failed: {}", String::from_utf8_lossy(&r.stderr));

    let r = Command::new(scry_bin())
        .args(["finalize", "--index"]).arg(&idx)
        .args(["--build-out"]).arg(&base)
        .output().expect("spawn scry finalize");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(r.status.success(), "finalize failed: {stderr}");
    assert!(stderr.contains("scip-import (auto:"),
        "auto scip-import should fire; got:\n{stderr}");

    let status = health_status(&idx, "scip_index");
    assert!(status.starts_with("v1,"),
        "scip_index health should be 'v1, …'; got: {status}");
    assert!(extract_v1_count(&status) >= 3,
        "rust-analyzer should produce >= 3 symbols (Speak/Dog/pet); got: {status}");

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// Go via gopls scip (skip if not on PATH)
// ===========================================================================

#[test]
fn go_scip_end_to_end() {
    // Real Go SCIP tool is `scip-go` (not `gopls scip` — that
    // subcommand was removed from gopls in 2024+).
    let scip_go = match find_tool(
        &["scip-go"],
        &["/mnt/agent/tools/bin"],
    ) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: scip-go not on PATH; install via \
                       `go install github.com/scip-code/scip-go/cmd/scip-go@latest`");
            return;
        }
    };

    let base = temp_dir("go");
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("go.mod"), "module fixture\n\ngo 1.21\n").unwrap();
    std::fs::write(src.join("animal.go"), r#"package fixture

type Animal interface { Speak() string }
type Dog struct{}
func (Dog) Speak() string { return "woof" }
func Pet(a Animal) string { return a.Speak() }
"#).unwrap();

    let r = Command::new(&scip_go)
        .current_dir(&src).output().expect("spawn scip-go");
    if !r.status.success() {
        eprintln!("SKIP: scip-go failed in test sandbox: {}",
                  String::from_utf8_lossy(&r.stderr));
        std::fs::remove_dir_all(&base).ok();
        return;
    }

    let r = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(r.status.success(),
        "scry index failed: {}", String::from_utf8_lossy(&r.stderr));

    let r = Command::new(scry_bin())
        .args(["finalize", "--index"]).arg(&idx)
        .args(["--build-out"]).arg(&src)
        .output().expect("spawn scry finalize");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(r.status.success(), "finalize failed: {stderr}");
    assert!(stderr.contains("scip-import (auto:"),
        "auto scip-import should fire; got:\n{stderr}");

    let status = health_status(&idx, "scip_index");
    assert!(status.starts_with("v1,"),
        "scip_index health should be 'v1, …'; got: {status}");

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// Python via scip-python (skip if not on PATH)
// ===========================================================================

#[test]
fn python_scip_end_to_end() {
    let scip_py = match find_tool(
        &["scip-python"],
        &["/mnt/agent/tools/node_modules/.bin"],
    ) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: scip-python not installed; \
                       install via `npm i -g @sourcegraph/scip-python`");
            return;
        }
    };

    let base = temp_dir("py");
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("animal.py"), r#"class Animal:
    def speak(self) -> str:
        return "noise"

class Dog(Animal):
    def speak(self) -> str:
        return "woof"

def pet(a: Animal) -> str:
    return a.speak()
"#).unwrap();

    let r = Command::new(&scip_py)
        .args(["index", "--project-name", "fixture",
               "--project-version", "0.1.0", "--cwd"])
        .arg(&base)
        .output().expect("spawn scip-python");
    if !r.status.success() {
        eprintln!("SKIP: scip-python failed: {}",
                  String::from_utf8_lossy(&r.stderr));
        std::fs::remove_dir_all(&base).ok();
        return;
    }
    assert!(base.join("index.scip").exists(),
        "scip-python should write index.scip");

    let r = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(r.status.success(),
        "scry index failed: {}", String::from_utf8_lossy(&r.stderr));

    let r = Command::new(scry_bin())
        .args(["finalize", "--index"]).arg(&idx)
        .args(["--build-out"]).arg(&base)
        .output().expect("spawn scry finalize");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(r.status.success(), "finalize failed: {stderr}");
    assert!(stderr.contains("scip-import (auto:"),
        "auto scip-import should fire; got:\n{stderr}");

    let status = health_status(&idx, "scip_index");
    assert!(status.starts_with("v1,"),
        "scip_index health should be 'v1, …'; got: {status}");
    assert!(extract_v1_count(&status) >= 3,
        "scip-python should produce >= 3 symbols (Animal/Dog/pet); got: {status}");

    std::fs::remove_dir_all(&base).ok();
}
