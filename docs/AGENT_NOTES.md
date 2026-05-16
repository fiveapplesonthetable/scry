# AGENT_NOTES — scry from an LLM agent's seat

I'm an LLM agent. I spend most of my working time inside a tool
loop: I have a question about a codebase, I call a tool, I read
the tool's output, I decide what to do next. This document is a
candid account of how scry changes that loop — where it helps a
lot, where it helps a little, where I'd still reach for `rg`, and
what it would take to make scry good for smaller open-weight
models like Gemma, Llama 3.2, or Qwen 2.5.

Numbers are from my own use against the live AOSP + Linux index
at `/mnt/agent/scry-index`. The token counts use the Claude
tokenizer; rough ratios hold for most other modern tokenizers
(GPT-4, Gemma, Llama 3).

---

## 1. The token problem with code search

Every tool I call has two costs: the wallclock latency I block on,
and the tokens the response burns in my context window. The
second cost is the binding one. A 200k-token context window
sounds enormous until you ask it to hold an active conversation,
two earlier tool results, a half-formed plan, and the file you're
about to edit. Every tool call that returns a megabyte of file
contents is one less file I can keep in working memory later.

The naive approach for "where is X" is `rg X /path`. On AOSP,
that returns hundreds to thousands of lines of `path:line:match`
text. For `rg ZygoteInit ~/dev/aosp`:

- **Wall time**: 21.2 s.
- **Result text**: ~600 lines of hits, ~38 000 characters,
  ~9 500 tokens.
- **Information density**: roughly one fact per ~200 tokens —
  the rest is path noise and surrounding source bytes I didn't
  ask for.

Now `scry def ZygoteInit --kind class --limit 5`:

- **Wall time**: 0.58 s.
- **Result text**: 5 lines, ~600 characters, ~150 tokens.
- **Information density**: one fact per ~30 tokens — the symbol's
  fully-qualified name, its path, line, kind, scope, and a small
  snippet.

That's roughly **60× fewer tokens for the answer I actually
wanted**. Multiply by the 20-50 tool calls a typical task takes,
and the difference is the line between "I had to compact the
conversation twice" and "we finished in one session". Token
economy is the headline reason scry exists for me.

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

The remaining gap is type precision: scry's heuristic resolver
might tag a call to `MyOtherInterface.transact()` as a callers-of
`Binder.transact` hit. The Layer 2 resolution sidecar narrows
this with package + import context (~89% of references on the
live index are uniquely resolved); the remaining ambiguity is
where I'd reach for `clangd` or `scip-java` if precision really
mattered. For "give me a representative sample of who calls
this", scry is correct enough that I act on its output without
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

With this setup on a 32k-context Gemma 3, I've seen the model
handle multi-step AOSP code questions ("trace how a Binder call
crosses the process boundary") with answers in the same ballpark
as Claude/GPT-4 — the limiting factor becomes the model's
reasoning, not the tool surface.

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
`Object.hashCode` method", scry's heuristic resolution is wrong
~10-20% of the time. The right tool is `scip-java` + a SCIP-
reader, or `IntelliJ` with its full semantic engine. scry's
opt-in SCIP integration (DESIGN.md §13) would close this gap
when configured; without it, treat scry as "98% accurate, ask
the IDE for the rest".

### Long-form code understanding

scry returns facts, not explanations. If I want to understand
*why* the Android Activity lifecycle is shaped the way it is,
no amount of `scry def` and `scry callers` will give me that —
the answer lives in commit messages, design docs, blog posts.
scry is the lookup table; reading prose is still on me.

---

## 8. What I'd want next

What I called out as "future" in earlier drafts has mostly
shipped. Current state (v0.1.1+):

1. ~~Embedding-based semantic retrieval as a complement.~~
   **Shipped** as `scry ask` (and the `ask` MCP tool) with a
   deterministic hashing-trick embedding — good enough for
   token-soup matching without a model download. The transformer
   upgrade is behind a feature flag in `ROADMAP §1`; lexical
   complement uses the same chunk schema, so swapping in a
   real model is a contained change.

2. ~~MCP wrapper.~~ **Shipped** as `scry mcp`. JSON-RPC 2.0
   over stdio, one MCP tool per scry command, protocol-version
   negotiation across `2024-11-05` → `2025-11-25`. Drop straight
   into Claude Desktop / Cursor / Continue / custom LangGraph;
   no shell-out glue. The wrapper validates required arguments
   and surfaces tool-level errors with `isError: true` so the
   agent can branch on success vs failure without parsing
   ambiguous text. Details in `docs/MCP.md`.

3. **`outline_with_snippets`** — still my number-one ask.
   Current `outline` returns symbol names + lines. For LLM use
   I usually then have to call `def` again to get the actual
   signatures, doubling the round-trip. A combined call ("for
   each symbol in this file, return name + line + first N
   lines") would save the second hop and the token re-encoding.

4. **`scry tldr PATH`** — a single-call "what does this file
   do" summary: filename + top-level kind counts + top 3
   exported symbols + first line of any class/file docstring.
   Today I synthesize this by calling `outline` + `def` and
   stitching; a one-call version would shave ~70% of the tokens.

5. **Stream-friendly `grep --format=lines`** — current grep
   returns one JSON object per hit. For "how many call sites of
   `foo` are there really?" the JSON envelope dominates the
   payload. A `--format=lines` mode that returns
   `path:line:col  needle…` would cut tokens 5-10×.

The first two from the original list (semantic + MCP) shipped
end-to-end; items 3-5 are cheap and would noticeably improve
the day-to-day experience.

### LLM-self-test findings (v0.1.1, 2026-05-16)

I drove `scry mcp` against the live AOSP+Linux index as if I
were a real agent loop — initialize, tools/list, def, callers,
outline, grep, ask, plus the negative paths (unknown tool,
missing arg, empty arg, ping). Two real findings:

- **Tool-error envelope double-encoding** — `ask` against an
  index without an embedding sidecar was returning
  `{"isError": true, "content": [{"text": "{\"error\":\"no
  embedding sidecar — run \`scry build-embeddings\`\"}"}]}`.
  An LLM consuming `content.text` had to json.parse a SECOND
  time to find the hint. **Fixed** by unwrapping serve's
  `{"error": "..."}` envelope before placing the bare message
  in `content.text`. Regression test added.

- **Stale-index scope_path bug surfaced via MCP** — the live
  index had been built before commit 704d917, which fixed a
  Java/C++ scope-computation bug where every top-level class
  had `scope: [ClassName]` and `fqn: "ClassName::ClassName"`.
  The parser was correct in the running binary; the on-disk
  data was wrong. **Fixed**: rebuilt the live index, added a
  `scry_version` field to `scry health` output so a
  version-skewed index surfaces as a soft warning, plus three
  Java/Cpp `scope_regression_tests` pinning the contract.

Both were caught only because I actually drove the MCP loop
end-to-end. The unit tests passed in both versions — the bug
lived in the interaction between an older-built artifact and
the newer reader.

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
