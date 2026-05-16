# MCP — Model Context Protocol integration

scry ships a drop-in MCP server so any MCP-aware agent runtime
(Claude Desktop, Cursor, Continue, Cline, Windsurf, custom
LangGraph/LangChain agents, anything that speaks the spec) can use
scry without writing a custom shell-out wrapper.

This doc covers the wire shape, the tool surface, error behavior,
and concrete client-configuration recipes.

---

## TL;DR

```sh
scry mcp --index /mnt/agent/scry-index
```

Reads newline-delimited JSON-RPC 2.0 on stdin, writes responses on
stdout. One tool per scry command. Drop into your client's MCP
configuration; nothing else to install.

The MCP wrapper reuses the same `serve_one_request` code path as
`scry serve`, so anything that works through serve works through MCP
without extra implementation effort.

---

## Protocol details

scry supports every MCP protocol version from `2024-11-05` (the
original) through the current `2025-11-25` revision. We negotiate
per spec: if the client requests a version we support, we echo it;
otherwise we reply with our latest. The client then decides whether
to continue or disconnect.

Only the `tools` capability is advertised — we don't implement
prompts, resources, sampling, logging, or tasks. The wire format is
JSON-RPC 2.0 over stdio, one JSON message per line.

### Supported methods

| method                       | what it does                                 |
|------------------------------|----------------------------------------------|
| `initialize`                 | returns `{protocolVersion, capabilities, serverInfo}` |
| `tools/list`                 | returns the array of available tools + their JSON schemas |
| `tools/call`                 | invokes a named tool with arguments, returns content[] |
| `ping`                       | returns `{}` — for liveness checks            |
| `notifications/*` (any)      | silently consumed (no reply per spec)        |

Any other method gets a JSON-RPC `-32601` (method not found) error.

### Available tools

| tool       | required args | optional args                          | what it returns                        |
|------------|---------------|----------------------------------------|----------------------------------------|
| `def`      | `name`        | `lang`, `kind`, `in`, `limit`          | symbol definition records              |
| `ref`      | `name`        | `lang`, `kind`, `in`, `limit`          | reference records (any kind)           |
| `callers`  | `name`        | `lang`, `in`, `limit`                  | references with `kind=call`            |
| `prefix`   | `prefix`      | `in`, `limit`                          | symbols whose name starts with PREFIX  |
| `fuzzy`    | `substr`      | `in`, `distance`, `limit`              | edit-distance-ranked symbol matches    |
| `grep`     | `pattern`     | `regex`, `case_insensitive`, `lang`, `in`, `limit`, `format` | content matches (literal substring; `regex: true` for regex; `case_insensitive: true` for case-folded match) |
| `outline`  | `path`        | `limit`                                | every symbol in the file, by line      |
| `coverage` | `path`        | `by_kind`                              | per-language file/byte/symbol counts   |
| `stats`    | —             | —                                      | index metadata                         |
| `ask`      | `query`       | `in`, `limit`                          | semantic-retrieval chunks (cosine-ranked) |

`limit` defaults to 20. `in` is a path-substring filter, same
semantics as the CLI's `--in`. Tool `ask` requires the index to have
been processed by `scry build-embeddings`; otherwise the tool returns
a tool-level error (see "Error semantics" below).

---

## Error semantics

scry distinguishes **protocol errors** from **tool errors** the way
MCP intends — most clients render the two very differently in their UI.

### Tool-level errors (`isError: true`)

Returned as a successful `tools/call` response with `isError: true`
and a human-readable text part. The call shape was valid; the tool
just couldn't satisfy it. Cases:

- **Unknown tool name** (`tools/call name: "noSuchTool"`):
  ```json
  {"isError": true, "content": [{"type": "text",
    "text": "unknown tool: 'noSuchTool'. Call tools/list to see available tools."}]}
  ```
- **Missing required argument** (`{"name": "def", "arguments": {}}`):
  ```json
  {"isError": true, "content": [{"type": "text",
    "text": "missing or empty required argument 'name' for tool 'def'"}]}
  ```
  An empty string or `null` value counts as missing — silently
  treating `{"name": ""}` as "match anything" returned ~50 garbage
  results in a real session and is exactly the bug this validation
  closes.
- **Tool-couldn't-run** (e.g. `ask` against an index without
  embeddings):
  ```json
  {"isError": true, "content": [{"type": "text",
    "text": "no embedding sidecar — run `scry build-embeddings`"}]}
  ```
  The bare error message is in `text` — clients (and LLMs reading
  the content) don't need a second `json.parse()` to get the human
  hint.

### Protocol-level errors (`error: {code, message}`)

Returned per JSON-RPC 2.0 — the outer envelope has `error` instead
of `result`. Cases:

- **Unknown method** (not `initialize`, `tools/list`, `tools/call`,
  `ping`, or a notification): `-32601 method not found`.
- **Parse error** (request isn't valid JSON): `-32700 parse error: <detail>`.

---

## Client recipes

### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows) or
`~/.config/Claude/claude_desktop_config.json` (Linux):

```json
{
  "mcpServers": {
    "scry": {
      "command": "/mnt/agent/scry/target/release/scry",
      "args": ["mcp", "--index", "/mnt/agent/scry-index"]
    }
  }
}
```

Restart Claude Desktop; the scry tools (def, ref, callers, prefix,
fuzzy, grep, outline, coverage, stats, ask) appear in the tool
picker. The MCP server starts on demand and stays alive for the
session.

### Cursor

In Cursor settings → MCP → "New MCP Server":

- **Name**: `scry`
- **Command**: `/mnt/agent/scry/target/release/scry`
- **Arguments**: `mcp --index /mnt/agent/scry-index`

### Continue (continue.dev)

In `~/.continue/config.json`:

```json
{
  "experimental": {
    "modelContextProtocolServers": [
      {
        "transport": {
          "type": "stdio",
          "command": "/mnt/agent/scry/target/release/scry",
          "args": ["mcp", "--index", "/mnt/agent/scry-index"]
        }
      }
    ]
  }
}
```

### Custom agents (LangGraph / LangChain / direct stdio)

Spawn `scry mcp` as a subprocess; write line-delimited JSON-RPC to
stdin; read responses from stdout. The minimum bootstrap:

```python
import json, subprocess
p = subprocess.Popen(
    ["/mnt/agent/scry/target/release/scry", "mcp", "--index", "/mnt/agent/scry-index"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)
def call(method, params=None, id=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if id is not None: msg["id"] = id
    if params: msg["params"] = params
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()
    return json.loads(p.stdout.readline()) if id is not None else None

call("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}}, id=1)
call("notifications/initialized")            # notification — no reply

# Run a tool.
r = call("tools/call",
         {"name": "def", "arguments": {"name": "ActivityManagerService", "limit": 3}},
         id=2)
hits = json.loads(r["result"]["content"][0]["text"])   # text content holds JSON
```

---

## Performance notes

scry's MCP server is intentionally simple:

- **Cold start** is `open_index(...)` time — a few hundred ms for
  the mmap calls. After that the index pages are demand-paged on
  query.
- **Per-tool latency** matches `scry serve` because they share the
  underlying request path. Typical warm numbers on the live
  AOSP+Linux index: `def` ~8 ms, `callers` ~80 ms, `grep` ~600 ms,
  `outline` ~600 ms, `ask` ~50–500 ms (depending on warm vs cold
  embeddings.bin).
- **Concurrent calls**: clients that pipeline `tools/call` requests
  see no contention — the StoreReader is mmap'd + immutable and
  `serve_one_request` is a pure function over reader + request.
- **Tokens**: every tool result is a single text content part
  holding the serialized JSON. Clients pass the text through to
  the LLM; budget-conscious agents set `limit` low.

---

## Tradeoffs vs. `scry serve`

| capability                             | `serve`            | `mcp`                       |
|----------------------------------------|--------------------|-----------------------------|
| stdio transport                        | yes                | yes                         |
| Unix-socket transport                  | `--listen unix:`   | no (stdio per MCP spec)     |
| TCP transport                          | `--listen tcp:`    | no (stdio per MCP spec)     |
| streaming responses                    | yes (`stream:true`) | no (MCP doesn't define)    |
| `budget: BYTES` field                  | yes                | no (per-tool `limit` only)  |
| `name` fallback for primary args       | yes                | no — schema-enforced names  |
| schema validation                      | no                 | yes (rejects missing/empty) |
| `isError` discipline                   | no                 | yes (MCP semantic)          |
| arg names per command (schema-strict)  | optional           | required                    |

Rule of thumb: when integrating with an MCP-aware client, use `mcp`
— the validation + envelope discipline pay off. When wiring scry
into a custom shell pipeline or a long-running daemon you control,
use `serve` — the stream + budget + unix-socket flexibility wins.

---

## Testing your MCP setup

Smoke-test from a shell:

```sh
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"def","arguments":{"name":"Binder","limit":1}}}' \
  | scry mcp --index /mnt/agent/scry-index
```

You should see three response lines (the notification produces
nothing): `initialize` reports `serverInfo.name: "scry"`,
`tools/list` returns 10 tools, `tools/call def Binder` returns a
content array with one text part holding a JSON array of symbol
records. If any of those don't match, the MCP wrapper is
mis-configured or the index is missing.

Error-path smoke:

```sh
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"def","arguments":{}}}' \
  | scry mcp --index /mnt/agent/scry-index
```

The third response must have `result.isError: true` and the text
must mention the missing `name` argument. If it returns hits, the
arg validation has regressed.
