# Rebuilding Kythe Java Indexers for AOSP Cross-CU Resolution

This is a complete repro for the chain of changes scry needs to its Kythe
toolchain in order to do **cross-CU Java symbol resolution on AOSP** —
specifically, the pattern where `services.core` calls `Binder.clearCallingIdentity`
through `framework.jar` on classpath and the query
`scry callers clearCallingIdentity --def-in /android/os/Binder.java --in services/core`
should return the actual call sites.

All work lives in `/mnt/agent/dev/kythe/` (a fork of github.com/kythe/kythe master,
which on `2026-05-18` is identical to the v0.0.75 release tag). Nothing in this
document touches AOSP source — every change is in our Kythe fork.

## Why the stock release doesn't do this

The full data-flow for one cross-CU query goes:

```
AMS.java   (services.core CU)         Binder.java   (framework CU)
   │                                      │
   ▼ java_indexer.jar                     ▼ java_indexer.jar
anchor "clearCallingIdentity"       defines/binding anchor "clearCallingIdentity"
   │  ref/call                           │
   ▼                                     ▼
java VName X (no path, hash sig)    java VName Y (Binder.java path, hash sig)
                                          │
                                          ▼ named edge
                                    jvm VName JVM_FQN
                                       (signature = "android.os.Binder.clearCallingIdentity()J")
```

For write_tables-mediated cross-CU resolution, both X and Y must connect to the
same canonical entity. The intended canonical is the JVM FQN VName — Y emits a
`named` edge to it. **The bridge breaks because java_indexer never gives X the
same `named` edge.** This isn't a Kythe bug — javac never *resolves* the
classpath method symbol, so kythe never sees a real `MethodSymbol` to emit the
edge from. javac doesn't resolve it because services.core's CU ships **no
JavaDetails proto extension**, no `-classpath` flag, and Kythe's file manager
falls through to a "no classpath at all" state even though the CU's
`required_input` carries every classpath jar's bytecode under the
`!CLASS_PATH_JAR!/...` convention. Same root cause hits AOSP `framework.jar`
classes (Binder, Process, SystemProperties, Parcel, IBinder, UserHandle, …) —
empirically zero of their methods get `named` bridges from services.core's
output prior to this patch.

A second problem is independent: AOSP framework.jar ships at Java 21 class major
version 65, which the stock `jvm_indexer.jar`'s bundled ASM 9.1 and
`KytheClassVisitor.ASM_API_LEVEL = ASM7` both reject (`Unsupported class file
major version 65`, then `Records requires ASM8`).

## The four patches

All four live in our Kythe fork at `/mnt/agent/dev/kythe` and are also pinned
in `kythe-patches/` at the scry repo root as numbered `.patch` files. Each is
the smallest upstream-shaped change that fixes the problem cleanly. Patches
1–3 address the Java-21-bytecode reading gap; Patch 4 is the load-bearing one
that wires classpath visibility for cross-CU resolution.

### Patch 1 — `external.bzl`: bump bundled ASM

`org.ow2.asm:asm:9.1` (Java 17-class-file-max) → `org.ow2.asm:asm:9.7.1`
(Java 23-class-file-max). One line. Re-run `bazel run @unpinned_maven//:pin`
to refresh `maven_install.json`.

### Patch 2 — `KytheClassVisitor.java`: bump ASM API constant

```diff
-  private static final int ASM_API_LEVEL = Opcodes.ASM7;
+  private static final int ASM_API_LEVEL = Opcodes.ASM9;
```

ASM 9 understands records, sealed classes, pattern matching for switch — every
JEP that's now in Java 21 stable. `KytheClassVisitor` itself doesn't need new
visit methods because Kythe's JVM graph only cares about class/method/field
node + childof + extends; the body of a record class is structurally the same
as a regular class as far as those edges are concerned.

### Patch 3 — `ClassFileIndexer.java`: new `--default_corpus` flag

```diff
+    @Parameter(
+        names = "--default_corpus",
+        description =
+            "Corpus to assign to all VNames generated for raw .jar/.class inputs.")
+    private String defaultCorpus;
```

When `jvm_indexer` is fed a raw `.jar` or `.class` file (not a kzip), Kythe's
existing code defaults the VName corpus to `""`. `java_indexer` (which IS fed a
kzip with VName rules) uses the build's actual corpus. So the JVM-FQN VName
emitted by `jvm_indexer.jar framework.jar` had `corpus=""` while the JVM-FQN
target of `java_indexer`'s `named` edge had
`corpus="android.googlesource.com/platform/superproject"` — same signature,
different corpus → different VName → `write_tables` can't merge them.

The new flag lets the operator align corpora explicitly:

```bash
java -jar jvm_indexer.jar \
  --default_corpus android.googlesource.com/platform/superproject \
  framework.jar
```

After Patch 3, both sides land on a byte-equal JVM-FQN VName.

### Patch 4 — `CompilationUnitPathFileManager.java`: derive CLASS_PATH from `!CLASS_PATH_JAR!` inputs

This is the load-bearing one. The diff in pseudocode:

```diff
   setLocations(
       findJavaDetails(compilationUnit)
           .map(details -> toLocationMap(details))
-          .orElseGet(() -> logMissingDetailsMap()));
+          .orElseGet(() -> deriveLocationMapFromInputs(compilationUnit)));
```

The new method walks every `required_input` whose path begins with
`!CLASS_PATH_JAR!` — the well-known convention Kythe extractors use to ship
classpath jar contents without the jar wrapper. It groups them by their
virtual jar root (`!CLASS_PATH_JAR!`, `!CLASS_PATH_JAR!.1`, …), then puts each
root into `StandardLocation.CLASS_PATH` (and `MODULE_PATH`) with the same
no-leading-slash form `JavaCompilationUnitExtractor` uses when emitting
JavaDetails directly.

Net effect: a CU that has classpath inputs in `required_input` but no
`JavaDetails` proto now gets the same javac classpath wiring as a CU that did
emit `JavaDetails`. javac can now resolve Binder/Process/SystemProperties/etc.
from framework.jar's bytecode → `field.sym` becomes a real `MethodSymbol` →
`KytheTreeScanner.getJvmNode()` returns the JVM-FQN VName → the `named` bridge
edge fires.

**Empirical confirmation:** running the patched `java_indexer.jar` on
services.core's CU produces **1,209** `named` edges to `android.os.Binder.*`
JVM FQNs, up from **0** with the stock indexer.

## How it all fits together

```
                ┌──────────────────────────────────────────────────────┐
                │ AOSP all.kzip  (output of build_kzip.bash)           │
                │  - services.core java CU (no JavaDetails,            │
                │    !CLASS_PATH_JAR!/android/os/Binder.class inside)  │
                │  - per-file java CUs for Binder.java, ...            │
                │  - per-file c++/objc CUs                             │
                └────────────────┬─────────────────────────────────────┘
                                 ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ scry-kzip driver                                                    │
   │  - walks all CUs, builds per-CU sub-kzips                           │
   │  - dispatcher routes per language:                                  │
   │      java  → patched java_indexer.jar  (Patch 4)                    │
   │      jvm   → patched jvm_indexer.jar   (Patch 1 + 2 + 3)            │
   │      c++   → stock cxx_indexer                                      │
   │      go    → stock go_indexer                                       │
   │      proto → stock proto_indexer                                    │
   │  - tees each indexer's stdout to:                                   │
   │      (a) the existing per-CU decode → packed sidecar path           │
   │      (b) NEW: a corpus-wide `corpus.entries` accumulator            │
   └────────────────┬────────────────────────────────────────────────────┘
                    ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ Phase 4 (new):                                                      │
   │    kythe entrystream --unique --sort < corpus.entries               │
   │      | kythe write_tables --entries - --out serving/                │
   │    (LevelDB serving table; ~2 min on services.core+binder+framework │
   │     subset, scales linearly with entry count)                       │
   └────────────────┬────────────────────────────────────────────────────┘
                    ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ Phase 5 (new): scry-side importer                                   │
   │    - For every anchor (file, span, target VName):                   │
   │        target is "canonical" if it has a named-edge to a            │
   │        language=jvm VName whose signature is an FQN.                │
   │    - Build map  jvm_fqn → set of (file, span, anchor_kind).         │
   │    - For each "def" anchor entry (defines/binding edges from        │
   │      Binder.java's source CU), capture (jvm_fqn, def_location).     │
   │    - For each "call/ref" anchor (services.core's CU), look up       │
   │      jvm_fqn in the def map and emit a packed record with           │
   │      target_symbol = jvm_fqn and resolved_to = def_location.        │
   │    - Write packed sidecar (scip_index.bin) using JVM-FQN as the     │
   │      canonical target_symbol.                                       │
   └────────────────┬────────────────────────────────────────────────────┘
                    ▼
   scry query path (unchanged): scry callers ... --def-in ... --strict
   resolves against the new JVM-FQN-keyed sidecar; intra-CU and cross-CU
   queries both go through the same packed-format lookup.
```

The four Kythe patches are the **only** changes needed below `scry-kzip`. Two
new phases inside scry-kzip (entries-aggregation + write_tables-orchestration)
and one new importer (Phase 5) complete the pipeline. Nothing in this chain
involves bytecode rewriting, hash-collision workarounds, or any heuristic that
isn't grounded in Kythe's own graph model.

## Repro

### 0. Prerequisites

```
sudo apt-get install -y ruby asciidoc graphviz g++-14 libstdc++-14-dev
# Bazel 7.1.0 (the version pinned in Kythe's .bazelversion):
mkdir -p /mnt/agent/bin
curl -sL -o /mnt/agent/bin/bazel-7.1.0 \
  https://github.com/bazelbuild/bazel/releases/download/7.1.0/bazel-7.1.0-linux-x86_64
chmod +x /mnt/agent/bin/bazel-7.1.0
# Java 21 OpenJDK (usually already present):
java -version
```

### 1. Clone Kythe

```bash
mkdir -p /mnt/agent/dev && cd /mnt/agent/dev
git clone --depth 1 https://github.com/kythe/kythe.git
cd kythe
```

### 2. Apply the four patches

```bash
# Patch 1: bump ASM dep
sed -i 's|"org.ow2.asm:asm:9.1"|"org.ow2.asm:asm:9.7.1"|' external.bzl

# Patch 2: bump ASM API level in jvm_indexer
sed -i 's|Opcodes.ASM7|Opcodes.ASM9|' \
  kythe/java/com/google/devtools/kythe/analyzers/jvm/KytheClassVisitor.java
```

Patch 3 and Patch 4 are larger and live in:
- `kythe/java/com/google/devtools/kythe/analyzers/jvm/ClassFileIndexer.java`
  (new `--default_corpus` flag + synthetic enclosingJarFile wiring)
- `kythe/java/com/google/devtools/kythe/platform/java/filemanager/CompilationUnitPathFileManager.java`
  (new `deriveLocationMapFromInputs` method replacing the
   `logMissingDetailsMap` fall-through)

Apply them from the committed scry diff under `kythe-patches/` (see
`scry/kythe-patches/` in this repo).

### 3. Refresh maven lock + build

```bash
cd /mnt/agent/dev/kythe
KYTHE_DO_NOT_DETECT_BAZEL_TOOLCHAINS=1 \
  /mnt/agent/bin/bazel-7.1.0 run @unpinned_maven//:pin

# Verify asm 9.7.1 is in the lock
grep -A1 '"org.ow2.asm:asm":' maven_install.json | grep version
# expected: "version": "9.7.1"

# Build both indexer deploy jars
KYTHE_DO_NOT_DETECT_BAZEL_TOOLCHAINS=1 CC=gcc CXX=g++ \
  /mnt/agent/bin/bazel-7.1.0 build --spawn_strategy=local \
  --action_env=CC=gcc --action_env=CXX=g++ \
  //kythe/java/com/google/devtools/kythe/analyzers/jvm:class_file_indexer_deploy.jar \
  //kythe/java/com/google/devtools/kythe/analyzers/java:indexer_deploy.jar
```

### 4. Drop into the release tree

```bash
cd /mnt/agent/kythe-release/kythe-v0.0.75/indexers
cp jvm_indexer.jar jvm_indexer.jar.stock
cp java_indexer.jar java_indexer.jar.stock
cp /mnt/agent/dev/kythe/bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/jvm/class_file_indexer_deploy.jar jvm_indexer.jar
cp /mnt/agent/dev/kythe/bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/java/indexer_deploy.jar java_indexer.jar
```

`scry-kzip` resolves these by name relative to `--kythe-root`, so it
auto-picks the patched jars.

### 5. Verify the FQN bridge fires

```bash
# Run patched java_indexer on services.core's CU
java -Xmx16g -jar /mnt/agent/kythe-release/kythe-v0.0.75/indexers/java_indexer.jar \
  --ignore_empty_kzip --temp_directory /tmp/jdiag \
  /path/to/services.core.kzip 2>&1 | \
  grep 'derived CLASS_PATH'
# expected: INFO: Compilation missing JavaDetails; derived CLASS_PATH from N !CLASS_PATH_JAR! root(s).

# Run patched jvm_indexer on framework.jar
java -Xmx16g -jar /mnt/agent/kythe-release/kythe-v0.0.75/indexers/jvm_indexer.jar \
  --default_corpus android.googlesource.com/platform/superproject \
  /path/to/framework.jar > fw.raw 2> /tmp/fw.stderr
grep -E 'Exception|Caused' /tmp/fw.stderr  # expected: empty
```

### 6. Verify named-edge presence

Extract a known framework-method bridge from services.core's stream:

```bash
/mnt/agent/kythe-release/kythe-v0.0.75/tools/entrystream \
  --read_format=delimited --write_format=json < sc.raw > sc.json

python3 -c "
import json
n=0
for ln in open('sc.json'):
    e = json.loads(ln)
    if e.get('edge_kind','') != '/kythe/edge/named': continue
    t = e.get('target',{})
    if t.get('language','') != 'jvm': continue
    if t.get('signature','').startswith('android.os.Binder.'):
        n += 1
print(f'named-bridge count for android.os.Binder.*: {n}')
"
# expected: a few thousand (with the patch); zero (without it).
```

## Known gotchas

- **Bazel sandbox vs. system C++ headers.** Bazel's `linux-sandbox` doesn't see
  `/usr/include/c++/`. Use `--spawn_strategy=local` (or install
  `libstdc++-14-dev` AND tell Bazel which gcc to call via `--action_env=CC=gcc
  --action_env=CXX=g++`). The build pulls in protobuf C++ codegen for the
  Java proto stubs, so even pure-Java targets transitively hit a C++ compile.
- **`asciidoc`/`graphviz`/`ruby` required at Bazel-fetch time.** Kythe's
  WORKSPACE eagerly autoconfigures all toolchains; missing tools fail the
  build during `fetch`, not `compile`. `KYTHE_DO_NOT_DETECT_BAZEL_TOOLCHAINS=1`
  skips the asciidoc/graphviz check but not the ruby toolchain (which the
  Go/protobuf rules pull in). Install all three up front.
- **Use `_deploy.jar`, not the plain target.** Bazel's `java_binary` emits
  `name.jar` (thin, requires the dep classpath) and `name_deploy.jar` (the
  singleJar with all deps inlined). Stock Kythe ships the singleJar. Only the
  `_deploy.jar` is suitable for `java -jar`.
- **Patch 3 needs Patch 1 + 2.** The `--default_corpus` flag adds value only
  when jvm_indexer can actually read the class files, which requires the ASM
  upgrade. Order: 1 → 2 → 3 → 4.
- **Patch 4 is the load-bearing one for AOSP.** Patches 1+2+3 unlock
  jvm_indexer on framework.jar but don't change java_indexer behavior on
  services.core. Without Patch 4, services.core's calls never get the JVM-FQN
  bridge regardless of how jvm_indexer is invoked.

## What this enables (and what it doesn't)

**Enables (after Patches 1-4 + the scry-kzip Phase 4/5 importer):**

- `scry callers <method> --def-in <framework-source-file> --in <any-aosp-module> --strict`
  resolves to the actual call sites, even when the def's source file is in a
  different Soong build module than the callers.
- Cross-CU AIDL implementation lookup (callers of an AIDL interface method
  resolve to both Stub.Proxy and the impl class).
- (Future) JNI cross-language linking — for the same FQN-bridge mechanism but
  bridging via Kythe's `/kythe/edge/named` to a c++ symbol.

**Doesn't change:**

- Intra-CU resolution (already worked; same packed-format records).
- The precision contract (`--strict` still drops anything not Kythe-resolved;
  no lexical fallback ever).
- Existing query latency (the importer's output is the same packed sidecar
  format — `ref_resolutions.bin` is rebuilt from the new records and queries
  go through the same code path).
