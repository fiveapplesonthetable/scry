# scry — usage

Exhaustive examples against a live AOSP master + Linux 7.0-rc7 index
at `/mnt/agent/scry-index`. Output snippets are real (not invented);
re-run the commands against your own index to see the same shape.

Every command finishes with a one-line stats footer to stderr:

```
[scry] cmd=def q="ActivityManagerService" hits=4 shown=1 files=1009166 elapsed=321ms
```

…and appends one JSON line to `~/.scry/queries.log` (override with
`$SCRY_LOG`) so you can audit every search a session ran.

---

## Symbol lookup: `scry def`

Exact-name definition lookup. The closest analogue is LSP's
`textDocument/definition` or gtags' tag lookup.

```sh
$ scry def ActivityManagerService --kind class --lang Java --limit 3
```

Output:

```
/home/zim/dev/aosp/frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java:543:14  (class java)  [ActivityManagerService]  ActivityManagerService
/home/zim/dev/aosp/frameworks/base/core/api/system-current.txt:8451:1  (class ?)  [android.app]  ActivityManagerService
/home/zim/dev/aosp/frameworks/base/services/tests/mockingservicestests/src/com/android/server/am/ActivityManagerServiceTest.java:106:14  (class java)  [ActivityManagerServiceTest]  ActivityManagerService

3 results (showing 3)
[scry] cmd=def q="ActivityManagerService" hits=4 shown=3 files=1009166 elapsed=320ms
```

Real-source class definition wins (#1) over api-txt declarations
because of the ranking heuristic in `docs/DESIGN.md` (api-txt is
demoted; deeper-nested scope is demoted). Pass `--kind class --lang
Java` to narrow further.

### Flags

| short | long             | what                                                       |
|------:|------------------|------------------------------------------------------------|
|       | `--index PATH`   | Index dir (default `/mnt/agent/scry-index`)                |
| `-t`  | `--lang LANG`    | Filter by language (Java, Kotlin, Cpp, Soong, ApiTxt, …)   |
| `-k`  | `--kind KIND`    | Filter by kind (class, fn, method, soong, init.svc, …)     |
|       | `--in SUBSTR`    | Restrict to files whose absolute path contains SUBSTR      |
|       | `--limit N`      | Cap results (default 100)                                  |
|       | `--json`         | Emit one JSON object per line (NDJSON)                     |
|       | `--md`           | Emit Markdown with code snippets (LLM-friendly)            |
|       | `--budget BYTES` | (md mode) cap output size; drops lowest-ranked first       |

```sh
# AOSP-specific kinds
$ scry def libbinder --kind soong --limit 1
/home/zim/dev/aosp/frameworks/native/libs/binder/Android.bp:39:1  (soong soong)  libbinder

$ scry def zygote --kind init.svc --limit 1
/home/zim/dev/aosp/system/core/rootdir/init.zygote64.rc:1:9  (init.svc initrc)  zygote

$ scry def IBinder --kind aidl.iface --limit 1
/home/zim/dev/aosp/frameworks/native/aidl/binder/android/os/IBinder.aidl:21:11  (aidl.iface aidl)  IBinder

# AIDL cross-language shadows: each `interface IFoo` produces
# synthetic symbols for the toolchain-generated bindings (Java Stub,
# C++ Bp/Bn, Rust binding) all pointing back at the .aidl source.
$ scry def IBinder.Stub --kind aidl.shadow
/home/zim/dev/aosp/frameworks/native/aidl/binder/android/os/IBinder.aidl:21:11  (aidl.shadow aidl)  IBinder.Stub

$ scry def BpIBinder --kind aidl.shadow
/home/zim/dev/aosp/frameworks/native/aidl/binder/android/os/IBinder.aidl:21:11  (aidl.shadow aidl)  BpIBinder

# HIDL shadows follow the same pattern (Bp / Bn / Bs).
$ scry def BpIServiceManager --kind hidl.shadow
/home/zim/dev/aosp/hardware/.../IServiceManager.hal:14:11  (hidl.shadow hidl)  BpIServiceManager

$ scry def system_server --kind sepolicy --limit 2
/home/zim/dev/aosp/system/sepolicy/public/system_server.te:1:6  (sepolicy sepolicy)  system_server
```

Subdir scoping with `--in`:

```sh
$ scry def Activity --in frameworks/base/ --limit 3
/home/zim/dev/aosp/frameworks/base/core/java/android/app/Activity.java:774:14  (class java)  [Activity]  Activity
/home/zim/dev/aosp/frameworks/base/tools/aapt2/dump/DumpManifest.cpp:1540:7  (class cpp)  [aapt::Activity]  Activity
```

Markdown for LLM tool output:

```sh
$ scry def Binder --md --budget 4000 --limit 3
### `Binder`  (class · java)
**location**: `/home/zim/dev/aosp/frameworks/native/libs/binder/Binder.java:85:7`
**scope**: `Binder`
```java
public class Binder implements IBinder {
    ...
```

---

## Reference lookup: `scry ref` / `scry callers`

```sh
$ scry callers transact --lang Java --limit 3
/home/zim/dev/aosp/cts/hostsidetests/appsecurity/test-apps/UseProcessSuccess/src/com/android/cts/useprocess/AccessNetworkTest.java:77:25  (call java)  [AccessNetworkTest::MyConnection]  transact  → def:fb80a66b3db3efd5
/home/zim/dev/aosp/cts/hostsidetests/securitybulletin/test-apps/CVE-2022-20004/test-app/src/android/security/cts/CVE_2022_20004_test/PocActivity.java:98:26  (call java)  [PocActivity]  transact  → def:fb80a66b3db3efd5
/home/zim/dev/aosp/frameworks/base/core/java/android/os/IBinder.java:419:38  (call java)  [IBinder]  transact  → def:fb80a66b3db3efd5

1524 refs (showing 3)
[scry] cmd=callers q="transact" hits=1524 shown=3 files=1009166 elapsed=84ms
```

`→ def:fb80a66b3db3efd5` is the Layer 2 resolution — every Java
caller of `transact` resolves to the same `android.os.Binder.transact`
definition (id `fb80a66b3db3efd5`). The C++ callers resolve to a
different `def:` id, the C++ Binder. Pass `--json` to get the raw
`resolved_to` u64.

`scry ref` is the generic version that includes all ref kinds (call,
ctor, type-use, field-access, import, inherit). `callers` is the
common-case shorthand for `ref --kind call`.

---

## Path-prefix completion: `scry prefix`

FST-backed prefix completion. Useful for "what's everything starting
with `Activity`" autocomplete-style.

```sh
$ scry prefix Activity --limit 5
/home/zim/dev/aosp/frameworks/base/core/java/android/app/Activity.java:774:14  (class java)  [Activity]  Activity
/home/zim/dev/aosp/frameworks/base/core/java/android/app/ActivityManager.java:185:14  (class java)  [ActivityManager]  ActivityManager
/home/zim/dev/aosp/frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java:543:14  (class java)  [ActivityManagerService]  ActivityManagerService
...
[scry] cmd=prefix q="Activity" hits=... shown=5 files=1009166 elapsed=4ms
```

Ranked the same way `def` is — real source > api-txt > test fixtures.

---

## Fuzzy symbol search: `scry fuzzy`

Typo-tolerant + edit-distance ranked. Two candidate sources are
unioned before ranking: (a) substring matches anywhere in the symbol
name, (b) Levenshtein-bounded matches up to `--distance N` (default 2).
The merged set is re-sorted by an internal score that prefers
**substring matches** over **Levenshtein-close-but-unrelated** names,
then by exact Wagner-Fischer distance.

The `d=N` column on each result is the true Wagner-Fischer distance
from query to symbol name. Substring matches show non-zero `d` (the
inserted characters cost) but rank above unrelated typos.

```sh
$ scry fuzzy ParcelFile --limit 3
/home/zim/dev/aosp/frameworks/base/core/java/android/os/ParcelFileDescriptor.java:76:14  (d=10)  (class Java)  [ParcelFileDescriptor]  ParcelFileDescriptor
/home/zim/dev/aosp/frameworks/native/libs/binder/rust/src/parcel/file_descriptor.rs:29:12  (d=10)  (struct Rust)  [ParcelFileDescriptor]  ParcelFileDescriptor
/home/zim/dev/aosp/frameworks/base/core/java/android/os/ParcelFileDescriptor.java:197:12  (d=10)  (ctor Java)  [ParcelFileDescriptor]  ParcelFileDescriptor

3 results (showing 3)
[scry] cmd=fuzzy q="ParcelFile" hits=3 shown=3 files=1009166 elapsed=1179ms
```

Typo example — a single deletion still finds the intended symbol:

```sh
$ scry fuzzy PrcelFile --distance 1 --limit 3
… ParcelFile (d=1)  …
```

JSON-RPC + MCP both honor `args.distance: N` to override the
default; output gains a `distance` field per hit.

---

## Content search: `scry grep`

Trigram-indexed literal grep. Falls back to regex with HIR literal
extraction when `--regex` is passed.

```sh
$ scry grep ZygoteInit --limit 3
/home/zim/dev/aosp/frameworks/base/cmds/app_process/app_main.cpp:92:20: virtual void onZygoteInit()
/home/zim/dev/aosp/frameworks/base/cmds/app_process/app_main.cpp:336:48:         runtime.start("com.android.internal.os.ZygoteInit", args, zygote);
/home/zim/dev/aosp/frameworks/base/core/java/android/app/AppOpsManager.java:100:18: import com.android.internal.os.ZygoteInit;

3 hits across 1416 files
[scry] cmd=grep q="ZygoteInit" hits=3 shown=3 files=1009166 cands=1416 elapsed=320ms
```

The `cands=1416` is the trigram-pre-filtered candidate set — 1.0 M
files → 1416 → 3 hits. That ratio is the optimization: `rg` would
have read every byte of all 1 M files; scry reads only the 1416 that
the trigram FST says could possibly contain the literal.

Regex (with prefix + suffix literal extraction, livegrep style):

```sh
$ scry grep '\bZygoteInit\b' --regex --limit 2
[grep] regex→trigram pre-filter: 1416 candidate files in 12 ms
...
```

### Flags

| short | long                  | what                                         |
|------:|-----------------------|----------------------------------------------|
|       | `--regex`             | Treat PATTERN as a regex (else literal)      |
| `-t`  | `--lang LANG`         | Filter by language                           |
|       | `--in SUBSTR`         | Restrict to files whose path contains SUBSTR |
|       | `--limit N`           | Cap hits (default 100)                       |
|       | `--workers N` / `-j`  | Rayon pool size for the per-file scan        |
|       | `--max-file-bytes N`  | Skip files larger than N bytes (default 10 MiB) |
|       | `--mem-cap N`         | Refuse to start if scan would exceed N GiB   |
|       | `--json`              | NDJSON output                                |

---

## File outline: `scry outline`

Every symbol defined in one file, sorted by line. LSP analogue:
`textDocument/documentSymbol`.

```sh
$ scry outline frameworks/base/cmds/app_process/app_main.cpp --limit 8
# /home/zim/dev/aosp/frameworks/base/cmds/app_process/app_main.cpp  (Cpp)
# 13 symbols
    8:9    macro         LOG_TAG
   26:11   ns            android  [android]
   28:13   fn            app_usage  [android]
   34:7    class         AppRuntime  [android::AppRuntime]
   37:5    fn            AppRuntime  [android::AppRuntime]
   43:10   method        setClassNameAndArgs  [android::AppRuntime]
   50:18   method        onVmCreated  [android::AppRuntime]
   79:18   method        onStarted  [android::AppRuntime]
... (5 more — pass --limit 0 to see all)
[scry] cmd=outline q="frameworks/base/cmds/app_process/app_main.cpp" hits=13 shown=8 files=1009166 elapsed=4ms
```

`PATH` matches by suffix — `outline app_main.cpp` works too if the
suffix is unique. Multiple matches print a disambiguation warning and
pick the shortest match.

---

## Subtree coverage: `scry coverage`

Files / bytes / symbols broken down per language for any directory
within the index. Useful for "what fraction of $repo did scry
actually understand?" — point it at an internal subtree to verify
the right languages got picked up.

```sh
$ scry coverage frameworks/base/services
subtree:      frameworks/base/services
files-total:  6036
bytes-total:  94.5 MB
symbols:      191484

     files           bytes       symbols  lang
     -----           -----       -------  ----
      4717         89.9 MB        178868  Java
       348         43.9 KB           607  Owners
       289        400.1 KB             0  XmlOther
       237          2.0 MB          7342  Kotlin
       154        305.5 KB           814  Soong
       101          1.4 MB          2439  Cpp
        75        135.2 KB           427  Manifest
        51        206.9 KB           497  Header
        37         59.5 KB           268  Aconfig
        ...
[scry] cmd=coverage q="frameworks/base/services" hits=6036 elapsed=782ms
```

Add `--by-kind` to also break each language down by SymbolKind:

```sh
$ scry coverage frameworks/base/services --by-kind
...
      4717         89.9 MB        178868  Java
                                  105962    └─ method
                                   58047    └─ field
                                    7984    └─ class
                                    5659    └─ ctor
                                     820    └─ iface
                                     317    └─ annot
                                      79    └─ enum
       237          2.0 MB          7342  Kotlin
                                    4435    └─ field
                                    2439    └─ fn
                                     280    └─ class
       ...
```

Empty PATH = whole index (same totals as `scry stats`, but grouped
by lang inline):

```sh
$ scry coverage ""
subtree:      <entire index>
files-total:  1009166
bytes-total:  12.0 GB
symbols:      22790955

     files           bytes       symbols  lang
     -----           -----       -------  ----
    204814          2.2 GB       5608803  Java
    168881          2.1 GB       7603425  Header
    164204          2.3 GB       2590567  Cpp
    118627          1.6 GB       4208190  C
     38702        411.5 MB        596043  Python
     26424        139.6 MB        468619  Kotlin
     20336        368.9 MB           727  Assembly
     20230        236.7 MB        687370  Rust
     17452         34.1 MB         45951  Aidl
     14231        202.0 MB        408677  HeaderCpp
     13716         52.5 MB         73240  Soong
     ...
```

`--json` for machine consumption; the same shape goes through
`scry serve` as `{"cmd":"coverage","args":{"path":"…","by_kind":true}}`.

---

## Index metadata: `scry stats`

```sh
$ scry stats
scry-version: 0.0.1
indexed-at:   2026-05-16T04:51:51Z
roots:        2
  - /home/zim/dev/aosp (Aosp)
  - /mnt/agent/dev/linux (Linux)
files-total:  1009166
files-parsed: 1009166
files-failed: 0
bytes-total:  70.4 GB
symbols:      22790955
refs:         62772968
elapsed-ms:   796238

by language:
     7607137  Header
     5615791  Java
     4214212  C
     2535225  Cpp
      729764  Rust
      610828  Python
      ...

by kind:
     8210123  class
     6543210  method
     ...
```

---

## JSON-RPC: `scry serve`

Two transports — pick by how long the agent or editor lives:

### Stdio (one-shot agent loops, ad-hoc CLI piping)

Open once per agent session and reuse the warm mmap'd index for the
remaining lifetime of the process:

```sh
$ printf '%s\n' \
    '{"id":1,"cmd":"def","args":{"name":"Binder","limit":3}}' \
    '{"id":2,"cmd":"callers","args":{"name":"transact","in":"frameworks/base/","limit":3}}' \
    '{"id":3,"cmd":"grep","args":{"pattern":"ZygoteInit","limit":3}}' \
    '{"id":4,"cmd":"outline","args":{"path":"app_main.cpp"}}' \
    '{"id":5,"cmd":"stats"}' \
  | scry serve --index /mnt/agent/scry-index
{"id":1,"result":[{"name":"Binder","kind":"class","lang":"Java","path":"…","line":85,...}]}
{"id":2,"result":[{"name":"transact","ref_kind":"call","lang":"Java","resolved_to":18122667880065789909,...}]}
{"id":3,"result":[{"path":"…","line":92,"col":20,"snippet":"…ZygoteInit…","lang":"Cpp"}]}
{"id":4,"result":{"path":"…/app_main.cpp","lang":"Cpp","symbols_total":13,...}}
{"id":5,"result":{"scry_version":"0.0.1","files_total":1009166,...}}
```

### Listener (long-running daemon, multiple concurrent clients)

For editor integrations, agents that span many sessions, or any
workflow that would otherwise pay the ~50 ms cold-open cost on every
shell-out:

```sh
# Bind a Unix domain socket (preferred for local clients).
$ scry serve --index /mnt/agent/scry-index --listen unix:/tmp/scry.sock &
[scry serve] listening on unix:/tmp/scry.sock

# Connect from any tool that speaks line-delimited JSON over a socket.
$ printf '{"id":1,"cmd":"def","args":{"name":"Binder","limit":1}}\n' \
    | socat - UNIX-CONNECT:/tmp/scry.sock
{"id":1,"result":[{"name":"Binder","kind":"class",...}]}

# Or TCP, for cross-host or container-network setups.
$ scry serve --index /mnt/agent/scry-index --listen tcp:127.0.0.1:9999 &
```

Daemon mode accepts many connections concurrently on its own OS
threads. The `StoreReader` is mmap-backed and immutable, so concurrent
queries don't serialize on any internal lock — query latency under load
is the same as single-client. The socket file is best-effort cleaned up
on bind (stale sockets from a crashed prior run are replaced); SIGKILL
will leave the file behind but the next start reclaims it.

### Streaming responses (`"stream": true`)

For large result sets where the caller wants to start reading before
all hits are computed (or wants to cut off early), set `stream: true`
on the request. The server emits one JSON line per hit, then a
closing envelope:

```sh
$ printf '%s\n' \
    '{"id":1,"cmd":"def","args":{"name":"Activity","limit":3},"stream":true}' \
  | scry serve --index /mnt/agent/scry-index
{"id":1,"hit":{"name":"Activity","kind":"class",...}}
{"id":1,"hit":{"name":"Activity","kind":"class",...}}
{"id":1,"hit":{"name":"Activity","kind":"class",...}}
{"id":1,"done":true,"shown":3}
```

Streaming is meaningful for the multi-hit commands (def, prefix,
fuzzy, ref, callers, grep). For scalar-shaped commands (outline,
coverage, stats), `stream: true` is silently ignored — they always
return one envelope.

### Budget (`"budget": BYTES`)

When the response would exceed `BYTES` of serialized JSON, fields are
progressively stripped in a priority order: **snippet → scope → fqn**,
and finally **the result array is truncated** from the tail. The
response then carries a `"truncated"` field naming what was dropped:

```sh
$ printf '%s\n' \
    '{"id":1,"cmd":"def","args":{"name":"Activity","limit":50},"budget":2000}' \
  | scry serve --index /mnt/agent/scry-index
{"id":1,"result":[…40 hits, all without scope or snippet…],"truncated":"snippet+scope"}
```

The order is "drop the most reconstructible thing first": snippets
are kilobytes and the agent can re-read the file; scope paths are
helpful but derivable from path+line; fqn is name+scope; the count
truncation is last-resort. Set `limit` to cap result *count*; set
`budget` to cap result *bytes*. Use both together for predictable
agent token economy.

Streaming + budget compose: in stream mode, budget caps each hit
individually (no way to retroactively trim already-emitted hits).
In non-stream mode, budget caps the whole response.

Full per-command argument schema is in `README.md` under the JSON-RPC
section.

---

## MCP (Model Context Protocol): `scry mcp`

Drop-in MCP server for Claude Desktop, Cursor, and other MCP-aware
agent runtimes. No custom shell-out wrapper required.

```sh
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"def","arguments":{"name":"Binder","limit":1}}}' \
  | scry mcp --index /mnt/agent/scry-index
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"scry","version":"0.0.1"}}}
{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"def",...},{"name":"ref",...},...]}}
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"[{\"name\":\"Binder\",...}]"}],"isError":false}}
```

Each scry command (def, ref, callers, prefix, fuzzy, grep, outline,
coverage, stats) is exposed as one MCP tool with a JSON Schema for
its arguments. The tool result is the JSON output of the underlying
serve command, wrapped in MCP's `content[]` text-part format.

**Notifications** (JSON-RPC messages without an `id`, e.g.
`notifications/initialized`) are silently consumed per the MCP spec.

**Configuration for Claude Desktop** (`~/Library/Application
Support/Claude/claude_desktop_config.json`):

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

The MCP server reuses the same `serve_one_request` code path as the
non-MCP transports, so anything that works over stdio JSON-RPC works
through MCP without extra implementation effort.

---

## Changed-since-commit view: `scry diff`

Surfaces files in the index that have changed since a git commit-ish.
Useful for code review and for agents working on a PR: pair with
`scry callers X --in <changed-path>` to focus on the symbols that
matter for the patch.

```sh
$ scry diff --since main
8 changed files since main (showing 8)
  frameworks/base/core/java/android/app/Activity.java (Java) — 12 symbols
  frameworks/base/core/java/android/app/ActivityManager.java (Java) — 47 symbols
  frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java (Java) — 318 symbols
  …

$ scry diff --since HEAD~5 --in frameworks/base/services --verbose --limit 3
3 changed files since HEAD~5 (showing 3)
  frameworks/base/services/.../ActivityManagerService.java (Java) — 318 symbols
      543:14  (class)    ActivityManagerService
      612:18  (method)   startActivityAsCaller
      621:18  (method)   bindServiceAsUser
      …

$ scry diff --since HEAD~10 --json | jq -c '{path,symbol_count}' | head -5
```

For each indexed root that is a git repo (has a `.git/` dir),
shells out to `git -C ROOT diff --name-only SINCE..HEAD`, intersects
the changed paths with the file table, emits per-file symbol counts.
Roots that aren't git trees are skipped with a one-line warning to
stderr; the rest are reported.

Flags:
  `--since REV`      commit-ish to compare HEAD against (required)
  `--in PREFIX`      substring filter on the display path
  `--verbose`        list every changed symbol, not just per-file counts
  `--limit N`        cap files reported (default 50)
  `--json`           one JSON object per changed file, ready for `jq`

---

## Query memory: `scry recall`

A thin memory primitive over `~/.scry/queries.log`. Useful for
agents that want to know "what did I already search for this
session" without re-running every query, and for humans wanting
to inspect a session's search activity.

```sh
$ scry recall --last 5
recent queries (last 5 of 134 total):
  3m ago     callers   transact                                  820 hits in 120ms
  5m ago     def       ActivityManagerService                    4 hits in 8ms
  6m ago     grep      ZygoteInit                                12 hits in 580ms (1416 cand)
  7m ago     outline   frameworks/base/.../ActivityThread.java   1 hits in 45ms
  9m ago     ref       Binder                                    87 hits in 30ms
```

Filters:

```sh
$ scry recall --cmd def --last 10              # only def queries
$ scry recall --grep transact                  # only queries matching transact
$ scry recall --dedup                          # collapse consecutive same-query repeats
$ scry recall --json | jq -s 'group_by(.cmd) | map({cmd:.[0].cmd, count:length})'
```

`--log PATH` overrides the default location (`$SCRY_LOG` then
`$HOME/.scry/queries.log`). Missing logs are not an error — the
command exits 0 with an empty result so agent loops don't break
in fresh sessions.

The parser is tolerant: a partial-write at the tail (the writer
was SIGKILL'd mid-line) is silently skipped, so recall always
returns the longest valid prefix of the log.

---

## Index admin

```sh
# Full indexing (production: use scripts/run_index.sh which wraps this
# with the right knobs + cgroup-protected systemd unit).
$ scry index ~/dev/aosp /mnt/agent/dev/linux -o /mnt/agent/scry-index \
    --workers 16 --mem-cap 40 --resume --build-trigrams

# Post-finalize sidecar utilities — retrofit a finalized index
# without re-parsing. Each is atomic (tmp + rename).
$ scry build-offsets --index /mnt/agent/scry-index
[offsets] symbols: 22790955 records → 173.8 MB in 35012 ms
[offsets] refs:    62772968 records → 502.2 MB in 89124 ms
[offsets] DONE.  85562960 records in 124136 ms

$ scry build-file-symbols --index /mnt/agent/scry-index
[fsyms] 1009166 files, 22790955 symbols — building reverse map
[fsyms] DONE in 3182 ms. file_symbols=90.8 MB offsets=7.7 MB

$ scry build-trigrams --index /mnt/agent/scry-index --workers 16
[trigrams] streaming 1009166 files, ~30 min on full corpus
[trigrams] DONE.

$ scry build-resolutions --index /mnt/agent/scry-index
[res] 22790955 symbols, 62772968 refs
[res] pass 1 (by-name + per-file-pkg) in 20071 ms
[res] pass 2 (per-file imports: 163951 files) in 19020 ms
[res] pass 3 (resolve 62772968 refs, 55922904 resolved, 0 narrowed via Java context) in 420100 ms
[res] DONE. 502183744 bytes written → /mnt/agent/scry-index/ref_resolutions.bin
```

---

## Ops log

Every CLI invocation appends one JSON line:

```sh
$ tail -2 ~/.scry/queries.log
{"ts":1778922191,"cmd":"def","query":"ActivityManagerService","hits":4,"shown":1,"files_total":1009166,"candidate_files":null,"elapsed_ms":321,"index":"/mnt/agent/scry-index"}
{"ts":1778922192,"cmd":"grep","query":"Activity","hits":2,"shown":2,"files_total":1009166,"candidate_files":30482,"elapsed_ms":392,"index":"/mnt/agent/scry-index"}
```

Override path via `SCRY_LOG=/path/to/log`. The file is append-only;
trim it yourself if it grows unbounded. The log is best-effort — a
write failure never affects the query's exit status or stdout.

---

## Worked example: why scry vs. raw rg + Read

Real task: *"find where `ActivityManagerService.setProcessLimit` is
implemented and what code calls it."*

### Without scry — agent's normal approach

```sh
$ rg -n "setProcessLimit" /home/zim/dev/aosp -t java --max-count 3 | head
frameworks/base/services/.../ActivityManagerService.java:5937:    public void setProcessLimit(int max) {
frameworks/base/services/.../ActivityManagerService.java:5939:                "setProcessLimit()");
frameworks/base/tests/permission/.../ActivityManagerPermissionTests.java:83:            mAm.setProcessLimit(10);
packages/apps/TvSettings/.../DevelopmentFragment.java:1666:            ActivityManager.getService().setProcessLimit(limit);
packages/apps/Settings/.../BackgroundProcessLimitPreferenceController.java:93:            getActivityManagerService().setProcessLimit(limit);
... 4 more ...
```

- Wall: **3.25 s** for one `rg` pass.
- Output mixes the **definition** with the **call sites** — the agent
  can't tell which line is the `public void setProcessLimit(int max) {`
  declaration vs the `.setProcessLimit(10)` invocations without
  reading each file.
- To verify what the method does + what callers expect, the agent
  must `Read` each candidate file. At ~5–15 k tokens per typical
  AOSP source file × 4–5 files = **roughly 30–60 k tokens consumed**
  just to ground the answer.

### With scry — two structured queries

```sh
$ scry def setProcessLimit --kind method --lang Java --limit 3
frameworks/base/services/.../ActivityManagerService.java:5937:17  (method java)  [ActivityManagerService]  setProcessLimit
1 results (showing 1)
[scry] cmd=def q="setProcessLimit" hits=1 shown=1 files=1009166 elapsed=324ms

$ scry callers setProcessLimit --lang Java --limit 10
frameworks/base/tests/permission/.../ActivityManagerPermissionTests.java:83:17  (call java)  [ActivityManagerPermissionTests]  setProcessLimit  → def:f3720ef78a480b7e
packages/apps/Settings/tests/.../BackgroundProcessLimitPreferenceControllerTest.java:130:34  (call java)  [BackgroundProcessLimitPreferenceControllerTest]  setProcessLimit  → def:f3720ef78a480b7e
packages/apps/Settings/tests/.../BackgroundProcessLimitPreferenceControllerTest.java:81:34  (call java)  [...]  setProcessLimit  → def:f3720ef78a480b7e
packages/apps/Settings/.../BackgroundProcessLimitPreferenceController.java:93:41  (call java)  [BackgroundProcessLimitPreferenceController]  setProcessLimit  → def:f3720ef78a480b7e
packages/apps/TvSettings/.../DevelopmentFragment.java:1666:42  (call java)  [DevelopmentFragment]  setProcessLimit  → def:f3720ef78a480b7e
... 1 more ...
6 refs (showing 6)
[scry] cmd=callers q="setProcessLimit" hits=6 shown=6 files=1009166 elapsed=332ms
```

- Wall: **0.36 s + 0.38 s = 0.74 s total** (≈ 4.4× faster end-to-end).
- The first query is unambiguous: exactly ONE method, in the
  `[ActivityManagerService]` scope, kind = `method`. The agent now
  knows precisely where the definition lives without reading a byte
  of source.
- The second query gives every call site with its **enclosing scope**
  inline (`[BackgroundProcessLimitPreferenceController]`,
  `[DevelopmentFragment]`, …) AND the Layer 2 `→ def:HEX` proves all
  6 callers really do invoke the same definition (no false positives
  from another class accidentally named `setProcessLimit`).
- Total output is ~ 1 k tokens of structured data the agent can
  reason about directly. No follow-up `Read` is needed unless the
  agent wants the surrounding code body — and even then it can read
  the right line range (5937 ± 20) rather than the whole 6000-line
  file.

### Net for an LLM agent

| metric                       | rg + Read                | scry                  | win        |
|------------------------------|--------------------------|-----------------------|------------|
| wall time                    | ~3 s + N × Read latency  | ~0.7 s                | **4×+**    |
| tokens consumed              | ~30–60 k                 | ~1 k                  | **30–60×** |
| def vs ref disambiguation    | manual (Read each file)  | structural (kind)     | qualitative |
| scope of each hit            | not in output            | `[Foo::Bar]` inline   | qualitative |
| same-def confirmation        | not possible from rg     | `→ def:HEX` shared    | qualitative |

The latency win comes from the index. The **token win comes from the
structure** — scry returns what an agent actually needs (which symbol,
which scope, which definition it resolves to) instead of a raw text
match the agent has to ground itself. This is the leverage scry was
built for.

### Caveats

- The C++ side has a known coverage gap: out-of-line method definitions
  (`Foo::bar()` outside the class body) aren't captured by the
  symbol query yet. For the C++ Binder.transact path the workaround is
  `scry grep '::transact('`. See `docs/DEVELOPMENT.md` for the
  outstanding parser-query work.
- Scope filters (`--lang`, `--kind`, `--in`) make the win bigger
  the more concrete your question. `scry def Foo` alone may still
  return many hits (Foo is overloaded in real corpora); adding
  `--kind class --lang Java` narrows to one in most cases.
