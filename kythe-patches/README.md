# Kythe patches for scry's AOSP cross-CU Java resolution

These four patches apply against `https://github.com/kythe/kythe` master HEAD
on 2026-05-18 (commit `954bc79 release: v0.0.75 (#6220)`, which is byte-identical
to the v0.0.75 release tag). Together they let `jvm_indexer.jar` read Java 21
bytecode AND let `java_indexer.jar` resolve classpath-bytecode references when
the CompilationUnit lacks a `JavaDetails` proto extension (the AOSP norm).

Why these patches exist and how they fit into scry's pipeline is documented in
[`../docs/KYTHE_JVM_INDEXER_REBUILD.md`](../docs/KYTHE_JVM_INDEXER_REBUILD.md).

## Apply

```bash
cd /path/to/kythe   # a fresh master checkout
for p in /path/to/scry/kythe-patches/*.patch; do git apply "$p"; done
```

## Order matters

- `0001` bumps the ASM Maven dep from 9.1 → 9.7.1 (needed before any further
  changes that rely on the ASM 9 API).
- `0002` bumps `KytheClassVisitor.ASM_API_LEVEL` from `ASM7` → `ASM9`. Without
  this jvm_indexer throws on Java 17+ class file features (records etc.) even
  with the dep upgrade.
- `0003` adds the `--default_corpus` flag to `ClassFileIndexer`. Without it,
  `jvm_indexer` on raw .jar/.class inputs emits VNames with empty corpus,
  preventing FQN bridges from merging with `java_indexer`'s output.
- `0004` adds a fallback in `CompilationUnitPathFileManager` that derives
  `CLASS_PATH` from `!CLASS_PATH_JAR!`-prefixed `required_input` entries when
  the CU has no `JavaDetails`. This is the load-bearing one — without it,
  javac silently can't resolve framework.jar classes during services.core
  indexing, no MethodSymbol exists for the call sites, and no JVM-FQN bridge
  ever fires.

After applying, rebuild + drop into the release tree as documented.

## Upstream-able

All four are upstream-shaped: minimal, scope-limited, no AOSP-specific
strings, no scry-specific wiring. The bigger ones (`0003` and `0004`) add
flags / fallbacks that are useful for any Kythe consumer dealing with
classpath bytecode jars or kzips emitted without `JavaDetails`. If we ever
get bandwidth, send them upstream.
