//! Minimal LSP client for clangd integration (ROADMAP #3).
//!
//! Why hand-rolled rather than pulling in `lsp-server` / `tower-lsp`:
//! we need a very small subset of LSP (initialize, didOpen,
//! textDocument/references). The whole client fits in one file
//! and avoids dragging in a full async runtime + framework for the
//! handful of round-trips a `--precise` query needs.
//!
//! ## Wire format
//!
//! LSP messages are JSON-RPC 2.0 with a Content-Length header:
//!
//! ```text
//! Content-Length: 78\r\n
//! \r\n
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}
//! ```
//!
//! Notifications (no `id`) are one-way. Requests carry an `id`
//! that the server echoes in the response. We use sequential `id`s
//! per process.
//!
//! ## Lifecycle
//!
//! Each `ClangdSession` spawns one clangd subprocess and holds the
//! pipes until dropped. The intended use is short-lived (one session
//! per CLI invocation) but the same struct supports long-running
//! reuse — `scry serve` could keep one alive across the warm
//! StoreReader if that pattern becomes useful.
//!
//! ## Error policy
//!
//! Every public method returns `Result` so the caller can decide
//! whether to fall back to the heuristic path or surface the failure.
//! Clangd-not-on-PATH is the most common error and gets an explicit
//! message pointing at the installation step.

use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A live connection to a clangd subprocess. Drop closes the
/// stdin pipe, which signals clangd to exit (per LSP convention).
pub struct ClangdSession {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
    initialized: bool,
}

impl ClangdSession {
    /// Spawn `clangd` and complete the LSP `initialize` handshake.
    /// `compile_commands_dir` is the dir containing
    /// `compile_commands.json` — clangd discovers it by walking up
    /// from the file being analyzed, but pre-pointing it is more
    /// reliable for arbitrary file paths.
    ///
    /// Returns an actionable error when clangd is not on PATH or
    /// when initialize fails; both are common in fresh setups.
    pub fn start(compile_commands_dir: Option<&Path>) -> Result<Self> {
        let mut cmd = Command::new("clangd");
        cmd.arg("--background-index=false")  // we drive sync RPCs
            .arg("--header-insertion=never")
            .arg("--log=error");
        if let Some(dir) = compile_commands_dir {
            cmd.arg(format!("--compile-commands-dir={}", dir.display()));
        }
        let mut child = cmd
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!(
                "clangd not available on PATH: {e}. Install clangd to use --precise.\n\
                 Debian/Ubuntu: apt install clangd"
            ))?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut s = Self {
            child, stdin, stdout, next_id: 1, initialized: false,
        };
        s.initialize(compile_commands_dir)?;
        Ok(s)
    }

    /// Send the LSP `initialize` request + `initialized` notification.
    /// Called once at session start by `ClangdSession::start`.
    fn initialize(&mut self, root: Option<&Path>) -> Result<()> {
        let root_uri = root.map(|p| format!("file://{}", p.display()))
            .unwrap_or_else(|| "file:///".to_string());
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": false, "willSave": false },
                    "references": { "dynamicRegistration": false },
                    "definition": { "dynamicRegistration": false },
                }
            },
            "trace": "off",
        });
        let _resp = self.request("initialize", params)?;
        // Notify clangd we're ready (no response expected per spec).
        self.notify("initialized", serde_json::json!({}))?;
        self.initialized = true;
        Ok(())
    }

    /// Open a document so subsequent textDocument/* requests can
    /// reference it. clangd retains the buffer in memory across
    /// the session.
    pub fn did_open(&mut self, path: &Path, language: &str) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read {} for didOpen", path.display()))?;
        let params = serde_json::json!({
            "textDocument": {
                "uri": format!("file://{}", path.display()),
                "languageId": language,
                "version": 1,
                "text": text,
            }
        });
        self.notify("textDocument/didOpen", params)
    }

    /// Find references to the symbol at `path:line:character`
    /// (1-based lines, 0-based characters per LSP spec; we accept
    /// 1-based from callers and convert). Returns Vec of
    /// (uri, line, character) tuples; caller maps URIs back to
    /// scry file_ids.
    pub fn references(
        &mut self,
        path: &Path,
        line_1based: u32,
        character_0based: u32,
        include_declaration: bool,
    ) -> Result<Vec<LspLocation>> {
        let params = serde_json::json!({
            "textDocument": { "uri": format!("file://{}", path.display()) },
            "position": {
                "line": line_1based.saturating_sub(1),
                "character": character_0based,
            },
            "context": { "includeDeclaration": include_declaration },
        });
        let resp = self.request("textDocument/references", params)?;
        let arr = match resp.get("result") {
            Some(serde_json::Value::Array(a)) => a.clone(),
            Some(serde_json::Value::Null) | None => return Ok(Vec::new()),
            other => return Err(anyhow!("unexpected references reply: {other:?}")),
        };
        let mut out = Vec::with_capacity(arr.len());
        for loc in arr {
            let uri = loc.get("uri").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let line = loc.pointer("/range/start/line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let character = loc.pointer("/range/start/character").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            out.push(LspLocation {
                uri,
                line: line + 1,            // convert back to 1-based
                character,
            });
        }
        Ok(out)
    }

    /// Send a JSON-RPC request and wait for the matching response.
    /// Notifications and unrelated responses on the wire are skipped.
    fn request(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let env = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        });
        self.write_message(&env)?;
        loop {
            let msg = self.read_message()?;
            // Skip server-originated requests/notifications we don't
            // handle (e.g. window/logMessage, $/progress).
            if msg.get("method").is_some() && msg.get("id").is_some() {
                // Server request → respond with method-not-found.
                let reply = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": msg.get("id"),
                    "error": { "code": -32601, "message": "not implemented" },
                });
                self.write_message(&reply)?;
                continue;
            }
            if msg.get("method").is_some() && msg.get("id").is_none() {
                continue;  // notification — ignore
            }
            // Response — match by id.
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(anyhow!("lsp error on {method}: {err}"));
                }
                return Ok(msg);
            }
            // Unrelated response (id mismatch) — keep reading.
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
        let env = serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params,
        });
        self.write_message(&env)
    }

    fn write_message(&mut self, msg: &serde_json::Value) -> Result<()> {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Read one LSP-framed message. Returns the parsed JSON object.
    fn read_message(&mut self) -> Result<serde_json::Value> {
        // Read headers until empty line.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 { return Err(anyhow!("clangd closed stdout")); }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() { break; }
            if let Some(rest) = line.strip_prefix("Content-Length:") {
                content_length = Some(rest.trim().parse()?);
            }
        }
        let len = content_length.ok_or_else(|| anyhow!("missing Content-Length"))?;
        let mut body = vec![0u8; len];
        self.stdout.read_exact(&mut body)?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| anyhow!("lsp body decode: {e}"))?;
        Ok(v)
    }

    /// Send the LSP `shutdown` + `exit` sequence. Best-effort; drop
    /// implementation calls this if not already shut down.
    pub fn shutdown(&mut self) -> Result<()> {
        if !self.initialized { return Ok(()); }
        let _ = self.request("shutdown", serde_json::Value::Null);
        let _ = self.notify("exit", serde_json::Value::Null);
        Ok(())
    }
}

impl Drop for ClangdSession {
    fn drop(&mut self) {
        let _ = self.shutdown();
        // Reap the child after closing stdin.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A single LSP `Location` mapped to scry-shaped fields.
#[derive(Debug, Clone)]
pub struct LspLocation {
    /// `file://...` URI from the LSP response.
    pub uri: String,
    /// 1-based line, converted from LSP's 0-based.
    pub line: u32,
    /// 0-based character (LSP uses UTF-16 code units; we pass through).
    pub character: u32,
}

impl LspLocation {
    /// Convert the URI back to a filesystem path. Returns None for
    /// non-file URIs (clangd never produces these in practice, but
    /// the spec allows them).
    pub fn fs_path(&self) -> Option<PathBuf> {
        let rest = self.uri.strip_prefix("file://")?;
        Some(PathBuf::from(rest))
    }
}

/// Check whether `clangd` is available on PATH. Cheap — runs
/// `clangd --version` with stdio piped and inspects the exit code.
/// Used by the `--precise` query path to fail fast with an
/// actionable message instead of partway through a session start.
pub fn clangd_available() -> bool {
    Command::new("clangd")
        .arg("--version")
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Walk up from `start` looking for `compile_commands.json`. Returns
/// the dir containing it, or None if none found before the filesystem
/// root. clangd will do the same walk on its own, but knowing the
/// answer up front lets the CLI surface a clear "configure your
/// build" error before spawning the subprocess.
pub fn find_compile_commands(start: &Path) -> Option<PathBuf> {
    let mut cur = if start.is_file() { start.parent()?.to_path_buf() } else { start.to_path_buf() };
    loop {
        if cur.join("compile_commands.json").is_file() {
            return Some(cur);
        }
        cur = cur.parent()?.to_path_buf();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LspLocation::fs_path strips the file:// scheme and yields the
    /// path. Non-file schemes return None.
    #[test]
    fn fs_path_strips_file_scheme() {
        let loc = LspLocation {
            uri: "file:///home/zim/dev/aosp/foo.cpp".into(),
            line: 10, character: 5,
        };
        assert_eq!(loc.fs_path(),
            Some(PathBuf::from("/home/zim/dev/aosp/foo.cpp")));
        let bad = LspLocation { uri: "http://example/foo".into(), line: 1, character: 0 };
        assert!(bad.fs_path().is_none());
    }

    /// find_compile_commands walks up from a file location.
    #[test]
    fn find_compile_commands_walks_up() {
        let tmp = std::env::temp_dir().join(
            format!("scry-cc-{}", std::process::id())
        );
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        // Place compile_commands at tmp/a level.
        std::fs::write(tmp.join("a/compile_commands.json"), "[]").unwrap();
        // Walk up from c — should find a/.
        let found = find_compile_commands(&nested);
        assert_eq!(found, Some(tmp.join("a")));
        // Walk from somewhere with no compile_commands above — None.
        let other = std::env::temp_dir().join(format!("scry-no-cc-{}", std::process::id()));
        std::fs::create_dir_all(&other).unwrap();
        assert!(find_compile_commands(&other).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&other);
    }
}
