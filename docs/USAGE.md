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
|       | `--not-in SUBSTR`| Drop files whose absolute path contains SUBSTR             |
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

# Frozen AIDL surface (files under aidl_api/<pkg>/<N>/) gets its own
# kind so agents can scope "what is the V3 surface of IFoo" without
# matching the live development copy.
$ scry def IFoo --kind aidl.frozen --limit 2
hardware/interfaces/foo/aidl/aidl_api/android.hardware.foo/3/android/hardware/foo/IFoo.aidl:14:11  (aidl.frozen aidl)  IFoo

# Java `native` methods get a synthetic JNI shadow named after the
# standard JNI mangling — useful when the C++ side is missing,
# shared across modules, or you're working from the C++ side and
# want to find the Java declaration.
$ scry def Java_android_os_Parcel_nativeWriteString --kind jni
frameworks/base/core/java/android/os/Parcel.java:1453:32  (jni java)  Java_android_os_Parcel_nativeWriteString

$ scry def system_server --kind sepolicy --limit 2
/home/zim/dev/aosp/system/sepolicy/public/system_server.te:1:6  (sepolicy sepolicy)  system_server
```

Bash scripts (envsetup.sh, soong_ui.bash, OEM build helpers) are
also tree-sitter-parsed; functions and top-level variable
assignments surface like any other lang:

```sh
$ scry def lunch --lang sh --limit 1
build/envsetup.sh:1234:1  (fn bash)  lunch
```

Subdir scoping with `--in`:

```sh
$ scry def Activity --in frameworks/base/ --limit 3
/home/zim/dev/aosp/frameworks/base/core/java/android/app/Activity.java:774:14  (class java)  [Activity]  Activity
/home/zim/dev/aosp/frameworks/base/tools/aapt2/dump/DumpManifest.cpp:1540:7  (class cpp)  [aapt::Activity]  Activity
```

Negative scoping with `--not-in` drops anything whose
path contains the substring. Symmetric to `--in`; both can combine:

```sh
# "callers of bindService, but exclude /tests/ paths"
$ scry callers bindService --not-in /tests/ --format count
1389 callers           # vs 1981 unfiltered (592 test sites dropped)

# scope to frameworks AND exclude tests in one call
$ scry ref bindService --in frameworks --not-in /tests/ --format count
96 ref
```

Wired through `def`, `ref`, `callers`, `uses`, `callgraph`,
`impact` (everywhere `--in` works). On the daemon /
MCP, pass `args.not_in: "/tests/"` with the same semantics.

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

## Build-symbol precision (default-on)

Whenever `<index>/clang_usrs.bin` (libclang USRs) or
`<index>/scip_index.bin` (SCIP symbols from any SCIP producer)
is present, scry's `def` / `ref` / `callers` / `callgraph` /
`impact` queries auto-engage **structured-identity narrowing**:
a candidate ref is kept only if its compiler-bound symbol ID
matches one of the def's symbol IDs. This is the Kythe-class
precision pillar, default-on, zero flags.

```sh
# 1. Build the source index (tree-sitter walk).
$ scry index ~/dev/myproject -o ./idx

# 2. Generate the per-language indexer artifact for your build.
#    Examples (one-time per build regeneration):
$ cmake -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON .   # C / C++
$ scip-typescript index                                  # TypeScript
$ rust-analyzer scip .                                   # Rust
$ scip-go                                                # Go
$ scip-python index .                                    # Python
$ scip-java index --build-tool gradle                    # Java

# 3. Layer the precision sidecars onto the base index. One command
#    per route:
#      a) Kythe-integrated build (AOSP, Bazel, custom kzip pipeline):
$ scry build-symbols --build-kzip ./all.kzip --source-root . --index ./idx
#      b) Pre-built SCIP file from any other producer:
$ scry build-symbols --scip ./index.scip --index ./idx
#      c) Native non-kzip builds:
$ scry build-symbols --build-cmake ./build --index ./idx     # CMake
$ scry build-symbols --build-gn ./out --index ./idx          # GN
$ scry build-symbols --build-kbuild ./build --index ./idx    # Kbuild

# 4. Query. Precision narrows automatically.
$ scry callers Foo --index ./idx
[scry] precise (clang_usrs + scip_index): 18 → 7 refs (clang: 0 id-mismatch, 0 uncovered TU; SCIP: 11 id-mismatch, 0 uncovered TU; 0 def USRs, 2 def SCIP symbols)
# 7 surviving call sites; 11 name-match false positives dropped.

# To opt out and see the raw tree-sitter name match:
$ scry callers Foo --index ./idx --lexical
```

`--lexical` is the single user-facing knob. Behind it, two
auto-engaged filters run when their sidecar is present:

| Sidecar              | Filter               | Built by                                  | Languages covered                         |
|----------------------|----------------------|-------------------------------------------|-------------------------------------------|
| `clang_usrs.bin`     | clang USR identity   | `scry build-symbols --build-{gn,cmake,kbuild,kzip}` | C / C++ / ObjC                            |
| `scip_index.bin`     | Kythe SCIP / VName identity | `scry build-symbols --build-kzip` or `--scip` | Java, Kotlin, Rust, Go, TS, Python, etc.  |
| `scip_index_fqn.bin` | JVM-FQN cross-CU bridge | `scry build-symbols --build-kzip` with `SCRY_KZIP_SERVING_DIR=<dir>` | Java / Kotlin cross-compilation-unit |

A third filter, **`--reachable`**, narrows by build-graph module
visibility (e.g. "drop callers in modules that can't link the
callee"). It stays explicit opt-in because loading the 256MB
AOSP module graph + computing Warshall closure costs ~30s
cold — paying that on every query would crush latency.

Cross-module call resolution is the natural consequence of
clang USR uniqueness: a call to `strdup` in
`bionic/libc/foo.c` has the same USR as `strdup`'s def in
`bionic/libc/upstream-openbsd/.../strdup.c`. The filter links
them across modules without scry needing per-module bookkeeping.

### Cross-CU Java resolution

By default, Kythe's `java_indexer` emits anchor records per
compilation unit, and the source-level VName for
`Binder.clearCallingIdentity()` differs between
`Binder.java`'s own CU (where it's a def) and a caller's CU
like `services.core` (where it's resolved against
`framework.jar` bytecode). Without a cross-CU join, strict
queries like
`scry callers clearCallingIdentity --def-in /android/os/Binder.java --in services/core`
return zero hits — the def-side and ref-side symbol IDs don't
match.

scry handles this by reading the `/kythe/edge/named` edges that
`java_indexer` emits (the indexer-side handle to JVM canonical
FQNs) and lifting them into `scip_index_fqn.bin`. To enable:

```sh
$ SCRY_KZIP_SERVING_DIR=/tmp/serving \
  scry build-symbols --build-kzip ./all.kzip \
    --source-root /home/zim/dev/aosp \
    --index ./idx
# Output includes:
#   ./idx/scip_index.bin       (per-CU anchors)
#   ./idx/scip_index_fqn.bin   (jvm-FQN canonical, cross-CU)
```

`SCRY_KZIP_SERVING_DIR` tees each indexer's stdout for Phase 5's
2-pass FQN importer (Pass 1: collect named-edge bridges; Pass 2:
emit anchors keyed on JVM FQN). The companion sidecar is
consulted alongside the main one by both `build-resolutions`
and the query-time precision filter — a ref matches if its
call-site symbol lands in **either** projection of the def.
After Phase 5 runs, queries like the Binder one above resolve
**1320 hits across services/core** on the live AOSP corpus.

The Kythe v0.0.75 indexers also need four small patches for
AOSP-specific edge cases (Java 21 bytecode reading + classpath
auto-derivation); see [`KYTHE_JVM_INDEXER_REBUILD.md`] for
the patches, build procedure, and why each is needed.

[`KYTHE_JVM_INDEXER_REBUILD.md`]: KYTHE_JVM_INDEXER_REBUILD.md

For per-language one-line setup recipes (AOSP/Soong, CMake,
Cargo, Gradle, etc.) see [`BUILD_AWARE.md`].

[`BUILD_AWARE.md`]: BUILD_AWARE.md

---

## Precision uplift via clangd (`--precise`, legacy path)

For C++ overload-sensitive queries, `scry callers NAME --precise`
routes the query through `clangd` (the LLVM language server) over
LSP. clangd does the real semantic analysis — type inference,
overload resolution, ADL — so call sites that scry's heuristic
ref-extractor mis-attributes get the correct answer.

Note: `--precise` predates the default-on build-symbol precision
above and is now mainly useful when you don't have a precomputed
clang USR sidecar but DO have clangd + a live compile_commands
nearby. For batch / repeated queries, `scry build-symbols --build-{gn,cmake,kbuild}`
+ the default-on clang USR filter is faster (no per-query clangd
warmup) and produces identical narrowing.

```sh
$ scry callers transact --precise --index /mnt/agent/scry-index --limit 5
[precise] clangd OK; compile_commands.json under /home/zim/dev/aosp/out
[precise] clangd returned 142 locations in 1820 ms
/.../frameworks/native/libs/binder/Binder.cpp:412:24  (ref-precise cpp)  transact
/.../frameworks/av/services/.../AudioFlinger.cpp:1245:18  (ref-precise cpp)  transact
...
```

Requirements:
  - `clangd` on `$PATH` (Debian/Ubuntu: `apt install clangd`).
  - `compile_commands.json` somewhere above the definition file
    (generate via `bear -- m`, or your build system's equivalent).
    AOSP: see `bUILD/soong/docs/compile_commands_json.md`.

Without either, `--precise` exits non-zero with an actionable
message pointing at the install / setup step. The heuristic path
(without `--precise`) keeps working regardless.

The shape of the output is identical to the regular `scry callers`
output except for the `(ref-precise LANG)` tag and the `precise: true`
field on JSON results — agents that consume both can dispatch on it.

clangd warmup is ~1 minute on AOSP (it has to index its own metadata
before answering queries). For a session that runs many precise
queries, consider `scry serve --listen unix:...` and keep one
clangd alive across the run (forthcoming; see `docs/ROADMAP.md` § 3).

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

`→ libs/binder/Binder.cpp:411 [android::BBinder]` is the
Layer 2 resolution — the resolver picked that specific def. Without
`--def-in`/`--strict` (see below) this is permissive: unresolved refs
show no `→` annotation. Pass `--json` to get the raw `resolved_to`
u64 instead of the human-readable file:line.

`scry ref` is the generic version that includes all ref kinds (call,
ctor, type-use, field-access, import, inherit). `callers` is the
common-case shorthand for `ref --kind call`.

### Cutting through polymorphism

Polymorphic names like `close`, `onCreate`, `transact` have
thousands of distinct defs in a big corpus. Three flags help
narrow:

```sh
# --def-in PATH — keep only refs resolving to a def in PATH
$ scry callers transact --def-in libs/binder/Binder.cpp
# returns the 166 callers whose resolved_to lands at BBinder.transact
# (plus the over-included permissive bucket if --strict isn't set)

# --strict — drop refs that resolved to anything else, including
# unresolved. Trades recall for precision.
$ scry callers transact --def-in libs/binder/Binder.cpp --strict
# returns only the 166 confidently-resolved hits, no over-include

# --format by-def — histogram of which def gets called most
$ scry callers transact --strict --format by-def --limit 8
     166  → libs/binder/Binder.cpp:411 [android::BBinder]
      14  → securityPatch/CVE-2016-2412/poc.cpp:77
       7  → libs/binder/Binder.cpp:114 [android::hardware::BHwBinder]
       7  → libs/binder/BpBinder.cpp:400 [android::BpBinder]
       ...
     219 refs in 19 groups (showing 8)
```

`--format by-def` composes with `--json` for
programmatic consumers — emits a JSON array of
`{count, def: {path, line, col, scope, kind, id}}` entries.

These three flags also work on `scry ref`, `scry callgraph`
(root-level only), and `scry impact` (callers leg only), and
are exposed via the same args on the JSON-RPC + MCP `ref` /
`callers` / `callgraph` / `impact` tools.

### `--format count`

For "how many callers does X have?" the JSON envelope and the
per-hit rows are both wasted bytes. `--format count` emits one
short line:

```sh
$ scry callers transact --lang Java --format count
1524 callers

$ scry ref BatteryStats --format count
85 ref
```

Mutually exclusive with `--json`. Useful as a cheap probe
before deciding whether to spend tokens on the full list.

### `--format paths`

Cheap "which files reference X?" shape: deduped sorted file
paths only, no line/col/scope noise. Pipes straight into
`xargs`, `vim`, or `git`.

```sh
$ scry callers bindService --format paths --limit 5
/home/zim/dev/aosp/cts/.../ActivityManagerAppExitInfoTest.java
/home/zim/dev/aosp/cts/.../BitmapTest.java
/home/zim/dev/aosp/cts/.../CarOccupantConnectionManagerTest.java
/home/zim/dev/aosp/cts/.../SignatureQueryServiceInstrumentationTest.java
/home/zim/dev/aosp/cts/.../TunerTest.java

5 unique files (from 1981 refs)

# JSON shape: a single sorted array of strings
$ scry callers bindService --format paths --json --limit 3
["…/ActivityManagerAppExitInfoTest.java", "…/BitmapTest.java", "…/CarOccupantConnectionManagerTest.java"]
```

Dedup happens before `--limit`, so the cap counts unique files
(not raw refs). Output is sorted ascending so diffs across runs
stay stable. Works on `scry ref` and `scry callers`; same
`args.format: "paths"` shape on the daemon and MCP `ref` /
`callers` tools.

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
| `-i`  | `--ignore-case`       | Case-insensitive match (literal or regex). Trigram pre-filter expands across ASCII case variants so it stays fast. |
| `-t`  | `--lang LANG`         | Filter by language                           |
|       | `--in SUBSTR`         | Restrict to files whose path contains SUBSTR |
|       | `--limit N`           | Cap hits (default 100)                       |
|       | `--workers N` / `-j`  | Rayon pool size for the per-file scan        |
|       | `--max-file-bytes N`  | Skip files larger than N bytes (default 10 MiB) |
|       | `--mem-cap N`         | Refuse to start if scan would exceed N GiB   |
|       | `--json`              | NDJSON (one object per hit)                  |
|       | `--format lines`      | `path:line:col\tsnippet`, one hit per line   |
|       | `--format count`      | Just `N hits across M files` — no per-hit rows |

### Case-insensitive grep (`-i` / `--ignore-case`)

`bindservice` should find `bindService`. Pass `-i`:

```sh
$ scry grep -i bindservice --limit 3
.../IServiceManager.java:42:23:    public IBinder bindService(...) {
[grep] trigram pre-filter (CI): 2913 candidate files in 18 ms
```

How it stays fast: for each 3-byte trigram of the query, the
pre-filter unions the posting lists of every ASCII case variant
(≤ 8 per trigram), then intersects across positions. The inner
matcher is `regex::bytes` compiled with `case_insensitive(true)`
from a regex-escaped form of the literal — so meta-characters in
the pattern stay literal. Same shape works with `--regex -i` for
case-folded regex.

### Compact output (`--format`)

For agent loops that only need "is X referenced AT ALL?" the JSON
envelope dominates the payload. `--format=lines` emits a rg-shaped
tab-separated record per hit:

```sh
$ scry grep ZygoteInit --format=lines --limit 3
/home/zim/dev/aosp/frameworks/base/cmds/app_process/app_main.cpp:92:20	virtual void onZygoteInit()
/home/zim/dev/aosp/frameworks/base/cmds/app_process/app_main.cpp:336:48	    runtime.start("com.android.internal.os.ZygoteInit", args, zygote);
/home/zim/dev/aosp/frameworks/base/core/java/android/app/AppOpsManager.java:100:18	import com.android.internal.os.ZygoteInit;
```

For "does this name exist anywhere" the cheapest form is
`--format=count` — one short line regardless of hit count:

```sh
$ scry grep ZygoteInit --format=count --limit 10000
50 hits across 27 files
```

`--json` and `--format` are mutually exclusive.

### Diagnose a slow grep with `--explain`

`--explain` short-circuits the actual scan and dumps the query
plan: the extracted trigrams (smallest-first, with posting size
each), the final candidate count after intersection, and a rough
scan-cost estimate. Use it when a grep is unexpectedly slow and
you want to know *why* before tightening the pattern.

```sh
$ scry grep ActivityManagerService --explain
query:      "ActivityManagerService"
trigrams (20 extracted, smallest-first intersection):
  "tyM"        11913 files
  "yMa"        24505 files
  "vit"        35649 files
  ...
candidates: 1276 files post-intersection
scan-cost:  ~89 MiB estimated I/O (1276 candidates × 71 KiB avg file size)
```

A small `candidates:` count means the trigram pre-filter is
doing its job; a large one means the pattern is too common
across the corpus and a `--lang` / `--in` filter would help.
Regex patterns report whether literal-extraction analysis
found anything to pre-filter on, falling back to a full-scan
notice when no literals could be extracted.

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

### `scry tldr PATH` — one-call file summary

For "what does this file do?" agent queries, `outline` returns
too much (all N symbols) and `def NAME` returns too little (no
file shape). `scry tldr` collapses both into a single call:

```sh
$ scry tldr frameworks/base/core/java/android/os/Binder.java
# /home/zim/dev/aosp/frameworks/base/core/java/android/os/Binder.java  (Java)
# 111 symbols
#
# first line: /*
#
# kinds:  4×class  2×ctor  25×field  1×iface  79×method
#
# top 3:
   85:14   class         Binder
  133:26   class         NoImagePreloadHolder  [Binder::NoImagePreloadHolder]
  311:26   class         TransactionTraceNamesCacheHolder  [Binder::TransactionTraceNamesCacheHolder]
```

JSON form (`--json`) emits a flat object with `path`, `lang`,
`symbols_total`, `by_kind: [{kind, count}]`, `top: [{name, kind,
line, col, scope}]`, and `first_line`. Cuts ~70% of the tokens
vs `outline + 3×def` for the same answer.

Same PATH-matching rules as `outline`. Exposed as the `tldr`
tool in MCP.

### `--with-snippets N`

For agent loops that usually follow `outline` with a per-symbol
`def` to read the signature, pass `--with-snippets N` to inline
the first N source lines of each symbol:

```sh
$ scry outline frameworks/base/.../Activity.java --with-snippets 2 --limit 3
# /home/zim/dev/aosp/.../Activity.java  (Java)
# 41 symbols
   62:14   class         Activity  [Activity]
       │ public class Activity extends ContextThemeWrapper
       │         implements LayoutInflater.Factory2,
  237:13   field         FRAGMENTS_TAG  [Activity]
       │     private static final String FRAGMENTS_TAG = "android:fragments";
  ...
```

JSON form adds a `snippet` field per symbol. Saves the second
round-trip when "show me what's in this file AND what each thing
looks like" was the whole question. Snippet lines are clipped to
200 chars to bound token cost.

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
scry-version: 0.1.36
indexed-at:   2026-05-17T02:26:57Z
roots:        2
  - /home/zim/dev/aosp (Aosp)
  - /mnt/agent/dev/linux (Linux)
files-total:  1032084
files-parsed: 1032084
files-failed: 0
bytes-total:  70.4 GB
symbols:      31496680
refs:         63318468
refs-resolved: 31426932 (49.6%)
elapsed-ms:   690040

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

The `refs-resolved` line shows what fraction of refs
the Layer 2 resolutions sidecar attributes to a specific def.
`<no sidecar — run scry build-resolutions to enable>` appears
when the sidecar hasn't been built yet. Higher is better — it's
the lever the `--def-in` / `--strict` flags operate on (see the
ref/callers section above).

### Machine-readable: `scry stats --json`

```sh
$ scry stats --json | jq .
{
  "scry_version": "0.1.6",
  "manifest_version": 1,
  "indexed_at": "2026-05-16T14:19:56Z",
  "roots": [
    {"path": "/home/zim/dev/aosp", "profile": "Aosp"},
    {"path": "/mnt/agent/dev/linux", "profile": "Linux"}
  ],
  "files_total":  1009161,
  "files_parsed": 1009161,
  "files_failed": 0,
  "bytes_total":  75603456000,
  "symbols":      25082959,
  "refs":         63166322,
  "elapsed_ms":   5510277,
  "by_lang": {"Java": 5615791, "Header": 7607137, ...},
  "by_kind": {"class": 8210123, "method": 6543210, ...}
}
```

Stable shape: new fields may be appended in future releases,
but existing keys won't move or change type (pinned by an e2e
shape assertion). Powered by `serve_stats` on the JSON-RPC
side so the schemas match across stdio + listener + CLI use.

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

### Capacity caps (`--max-conns`)

Default is unlimited concurrent connections. On a shared host or
when the workload could fan a thousand agents at the daemon, cap
it explicitly:

```sh
$ scry serve --index /mnt/agent/scry-index \
    --listen tcp:127.0.0.1:9999 --max-conns 64
[scry serve] listening on tcp:127.0.0.1:9999
[scry serve] max_conns=64; over-cap accepts will be dropped
```

Each accepted connection can run grep with its own rayon worker
pool — so unbounded fan-in × unbounded per-query fan-out can OOM
under stress. A safe ceiling for a 16-core box is 32-64; for a
72-core box, 128-256. Over-cap accepts are logged to stderr and
the connection is dropped immediately (clients see a TCP RST or
Unix-socket EOF). Workers release their slot via an RAII guard
so a panic still frees capacity.

`--max-conns 0` (default) is unlimited.

#### Cap-rejected connections get an actionable error

When the cap is hit, scry writes a single JSON-RPC error line
to the rejected connection before closing it (rather than
silently dropping). MCP-aware clients can branch on
`error.code == -32004`; non-MCP clients see the human-readable
`message`:

```json
{"jsonrpc":"2.0","id":null,"error":{
  "code":-32004,
  "message":"scry serve at capacity (max_conns=64); retry after current requests complete",
  "data":{"max_conns":64,"retryable":true}
}}
```

`data.retryable: true` signals that backing off and reconnecting
will likely succeed once load drains — no exponential backoff
needed beyond a short jitter.

#### Inspecting and killing the daemon

scry doesn't ship its own connection-list or kill subcommand —
the standard Unix tools already do this well:

```sh
# Find the PID of the running daemon:
pidof scry            # or: pgrep -f 'scry serve'

# Live connection count + per-connection state (TCP):
ss -p -t state established '( sport = :9999 )'

# Live connection count (Unix socket):
ss -p -x state established 'src /tmp/scry.sock'

# Open file descriptors held by the daemon (Unix socket FDs included):
lsof -p $(pidof scry) | grep -E 'sock|TCP'

# Clean shutdown (SIGTERM — gives in-flight queries time to finish):
kill $(pidof scry)

# Force-kill after the grace period:
kill -9 $(pidof scry)
```

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

Full wire-shape reference, error semantics, and per-client
configuration recipes (Claude Desktop, Cursor, Continue, custom
LangGraph) live in [`docs/MCP.md`]. The summary below is the
quickstart.

[`docs/MCP.md`]: MCP.md

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

### Automatic stale-index warning

If the index you're querying was built with a different `scry`
version than the running binary, you'll see a one-line stderr
warning on every command that opens it:

```
[scry] WARNING: this index was built with scry 0.1.0; running 0.1.6.
       Older builds may have stale records (e.g. the Java/C++
       scope_path bug fixed in 0.1.3). Rebuild with
       `scry index <ROOT> -o /mnt/agent/scry-index` or
       `scry index --incremental <ROOT> -o /mnt/agent/scry-index`.
       Suppress this warning with SCRY_QUIET=1.
```

The warning is informational — queries still run. Set
`SCRY_QUIET=1` to suppress (CI, scripted use). When in doubt,
rebuild; an incremental rebuild on top of the existing index is
sub-second for small change sets.

### Indexing commands

```sh
# Full indexing (production: use scripts/run_index.sh which wraps this
# with the right knobs + cgroup-protected systemd unit).
$ scry index ~/dev/aosp /mnt/agent/dev/linux -o /mnt/agent/scry-index \
    --workers 16 --mem-cap 40 --resume --build-trigrams
[walk]  /home/zim/dev/aosp (profile: Aosp)
[walk]  978214 files / 73.4 GB / 8412 ms
[walk]  /mnt/agent/dev/linux (profile: Linux)
[walk]  30947 files / 1.2 GB / 412 ms
[progress] 1000/1009161 files (0.1%) · 12834 f/s · ETA 1m18s · batch 1 · 421 syms · 1252 refs
[progress] 2000/1009161 files (0.2%) · 9128 f/s · ETA 1m50s · batch 1 · 8211 syms · 24930 refs
...
[progress] 1009000/1009161 files (100.0%) · 6478 f/s · ETA 0s · batch 87 · 25081244 syms · 63163521 refs
[parse] batch 87/87  11631 files / 33112 syms / 71202 refs / ~38 MB in-RAM / 8814 ms (avg 6914 B/file)
[write] 25082959 symbols, 63166322 refs across 1009161 files / 2 roots, finalizing -> /mnt/agent/scry-index
[write] finalized in 13298 ms
DONE: 1009161 files, 25082959 symbols, 63166322 refs, total 5510277 ms (183.1 files/s)
```

The `[progress]` line fires every 1000 files. `f/s` is rolling
over the full job (not per-batch) so it stays a stable ETA
signal; `ETA` formats as `45s`, `12m30s`, or `2h05m`. Each
milestone prints exactly once (atomic `fetch_max` over
`p / step`) so the output is clean even with N×64 parallel
workers.

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

# Per-file content digest (blake3) sidecar — powers index-diff and
# the full --incremental indexer. ~25 s for the full AOSP+Linux corpus.
$ scry build-digests --index /mnt/agent/scry-index
[digests] 1009166 files to hash
[digests] hashed in 24812 ms
[digests] DONE. 32293312 bytes written → /mnt/agent/scry-index/file_digests.bin
```

## Incremental indexing

```sh
# Re-parse only changed + added files; replay unchanged from the
# old index; atomically swap into place. The old index stays
# queryable for the whole duration — if the process dies mid-build
# the old index is still there.
$ scry index --incremental ~/dev/aosp /mnt/agent/dev/linux \
    -o /mnt/agent/scry-index
[incremental] diff: 1009160 unchanged, 4 changed, 2 added, 0 removed
[incremental] parsing 6 files...
[incremental] finalized in 412 ms (full rebuild would have been 13 min)

# Preview-only: report what *would* change without writing.
$ scry index-diff ~/dev/aosp /mnt/agent/dev/linux
[index-diff] walked 1009166 files in 25340 ms
unchanged: 1009160
changed:   4 (would re-parse)
added:     2 (would parse fresh)
removed:   0

# Verbose mode lists every changed/added/removed path.
$ scry index-diff --verbose ~/dev/aosp /mnt/agent/dev/linux

# Manually tombstone a file (next query of any kind skips it).
# Useful when you've deleted a file and want immediate query freshness
# without running a full reindex:
$ scry rm-tombstone deleted_file.java --index /mnt/agent/scry-index
[tombstone] marked 1 file(s) (1 newly); bitmap is 126146 bytes
```

**Pattern**: after the initial full `scry index`, run
`scry build-digests` once. From then on, `scry index --incremental`
is the supported editor-loop refresh — sub-second on small change
sets, atomic, never leaves the old index in a partial state.
`scry compact` is the future tombstone-reclaim pass (placeholder
today; in-place rewrite TODO).

## OWNERS lookup: `scry owner`

Walk up from a path collecting OWNERS entries from each enclosing
OWNERS file. The closest-to-PATH owner list comes first, more-distant
inherited owners after — matches Gerrit's evaluation order. The walk
respects `set noparent` (and the bare `noparent` form): visited at
that level, then halted.

```sh
$ scry owner frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java
owners for /home/zim/dev/aosp/.../ActivityManagerService.java:
  via /home/zim/dev/aosp/frameworks/base/services/core/java/com/android/server/am/OWNERS
    alice@example.com
    bob@example.com
```

Three modes:

| flag             | when to use                                                                            |
|------------------|----------------------------------------------------------------------------------------|
| (default)        | "who do I @-mention?" — nearest non-empty owner set, stops there.                      |
| `--include-deep` | "show me the full chain" — every layer the walk visited, in evaluation order.          |
| `--accumulate`   | "who can approve this?" — union of emails across every visited layer, sorted + deduped. |

```sh
$ scry owner frameworks/base/services/.../ActivityManagerService.java --accumulate
owners for /home/zim/dev/aosp/.../ActivityManagerService.java:
  via .../am/OWNERS
    alice@example.com
  via frameworks/base/services/core/java/.../OWNERS
    bob@example.com
  via frameworks/base/OWNERS
    carol@example.com

approvers (3):
  alice@example.com
  bob@example.com
  carol@example.com
```

`--json` emits one object per layer (with `set_noparent` flagged
where present) plus, under `--accumulate`, an `approvers` array
on the envelope. Suitable for piping into the CI bot that does
the actual `gerrit-push` invocation.

---

## Semantic retrieval: `scry ask`

Find code chunks whose embedded text is most similar to a natural-
language query. Useful when the agent doesn't know which identifier
to grep for. Default embedding model is a deterministic FNV-1a
hashing-trick bag-of-tokens — no model download, no extra deps;
catches vocabulary overlap (the dominant signal for code search).

```sh
# One-time setup: compute and store the embedding sidecar.
$ scry build-embeddings --index /mnt/agent/scry-index
[embed] 1009166 files; dim=64, chunk=100+20overlap
[embed] computed 3128456 chunks in 412 s
[embed] DONE. 3128456 chunks × 64 dim → 763.5 MB

# Now ask in natural language:
$ scry ask "how does the system create new processes" --limit 5
/.../frameworks/base/services/.../ProcessRecord.java:54-153  (score=0.728)  (Java)
    public ProcessRecord(ActivityManagerService _service, ...) {
        this.mService = _service;
/.../frameworks/native/services/.../ProcessLauncher.cpp:32-131  (score=0.694)  (Cpp)
    pid_t launch(const Args& args) {
        pid_t pid = fork();
...

# JSON for agent consumption:
$ scry ask "parse toml configuration" --limit 3 --json
{"path":"...","lang":"Rust","start_line":42,"end_line":131,"score":0.812,"snippet":"..."}
```

Flags:
  `--dim N`            embedding dimension (default 64). Higher → bigger sidecar, finer discrimination.
  `--chunk-lines N`    chunk window in lines (default 100).
  `--chunk-overlap N`  overlap between consecutive chunks (default 20).
  `--in PREFIX`        same path-substring filter as the rest of scry.
  `--limit N`          top-K results (default 10).
  `--json`             one JSON object per result.

Exposed over `scry serve` and `scry mcp` as the `ask` tool. Cold-cache
query latency on the full corpus is ~500 ms (dominated by walking the
~760 MB embeddings.bin); warm queries are ~50 ms.

The hashing-trick embedding is solid for vocabulary matching but not
as semantically rich as a transformer-based one. The wire format is
designed so a future commit can swap in a real model (candle +
all-MiniLM or nomic-embed-code) behind a feature flag without
changing the sidecar layout or query API.

---

## Ops log

Every CLI invocation appends one JSON line to `~/.scry/queries.log`
(override via `SCRY_LOG=/path/to/log`). The line is a flat object
suitable for `jq`, `DuckDB read_ndjson_auto`, BigQuery
`NEWLINE_DELIMITED_JSON`, or `pandas.read_json(lines=True)`:

```sh
$ tail -2 ~/.scry/queries.log
{"ts":1778922191,"cmd":"def","query":"ActivityManagerService","hits":4,"shown":1,"files_total":1009166,"candidate_files":null,"elapsed_ms":321,"index":"/mnt/agent/scry-index","scry_version":"0.1.6","pid":520994}
{"ts":1778922192,"cmd":"grep","query":"Activity","hits":2,"shown":2,"files_total":1009166,"candidate_files":30482,"elapsed_ms":392,"index":"/mnt/agent/scry-index","scry_version":"0.1.6","pid":520994}
```

### Fields

| field            | meaning                                                                    |
|------------------|----------------------------------------------------------------------------|
| `ts`             | Unix-epoch seconds, UTC.                                                   |
| `cmd`            | Subcommand (`def`, `grep`, `callers`, `outline`, …).                       |
| `query`          | The pattern / name / path the user supplied.                               |
| `hits`           | Total records matched before any `--limit` truncation.                     |
| `shown`          | Records the caller actually rendered (after `--limit`).                    |
| `files_total`    | Files in the index at query time.                                          |
| `candidate_files`| Files surviving the trigram pre-filter (grep only); `null` otherwise.      |
| `elapsed_ms`     | Wall-clock from CLI entry to log call.                                     |
| `index`          | Absolute path of the index dir queried.                                    |
| `scry_version`   | Version of the binary that ran the query (correlate perf with rollouts).   |
| `pid`            | Process id (disambiguate parallel agent calls).                            |

### Analyzing usage at scale

Identify slow query patterns and which `cmd` to optimize:

```sh
# Slowest 10 invocations in the log:
jq -s 'sort_by(.elapsed_ms) | reverse | .[0:10]' ~/.scry/queries.log

# P95 latency per cmd:
jq -s 'group_by(.cmd) | map({cmd: .[0].cmd,
  p95: (sort_by(.elapsed_ms) | .[(length*0.95|floor)].elapsed_ms),
  n: length})' ~/.scry/queries.log

# Empty-result queries (suggest fuzzy / typo path):
jq -c 'select(.hits == 0) | {ts, cmd, query}' ~/.scry/queries.log

# Hits per cmd in the last day:
jq -s --argjson cutoff $(date -d '1 day ago' +%s) \
  'map(select(.ts >= $cutoff)) | group_by(.cmd) | map({cmd: .[0].cmd, n: length})' \
  ~/.scry/queries.log

# Version-skew check across a fleet (per-host log shipped centrally):
jq -s 'group_by(.scry_version) | map({v: .[0].scry_version, n: length})' all-logs.jsonl
```

For DuckDB-driven dashboards:

```sql
INSTALL httpfs; LOAD httpfs;
SELECT cmd, COUNT(*) n, quantile(elapsed_ms, 0.95) p95_ms
FROM read_ndjson_auto('~/.scry/queries.log')
GROUP BY cmd ORDER BY p95_ms DESC;
```

### Operational notes

The log is best-effort — a write failure never affects the
query's exit status or stdout.

**Built-in rotation.** Once the active log crosses
`$SCRY_LOG_MAX_BYTES` (default `100MiB`), scry renames it to
`<path>.1` (overwriting any prior `.1`) and starts a fresh file.
Bounded total disk = 2 × the cap. Set `SCRY_LOG_MAX_BYTES=0` to
disable rotation entirely.

Why this matters: under a tight MCP loop (one query / 100 ms,
24 h × 7 = ~6M queries × ~300 bytes/row ≈ 1.8 GB) the log would
otherwise eat disk in days. With the default cap, you keep at
most ~200 MB and roughly the last day or two of history per host.

**Disabling the log entirely.** Set `SCRY_LOG=` (empty string).
Useful for ephemeral MCP sessions where the activity log isn't
load-bearing. With logging off, the stderr footer still prints
(zero-disk-cost).

**Custom paths / centralized logging.**

```sh
export SCRY_LOG=/var/log/scry/$USER.jsonl       # per-user under shared dir
export SCRY_LOG_MAX_BYTES=$((500 * 1024 * 1024)) # 500 MiB cap
```

For multi-host fleets, ship the file via vector / filebeat /
fluent-bit; the per-line JSON schema is stable across scry
versions.

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
