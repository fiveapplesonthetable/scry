//! End-to-end regression test: build a synthetic source tree, run the
//! real `scry index` binary against it, then query the resulting index
//! via the CLI and `scry serve`. Catches cross-crate API breakage that
//! per-crate unit tests can't see (writer/reader format drift, CLI flag
//! plumbing, JSON-RPC shape changes).
//!
//! The fixture is intentionally TINY (5 files, < 50 LOC each) so the
//! whole test runs in well under a second. We use `CARGO_BIN_EXE_scry`
//! so the test always invokes the just-built binary, never some PATH-
//! shadowed one.

use std::path::PathBuf;
use std::process::Command;

fn scry_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scry"))
}

/// One-shot fixture: builds a source tree under `root`, runs the
/// indexer, returns the index dir. Panics on any failure so the test
/// fails loudly at the offending step.
fn build_index(root: &std::path::Path, index: &std::path::Path) {
    std::fs::create_dir_all(root.join("frameworks/base/core/java/android/app")).unwrap();
    std::fs::create_dir_all(root.join("system/core/init")).unwrap();
    std::fs::create_dir_all(root.join("frameworks/native/libs/binder")).unwrap();

    // Java class with an inner class + a method that "calls" another.
    std::fs::write(
        root.join("frameworks/base/core/java/android/app/Activity.java"),
        r#"package android.app;
public class Activity {
    public void onCreate() {
        Binder b = new Binder();
        b.transact();
    }
    public static class InnerHelper {
        public void noop() {}
    }
}
"#,
    ).unwrap();

    // A second Java class that defines transact (the call target).
    std::fs::write(
        root.join("frameworks/base/core/java/android/app/Binder.java"),
        r#"package android.app;
public class Binder {
    public void transact() {}
}
"#,
    ).unwrap();

    // Soong build module (Android.bp).
    std::fs::write(
        root.join("frameworks/native/libs/binder/Android.bp"),
        r#"cc_library {
    name: "libbinder_e2e",
    srcs: ["IBinder.cpp"],
}
"#,
    ).unwrap();

    // init.rc service definition (custom AOSP parser).
    std::fs::write(
        root.join("system/core/init/zygote.rc"),
        r#"service zygote_e2e /system/bin/app_process
    class main
    user root
"#,
    ).unwrap();

    // A C++ source file for libbinder so the .bp srcs reference resolves.
    std::fs::write(
        root.join("frameworks/native/libs/binder/IBinder.cpp"),
        r#"namespace android {
class IBinder {
public:
    void transact() {}
};
int main() { return 0; }
}
"#,
    ).unwrap();

    // Run the indexer. We let it use defaults except for paths.
    let out = Command::new(scry_bin())
        .args(["index"])
        .arg(root)
        .args(["-o"])
        .arg(index)
        .output()
        .expect("spawn scry index");
    assert!(
        out.status.success(),
        "scry index failed: status={} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// `scry def NAME --json` → parsed JSON array.
fn query_def(index: &std::path::Path, name: &str) -> serde_json::Value {
    let out = Command::new(scry_bin())
        .args(["def", name, "--index"])
        .arg(index)
        .args(["--json", "--limit", "20"])
        .output()
        .expect("spawn scry def");
    assert!(out.status.success(),
            "scry def {name} failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8(out.stdout).unwrap();
    // CLI def --json prints one JSON object per line, not an array. Take
    // them all into a Vec so the caller gets array-like semantics.
    let arr: Vec<serde_json::Value> = s.lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    serde_json::Value::Array(arr)
}

/// `scry outline PATH --json` → parsed JSON object.
fn query_outline(index: &std::path::Path, path: &str) -> serde_json::Value {
    let out = Command::new(scry_bin())
        .args(["outline", path, "--index"])
        .arg(index)
        .args(["--json", "--limit", "0"])
        .output()
        .expect("spawn scry outline");
    assert!(out.status.success(),
            "scry outline {path} failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn synthetic_tree_roundtrip() {
    // Tempdir holds both the synthetic source root and the index dir.
    // We don't use any external tempfile crate; std + a per-test
    // nanos-suffix avoids the dependency and is good enough for one test.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-e2e-{}", nanos));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(&src).unwrap();
    build_index(&src, &idx);

    // 1. Java class definitions are found.
    let activity = query_def(&idx, "Activity");
    let arr = activity.as_array().expect("def returns array");
    assert!(!arr.is_empty(), "expected Activity defs, got {}", activity);
    assert!(arr.iter().any(|s| s["kind"] == "class" && s["lang"] == "Java"),
            "expected Java class Activity, got {:?}", arr);

    let binder = query_def(&idx, "Binder");
    let arr = binder.as_array().unwrap();
    assert!(arr.iter().any(|s| s["kind"] == "class" && s["lang"] == "Java"),
            "expected Java class Binder, got {:?}", arr);

    // 2. AOSP-specific kinds: Soong module + init service.
    let soong = query_def(&idx, "libbinder_e2e");
    let arr = soong.as_array().unwrap();
    assert!(arr.iter().any(|s| s["kind"] == "soong"),
            "expected SoongModule libbinder_e2e, got {:?}", arr);

    let zygote = query_def(&idx, "zygote_e2e");
    let arr = zygote.as_array().unwrap();
    assert!(arr.iter().any(|s| s["kind"] == "init.svc"),
            "expected InitService zygote_e2e, got {:?}", arr);

    // 3. outline returns every symbol defined in the Activity.java file.
    let outline = query_outline(&idx, "android/app/Activity.java");
    let names: Vec<&str> = outline["symbols"].as_array().unwrap().iter()
        .filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"Activity"),
            "outline should include Activity class, got {:?}", names);
    assert!(names.contains(&"onCreate"),
            "outline should include onCreate method, got {:?}", names);
    assert!(names.contains(&"InnerHelper"),
            "outline should include InnerHelper inner class, got {:?}", names);

    // 4. JSON-RPC serve: send 2 reqs over stdin, parse 2 responses.
    use std::io::Write;
    let mut child = Command::new(scry_bin())
        .args(["serve", "--index"])
        .arg(&idx)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn scry serve");
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"id":1,"cmd":"def","args":{{"name":"Binder","limit":3}}}}"#).unwrap();
        writeln!(stdin, r#"{{"id":2,"cmd":"outline","args":{{"path":"android/app/Activity.java"}}}}"#).unwrap();
    }
    let out = child.wait_with_output().expect("serve wait");
    assert!(out.status.success(),
            "serve failed: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "expected 2 RPC responses, got {:?}", lines);
    let r1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(r1["id"], 1);
    assert!(!r1["result"].as_array().unwrap().is_empty(), "def Binder via RPC");
    let r2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(r2["id"], 2);
    let outline_names: Vec<&str> = r2["result"]["symbols"].as_array().unwrap().iter()
        .filter_map(|s| s["name"].as_str()).collect();
    assert!(outline_names.contains(&"onCreate"),
            "RPC outline should include onCreate, got {:?}", outline_names);

    // 5. Run build-trigrams + grep — exercises grep_candidates +
    // read_trigram_posting + intersection on a small real index. Without
    // this assertion, an off-by-one in the trigram posting decode would
    // surface only as silently-empty greps in production.
    let out = Command::new(scry_bin())
        .args(["build-trigrams", "--index"])
        .arg(&idx)
        .output()
        .expect("spawn scry build-trigrams");
    assert!(out.status.success(),
            "scry build-trigrams failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new(scry_bin())
        .args(["grep", "transact", "--index"])
        .arg(&idx)
        .args(["--json"])
        .output()
        .expect("spawn scry grep");
    assert!(out.status.success(),
            "scry grep transact failed: {}", String::from_utf8_lossy(&out.stderr));
    let hits: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(hits.iter().any(|h| h["path"].as_str().unwrap_or("").contains("Activity.java")),
            "grep should hit Activity.java's b.transact() call, got {:?}",
            hits.iter().map(|h| h["path"].clone()).collect::<Vec<_>>());
    assert!(hits.iter().any(|h| h["path"].as_str().unwrap_or("").contains("Binder.java")),
            "grep should hit Binder.java's transact() definition, got {:?}",
            hits.iter().map(|h| h["path"].clone()).collect::<Vec<_>>());

    // 6. Run build-resolutions + assert Java refs get resolved_to set.
    // This pins the Layer 2 sidecar end-to-end — apply_resolution_override
    // in get_ref + the build-resolutions writer + the JSON serializer
    // all have to work together for resolved_to to show up.
    let out = Command::new(scry_bin())
        .args(["build-resolutions", "--index"])
        .arg(&idx)
        .output()
        .expect("spawn scry build-resolutions");
    assert!(out.status.success(),
            "scry build-resolutions failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new(scry_bin())
        .args(["callers", "transact", "--index"])
        .arg(&idx)
        .args(["--json"])
        .output()
        .expect("spawn scry callers");
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(!lines.is_empty(), "expected at least one transact caller");
    let resolved_count = lines.iter()
        .filter(|l| l["resolved_to"].as_u64().is_some()).count();
    assert!(resolved_count > 0,
            "build-resolutions should have populated at least one resolved_to, got 0/{} (refs: {:?})",
            lines.len(), lines);

    // 7. Unix-socket serve mode. Spawn the daemon, give it ~100 ms to
    // bind, connect with a UnixStream, send two requests, read two
    // responses, assert the shapes. Also exercises the
    // reader_clone_for_share path (Arc<StoreReader> across threads).
    use std::os::unix::net::UnixStream;
    use std::io::Read;
    let sock_path = base.join("scry-e2e.sock");
    let _ = std::fs::remove_file(&sock_path); // best-effort stale-cleanup
    let mut daemon = Command::new(scry_bin())
        .args(["serve", "--index"])
        .arg(&idx)
        .arg("--listen")
        .arg(format!("unix:{}", sock_path.display()))
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn scry serve --listen");
    // Wait for the bind. The daemon prints a "listening on unix:..."
    // banner to stderr; poll for the socket file with a tiny budget.
    let mut bound = false;
    for _ in 0..50 {
        if sock_path.exists() { bound = true; break; }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(bound, "socket {} never appeared", sock_path.display());

    let mut stream = UnixStream::connect(&sock_path).expect("connect to scry serve socket");
    writeln!(stream, r#"{{"id":1,"cmd":"def","args":{{"name":"Binder","limit":1}}}}"#).unwrap();
    writeln!(stream, r#"{{"id":2,"cmd":"stats"}}"#).unwrap();
    stream.shutdown(std::net::Shutdown::Write).ok(); // signal EOF
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read socket reply");
    let lines: Vec<&str> = buf.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "expected 2 socket responses, got: {buf:?}");
    let r1: serde_json::Value = serde_json::from_str(lines[0]).expect("socket reply 1 is JSON");
    assert_eq!(r1["id"], 1, "first reply id must echo request");
    assert!(!r1["result"].as_array().unwrap().is_empty(),
            "def Binder via socket should return at least one hit");
    let r2: serde_json::Value = serde_json::from_str(lines[1]).expect("socket reply 2 is JSON");
    assert_eq!(r2["id"], 2);
    assert!(r2["result"]["files_total"].as_u64().is_some(),
            "stats should report files_total via socket");

    // 7a. Streaming mode: same socket, send a request with stream:true,
    // expect per-hit lines + a closing {done:true,shown:K} envelope.
    let mut stream2 = UnixStream::connect(&sock_path).expect("reconnect for stream test");
    writeln!(stream2, r#"{{"id":42,"cmd":"def","args":{{"name":"Binder","limit":5}},"stream":true}}"#).unwrap();
    stream2.shutdown(std::net::Shutdown::Write).ok();
    let mut sbuf = String::new();
    stream2.read_to_string(&mut sbuf).expect("read stream reply");
    let slines: Vec<&str> = sbuf.lines().filter(|l| !l.is_empty()).collect();
    assert!(slines.len() >= 2, "expected ≥1 hit line + 1 done line; got {sbuf:?}");
    let last: serde_json::Value = serde_json::from_str(slines.last().unwrap())
        .expect("last stream line is JSON");
    assert_eq!(last["id"], 42);
    assert_eq!(last["done"], true, "last line must be the done envelope");
    let shown = last["shown"].as_u64().expect("done envelope has shown count");
    // Every non-last line must be a hit envelope for the same id.
    for hit_line in &slines[..slines.len()-1] {
        let h: serde_json::Value = serde_json::from_str(hit_line).expect("hit line is JSON");
        assert_eq!(h["id"], 42);
        assert!(h["hit"].is_object(), "hit field should hold the symbol object: {h}");
    }
    assert_eq!(shown as usize, slines.len() - 1,
               "shown count should match number of emitted hit lines");

    // 7b. Budget: ask for a Binder lookup with a tiny byte cap;
    // assert the response carries a "truncated" tag and that snippet
    // / scope / fqn have been stripped progressively.
    let mut stream3 = UnixStream::connect(&sock_path).expect("reconnect for budget test");
    writeln!(stream3, r#"{{"id":7,"cmd":"def","args":{{"name":"Binder","limit":5}},"budget":120}}"#).unwrap();
    stream3.shutdown(std::net::Shutdown::Write).ok();
    let mut bbuf = String::new();
    stream3.read_to_string(&mut bbuf).expect("read budget reply");
    let bline = bbuf.lines().find(|l| !l.is_empty()).expect("at least one line");
    let bresp: serde_json::Value = serde_json::from_str(bline).expect("budget reply is JSON");
    assert_eq!(bresp["id"], 7);
    assert!(bresp.get("truncated").is_some(),
            "small budget should produce a truncated tag; got {bresp}");
    let tag = bresp["truncated"].as_str().unwrap();
    assert!(tag.contains("snippet") || tag.contains("truncated"),
            "truncated tag should describe what was dropped; got {tag:?}");

    // Cleanup the daemon and the socket file.
    daemon.kill().ok();
    daemon.wait().ok();
    let _ = std::fs::remove_file(&sock_path);

    // Best-effort cleanup; on a panic, the dir leaks under /tmp which
    // is fine for one test fixture.
    std::fs::remove_dir_all(&base).ok();
}
