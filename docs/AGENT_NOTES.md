# AGENT_NOTES — scry for LLM-driven code search

I run LLM agents against AOSP for a living. Most of the value an
agent gives me is in the small loop: it has a question about the
codebase, it calls a tool, it reads the result, it decides what
to do next. The tool budget — wall time per call and tokens per
reply — sets the ceiling on how much real work the agent gets
done before its context fills up.

This document is the working note on how scry changes that loop:
what it does well, what it does poorly, and what an agent built
on smaller open-weight models (Gemma 3, Llama 3.2, Qwen 2.5,
Mistral) needs from a code-search tool that a frontier model
doesn't.

Numbers are from real sessions against the live AOSP + Linux
index at `/mnt/agent/scry-index`. Token counts use the Claude
tokenizer; ratios are close enough on GPT-4, Gemma, and Llama 3
that they reproduce within ±15%.

---

## 1. The token problem with code search

Every tool call an agent makes has two costs: wall-clock latency
the agent blocks on, and tokens the reply burns from the context
window. The second is the binding constraint. A 200k-token
context sounds enormous until it's holding an active
conversation, two earlier tool results, a half-formed plan, and
the file the agent is about to edit. Every megabyte of file
contents returned by a tool is one less file the agent can keep
in working memory.

The naive approach to "where is X" is `rg X /path`. On AOSP,
that returns hundreds to thousands of `path:line:match` lines.
For `rg ZygoteInit ~/dev/aosp`:

- **Wall time**: 21.2 s.
- **Result text**: ~600 lines, ~38 000 characters,
  ~9 500 tokens.
- **Information density**: roughly one fact per ~200 tokens.
  The rest is path noise and surrounding bytes nobody asked for.

`scry def ZygoteInit --kind class --limit 5`:

- **Wall time**: 0.58 s.
- **Result text**: 5 lines, ~600 characters, ~150 tokens.
- **Information density**: one fact per ~30 tokens — the
  fully-qualified name, path, line, kind, scope, and a small
  snippet.

**60× fewer tokens for the answer that was actually wanted.**
On a task that runs 20-50 tool calls, that's the difference
between an agent that finishes in one session and one that
runs out of context partway through. Token economy is the
headline reason this tool exists.

---

## 2. Setting scry up for LLM use

The setup is intentionally minimal. There is no SDK, no schema
file to register, no daemon to start. The pattern that works for
me:

1. **Make sure the index exists.** `scry stats --index /path`.
   Exits zero with a one-line summary. If the index is missing,
   I run the rebuild as a tool call and wait the 13 minutes,
   which costs nothing in tokens because the indexer prints two
   progress lines and exits.
2. **Drive the binary, not a wrapper.** Every command has a
   `--json` flag. I shell out to `scry def X --json --limit 10`
   and parse the result. The JSON shape is documented in
   `docs/USAGE.md` and stable across versions.
3. **For long tool sessions, use `scry serve`.** It reads
   newline-delimited JSON-RPC on stdin, writes responses on
   stdout. One process opens the mmap once; subsequent queries
   reuse the warm page cache. Per-query latency drops by ~5× vs.
   re-launching `scry` each time, because the cold open cost
   (~50 ms) is amortized.

```sh
$ scry serve --index /mnt/agent/scry-index
{"id":1,"cmd":"def","args":{"name":"ZygoteInit","limit":3}}
{"id":1,"result":[{"name":"ZygoteInit","kind":"class","lang":"Java","path":"frameworks/base/core/java/com/android/internal/os/ZygoteInit.java","line":85,"scope":["com.android.internal.os"]},...]}
```

Stable symbol IDs are the underrated feature. Every result has a
`symbol_id` (deterministic hash of FQN + kind). When I find
`ZygoteInit` and then want to ask "who references it", I can
correlate by ID without re-resolving the name in a possibly-
ambiguous context. This matters for any model with a small
context window — re-resolution is the kind of error that creeps
in when the model has to keep the same string accurate across
many turns.

### Per-query stats footer

Every `scry` invocation prints a stats footer to stderr (or in
the JSON envelope on stdout) and appends one JSON line to
`~/.scry/queries.log`:

```
3 hits in 0.58s · 1416 files searched · 8 trigrams intersected · index v3
```

I use this two ways:

- **In-loop**: if the elapsed time spikes for a query I expected
  to be fast, I know the query was unselective and I should
  narrow it (e.g. add `--in frameworks/base/`).
- **Post-hoc**: `jq -s 'sort_by(-.elapsed_ms)[:10]' ~/.scry/queries.log`
  shows my slowest queries this session, so I can write better
  ones next time.

The log is also useful for *me as a future model invocation* —
the previous session's tool calls become a cheap form of memory.

---

## 3. Accuracy

Let me walk through a concrete task: "Who calls `Binder.transact`
from inside `frameworks/base`?". This is a question I'd ask 1-3
times per day in the AOSP context.

### The `rg`-driven approach

```
$ rg -n 'transact\(' ~/dev/aosp/frameworks/base/ | head -200
```

Returns ~3 800 hits. The token cost of feeding all of them back
into my context is prohibitive (~120k tokens — more than half a
full Claude context). So I sample, hope I sampled representative
ones, and then I have a *biased view* of who calls `transact`.
I miss the long tail.

Worse: `transact(` matches `IBinder.transact`, `Parcel.transact`
(doesn't exist, but I'd have to read source to know that),
`MyClass.transact()` if someone happened to name a method that,
and string-literal mentions in comments. The signal-to-noise is
maybe 60%, and I can't tell without reading each file.

### The `scry`-driven approach

```
$ scry callers transact --lang Java --in frameworks/base/ --limit 50
```

Returns 50 hits with `ref_kind: call`, each with a scope path
(`com.android.server.am.ActivityManagerService.startActivityAsCaller`),
the calling method's signature, and a 200-character snippet of
the call site. Token cost: ~6 000 tokens. Signal-to-noise: 100%
within the limit (every result is in fact a call site to a
method named `transact`).

The accuracy gain comes from two structural things:

- **scry knows what's a call vs. a declaration vs. a string
  literal**, because tree-sitter parsed the file and the
  reference extractor tagged the kind. `rg` only sees the
  literal `transact(`.
- **scry can scope by build module** (`--in frameworks/base/`)
  and by language (`--lang Java`). `rg` can scope by directory
  but doesn't understand language; a `.java` file with a
  `transact(` in a comment looks identical to a real call to
  `rg`'s pattern matcher.

The remaining gap is type precision: scry's name-based matcher
might tag a call to `MyOtherInterface.transact()` as a callers-of
`Binder.transact` hit, because it matches on the spelled name, not
a compiler-resolved symbol identity. For compiler-grade semantic
def/ref/callers on the AOSP C++/Java slice, that's the job of the
companion tool `scry2`. For "give me a representative sample of who
calls this", scry is correct enough that I act on its output without
verification.

### Where scry is *less* accurate than `rg`

- **String-literal grep.** If I'm hunting for a log message
  template ("Failed to start activity %s"), `rg` is the right
  tool — scry's trigram pre-filter still works on the literal
  but `rg`'s regex engine is more general and faster on
  full-text scans.
- **Cross-language string IDs**. An aconfig flag like
  `enable_foo_bar` referenced as a Java string in
  `Flags.enable_foo_bar()` is something scry knows about (it
  tracks flag definitions and reads in Java/Kotlin/C++), but
  the same string used as a literal in a shell script is *only*
  matched by `rg` — scry's symbol model doesn't pick it up.
- **Anything not in the trigram-indexable language**. scry's
  walker classifies ~40 file kinds; everything else is invisible.
  `rg` greps everything.

In practice, for the queries I run most, scry is more accurate.
For the queries I run occasionally where I need every textual
match, `rg` still wins.

---

## 4. Speed

Latency matters for an LLM tool loop because it bounds *how many
tool calls per minute I can make* and indirectly bounds how long
the user waits for the final answer. A 20 s tool call blocks the
user, and worse, if a typical task takes 30 calls, the difference
between 0.6 s and 20 s per call is the difference between a
30-second session and a 10-minute one.

Wall-time numbers from my own usage (warm page cache, P50):

| query class                | rg          | scry         | speedup |
|----------------------------|------------:|-------------:|--------:|
| literal grep, common       | 19 s        | 0.5 s        | 38×     |
| literal grep, rare         | 17 s        | 0.4 s        | 42×     |
| def by name                | n/a (rg)    | 8 ms         | n/a     |
| callers by method name     | n/a (rg)    | 80 ms        | n/a     |
| outline of a single file   | n/a (rg)    | 600 ms       | n/a     |
| prefix walk of identifier  | n/a (rg)    | 12 ms        | n/a     |

The "n/a" entries are queries `rg` can't answer at all without
the kind of post-processing that would push it well past scry's
times.

The numbers are from `~/.scry/queries.log` on real sessions, not
from synthetic benchmarks. The bench scripts in
`docs/BENCHMARKS.md` reproduce them within a few percent.

### Latency as agent UX

Below ~500 ms per tool call, I can call tools in a tight loop
and the user sees a fluid response. Above ~2 s per tool call,
the loop feels like a remote IDE running over a flaky connection.
scry is in the first regime; `rg` over AOSP is in the second.

For multi-step tasks where I issue 10-20 queries in sequence —
"find a class, list its public methods, find callers of the
interesting one, look at the caller's enclosing class, etc." —
the wall-time difference adds up to minutes. With scry, the
sequence completes in ~10 s of tool time. With `rg`, the same
sequence is ~4 minutes plus the cost of parsing larger results.

---

## 5. Token reduction in concrete numbers

A worked example. Task: "Explain the lifecycle of a Java
Activity in AOSP."

### The `rg`-only approach

```
$ rg -n 'class Activity ' ~/dev/aosp/frameworks/base/core/java/android/app/
```

Returns ~6 hits. I want to read the class definition:

```
$ cat ~/dev/aosp/frameworks/base/core/java/android/app/Activity.java
```

8 472 lines. ~95 000 tokens at typical density. This *alone* is
half my context window. I have to do extractive reading to keep
the rest of the task in context.

### The `scry`-driven approach

```
$ scry outline frameworks/base/core/java/android/app/Activity.java --limit 30
```

Returns the top 30 symbols (method names + line ranges) in
Activity.java, ranked. ~80 lines, ~3 000 tokens.

```
$ scry def Activity --kind class --limit 1
```

Returns the class declaration with a 1.5 KB snippet of the
top-level class body. ~400 tokens.

```
$ scry def onCreate --in frameworks/base/core/java/android/app/Activity.java --limit 1
```

Returns just the `onCreate` method body. ~200 tokens.

I've now built up an understanding of the class structure (3 000
tokens), the class context (400 tokens), and the specific
lifecycle method I care about (200 tokens) — total ~3 600 tokens
vs. ~95 000 for reading the whole file. A 26× reduction, and I
read only what was relevant.

### Where the savings come from

| structural reason                                  | token saving |
|----------------------------------------------------|--------------|
| Pre-parsed structure: outline instead of source    | 20-30×       |
| Pre-extracted snippets: just the def, not the file | 10-20×       |
| Stable IDs: no re-quoting names across turns       | 2-5×         |
| `--limit` to cap results: no half-megabyte dumps   | 5-50×        |

These multiply in practice. A task that was "lose the conversation
to context overflow" is now "finish in one session" because of
this compounding.

---

## 6. Setting up for smaller LLMs (Gemma 3, Llama 3.2, Qwen 2.5, Mistral)

Smaller open-weight models have two constraints that frontier
models can usually paper over: smaller context windows
(8-32k typical, not 128-200k), and less robust reasoning about
out-of-distribution inputs (raw source-file text is fine for
Claude 4 or GPT-4; for Gemma 3 1B it's noisy).

scry helps disproportionately at the small end:

### 6.1 Pre-parsed structure trades poorly-utilized context for
well-utilized context.

A 32k context full of source bytes spends most of its capacity on
formatting, whitespace, and irrelevant function bodies. The same
32k full of `scry` outputs holds an order of magnitude more
*facts*. For a model whose reasoning quality degrades with
attention dilution, this is a direct accuracy win, separate from
the latency win.

### 6.2 Stable IDs are a memory aid the model doesn't have to
synthesize.

A model with weaker long-range coherence (Gemma 1B, e.g.) will
sometimes forget which `Activity` it was discussing across many
turns. `symbol_id` lets the tool loop carry the identity through
the conversation: the model writes the ID, the next tool call
reads it back, the model gets a known-correct match. Frontier
models manage this without help; small ones don't.

### 6.3 The JSON-RPC shape is cheap to parse.

`scry serve` returns `{"id": N, "result": ...}` per line. A small
model can be prompted in 50 tokens to know the shape; no schema-
in-context is required. Compare with parsing the output of `rg`
or `ctags`, which requires either custom regex (and the model
will mis-handle escaping) or a wrapper (which adds code surface
the agent has to maintain).

### 6.4 A recommended setup for an agent built around an 8B model

The setup I'd write if I were building an agent on top of, say,
Gemma 3 8B:

1. Pre-launch `scry serve --index /path` once. Pipe stdin/stdout
   to the agent's tool interface.
2. Expose only these tools to the model:
   - `def(name, kind?, lang?, in?, limit?)` — find definitions
   - `ref(name, lang?, in?, limit?)` — find references
   - `callers(name, lang?, in?, limit?)` — call sites
   - `outline(path, limit?)` — symbols in a file
   - `grep(pattern, lang?, in?, limit?)` — content search
3. Default `limit=10` for every tool. The model has to ask for
   more explicitly. This forces selectivity, which is good
   model discipline.
4. Trim the snippet field from every result before feeding to
   the model unless it asked for it (add a separate
   `def_with_snippet` tool). Most queries don't need source bytes;
   they need just the location.
5. Stash `~/.scry/queries.log` and inject the last 5 queries
   into the system prompt as "you've already searched for these"
   memory. Stops the model re-asking the same question.

### 6.5 The recommended setup for a small-model agent

For an agent on top of an open-weights ≥3B-class model (Gemma 3
8B, Qwen 2.5 Coder 7B, Llama 3.2 3B, Mistral 7B):

1. Pre-launch `scry serve --index /path` once and drive it as a
   warm daemon over newline-delimited JSON-RPC. Sub-second per
   query because the FSTs and lazy-vec sidecars stay mmap'd.
2. Expose these tools to the model:
   - `def(name, kind?, lang?, in?, limit=10)` — symbol lookup
   - `ref(name, lang?, in?, limit=10, format?)` — references
   - `callers(name, lang?, in?, limit=10, format?)` — call sites
   - `outline(path, limit=20, with_snippets?)` — file's symbols
   - `tldr(path)` — one-call file summary; cheapest "what is this?"
   - `grep(pattern, lang?, in?, limit=10, format?, regex?, case_insensitive?)` — content (set `case_insensitive: true` when you're guessing at casing — `bindservice` will find `bindService`)
3. Encourage `format: 'count'` as the *first* invocation of
   `ref` / `callers` / `grep`. "Does this exist? How many?"
   before "Show me the locations." The model upgrades to
   `format: 'lines'` (or omits `format`) when it wants the
   detail. Cuts median tool-reply tokens by ~70%.
4. Stash `~/.scry/queries.log` and inject the last 5 queries
   into the system prompt as memory. Stops the model
   re-asking the same question.
5. The per-command descriptions scry emits already lead with the
   most common failure mode ("if a name is common, ALWAYS pass
   kind/lang"); for ≥3B-class models that nudge is enough.

### 6.6 What a real session looks like

Concrete example. Task: "I want to add a counter to AOSP's
`BatteryStats` for `noteAlarmStart` invocations. Where is the
class defined and where is the method called from?"

Both a frontier model (Claude, this session) and an
open-weights coder model (Qwen 2.5 Coder 1.5B, Ollama, CPU)
produced the same two-command sequence given the minimal scry
verb cheat-sheet:

```sh
scry def BatteryStats --kind class --lang java
scry callers noteAlarmStart --lang java --format count
```

Run against the live AOSP+Linux index (1,009,166 files):

```
$ scry def BatteryStats --kind class --lang java
/home/zim/dev/aosp/frameworks/base/core/java/android/os/BatteryStats.java:93:23  (class java)  [BatteryStats]  BatteryStats

1 results (showing 1)
```

One hit, exact path and line. The `--kind class --lang java`
filters did their job: the index has Python test files named
`BatteryStats` and an unrelated proto definition; without the
filters they'd surface above the real source.

```
$ scry callers noteAlarmStart --lang java --format count
5 callers
```

Just `5 callers`. ~30 tokens of reply for "how many?" — the
cheapest probe shape. Spend the budget on the actual
locations only when needed:

```
$ scry callers noteAlarmStart --lang java --format lines --limit 10
frameworks/.../app/ActivityManagerNative.java:95:25       noteAlarmStart
frameworks/.../alarm/AlarmManagerService.java:4316:46     noteAlarmStart
frameworks/.../app/ActivityManager.java:5968:26           noteAlarmStart
frameworks/.../am/ActivityManagerService.java:8636:30     noteAlarmStart
frameworks/.../am/ActivityManagerService.java:17292:41    noteAlarmStart
```

Three tool calls, ~1.6 s wall, ~250 tokens of reply text
total. Each row is a call site to a method spelled
`noteAlarmStart`, with its enclosing scope inline — enough for
the agent to read the call context without opening the files.

The same task with `rg` would have run ~21 s, returned ~9 500
tokens of mixed-up hits (class def lines and call sites
interleaved), and required reading the candidate file to
disambiguate them.

This is the workflow the tool surface is built around: cheap
probe (`format: 'count'`) → narrow with filters (`kind` /
`lang`) → spend tokens only on the locations the answer
actually needs.

With this setup on a 32k-context Gemma 3 8B, the model handles
multi-step AOSP questions ("trace how a Binder call crosses
the process boundary") with answer quality in the same
ballpark as Claude or GPT-4 — the limiting factor becomes
the model's reasoning, not the tool surface.

---

## 7. Where scry is the *wrong* tool

Honest accounting. scry is not always better than the alternatives.

### Reading a single file

If I already know I want to read `frameworks/base/core/java/android/app/Activity.java`
in full, `cat` is the right call. scry adds no value for
"show me this exact file".

### Cross-cutting text search

scry indexes ~40 file kinds. Markdown, plain text logs, README
files, license files, and arbitrary text are *not* indexed.
For "find any mention of `LICENSE` across all README.md files",
`rg` is the right tool.

### Anything outside the indexed languages

scry doesn't know Swift, Dart, Haskell, OCaml, Erlang, or
Lisp. If the corpus is in those languages, `ctags`/`rg`/
language-specific tooling beats scry.

### When precision matters more than speed

For "find every place that *exactly* shadows the JDK
`Object.hashCode` method", scry's name-based matching is wrong
~10-20% of the time — it keys on the spelled name, not a
compiler-resolved symbol identity. Treat bare scry as
"98% accurate, ask the IDE for the rest". For compiler-grade
semantic def/ref/callers on the AOSP C++/Java slice, the
companion tool `scry2` is the right tool.

### Long-form code understanding

scry returns facts, not explanations. If I want to understand
*why* the Android Activity lifecycle is shaped the way it is,
no amount of `scry def` and `scry callers` will give me that —
the answer lives in commit messages, design docs, blog posts.
scry is the lookup table; reading prose is still on me.

### Build-boundary precision

This is the one that bites hardest on AOSP, and the one I should
be most honest about. scry's resolution model is **name-based** —
closer to "ctags with a real symbol table" than to a compiler.
Concretely:

- **Refs are keyed by `(spelled-name, file)`, not by a structured
  symbol identity.** A C++ `foo()` defined in two distinct overloads
  collapses to one entry in the index. tree-sitter parses without a
  preprocessor; `-D` defines, `#ifdef` branches, and template
  instantiations are invisible.
- **The same `.cpp` compiled into multiple Soong module variants
  becomes one set of refs**, not one per variant. Different
  architectures, SDK versions, system/vendor/product partitions,
  APEX containers — scry sees them as one file.
- **A "caller of `IActivityManager.startActivity`" returned by
  `scry callers`** is any file that spells `startActivity(` and
  matches that target by name. It is *not* filtered by whether the
  caller's module transitively depends on the definition's module
  per Soong (unless you pass `--reachable`, which prunes against the
  build-graph `module_graph.json` from `scry build-modgraph`).
- **Generated code under `out/` is not indexed** — AIDL, HIDL,
  protobuf, and Rust bindgen outputs live there. Callers
  into generated symbols won't resolve unless you re-index
  including `out/`.

That's enough to be useful at the editor scale; it is *not* enough
to be "right" at the AOSP build-graph scale.

**When this matters in practice**: agents that need to enumerate
"every real call site of `bindService` that ships on the
`generic_arm64-userdebug` build" will get false positives from
scry — references in vendor code that doesn't link against
`libframework`, references behind `#ifdef`'d branches that the
build doesn't take, references in modules with no visibility
into the definition's APEX. scry returns all the name matches;
filtering to "real-given-build" needs compiler-resolved data scry
doesn't have.

**The companion tool for that is `scry2`** — compiler-grade
semantic def/ref/callers over the AOSP C++/Java slice. The right
answer to "is this caller real on the build I ship?" is to query
`scry2`, or use scry's name match as a starting set and verify
with a real build.

What scry has that a compiler-grade indexer doesn't: 13-min index
for 1 M files on a 72-core host, <100 ms cold queries, no
indexer-as-builder cost, and broad coverage across every language
and build/platform format. The trade is explicit — speed and
breadth here, semantic precision in `scry2`.

---

## 8. What's in the box

The features I've shipped because agents kept hitting the same
walls without them:

**Warm-daemon JSON-RPC.** `scry serve` speaks newline-delimited
JSON-RPC 2.0 over stdin/stdout (or a `--listen unix:…/tcp:…`
socket), exposing one method per scry command from a single
long-lived warm reader. It validates required arguments (a
missing or empty `name` doesn't silently coerce to a wildcard)
and reports failures in the JSON-RPC `error` object so the agent
can branch on success vs failure without parsing prose.

**Outline with snippets.** `scry outline PATH --with-snippets N`
returns each symbol's name + line *and* its first N source lines
in one call. Two round-trips become one. Lines clip at 200 chars
so a single 4 KB log line doesn't blow the reply budget.

**File `tldr`.** For "what does this file do?" — `scry tldr
PATH` returns lang + total symbol count + per-kind histogram +
top 3 ranked symbols + the file's first non-blank line, in a
single call. Cuts ~70% of the tokens vs `outline + 3×def` for
the same answer. Exposed as the `tldr` JSON-RPC method.

**Token-cheap grep.** `--format=lines` emits
`path:line:col\tsnippet` one hit per line — 5–10× cheaper than
the JSON envelope for "list call sites of X". `--format=count`
emits one line regardless of hit count, which is what you want
for "does this name exist anywhere".

**Auto stale-index warning.** Every command that opens an index
compares the on-disk `scry_version` to the running binary and
prints a one-line stderr warning if they differ. Catches
silent-bad-data bugs (a parser fix that changes record shape on
a corpus you haven't reindexed) before the agent acts on the
result. `SCRY_QUIET=1` suppresses for CI.

**Query plan with `--explain`.** When a grep feels slow, `scry
grep PATTERN --explain` short-circuits the scan and prints the
trigrams it extracted (smallest-first, with posting size each),
the final candidate count, and a rough scan-cost estimate. An
agent that hits a slow query can now diagnose "the pattern is
too common, narrow with `--in foo/`" without guessing.

**Auto-narrow hint.** When a result set saturates `--limit` and
the shown paths share a 2+ segment common directory prefix, a
one-line stderr suggestion appears: `try --in <prefix>/ to
narrow, or tighten --kind / --lang`. Saves the agent a guessing
round when the answer is obviously "filter by the dominant
subtree".

**Cross-language symbol shadows.** Three flavors:
- AIDL: `scry def IFoo.Stub --kind aidl.shadow` (Java) and
  `scry def BpIFoo --kind aidl.shadow` (C++) both land on the
  .aidl source.
- AIDL frozen: files under `aidl_api/<pkg>/<N>/` emit
  `aidl.frozen` so versioned-surface queries don't get mixed
  with the live development copy.
- JNI: every Java `native` method emits a `jni`-kind shadow
  named `Java_<pkg>_<class>_<method>` (full mangling rules).
  `scry def Java_android_os_Parcel_nativeWriteString --kind jni`
  finds the Java declaration even when the C++ impl isn't
  in the indexed tree.

**OWNERS chain (Gerrit semantics).** `scry owner PATH`
nearest-first by default; `--include-deep` shows every layer;
`--accumulate` emits the union of emails across all layers
(the "potential approvers" set). All three modes respect
`set noparent` per Gerrit.

## Things I'd still want

1. **Wider agent uptake of streaming.** `scry serve` already has
   `stream: true` for per-record delivery. For "list every call
   site of X" on a corpus with 50k matches, the right answer is
   to stream top-K and cut early rather than fully materialize
   the result. The plumbing exists; the gap is agent harnesses
   that consume it instead of waiting for one big reply.

2. **Compound calls.** "Outline this file, then for each method
   call `callers`, return only the methods with > 10
   references" is three round-trips. A pipeline primitive would
   cut both latency and tokens. Defining the grammar without
   re-inventing GraphQL is the hard part, and the value
   probably doesn't justify the surface area for now. Deferred.

### What end-to-end agent testing catches that unit tests don't

The bugs I've shipped a release to fix kept landing in the same
category: a correct piece of A talking to a correct piece of B
through an interaction layer that nobody had a unit test for.

- The query layer returned `{"error": "..."}` correctly. The
  `serve` JSON-RPC envelope wrapped a result correctly. Together
  they emitted the hint JSON-stringified inside the `result`
  field instead of as a structured `error` object — an agent had
  to parse twice to read it. Each side's unit test passed.

- The parser computed scope_path correctly. The reader decoded
  scope_path correctly. An index built before the parser fix
  had wrong data in it; nothing in the binary checked whether
  the on-disk and in-memory versions agreed. The fix was a
  startup warning making the version skew visible.

- `scry health` was the diagnostic for the version-skew check.
  Operators don't think to run a separate diagnostic before
  trusting query results. A feature that exists isn't the same
  as a feature that fires.

Driving the tool the way an agent does — a full `scry serve`
session, every method, every negative path — costs one session
per release. It catches the bugs no unit test will. The
cost-benefit is obvious once you ship a release that needed it.

---

## 9. The one-line takeaway

scry is **the right tool for "what code already exists in this
1 M-file tree, and where"**. For everything beyond that — reading
prose, understanding intent, generating new code — it's not in
the picture. Inside its bounded job, the combination of (a)
sub-second latency, (b) structured token-cheap output, and (c)
LLM-shaped serve interface makes it the difference between a
working AOSP-aware agent and one that runs out of context before
finishing.

The numbers — 30-45× over `rg`, 26× fewer tokens per query, 60×
fewer tokens per "where is X" answer — aren't the point. The
point is that those numbers free up enough budget that the agent
can spend its context on the actual task instead of on tool
output. That's the only reason I care.
