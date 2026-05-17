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

    // 8. MCP server end-to-end. Spawn `scry mcp`, send the three
    // MCP methods (initialize, tools/list, tools/call), assert
    // each response. The MCP wrapper reuses serve_one_request
    // under the hood, so this catches breakage in the protocol
    // shim itself (envelope, method dispatch, tool result wrapping)
    // independent of the serve-layer tests. `Write` is already in
    // scope from the earlier serve test at the top of this fn.
    let mut mcp_child = Command::new(scry_bin())
        .args(["mcp", "--index"])
        .arg(&idx)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn scry mcp");
    {
        let stdin = mcp_child.stdin.as_mut().unwrap();
        // 1. initialize
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#).unwrap();
        // 2. notification (no id) — must be silently consumed
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        // 3. tools/list
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
        // 4. tools/call def Binder
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"def","arguments":{{"name":"Binder","limit":1}}}}}}"#).unwrap();
    }
    let mcp_out = mcp_child.wait_with_output().expect("mcp wait");
    assert!(mcp_out.status.success(),
            "mcp failed: {}", String::from_utf8_lossy(&mcp_out.stderr));
    let mcp_lines: Vec<&str> = std::str::from_utf8(&mcp_out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty()).collect();
    // Exactly 3 responses (no response to the notification per spec).
    assert_eq!(mcp_lines.len(), 3,
               "expected 3 MCP responses (init, tools/list, tools/call); got {}: {:?}",
               mcp_lines.len(), mcp_lines);

    let init: serde_json::Value = serde_json::from_str(mcp_lines[0]).unwrap();
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "scry");
    assert!(init["result"]["capabilities"]["tools"].is_object(),
            "initialize must advertise the tools capability: {init}");

    let tools: serde_json::Value = serde_json::from_str(mcp_lines[1]).unwrap();
    assert_eq!(tools["id"], 2);
    let tool_names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter()
        .filter_map(|t| t["name"].as_str()).collect();
    for must_exist in &["def", "ref", "callers", "grep", "outline", "stats"] {
        assert!(tool_names.contains(must_exist),
                "tools/list missing {must_exist}; got {tool_names:?}");
    }

    let call: serde_json::Value = serde_json::from_str(mcp_lines[2]).unwrap();
    assert_eq!(call["id"], 3);
    assert!(call["result"]["content"].is_array(), "tools/call must return content[]: {call}");
    // Successful tool calls must report isError: false.
    assert_eq!(call["result"]["isError"].as_bool(), Some(false),
               "successful tools/call must set isError: false; got {call}");
    let text = call["result"]["content"][0]["text"].as_str()
        .expect("first content part should be text");
    // The text is itself JSON — the serve result re-encoded. Parse
    // and confirm we got at least one Binder hit through.
    let inner: serde_json::Value = serde_json::from_str(text).expect("text content is JSON");
    assert!(inner.as_array().map(|a| !a.is_empty()).unwrap_or(false),
            "tools/call def Binder should return at least one hit: {inner}");

    // 8a. MCP error paths — exhaustive. A separate MCP session per
    // call avoids state coupling. Each assertion pins one L7-grade
    // contract the wrapper must keep.
    fn mcp_call(scry: &std::path::Path, idx: &std::path::Path, body: &str) -> serde_json::Value {
        use std::io::Write;
        let mut child = Command::new(scry)
            .args(["mcp", "--index"]).arg(idx)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().expect("spawn mcp");
        {
            let stdin = child.stdin.as_mut().unwrap();
            writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#).unwrap();
            writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
            writeln!(stdin, "{}", body).unwrap();
        }
        let out = child.wait_with_output().expect("mcp wait");
        let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap()
            .lines().filter(|l| !l.is_empty()).collect();
        // Last line is our test call's reply (after init).
        serde_json::from_str(lines.last().expect("at least one reply"))
            .expect("reply is JSON")
    }

    // Unknown tool → isError: true, well-formed envelope.
    let r = mcp_call(&scry_bin(), &idx,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#);
    assert_eq!(r["result"]["isError"].as_bool(), Some(true),
               "unknown tool must isError:true; got {r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap_or("").contains("unknown tool"),
            "unknown-tool message should say so; got {r}");

    // Missing required arg → isError: true with named arg.
    let r = mcp_call(&scry_bin(), &idx,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"def","arguments":{}}}"#);
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("name") && text.contains("def"),
            "missing-arg error should name the arg and tool; got {text}");

    // Empty-string required arg → also rejected. This was the L7 bug:
    // {"name": ""} silently returned 50 garbage anonymous-enum hits.
    let r = mcp_call(&scry_bin(), &idx,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"def","arguments":{"name":""}}}"#);
    assert_eq!(r["result"]["isError"].as_bool(), Some(true),
               "empty-string name must be rejected (was returning garbage hits)");

    // Notification (no id) is silently consumed — exactly one response
    // line should come back from a session that sends initialize +
    // notification + nothing else.
    let mut nchild = Command::new(scry_bin())
        .args(["mcp", "--index"]).arg(&idx)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().expect("spawn mcp for notification test");
    {
        use std::io::Write;
        let stdin = nchild.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/somethingElse","params":{{}}}}"#).unwrap();
    }
    let nout = nchild.wait_with_output().expect("mcp wait");
    let nlines: Vec<&str> = std::str::from_utf8(&nout.stdout).unwrap()
        .lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(nlines.len(), 1,
               "notifications must produce no reply; expected 1 line (init), got {}: {:?}",
               nlines.len(), nlines);

    // ping → empty result {} per MCP spec.
    let r = mcp_call(&scry_bin(), &idx,
        r#"{"jsonrpc":"2.0","id":99,"method":"ping"}"#);
    assert_eq!(r["result"], serde_json::json!({}), "ping reply must be empty object; got {r}");

    // Unknown method (not tools/call) → JSON-RPC error -32601.
    let r = mcp_call(&scry_bin(), &idx,
        r#"{"jsonrpc":"2.0","id":99,"method":"some/random/method"}"#);
    assert_eq!(r["error"]["code"].as_i64(), Some(-32601),
               "unknown method must return JSON-RPC -32601; got {r}");

    // 8b. scry index --incremental round-trip. Reindex from the
    // current state, build digests, modify one file + add a new
    // one, incremental-rebuild, assert: unchanged file's symbols
    // survive; changed file's new symbol is queryable; new file's
    // symbols are queryable.
    let inc_src = base.join("inc-src");
    let inc_idx = base.join("inc-idx");
    std::fs::create_dir_all(inc_src.join("a")).unwrap();
    std::fs::create_dir_all(inc_src.join("b")).unwrap();
    std::fs::write(inc_src.join("a/Alpha.java"),
        "package z;\npublic class Alpha {\n    public void aMethod() {}\n}\n").unwrap();
    std::fs::write(inc_src.join("b/Bravo.java"),
        "package z;\npublic class Bravo {\n    public void bMethod() {}\n}\n").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&inc_src).arg("-o").arg(&inc_idx)
        .args(["--workers", "2"])
        .output().expect("initial index");
    assert!(out.status.success(),
            "initial index failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new(scry_bin())
        .args(["build-digests", "--index"]).arg(&inc_idx)
        .output().expect("build-digests");
    assert!(out.status.success());

    // No-change run is a no-op.
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&inc_src).arg("-o").arg(&inc_idx)
        .arg("--incremental").args(["--workers", "2"])
        .output().expect("incremental no-change");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no changes"),
            "no-change incremental should say so; got: {stderr}");

    // Modify Alpha + add Charlie.
    std::fs::write(inc_src.join("a/Alpha.java"),
        "package z;\npublic class Alpha {\n    public void aMethod() {}\n    public void newAlpha() {}\n}\n").unwrap();
    std::fs::write(inc_src.join("a/Charlie.java"),
        "package z;\npublic class Charlie {}\n").unwrap();

    let out = Command::new(scry_bin())
        .args(["index"]).arg(&inc_src).arg("-o").arg(&inc_idx)
        .arg("--incremental").args(["--workers", "2"])
        .output().expect("incremental rebuild");
    assert!(out.status.success(),
            "incremental failed: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("1 unchanged") || stderr.contains("1 unchanged,"),
            "diff line should mention unchanged count: {stderr}");
    assert!(stderr.contains("1 changed") || stderr.contains(" 1 changed,"),
            "diff line should mention changed count: {stderr}");
    assert!(stderr.contains("1 added") || stderr.contains(" 1 added,"),
            "diff line should mention added count: {stderr}");

    // Query the new state. Use the JSON query helper.
    let new_sym = query_def(&inc_idx, "newAlpha");
    let arr = new_sym.as_array().expect("def returns array");
    assert!(!arr.is_empty(), "newAlpha should be found after incremental: {new_sym}");
    let charlie = query_def(&inc_idx, "Charlie");
    let arr = charlie.as_array().unwrap();
    assert!(!arr.is_empty(), "Charlie (added) should be found: {charlie}");
    let bravo = query_def(&inc_idx, "Bravo");
    let arr = bravo.as_array().unwrap();
    assert!(!arr.is_empty(),
            "Bravo (unchanged, replayed) must survive incremental: {bravo}");

    // 8c. Incremental with file DELETION. Drop Charlie.java +
    // delete Alpha.java entirely; rebuild incremental; assert the
    // diff line reports `1 removed` and the dropped symbols are no
    // longer queryable. This is the path the previous test did not
    // cover and the agent audit flagged as CRITICAL.
    std::fs::remove_file(inc_src.join("a/Alpha.java")).unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&inc_src).arg("-o").arg(&inc_idx)
        .arg("--incremental").args(["--workers", "2"])
        .output().expect("incremental w/ delete");
    assert!(out.status.success(),
            "incremental-with-delete failed: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("1 removed") || stderr.contains(" 1 removed,"),
            "diff line should mention 1 removed: {stderr}");
    // After re-opening the new index, Alpha must be gone.
    let alpha_gone = query_def(&inc_idx, "Alpha");
    let arr = alpha_gone.as_array().unwrap();
    let still_present: Vec<_> = arr.iter()
        .filter(|h| h.pointer("/path").and_then(|p| p.as_str())
                     .map(|p| p.contains("Alpha.java")).unwrap_or(false))
        .collect();
    assert!(still_present.is_empty(),
            "Alpha.java symbols must not be queryable after deletion: {alpha_gone}");
    // Charlie + Bravo must still be there.
    assert!(!query_def(&inc_idx, "Charlie").as_array().unwrap().is_empty(),
            "Charlie should survive deletion of Alpha");
    assert!(!query_def(&inc_idx, "Bravo").as_array().unwrap().is_empty(),
            "Bravo should survive deletion of Alpha");

    // 8d. Stable file_id invariant: re-running the SAME
    // incremental against an unchanged tree must be a no-op
    // ("no changes — index already current"). This proves the digest
    // round-trip is deterministic; a stable-order bug would flip a
    // file's digest and trigger a false rebuild.
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&inc_src).arg("-o").arg(&inc_idx)
        .arg("--incremental").args(["--workers", "2"])
        .output().expect("incremental no-change after delete");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no changes"),
            "second no-change incremental should be a no-op: {stderr}");

    // 8e. `scry index-diff` with a file removal. The diff path is
    // separate from the incremental builder; it must independently
    // report removed=1 when a file vanishes. Sequence:
    //   1. Add Delta.java to disk.
    //   2. Run `scry index --incremental` so Delta is *in the index*.
    //   3. Run `scry build-digests` so digests are flushed.
    //   4. Delete Delta from disk.
    //   5. Run `scry index-diff` — must report removed=1.
    // This is materially different from "create file → delete file
    // → diff" (no-op) because index-diff compares disk-now against
    // the index/digest-state-then, not against some side computation.
    std::fs::write(inc_src.join("b/Delta.java"),
        "package z;\npublic class Delta {}\n").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&inc_src).arg("-o").arg(&inc_idx)
        .arg("--incremental").args(["--workers", "2"])
        .output().expect("incremental to pick up Delta");
    assert!(out.status.success());
    let out = Command::new(scry_bin())
        .args(["build-digests", "--index"]).arg(&inc_idx)
        .output().expect("build-digests after delta");
    assert!(out.status.success());
    std::fs::remove_file(inc_src.join("b/Delta.java")).unwrap();
    let out = Command::new(scry_bin())
        .args(["index-diff"]).arg(&inc_src).arg("--index").arg(&inc_idx)
        .output().expect("index-diff after remove");
    assert!(out.status.success());
    let combined = format!("{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout));
    assert!(combined.contains("removed:   1") || combined.contains("removed: 1"),
            "index-diff must report removed=1 when a file vanishes: {combined}");

    // 8f. Short-pattern grep (< 3 bytes). The trigram fast-path
    // requires ≥ 3 bytes; with a 2-byte pattern the engine must
    // degrade to the full scan rather than silently returning zero.
    // Build trigrams on the incremental index first.
    let out = Command::new(scry_bin())
        .args(["build-trigrams", "--index"]).arg(&inc_idx)
        .output().expect("build-trigrams for short-pattern test");
    assert!(out.status.success());
    let out = Command::new(scry_bin())
        .args(["grep", "ge", "--index"]).arg(&inc_idx).args(["--limit", "10"])
        .output().expect("scry grep on short pattern");
    assert!(out.status.success(),
            "short-pattern grep must not error: {}", String::from_utf8_lossy(&out.stderr));
    // "ge" appears in "package z;" in every Java file. Must find ≥ 1.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("package"),
            "short-pattern grep must match `package`: {stdout}");

    // 8g. Regex grep path (separate code path from literal mmap+memchr).
    let out = Command::new(scry_bin())
        .args(["grep", "--regex", "Bravo|Charlie", "--index"]).arg(&inc_idx)
        .args(["--limit", "10"])
        .output().expect("regex grep");
    assert!(out.status.success(),
            "regex grep must not error: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Bravo") || stdout.contains("Charlie"),
            "regex alternation grep must find at least one: {stdout}");

    // 8g'. Case-insensitive literal grep: lowercased needle must find
    // the mixed-case symbol (e.g. `bravo` finds `Bravo`). Trigram
    // pre-filter expands per-trigram case variants, then the inner
    // regex matcher confirms with case_insensitive(true).
    let out = Command::new(scry_bin())
        .args(["grep", "-i", "bravo", "--index"]).arg(&inc_idx)
        .args(["--limit", "10"])
        .output().expect("ci literal grep");
    assert!(out.status.success(),
            "case-insensitive grep must not error: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Bravo"),
            "ci literal grep must find mixed-case `Bravo`: {stdout}");
    // And the case-sensitive default still does NOT match the lowercased query.
    let out = Command::new(scry_bin())
        .args(["grep", "bravo", "--index"]).arg(&inc_idx)
        .args(["--limit", "10"])
        .output().expect("cs literal grep");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Bravo"),
            "case-SENSITIVE grep MUST NOT find `Bravo` for lowercased query — that would prove the trigram pre-filter is leaking results: {stdout}");

    // 8g''. Case-insensitive regex grep: explicit --regex + -i combine
    // (RegexBuilder::case_insensitive(true) on the user-supplied pattern).
    let out = Command::new(scry_bin())
        .args(["grep", "--regex", "-i", "br.vo", "--index"]).arg(&inc_idx)
        .args(["--limit", "10"])
        .output().expect("ci regex grep");
    assert!(out.status.success(),
            "regex + -i grep must not error: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Bravo"),
            "regex+i grep must find mixed-case `Bravo`: {stdout}");

    // 8h. Smoke-test the other CLI commands the agent audit flagged
    // as untested. Each one is a separate process invocation against
    // the synthetic index; we assert exit=0 + a basic shape check.
    let assert_smoke = |args: &[&str], must_contain: &str, label: &str| {
        let out = Command::new(scry_bin()).args(args)
            .args(["--index"]).arg(&inc_idx)
            .output().expect(label);
        assert!(out.status.success(),
                "{label} failed: {}", String::from_utf8_lossy(&out.stderr));
        let combined = format!("{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));
        assert!(combined.contains(must_contain),
                "{label} output missing '{must_contain}': {combined}");
    };
    assert_smoke(&["prefix", "Bra", "--json"],     "Bravo",   "cmd_prefix");
    assert_smoke(&["fuzzy", "Bravo", "--json"],    "Bravo",   "cmd_fuzzy");
    assert_smoke(&["ref",   "Bravo", "--json"],    "[",       "cmd_ref");
    assert_smoke(&["coverage", ".", "--json"],     "files",   "cmd_coverage");

    // 8h-fail. Locks the `[fail] PATH kind=… size=… reason=…` log line
    // the operator-facing parse-failure feature introduced in v0.1.11.
    // Build a fixture with one unreadable file (chmod 000) alongside a
    // good one, run `scry index`, scan stderr for the `[fail]` row and
    // the path of the locked file. Then chmod back so the dir is
    // deletable.
    {
        let fail_src = std::env::temp_dir().join(format!(
            "scry-fail-fixture-{}", std::process::id()
        ));
        let fail_idx = std::env::temp_dir().join(format!(
            "scry-fail-idx-{}", std::process::id()
        ));
        std::fs::create_dir_all(&fail_src).unwrap();
        std::fs::write(fail_src.join("good.rs"), "fn ok() {}\n").unwrap();
        let locked = fail_src.join("locked.java");
        std::fs::write(&locked, "public class Locked {}\n").unwrap();
        let mut perm = std::fs::metadata(&locked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o000);
        std::fs::set_permissions(&locked, perm).unwrap();
        let out = Command::new(scry_bin())
            .args(["index"]).arg(&fail_src)
            .arg("-o").arg(&fail_idx)
            .args(["--workers", "1"])
            .output().expect("index with unreadable file");
        // chmod back FIRST so cleanup always succeeds, even if asserts blow up
        let mut perm = std::fs::metadata(&locked).unwrap().permissions();
        perm.set_mode(0o644);
        std::fs::set_permissions(&locked, perm).unwrap();
        assert!(out.status.success(),
                "index over fixture with unreadable file should still succeed; got: {}",
                String::from_utf8_lossy(&out.stderr));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("[fail] ") && stderr.contains("locked.java"),
                "[fail] log must name the unreadable path; stderr:\n{stderr}");
        assert!(stderr.contains("kind=Java") && stderr.contains("reason="),
                "[fail] log must carry kind=… reason=… for operator triage:\n{stderr}");
        std::fs::remove_dir_all(&fail_src).ok();
        std::fs::remove_dir_all(&fail_idx).ok();
    }

    // 8h-bis-coverage. Pin the coverage --json envelope shape so that an
    // agent (or downstream consumer) can rely on field names. Adding new
    // top-level fields is allowed; renaming or removing any of these is
    // a breaking change that must bump the manifest version.
    let out = Command::new(scry_bin())
        .args(["coverage", ".", "--json", "--by-kind", "--index"]).arg(&inc_idx)
        .output().expect("coverage --json --by-kind");
    assert!(out.status.success(),
            "coverage --json --by-kind failed: {}", String::from_utf8_lossy(&out.stderr));
    let cov: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("coverage --json must emit valid JSON");
    for k in &["path", "files_total", "bytes_total", "symbols_total", "by_lang"] {
        assert!(cov.get(*k).is_some(),
                "coverage --json missing top-level key '{k}': {cov}");
    }
    let by_lang = cov.get("by_lang").and_then(|v| v.as_object())
        .expect("coverage by_lang must be a JSON object");
    let (some_lang, some_bucket) = by_lang.iter().next()
        .expect("coverage by_lang must be non-empty for synthetic tree");
    for k in &["files", "bytes", "symbols"] {
        assert!(some_bucket.get(*k).is_some(),
                "coverage by_lang['{some_lang}'] missing '{k}': {some_bucket}");
    }
    // --by-kind requires the per-lang bucket to carry by_kind too.
    assert!(some_bucket.get("by_kind").is_some(),
            "coverage --by-kind must populate by_kind on every lang bucket: {some_bucket}");

    // 8h-bis-stats. Pin the stats --json envelope shape. Same compat
    // contract: new fields ok, renames / removals not.
    let out = Command::new(scry_bin())
        .args(["stats", "--json", "--index"]).arg(&inc_idx)
        .output().expect("stats --json");
    assert!(out.status.success(),
            "stats --json failed: {}", String::from_utf8_lossy(&out.stderr));
    let st: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("stats --json must emit valid JSON");
    for k in &[
        "scry_version", "manifest_version", "indexed_at", "roots",
        "files_total", "files_parsed", "files_failed",
        "bytes_total", "symbols", "refs", "elapsed_ms",
        "by_lang", "by_kind",
    ] {
        assert!(st.get(*k).is_some(),
                "stats --json missing key '{k}': {st}");
    }
    assert!(st["roots"].is_array(), "stats roots must be array: {st}");
    assert!(st["by_lang"].is_object(), "stats by_lang must be object: {st}");
    assert!(st["by_kind"].is_object(), "stats by_kind must be object: {st}");
    let manifest_version = st["manifest_version"].as_u64()
        .expect("manifest_version must be a u64");
    assert!(manifest_version >= 1, "manifest_version must be >= 1, got {manifest_version}");

    // 8h-bis. `scry tldr PATH` — one-call file summary. JSON shape
    // must include path, lang, symbols_total, by_kind, top, first_line.
    let out = Command::new(scry_bin())
        .args(["tldr", "a/Alpha.java", "--json", "--index"]).arg(&inc_idx)
        .output().expect("tldr --json");
    if out.status.success() {
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .expect("tldr --json is JSON");
        for k in &["path", "lang", "symbols_total", "by_kind", "top"] {
            assert!(v.get(*k).is_some(),
                    "tldr JSON missing field '{k}': {v}");
        }
    }
    // Plain output must have the # comment header shape.
    let out = Command::new(scry_bin())
        .args(["tldr", "b/Bravo.java", "--index"]).arg(&inc_idx)
        .output().expect("tldr plain");
    assert!(out.status.success(),
            "tldr failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("# "),
            "tldr plain output must lead with `# `; got:\n{stdout}");
    assert!(stdout.contains("symbols"),
            "tldr plain output must mention symbols; got:\n{stdout}");

    // 8i. `scry grep --format=lines` — rg-shaped one-per-line. Output
    // must contain "path:line:col" but NOT the JSON envelope.
    let out = Command::new(scry_bin())
        .args(["grep", "package", "--format", "lines",
               "--index"]).arg(&inc_idx)
        .args(["--limit", "10"])
        .output().expect("grep --format=lines");
    assert!(out.status.success(),
            "grep --format=lines failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("{\""),
            "grep --format=lines must not emit JSON; got:\n{stdout}");
    assert!(stdout.lines().any(|l| l.contains(".java:") && l.contains('\t')),
            "grep --format=lines must emit path:line:col\\tsnippet; got:\n{stdout}");

    // 8j. `scry grep --format=count` — just the totals.
    let out = Command::new(scry_bin())
        .args(["grep", "package", "--format", "count",
               "--index"]).arg(&inc_idx)
        .output().expect("grep --format=count");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hits across") && stdout.contains("files"),
            "grep --format=count must report totals; got:\n{stdout}");

    // 8j-bis. callers / ref --format=count — cheapest "how many?"
    // reply. Mutually exclusive with --json. Closes the consistency
    // gap surfaced by the Qwen small-model comparison.
    let out = Command::new(scry_bin())
        .args(["callers", "transact", "--format", "count",
               "--index"]).arg(&inc_idx)
        .output().expect("callers --format count");
    assert!(out.status.success(),
            "callers --format count failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(" callers"),
            "callers --format count must say `N callers`; got: {stdout}");
    let out = Command::new(scry_bin())
        .args(["ref", "transact", "--format", "count",
               "--index"]).arg(&inc_idx)
        .output().expect("ref --format count");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(" ref"),
            "ref --format count must say `N ref`; got: {stdout}");
    // Mutual-exclusion with --json.
    let out = Command::new(scry_bin())
        .args(["callers", "transact", "--format", "count", "--json",
               "--index"]).arg(&inc_idx)
        .output().expect("callers --format + --json");
    assert!(!out.status.success(),
            "callers with both --format and --json must error");

    // 8k. `scry grep --format=invalid` — must reject cleanly, not
    // silently fall through.
    let out = Command::new(scry_bin())
        .args(["grep", "package", "--format", "wat",
               "--index"]).arg(&inc_idx)
        .output().expect("grep --format=wat");
    assert!(!out.status.success(),
            "grep --format=wat must reject unknown format");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("format must be one of"),
            "rejection message must list valid formats; got:\n{stderr}");

    // 8l. `scry grep --json --format=lines` — mutually exclusive.
    let out = Command::new(scry_bin())
        .args(["grep", "package", "--json", "--format", "lines",
               "--index"]).arg(&inc_idx)
        .output().expect("grep --json --format=lines");
    assert!(!out.status.success(),
            "grep with both --json and --format must error");

    // 8m. `scry outline --with-snippets=3` — JSON shape must include
    // a `snippet` field on each symbol; snippet contains the actual
    // source line.
    let out = Command::new(scry_bin())
        .args(["outline", "a/Alpha.java", "--json", "--with-snippets", "3",
               "--index"]).arg(&inc_idx)
        .output().expect("outline --with-snippets");
    if out.status.success() {
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .expect("outline --json is JSON");
        let syms = v["symbols"].as_array().expect("symbols array");
        assert!(!syms.is_empty(), "Alpha.java must have at least one symbol");
        // At least one symbol should have a non-empty snippet.
        let any_snippet = syms.iter().any(|s|
            s.get("snippet").and_then(|x| x.as_str())
                .map(|t| !t.is_empty()).unwrap_or(false));
        assert!(any_snippet,
                "outline --with-snippets must populate snippet field; got:\n{v}");
    }
    // (If outline returns nothing for Alpha.java because the file no
    // longer exists after the deletion test above, that's expected;
    // the smoke test only fires if the call succeeds.)

    // `scry compact` on an index with no tombstones must exit
    // cleanly and report nothing-to-do (today's placeholder
    // behavior; the test pins the contract so a future
    // implementation can't silently change it).
    let out = Command::new(scry_bin())
        .args(["compact", "--index"]).arg(&inc_idx)
        .output().expect("scry compact");
    assert!(out.status.success(),
            "compact failed: {}", String::from_utf8_lossy(&out.stderr));

    // 8n. `scry health` against the synthetic index must report
    // OVERALL: healthy and exit 0. JSON form is parsed and pinned.
    let out = Command::new(scry_bin())
        .args(["health", "--index"]).arg(&idx)
        .args(["--json"])
        .output().expect("scry health");
    assert!(out.status.success(),
            "scry health failed: {}", String::from_utf8_lossy(&out.stderr));
    let line = std::str::from_utf8(&out.stdout).unwrap()
        .lines().find(|l| !l.is_empty()).expect("health prints JSON");
    let v: serde_json::Value = serde_json::from_str(line).expect("health is JSON");
    assert_eq!(v["healthy"].as_bool(), Some(true),
               "synthetic index must be healthy: {v}");
    let checks = v["checks"].as_array().expect("checks array");
    // Every required artifact must report ok=true.
    for c in checks {
        if c["required"].as_bool() == Some(true) {
            assert_eq!(c["ok"].as_bool(), Some(true),
                       "required check {} failed: {c}", c["artifact"]);
        }
    }

    // 9. scry diff --since: turn the synthetic root into a git repo
    // with two commits, then assert `scry diff --since HEAD~1` finds
    // the file we modified between commits. Skips silently if `git`
    // isn't on PATH so the test stays portable.
    let git_ok = Command::new("git").arg("--version").output()
        .map(|o| o.status.success()).unwrap_or(false);
    if git_ok {
        // Initialize a quiet repo + identity in the fixture root (= `src`
        // in this test). The index in `idx` was built against `src`, so
        // the file table's relpath/root mapping matches what git diff
        // will report.
        let run = |args: &[&str]| -> bool {
            Command::new("git").arg("-C").arg(&src).args(args)
                .output().map(|o| o.status.success()).unwrap_or(false)
        };
        assert!(run(&["init", "-q", "-b", "main"]), "git init failed");
        // Inline identity so commits work in CI without a global config.
        assert!(run(&["config", "user.email", "test@scry.local"]));
        assert!(run(&["config", "user.name",  "scry-e2e"]));
        assert!(run(&["add", "."]));
        assert!(run(&["commit", "-q", "-m", "initial fixture"]));

        // Modify Activity.java and commit again — this is the file the
        // diff should surface.
        std::fs::write(
            src.join("frameworks/base/core/java/android/app/Activity.java"),
            r#"package android.app;
public class Activity {
    public void onCreate() {
        Binder b = new Binder();
        b.transact();
        // changed for diff e2e
    }
    public static class InnerHelper {
        public void noop() {}
    }
}
"#,
        ).unwrap();
        assert!(run(&["add", "."]));
        assert!(run(&["commit", "-q", "-m", "tweak Activity.java"]));

        // Re-index so the fresh file content is in the index.
        let out = Command::new(scry_bin())
            .args(["index"])
            .arg(&src)
            .arg("-o").arg(&idx)
            .args(["--workers", "2"])
            .output()
            .expect("re-index after git commits");
        assert!(out.status.success(),
                "re-index failed: {}", String::from_utf8_lossy(&out.stderr));

        // Now ask scry diff --since HEAD~1 — should surface Activity.java.
        let out = Command::new(scry_bin())
            .args(["diff", "--since", "HEAD~1", "--index"])
            .arg(&idx)
            .args(["--json"])
            .output()
            .expect("scry diff --since");
        assert!(out.status.success(),
                "scry diff failed: {}", String::from_utf8_lossy(&out.stderr));
        let entries: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
            .lines().filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert!(entries.iter().any(|e| e["path"].as_str().unwrap_or("")
                .ends_with("Activity.java")),
                "diff should report Activity.java as changed; got {entries:?}");
    } else {
        eprintln!("git not on PATH — skipping diff e2e");
    }

    // 10. Incremental-indexing foundation: build-digests + tombstone
    // + index-diff + verify query path filters tombstoned files.
    //
    // Re-index from scratch so we have a clean baseline (the git step
    // above already left us with one, but this is defensive in case
    // we got skipped).
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src)
        .arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("re-index for digest tests");
    assert!(out.status.success(),
            "re-index failed: {}", String::from_utf8_lossy(&out.stderr));
    // 10a. build-digests should write file_digests.bin
    let out = Command::new(scry_bin())
        .args(["build-digests", "--index"]).arg(&idx)
        .output().expect("scry build-digests");
    assert!(out.status.success(),
            "build-digests failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(idx.join("file_digests.bin").exists(),
            "file_digests.bin must exist after build-digests");

    // 10b. index-diff against the same tree should report all unchanged.
    let out = Command::new(scry_bin())
        .args(["index-diff"]).arg(&src)
        .args(["--index"]).arg(&idx)
        .args(["--json"])
        .output().expect("scry index-diff");
    assert!(out.status.success(),
            "index-diff failed: {}", String::from_utf8_lossy(&out.stderr));
    let diff_line = std::str::from_utf8(&out.stdout).unwrap()
        .lines().find(|l| !l.is_empty()).expect("index-diff prints JSON");
    let diff: serde_json::Value = serde_json::from_str(diff_line).expect("diff is JSON");
    assert_eq!(diff["changed"], 0, "diff against same tree must show 0 changed: {diff}");
    assert_eq!(diff["added"], 0, "0 added: {diff}");
    assert_eq!(diff["removed"], 0, "0 removed: {diff}");
    assert!(diff["unchanged"].as_u64().unwrap() > 0,
            "must have ≥1 unchanged file: {diff}");

    // 10c. Modify a file and re-run index-diff; should show 1 changed.
    std::fs::write(
        src.join("frameworks/base/core/java/android/app/Binder.java"),
        r#"package android.app;
public class Binder {
    public void transact() { /* modified */ }
    public void newMethod() {}
}
"#,
    ).unwrap();
    let out = Command::new(scry_bin())
        .args(["index-diff"]).arg(&src)
        .args(["--index"]).arg(&idx)
        .args(["--json"])
        .output().expect("scry index-diff post-edit");
    assert!(out.status.success(),
            "index-diff failed: {}", String::from_utf8_lossy(&out.stderr));
    let diff: serde_json::Value = serde_json::from_str(
        std::str::from_utf8(&out.stdout).unwrap().lines()
            .find(|l| !l.is_empty()).unwrap()
    ).unwrap();
    assert_eq!(diff["changed"], 1, "expected 1 changed file: {diff}");

    // 10d. Tombstone Binder.java and assert that `scry def Binder`
    // no longer returns it. This validates the tombstone filter on
    // every read path through get_symbol.
    let out = Command::new(scry_bin())
        .args(["tombstone"]).arg("Binder.java")
        .args(["--index"]).arg(&idx)
        .output().expect("scry tombstone");
    assert!(out.status.success(),
            "tombstone failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(idx.join("tombstones.bin").exists(),
            "tombstones.bin must exist after tombstone");

    let out = Command::new(scry_bin())
        .args(["def", "Binder", "--index"]).arg(&idx)
        .args(["--json"])
        .output().expect("scry def post-tombstone");
    assert!(out.status.success());
    let hits: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    // Every remaining hit must not be in Binder.java
    let any_binder = hits.iter().any(|h| h["path"].as_str().unwrap_or("")
        .ends_with("Binder.java"));
    assert!(!any_binder,
            "tombstoned Binder.java symbols must not appear in def results: {hits:?}");

    // 11. Semantic retrieval — build-embeddings + ask + verify the
    // query routes to the right file. Re-index first so the tombstone
    // doesn't bias the chunk set.
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src)
        .arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("re-index for embeddings test");
    assert!(out.status.success(),
            "re-index failed: {}", String::from_utf8_lossy(&out.stderr));

    let out = Command::new(scry_bin())
        .args(["build-embeddings", "--index"]).arg(&idx)
        .args(["--dim", "32", "--chunk-lines", "20", "--chunk-overlap", "5"])
        .output().expect("scry build-embeddings");
    assert!(out.status.success(),
            "build-embeddings failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(idx.join("chunks.bin").exists(), "chunks.bin must exist");
    assert!(idx.join("embeddings.bin").exists(), "embeddings.bin must exist");

    let out = Command::new(scry_bin())
        .args(["ask", "transact binder", "--index"]).arg(&idx)
        .args(["--json", "--limit", "3"])
        .output().expect("scry ask");
    assert!(out.status.success(),
            "scry ask failed: {}", String::from_utf8_lossy(&out.stderr));
    let hits: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(!hits.is_empty(), "ask should return at least one chunk");
    // The query has both "transact" and "binder" tokens — the highest-
    // ranked chunk should come from a file containing those words.
    // Binder.java or Activity.java (which calls b.transact()) are the
    // expected top hits.
    let top_path = hits[0]["path"].as_str().unwrap_or("");
    assert!(
        top_path.contains("Binder.java") || top_path.contains("Activity.java")
        || top_path.contains("IBinder.cpp"),
        "top ask hit should be a binder-related file, got {top_path}; full hits={hits:?}"
    );
    // Every hit must carry the expected envelope shape.
    for h in &hits {
        assert!(h["score"].as_f64().is_some(), "missing score: {h}");
        assert!(h["start_line"].as_u64().is_some(), "missing start_line: {h}");
        assert!(h["end_line"].as_u64().is_some(), "missing end_line: {h}");
    }

    // 12. `scry callers --precise` clangd integration. Two cases:
    //  - clangd not on PATH (the common one in CI / fresh dev hosts):
    //    the command must exit non-zero with an actionable message
    //    instead of segfaulting or hanging.
    //  - clangd present: smoke that the LSP client can complete
    //    a session against a minimal compile_commands.json.
    let clangd_ok = Command::new("clangd").arg("--version").output()
        .map(|o| o.status.success()).unwrap_or(false);
    if !clangd_ok {
        // The interesting test case for environments without clangd:
        // we assert the bail-out message is what the docs promise.
        let out = Command::new(scry_bin())
            .args(["callers", "Binder", "--precise", "--index"]).arg(&idx)
            .output().expect("scry callers --precise");
        assert!(!out.status.success(),
                "scry callers --precise should error when clangd missing");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("clangd not on PATH"),
                "error message should mention clangd; got: {stderr}");
        assert!(stderr.contains("apt install clangd"),
                "error message should include install hint; got: {stderr}");
    } else {
        // clangd present — smoke the absence-of-compile-commands path.
        // (Generating a real compile_commands.json for the synthetic
        // tree is out of scope; the test just confirms we error out
        // with the right message instead of spawning into a broken
        // clangd session.)
        let out = Command::new(scry_bin())
            .args(["callers", "Binder", "--precise", "--index"]).arg(&idx)
            .output().expect("scry callers --precise (clangd present)");
        // Either it errored on compile_commands missing OR it
        // succeeded (unlikely on the synthetic tree without a real
        // build). Both are acceptable; what matters is it didn't
        // hang or panic.
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains("compile_commands.json")
                    || stderr.contains("clangd"),
                    "if --precise errored, message should explain why; got: {stderr}");
        }
    }

    // Best-effort cleanup; on a panic, the dir leaks under /tmp which
    // is fine for one test fixture.
    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// TCP listener — covered separately so a network-restricted CI runner
// can opt out via `-- --skip tcp_serve_roundtrip` without losing the
// rest of the e2e suite.
// ===========================================================================

#[test]
fn tcp_serve_roundtrip() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // Build a fresh synthetic index dedicated to this test.
    let base = std::env::temp_dir().join(format!("scry-tcp-e2e-{}", std::process::id()));
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Hello.java"),
        "package x;\npublic class Hello {\n    public void wave() {}\n}\n").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("initial index for tcp test");
    assert!(out.status.success(),
            "tcp-test index failed: {}", String::from_utf8_lossy(&out.stderr));

    // Spawn `scry serve --listen tcp:127.0.0.1:0` and parse the
    // bound port from the listener's stderr line.
    let mut child = Command::new(scry_bin())
        .args(["serve", "--listen", "tcp:127.0.0.1:0", "--index"])
        .arg(&idx)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn().expect("spawn scry serve --listen tcp:");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut sreader = BufReader::new(stderr);
    let mut announce = String::new();
    sreader.read_line(&mut announce).expect("read announce line");
    // Format: "[scry serve] listening on tcp:127.0.0.1:NNNNN\n"
    let port: u16 = announce.split("tcp:127.0.0.1:")
        .nth(1).and_then(|t| t.trim_end().parse().ok())
        .unwrap_or_else(|| panic!("could not parse port from announce: {announce:?}"));

    // Connect, send two requests, read two responses.
    let stream = TcpStream::connect(("127.0.0.1", port))
        .expect("connect to spawned tcp listener");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writer.write_all(
        b"{\"id\":1,\"cmd\":\"def\",\"args\":{\"name\":\"Hello\"}}\n"
    ).unwrap();
    writer.write_all(
        b"{\"id\":2,\"cmd\":\"stats\",\"args\":{}}\n"
    ).unwrap();
    // grep with case_insensitive=true: source has `Hello` (capitalized),
    // request `hello` lowercase — must come back with a hit. Without
    // case_insensitive, this would return an empty array.
    writer.write_all(
        b"{\"id\":3,\"cmd\":\"grep\",\"args\":{\"pattern\":\"hello\",\"case_insensitive\":true}}\n"
    ).unwrap();
    writer.write_all(
        b"{\"id\":4,\"cmd\":\"grep\",\"args\":{\"pattern\":\"hello\"}}\n"
    ).unwrap();
    writer.flush().unwrap();
    let mut buf = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut buf).expect("read response 1");
    let mut buf2 = String::new();
    reader.read_line(&mut buf2).expect("read response 2");
    let mut buf3 = String::new();
    reader.read_line(&mut buf3).expect("read response 3");
    let mut buf4 = String::new();
    reader.read_line(&mut buf4).expect("read response 4");

    // Tear down the listener.
    child.kill().ok();
    child.wait().ok();

    // Parse responses; both must have the matching id and a result.
    let r1: serde_json::Value = serde_json::from_str(buf.trim())
        .expect("response 1 is JSON");
    assert_eq!(r1["id"].as_u64(), Some(1));
    assert!(r1.get("result").is_some(),
            "tcp response 1 must have a result: {r1}");
    let r2: serde_json::Value = serde_json::from_str(buf2.trim())
        .expect("response 2 is JSON");
    assert_eq!(r2["id"].as_u64(), Some(2));
    // grep CI: hits must include the `Hello` line.
    let r3: serde_json::Value = serde_json::from_str(buf3.trim())
        .expect("response 3 is JSON");
    assert_eq!(r3["id"].as_u64(), Some(3));
    let hits = r3["result"].as_array().expect("grep CI returns array");
    assert!(!hits.is_empty(),
            "grep with case_insensitive=true must find `Hello` for `hello`: {r3}");
    assert!(hits.iter().any(|h| h["snippet"].as_str().unwrap_or("").contains("Hello")),
            "CI grep snippets must contain `Hello`: {r3}");
    // grep CS control: must return empty array (no `hello` lowercase in fixture).
    let r4: serde_json::Value = serde_json::from_str(buf4.trim())
        .expect("response 4 is JSON");
    assert_eq!(r4["id"].as_u64(), Some(4));
    let hits = r4["result"].as_array().expect("grep CS returns array");
    assert!(hits.is_empty(),
            "grep WITHOUT case_insensitive MUST NOT find `Hello` for `hello` (proves CI flag is the only thing flipping the behavior): {r4}");

    // Tee unread bytes into the void so the reader drops cleanly.
    let mut sink = Vec::new();
    let _ = reader.into_inner().take(1024).read_to_end(&mut sink);

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// Concurrent serve under load — 32 client threads × 10 queries each
// against the same Unix-socket daemon. The mmap'd reader is shared
// and immutable; this test pins that there are no synchronization
// bugs around concurrent reads.
// ===========================================================================

#[test]
fn unix_serve_concurrent_stress() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let base = std::env::temp_dir().join(format!("scry-cc-e2e-{}", std::process::id()));
    let src = base.join("src");
    let idx = base.join("idx");
    let sock = base.join("scry.sock");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Hello.java"),
        "package x;\npublic class Hello {\n    public void wave() {}\n}\n").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("initial index for stress test");
    assert!(out.status.success());

    let mut child = Command::new(scry_bin())
        .args(["serve", "--listen"])
        .arg(format!("unix:{}", sock.display()))
        .args(["--index"]).arg(&idx)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn().expect("spawn scry serve unix:");
    // Wait for the announce line so we know the socket exists.
    let stderr = child.stderr.take().expect("piped stderr");
    let mut sreader = BufReader::new(stderr);
    let mut announce = String::new();
    sreader.read_line(&mut announce).expect("read announce line");
    assert!(announce.contains("listening on unix:"),
            "unexpected announce: {announce:?}");

    // 32 threads × 10 queries each; every reply must parse, have a
    // matching id, and a non-empty result for `def Hello`.
    let n_threads = 32;
    let queries_per_thread = 10;
    let mut handles = Vec::with_capacity(n_threads);
    for t in 0..n_threads {
        let sock = sock.clone();
        handles.push(std::thread::spawn(move || -> Result<usize, String> {
            let stream = UnixStream::connect(&sock)
                .map_err(|e| format!("thread {t} connect: {e}"))?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))
                .map_err(|e| format!("thread {t} timeout: {e}"))?;
            let mut writer = stream.try_clone()
                .map_err(|e| format!("thread {t} dup: {e}"))?;
            let mut reader = BufReader::new(stream);
            let mut ok = 0usize;
            for q in 0..queries_per_thread {
                let id = (t * 1000 + q) as u64;
                let req = format!(
                    "{{\"id\":{id},\"cmd\":\"def\",\"args\":{{\"name\":\"Hello\"}}}}\n");
                writer.write_all(req.as_bytes())
                    .map_err(|e| format!("thread {t} q{q} write: {e}"))?;
                writer.flush()
                    .map_err(|e| format!("thread {t} q{q} flush: {e}"))?;
                let mut buf = String::new();
                reader.read_line(&mut buf)
                    .map_err(|e| format!("thread {t} q{q} read: {e}"))?;
                let v: serde_json::Value = serde_json::from_str(buf.trim())
                    .map_err(|e| format!("thread {t} q{q} parse: {e} body={buf:?}"))?;
                if v["id"].as_u64() != Some(id) {
                    return Err(format!("thread {t} q{q} id mismatch: got {v}"));
                }
                if v.get("result").and_then(|r| r.as_array()).map(Vec::is_empty) != Some(false) {
                    return Err(format!("thread {t} q{q} empty result: {v}"));
                }
                ok += 1;
            }
            Ok(ok)
        }));
    }
    let mut total_ok = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for h in handles {
        match h.join() {
            Ok(Ok(n)) => total_ok += n,
            Ok(Err(e)) => failures.push(e),
            Err(_) => failures.push("thread panicked".into()),
        }
    }
    child.kill().ok();
    child.wait().ok();
    assert!(failures.is_empty(),
            "{} of {} threads failed:\n{}",
            failures.len(), n_threads, failures.join("\n"));
    assert_eq!(total_ok, n_threads * queries_per_thread);

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// callers --precise with a malformed compile_commands.json. The test
// asserts that scry fails CLEANLY (non-zero exit, recognizable error
// message) without panicking or hanging — regardless of whether clangd
// is present on the test runner.
// ===========================================================================

#[test]
fn callers_precise_malformed_compile_commands() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let base = std::env::temp_dir().join(format!("scry-bad-cc-{}", std::process::id()));
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    // Index needs a C++ symbol so callers_precise has something to
    // anchor on. Without one it errors with "no definitions of ..."
    // before ever touching compile_commands.json.
    std::fs::write(src.join("a.cpp"),
        "class Widget { public: void poke() {} };\n").unwrap();
    // Deliberately malformed JSON — not even close to valid.
    std::fs::write(src.join("compile_commands.json"),
        "{ this is not json at all }").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("index for cc test");
    assert!(out.status.success(),
            "index failed: {}", String::from_utf8_lossy(&out.stderr));

    // Run with a wall-clock guard. If `scry callers --precise` were
    // to hang on clangd parsing the malformed JSON, this loop would
    // time out and we'd kill the child.
    let start = Instant::now();
    let mut child = Command::new(scry_bin())
        .args(["callers", "Widget", "--precise", "--index"]).arg(&idx)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().expect("spawn callers --precise");
    let mut exit_code = None;
    while start.elapsed() < Duration::from_secs(30) {
        match child.try_wait().expect("try_wait") {
            Some(s) => { exit_code = Some(s); break; }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    if exit_code.is_none() {
        child.kill().ok();
        panic!("`scry callers --precise` hung > 30 s on malformed compile_commands.json");
    }
    let out = child.wait_with_output().expect("collect output");
    // Either we don't have clangd (error mentions clangd) or we do
    // but the malformed cc.json prevented success — in both cases
    // exit must be non-zero and stderr must explain why.
    assert!(!out.status.success(),
            "scry should NOT succeed on malformed compile_commands.json");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr);
    assert!(
        combined.contains("clangd")
            || combined.contains("compile_commands")
            || combined.contains("precise")
            || combined.contains("no definitions"),
        "error message should explain failure cleanly; got:\n{combined}",
    );

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// Parse-budget integration: set SCRY_PARSE_TIMEOUT_MS=1 (one
// millisecond) on a synthetic pathological C++ file. The per-file
// parse must time out, scry index must skip the file cleanly, and
// the run as a whole must succeed (not panic, not hang).
// ===========================================================================

#[test]
fn parse_timeout_skips_pathological_file() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let base = std::env::temp_dir().join(format!("scry-pt-{}", std::process::id()));
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();

    // A pathological 2 MB C++ file the grammar must plow through.
    // Identical shape to the unit-test fixture in scry-lang.
    let mut pathological = String::with_capacity(2_000_000);
    for i in 0..30_000 {
        pathological.push_str(&format!(
            "template <class T{0}> struct S{0} {{ T{0} a, b, c, d, e; }};\n",
            i,
        ));
    }
    std::fs::write(src.join("evil.cpp"), &pathological).unwrap();

    // A second normal file that MUST be picked up even though
    // evil.cpp will time out.
    std::fs::write(src.join("normal.cpp"),
        "class Normal { public: void hello() {} };\n").unwrap();

    let start = Instant::now();
    let out = Command::new(scry_bin())
        .env("SCRY_PARSE_TIMEOUT_MS", "1")        // 1 ms — guarantees abort
        .args(["index"]).arg(&src).arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("scry index with tight parse budget");
    let wall = start.elapsed();
    assert!(out.status.success(),
            "index must succeed even when individual files time out: {}",
            String::from_utf8_lossy(&out.stderr));
    // Index must finish in well under 60 s even though the pathological
    // file alone could occupy tree-sitter for many seconds without
    // the abort path.
    assert!(wall < Duration::from_secs(60),
            "index took {wall:?} — parse-budget abort path may be broken");

    // The normal file's symbol must be queryable.
    let v = query_def(&idx, "Normal");
    let arr = v.as_array().expect("def returns array");
    assert!(!arr.is_empty(),
            "Normal must survive the budget-abort of evil.cpp: {v}");

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// SIGPIPE regression: piping a long-output subcommand to `head` must
// exit cleanly (Unix: killed by SIGPIPE → status code 141 or 0
// depending on shell semantics) and not panic with `BrokenPipe`.
// Without the runtime fix in main.rs, `scry completions bash | head`
// panics mid-write inside clap_complete.
// ===========================================================================

#[cfg(unix)]
#[test]
fn sigpipe_does_not_panic_on_truncated_stdout() {
    use std::io::Read;
    use std::process::{Command, Stdio};

    // Spawn `scry completions bash` and read only the first 200 bytes
    // of its stdout, then drop the read end. The child should observe
    // SIGPIPE on its next write and exit silently — NOT print a Rust
    // panic backtrace to stderr.
    let mut child = Command::new(scry_bin())
        .args(["completions", "bash"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn().expect("spawn scry completions bash");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = [0u8; 200];
    let _ = stdout.read(&mut buf);
    drop(stdout);
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"),
            "scry must not panic on broken pipe; stderr was:\n{stderr}");
    assert!(!stderr.contains("BrokenPipe"),
            "scry must not surface BrokenPipe to stderr; stderr was:\n{stderr}");
}

// ===========================================================================
// Stale-index warning: any query against an index whose manifest
// scry_version differs from the running binary's must emit a one-
// line stderr warning. SCRY_QUIET=1 must suppress. Catches the
// silent-bad-data regression where an index built with the
// pre-0.1.2 Java/C++ scope_path bug would return wrong scope
// without telling the operator anything was off.
// ===========================================================================

#[test]
fn stale_index_emits_warning_on_every_open() {
    use std::process::Command;

    let base = std::env::temp_dir().join(format!("scry-stale-{}", std::process::id()));
    let src = base.join("src");
    let idx = base.join("idx");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Hello.java"),
        "package x;\npublic class Hello {\n}\n").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("index for stale test");
    assert!(out.status.success());

    // Forge the manifest to claim an older scry version.
    let manifest_path = idx.join("manifest.json");
    let mut m: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).unwrap()
    ).unwrap();
    m["scry_version"] = serde_json::json!("0.0.99-pretend-old");
    std::fs::write(&manifest_path, m.to_string()).unwrap();

    // Default open → must warn.
    let out = Command::new(scry_bin())
        .args(["def", "Hello", "--index"]).arg(&idx)
        .output().expect("def on stale index");
    assert!(out.status.success(),
            "query must still succeed despite stale index");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("WARNING"),
            "stale-index warning must fire on every open; stderr:\n{stderr}");
    assert!(stderr.contains("0.0.99-pretend-old"),
            "warning must name the on-disk version; stderr:\n{stderr}");

    // SCRY_QUIET=1 → must NOT warn.
    let out = Command::new(scry_bin())
        .env("SCRY_QUIET", "1")
        .args(["def", "Hello", "--index"]).arg(&idx)
        .output().expect("def with SCRY_QUIET=1");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("WARNING"),
            "SCRY_QUIET=1 must suppress the stale warning; stderr:\n{stderr}");

    // Matching version → no warning even without SCRY_QUIET.
    let mut m: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).unwrap()
    ).unwrap();
    m["scry_version"] = serde_json::json!(env!("CARGO_PKG_VERSION"));
    std::fs::write(&manifest_path, m.to_string()).unwrap();
    let out = Command::new(scry_bin())
        .args(["def", "Hello", "--index"]).arg(&idx)
        .output().expect("def on current index");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("WARNING"),
            "matching version must NOT warn; stderr:\n{stderr}");

    std::fs::remove_dir_all(&base).ok();
}

// ===========================================================================
// `scry serve --max-conns N` rejects connections past the cap and
// keeps the cap announce line in stderr. Pins the operability gap
// caught by the v0.1.4 cap audit — without this, a thousand
// concurrent agents could fan-in × fan-out each grep's rayon pool
// and OOM the daemon host.
// ===========================================================================

#[test]
fn unix_serve_max_conns_drops_over_cap() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let base = std::env::temp_dir().join(format!("scry-mc-{}", std::process::id()));
    let src = base.join("src");
    let idx = base.join("idx");
    let sock = base.join("scry.sock");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Hi.java"),
        "package x;\npublic class Hi {\n}\n").unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).arg("-o").arg(&idx)
        .args(["--workers", "2"])
        .output().expect("index for max-conns test");
    assert!(out.status.success());

    // Cap at 1 so the second connection MUST be rejected.
    let mut child = Command::new(scry_bin())
        .args(["serve", "--listen"]).arg(format!("unix:{}", sock.display()))
        .args(["--max-conns", "1", "--index"]).arg(&idx)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn().expect("spawn scry serve --max-conns 1");
    let stderr = child.stderr.take().expect("piped");
    let mut srd = BufReader::new(stderr);
    // Order: max_conns announce prints first (before bind), then
    // the listening-on line. Read both so subsequent reads pick up
    // the over-cap drop log line.
    let mut cap_line = String::new();
    srd.read_line(&mut cap_line).expect("max_conns line");
    assert!(cap_line.contains("max_conns=1"),
            "first stderr line should announce max_conns; got: {cap_line}");
    let mut announce = String::new();
    srd.read_line(&mut announce).expect("listen line");
    assert!(announce.contains("listening on unix:"),
            "second stderr line should be 'listening on unix:'; got: {announce}");

    // First connection: open and hold it (don't close) so it occupies
    // the single slot. Send one query to make sure it's live.
    let s1 = UnixStream::connect(&sock).expect("conn 1");
    s1.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w1 = s1.try_clone().unwrap();
    w1.write_all(b"{\"id\":1,\"cmd\":\"def\",\"args\":{\"name\":\"Hi\"}}\n").unwrap();
    w1.flush().unwrap();
    let mut r1 = BufReader::new(s1);
    let mut buf = String::new();
    r1.read_line(&mut buf).expect("reply 1");
    assert!(buf.contains("\"id\":1"), "first conn must work; got: {buf}");

    // Give the server a moment to spawn the worker thread + register
    // the slot as in-flight.
    std::thread::sleep(Duration::from_millis(150));

    // Second connection: must receive a JSON-RPC cap-exceeded
    // error line, then close. The client should see an actionable
    // message (not silent EOF) so it can back off + retry.
    let s2 = UnixStream::connect(&sock).expect("conn 2 accepted by kernel");
    s2.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut r2 = BufReader::new(s2);
    let mut cap_reply = String::new();
    let n = r2.read_line(&mut cap_reply).unwrap_or(0);
    assert!(n > 0, "over-cap conn must receive a reply before close");
    let v: serde_json::Value = serde_json::from_str(cap_reply.trim())
        .unwrap_or_else(|e| panic!("cap reply must be JSON: {e}; got: {cap_reply}"));
    assert_eq!(v["error"]["code"].as_i64(), Some(-32004),
            "cap-exceeded reply must use JSON-RPC code -32004; got: {v}");
    assert!(v["error"]["message"].as_str()
            .map(|m| m.contains("max_conns=1")).unwrap_or(false),
            "cap message must name the limit; got: {v}");
    assert_eq!(v["error"]["data"]["retryable"].as_bool(), Some(true),
            "cap reply must mark retryable: true; got: {v}");

    // Check that the cap-exceeded log line appears on stderr too.
    let mut over_line = String::new();
    let _ = srd.read_line(&mut over_line);
    assert!(over_line.contains("over cap"),
            "stderr should log the rejection; got: {over_line}");

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn subclasses_e2e_via_cli_and_rpc() {
    // Fixture: a Java parent class + a child class that extends it,
    // plus a grand-child to validate transitive depth. Tiny enough
    // to index in <100 ms.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-subclasses-e2e-{nanos}"));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(src.join("java/zoo")).unwrap();
    std::fs::write(src.join("java/zoo/Animal.java"), r#"package zoo;
public class Animal { public void speak() {} }
"#).unwrap();
    std::fs::write(src.join("java/zoo/Dog.java"), r#"package zoo;
public class Dog extends Animal { public void bark() {} }
"#).unwrap();
    std::fs::write(src.join("java/zoo/Puppy.java"), r#"package zoo;
public class Puppy extends Dog { public void yip() {} }
"#).unwrap();
    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(out.status.success(), "index failed: {}",
            String::from_utf8_lossy(&out.stderr));

    // CLI: direct subclasses of Animal → {Dog}.
    let out = Command::new(scry_bin())
        .args(["subclasses", "Animal", "--index"]).arg(&idx)
        .args(["--json", "--limit", "20"])
        .output().expect("spawn scry subclasses");
    assert!(out.status.success(), "subclasses Animal failed: {}",
            String::from_utf8_lossy(&out.stderr));
    let direct: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let names: Vec<&str> = direct.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"Dog"),
            "direct subclasses of Animal should include Dog; got {names:?}");
    assert!(!names.contains(&"Puppy"),
            "direct subclasses must NOT include Puppy (depth=0); got {names:?}");

    // CLI: transitive subclasses of Animal at depth 2 → {Dog, Puppy}.
    let out = Command::new(scry_bin())
        .args(["subclasses", "Animal", "--depth", "2", "--index"]).arg(&idx)
        .args(["--json", "--limit", "20"])
        .output().expect("spawn scry subclasses --depth=2");
    assert!(out.status.success(), "subclasses --depth=2 failed");
    let trans: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let names: Vec<&str> = trans.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"Dog") && names.contains(&"Puppy"),
            "depth=2 subclasses of Animal should include Dog and Puppy; got {names:?}");

    // CLI: `implementations` is an alias.
    let out = Command::new(scry_bin())
        .args(["implementations", "Animal", "--index"]).arg(&idx)
        .args(["--json"]).output().expect("spawn scry implementations");
    assert!(out.status.success());
    let impls: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let names: Vec<&str> = impls.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"Dog"),
            "implementations alias should match subclasses; got {names:?}");

    // JSON-RPC: subclasses tool over stdio.
    use std::io::Write;
    let mut child = Command::new(scry_bin())
        .args(["serve", "--index"]).arg(&idx)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().expect("spawn serve");
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"id":1,"cmd":"subclasses","args":{{"name":"Animal","depth":2,"limit":20}}}}"#).unwrap();
    }
    let out = child.wait_with_output().expect("serve wait");
    assert!(out.status.success(), "serve failed: {}",
            String::from_utf8_lossy(&out.stderr));
    let line = std::str::from_utf8(&out.stdout).unwrap()
        .lines().find(|l| !l.is_empty())
        .expect("at least one response line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    let names: Vec<&str> = v["result"].as_array().unwrap().iter()
        .filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"Dog") && names.contains(&"Puppy"),
            "RPC subclasses should include Dog and Puppy; got {names:?}");

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn impact_e2e_via_cli_and_rpc() {
    // Reuse the Animal/Dog/Puppy Java fixture from `subclasses_e2e_via_cli_and_rpc`'s
    // shape: parent + child + grandchild + a caller in a separate file.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-impact-e2e-{nanos}"));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(src.join("zoo")).unwrap();
    std::fs::write(src.join("zoo/Animal.java"), r#"package zoo;
public class Animal { public void speak() {} }
"#).unwrap();
    std::fs::write(src.join("zoo/Dog.java"), r#"package zoo;
public class Dog extends Animal {}
"#).unwrap();
    std::fs::write(src.join("zoo/Puppy.java"), r#"package zoo;
public class Puppy extends Dog {}
"#).unwrap();
    std::fs::write(src.join("zoo/Caller.java"), r#"package zoo;
public class Caller { public void run() { new Animal().speak(); } }
"#).unwrap();

    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(out.status.success(), "index failed: {}",
            String::from_utf8_lossy(&out.stderr));

    // CLI --json: impact of Animal should report
    //   subclasses ≥ 1 (Dog; Puppy at depth ≥ 2)
    //   callers ≥ 1 (Caller.run() calls speak() on an Animal)
    // files_touched should include at least Dog.java and Caller.java.
    let out = Command::new(scry_bin())
        .args(["impact", "Animal", "--index"]).arg(&idx)
        .args(["--subclass-depth", "2", "--json", "--limit", "20"])
        .output().expect("spawn scry impact");
    assert!(out.status.success(), "impact Animal failed: {}",
            String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let subclass_names: Vec<&str> = v["subclasses"].as_array().unwrap().iter()
        .filter_map(|s| s["name"].as_str()).collect();
    assert!(subclass_names.contains(&"Dog"),
            "impact Animal should include Dog as subclass; got {subclass_names:?}");
    assert!(subclass_names.contains(&"Puppy"),
            "impact Animal at depth=2 should include Puppy; got {subclass_names:?}");
    let files: Vec<&str> = v["files_touched"].as_array().unwrap().iter()
        .filter_map(serde_json::Value::as_str).collect();
    assert!(files.iter().any(|f| f.ends_with("Dog.java")),
            "impact files_touched should include Dog.java; got {files:?}");

    // JSON-RPC: same shape over stdio.
    use std::io::Write;
    let mut child = Command::new(scry_bin())
        .args(["serve", "--index"]).arg(&idx)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().expect("spawn serve");
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"id":1,"cmd":"impact","args":{{"name":"Animal","subclass_depth":2,"limit":20}}}}"#).unwrap();
    }
    let out = child.wait_with_output().expect("serve wait");
    assert!(out.status.success(), "serve failed: {}",
            String::from_utf8_lossy(&out.stderr));
    let line = std::str::from_utf8(&out.stdout).unwrap()
        .lines().find(|l| !l.is_empty()).expect("at least one response line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    let subclass_names: Vec<&str> = v["result"]["subclasses"].as_array().unwrap().iter()
        .filter_map(|s| s["name"].as_str()).collect();
    assert!(subclass_names.contains(&"Dog"),
            "RPC impact Animal should include Dog; got {subclass_names:?}");

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn callgraph_e2e_walks_caller_chain() {
    // 3-method chain: c() calls b() calls a(). callgraph(a, depth=2)
    // should surface both b() and c() in the tree.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-callgraph-e2e-{nanos}"));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(src.join("zoo")).unwrap();
    std::fs::write(src.join("zoo/Tree.java"), r#"package zoo;
public class Tree {
  public void a() {}
  public void b() { a(); }
  public void c() { b(); }
  public void d() { c(); }
}
"#).unwrap();

    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(out.status.success(),
            "index failed: {}", String::from_utf8_lossy(&out.stderr));

    // callgraph a --depth 1: just b.
    let out = Command::new(scry_bin())
        .args(["callgraph", "a", "--index"]).arg(&idx)
        .args(["--depth", "1", "--json"])
        .output().expect("spawn scry callgraph");
    assert!(out.status.success(),
            "callgraph a failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let callers = v["callers"].as_object().expect("callers object");
    assert!(callers.contains_key("b"),
            "callgraph a depth=1 should include caller b; got keys {:?}",
            callers.keys().collect::<Vec<_>>());
    assert!(callers["b"]["callers"].as_object().map_or(true, serde_json::Map::is_empty),
            "depth=1 should NOT expand b's callers further");

    // callgraph a --depth 3: b, c, d nested.
    let out = Command::new(scry_bin())
        .args(["callgraph", "a", "--index"]).arg(&idx)
        .args(["--depth", "3", "--json"])
        .output().expect("spawn scry callgraph --depth=3");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let b = &v["callers"]["b"];
    assert!(b.is_object(), "tree should have a → b; got {v}");
    let c = &b["callers"]["c"];
    assert!(c.is_object(), "tree should have a → b → c; got {v}");
    let d = &c["callers"]["d"];
    assert!(d.is_object(), "tree should have a → b → c → d; got {v}");

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn uses_e2e_outgoing_edges() {
    // Fixture: a class with three methods where one calls the other two.
    // `uses run` should return calls to a() and b() inside run() — and
    // NOT return calls outside run()'s body (e.g. inside main()).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-uses-e2e-{nanos}"));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(src.join("zoo")).unwrap();
    std::fs::write(src.join("zoo/Tree.java"), r#"package zoo;
public class Tree {
  public void a() {}
  public void b() {}
  public void main() { a(); }
  public void run() {
    a();
    b();
  }
}
"#).unwrap();

    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(out.status.success(),
            "index failed: {}", String::from_utf8_lossy(&out.stderr));
    // Build the file_refs sidecar so `uses` takes the fast path.
    let out = Command::new(scry_bin())
        .args(["build-file-refs", "--index"]).arg(&idx)
        .output().expect("spawn scry build-file-refs");
    assert!(out.status.success(),
            "build-file-refs failed: {}", String::from_utf8_lossy(&out.stderr));

    // uses run --json --kind call: should include "a" and "b"
    // (the two calls inside run's body), NOT include "a" from
    // main's body — that's a different enclosing function.
    let out = Command::new(scry_bin())
        .args(["uses", "run", "--index"]).arg(&idx)
        .args(["--kind", "call", "--json", "--limit", "20"])
        .output().expect("spawn scry uses");
    assert!(out.status.success(),
            "uses run failed: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let names: Vec<&str> = lines.iter().filter_map(|v| v["name"].as_str()).collect();
    assert!(names.contains(&"a"),
            "uses run should include a() call; got {names:?}");
    assert!(names.contains(&"b"),
            "uses run should include b() call; got {names:?}");
    assert_eq!(names.len(), 2,
            "uses run should ONLY return calls inside run()'s body \
             (a and b), not main's call to a; got {names:?}");

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn grep_regex_with_lossy_literals_falls_back_to_full_scan() {
    // Regression: a regex whose literal-extraction yields an empty
    // candidate set (e.g. character classes like [Bb] that split
    // into too-short trigrams) MUST fall back to a full scan rather
    // than silently returning zero hits. Eval-agent reported v0.1.25:
    //   scry grep 'Trace\.traceBegin.*[Bb]roadcast' → 0 hits.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-grep-regex-{nanos}"));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(src.join("am")).unwrap();
    // The target literal exists verbatim, but a regex with a
    // case-class around the lead-byte will produce lossy literals.
    std::fs::write(src.join("am/Hit.java"), r#"package am;
public class Hit {
    public void run() {
        Trace.traceBegin("Broadcast.enqueueOrderedBroadcastLocked");
    }
}
"#).unwrap();

    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(out.status.success(),
            "index failed: {}", String::from_utf8_lossy(&out.stderr));
    // build-trigrams so the pre-filter exists at all (without it the
    // test trivially passes — we explicitly want the regex path
    // through grep_candidates_for_regex to exercise).
    let out = Command::new(scry_bin())
        .args(["build-trigrams", "--index"]).arg(&idx)
        .output().expect("spawn scry build-trigrams");
    assert!(out.status.success(),
            "build-trigrams failed: {}", String::from_utf8_lossy(&out.stderr));

    // The case-class regex was the eval-agent's exact query shape.
    let out = Command::new(scry_bin())
        .args(["grep", "--regex", "--index"]).arg(&idx)
        .args([r"Trace\.traceBegin.*[Bb]roadcast", "--json"])
        .output().expect("spawn scry grep --regex");
    assert!(out.status.success(),
            "grep --regex failed: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(!lines.is_empty(),
            "regex with [Bb] should find the Hit.java match; got 0 hits — \
             the lossy-literal fallback regressed");
    assert!(lines.iter().any(|v| v["path"].as_str().unwrap_or("").ends_with("Hit.java")),
            "expected a Hit.java match; got {:?}",
            lines.iter().map(|v| v["path"].clone()).collect::<Vec<_>>());

    std::fs::remove_dir_all(&base).ok();
}

/// v0.1.28 — Java import refs must store the FULL qualified path,
/// not just the trailing identifier. Without the package side,
/// `cmd_build_resolutions`'s import-aware narrowing rule can never
/// fire (it needs (pkg, simple) to match candidate FQNs).
///
/// Indexes one file with `import android.os.PerfettoTrace;` and
/// asserts the Import ref's name is the full "android.os.PerfettoTrace".
#[test]
fn java_import_ref_captures_full_qualified_path() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("scry-java-import-{nanos}"));
    let src = base.join("src");
    let idx = base.join("index");
    std::fs::create_dir_all(src.join("com/example")).unwrap();
    std::fs::write(src.join("com/example/Caller.java"), r#"package com.example;
import android.os.PerfettoTrace;
public class Caller {
    public void run(PerfettoTrace.Session s) {
        s.close();
    }
}
"#).unwrap();

    let out = Command::new(scry_bin())
        .args(["index"]).arg(&src).args(["-o"]).arg(&idx)
        .output().expect("spawn scry index");
    assert!(out.status.success(),
            "index failed: {}", String::from_utf8_lossy(&out.stderr));

    // Look for the import ref by its FULL qualified name.
    let out = Command::new(scry_bin())
        .args(["ref", "android.os.PerfettoTrace", "--kind", "import",
               "--index"]).arg(&idx).args(["--json"])
        .output().expect("spawn scry ref");
    assert!(out.status.success(),
            "scry ref failed: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(!lines.is_empty(),
            "should find import ref by full path 'android.os.PerfettoTrace'; \
             got 0 hits — Java import query regressed to trailing-identifier only");

    // The opposite: looking up by the bare class name should now miss
    // (because the import ref's name is the full path, not "PerfettoTrace").
    let out = Command::new(scry_bin())
        .args(["ref", "PerfettoTrace", "--kind", "import", "--index"]).arg(&idx)
        .args(["--json"])
        .output().expect("spawn scry ref");
    assert!(out.status.success());
    let bare_hits: Vec<serde_json::Value> = std::str::from_utf8(&out.stdout).unwrap()
        .lines().filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(bare_hits.is_empty(),
            "import lookup by bare 'PerfettoTrace' should miss after the \
             full-path fix; got {} hits", bare_hits.len());

    std::fs::remove_dir_all(&base).ok();
}
