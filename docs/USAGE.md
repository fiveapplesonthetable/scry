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

Substring match anywhere in the symbol name. Slower than `prefix`
(walks the FST) but useful when you don't remember the leading
characters.

```sh
$ scry fuzzy ParcelFile --limit 3
/home/zim/dev/aosp/frameworks/base/core/java/android/os/ParcelFileDescriptor.java:67:14  (class java)  [ParcelFileDescriptor]  ParcelFileDescriptor
...
[scry] cmd=fuzzy q="ParcelFile" hits=22 shown=3 files=1009166 elapsed=190ms
```

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

## JSON-RPC over stdin: `scry serve`

Open once per agent session and reuse the warm mmap'd index:

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

Full per-command argument schema is in `README.md` under the JSON-RPC
section.

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
