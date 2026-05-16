scry editor-plugin protocol
===========================

Editor plugins talk to a long-lived `scry serve` subprocess over
line-delimited JSON-RPC. One socket per editor session; the mmap'd
index is shared across all requests, so per-query latency is the
in-process cost without per-request reconnect overhead.

This document is the wire-shape contract plugin authors target.
It does NOT change without bumping the scry minor version.

Spawning the daemon
-------------------

```
scry serve --listen unix:/tmp/scry-$EUID.sock --index $INDEX --max-conns 8
```

Pick the socket path so it is unique to the editor session. The
`--max-conns` cap is optional but recommended — a runaway editor
won't accidentally DoS the daemon if you set it to a modest
ceiling (4-16 is plenty for any one editor).

`scry serve` writes its first line to stdout once it is listening:

```
[scry serve] listening on unix:/tmp/scry-1000.sock
```

Wait for that line, or for the socket file to appear, before
sending requests.

TCP variant: `--listen tcp:127.0.0.1:9999`. Same protocol on the
wire; just connect with TCP instead of an AF_UNIX socket.

The request shape
-----------------

One JSON object per line, `\n`-terminated. Every request carries:

| key  | type     | meaning                                       |
|------|----------|-----------------------------------------------|
| id   | integer  | matches the response; plugin-assigned         |
| cmd  | string   | one of `prefix` / `def` / `callers` / `ref` / `outline` / `fuzzy` / `tldr` / `stats` / `coverage` / `grep` / `ask` |
| args | object   | command-specific (see below); may be omitted for arg-less cmds |

The response shape
------------------

```
{"id":N,"result":<value>}        // success
{"id":N,"error":"<message>"}     // failure (string)
```

The `result` shape depends on the command. The contract: every
field present in a response IS the same field across releases —
new fields may be appended, existing ones do not move or change
type. Plugins should ignore unknown fields.

Commands plugins actually use
-----------------------------

### `prefix` — autocomplete

Returns ranked symbols whose name starts with `prefix`. The
primitive editor autocomplete is built on.

```
→ {"id":1,"cmd":"prefix","args":{"prefix":"actv","limit":20}}
← {"id":1,"result":[
    {"id":N,"name":"...","fqn":null,"kind":"class","lang":"Java",
     "path":"...","line":NN,"col":NN,"scope":[...]},
    ...
  ]}
```

Stable fields on each row: `name`, `kind`, `lang`, `path`,
`line`, `col`, `scope`. Other fields (`id`, `fqn`) may be present
and may be useful but are not required.

### `def` — go to definition

Returns the definition(s) of `name`. Same row shape as `prefix`.
Filter by `lang` and `kind` if the call site has context:

```
→ {"id":2,"cmd":"def","args":{"name":"ZygoteInit","lang":"Java","kind":"class","limit":3}}
```

### `callers` — find references (call sites only)

Returns RefRecords (callers of the named symbol). Filter by
`lang` and `in` (path-prefix substring) if useful:

```
→ {"id":3,"cmd":"callers","args":{"name":"transact","limit":50}}
← {"id":3,"result":[
    {"name":"transact","ref_kind":"call","lang":"Cpp",
     "path":"...","line":NN,"col":NN,"scope":[...],
     "resolved_to":N}
  ]}
```

`resolved_to` is the def-id the resolver chose, or `null` if no
unambiguous resolution was found. Plugins can pass it back to
`def` (with `id` instead of `name`) if they want the definition
in the same round-trip pattern.

### `ref` — every reference (calls, type uses, field accesses)

Same shape as `callers`; broader than calls only.

### `outline` — file-level symbol list (LSP documentSymbol equivalent)

```
→ {"id":4,"cmd":"outline","args":{"path":"src/main.rs","limit":200}}
← {"id":4,"result":{"path":"...","lang":"Rust","symbols":[...],
                     "symbols_total":N,"symbols_shown":N}}
```

### `fuzzy` — substring + Levenshtein search

For "I half-remember the symbol name":

```
→ {"id":5,"cmd":"fuzzy","args":{"substr":"transct","limit":10}}
← {"id":5,"result":[ {row..., "distance":N}, ...]}
```

### `tldr` — one-call file summary

```
→ {"id":6,"cmd":"tldr","args":{"path":"src/main.rs"}}
← {"id":6,"result":{"path":"...","lang":"Rust","symbols_total":N,
                     "by_kind":{...},"top":[...],"first_line":"..."}}
```

### `stats` — daemon health / index metadata

Stable shape (pinned by an e2e assertion). Useful as a
keep-alive probe.

Errors plugins should expect
----------------------------

- **`"error": "<message>"`** at the response level — pass it on
  to the user; don't crash the plugin.
- **Connection lost mid-request** — the daemon died (cgroup OOM,
  manual kill, host shutdown). Plugins should respawn and
  re-issue the request once, then surface the error.
- **`-32004` JSON-RPC error code** — daemon hit its `--max-conns`
  cap; bump the limit or wait and retry. The error field carries
  a human-readable explanation.

Latency budget plugins should target
------------------------------------

| command      | warm  | cold (just after rebuild) |
|--------------|-------|---------------------------|
| `prefix`     | <5 ms | <20 ms                    |
| `def`        | <5 ms | <20 ms                    |
| `callers`    | <50 ms| <200 ms                   |
| `outline`    | <50 ms| <300 ms                   |
| `fuzzy`      | <50 ms| <300 ms                   |
| `grep`       | <600 ms (depends on candidate count) |

`prefix` is the only command in the autocomplete loop and the
only one with a sub-frame budget. Editors should pipeline
keystrokes (cancel an in-flight request when the user types
another char) rather than waiting on each one.

A complete minimal client (pseudocode)
--------------------------------------

```
spawn `scry serve --listen unix:$SOCK --index $INDEX`
wait for socket to exist
connect to $SOCK
nextId = 1; pending = {}; buf = ""
loop:
  on socket data:
    buf += data
    while buf has "\n":
      line, buf = split-first-newline(buf)
      msg = json.parse(line)
      pending[msg.id].resolve(msg)
  on plugin call(cmd, args):
    id = nextId++
    line = json.stringify({id, cmd, args}) + "\n"
    promise = new Promise()
    pending[id] = promise
    socket.write(line)
    return promise
```

This pattern is what scry-ui's `server/scry.ts` implements; the
three plugin reference implementations in this tree
(`emacs/scry.el`, `vim/autoload/scry.vim`, `vscode/src/extension.ts`)
are the same shape in their respective languages.
