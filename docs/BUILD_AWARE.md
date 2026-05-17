# Build-aware indexing

`scry build-symbols` produces the Kythe-class symbol-identity sidecars
(`clang_usrs.bin` and `scip_index.bin`) for an existing scry index.
Strict-precision queries (`callers`, `ref`, `impact`, `callgraph`,
`uses`) read these sidecars automatically; `--lexical` opts out.

For SCIP-producer command lines (one-liners per tool), see
[`SCIP_PRODUCERS.md`](SCIP_PRODUCERS.md). This file is about wiring
the producer outputs into scry.

## One flag per build system

`scry build-symbols` takes exactly one `--build-*` flag. Pick the one
that matches the build:

```bash
# Kythe-integrated build (AOSP via Soong, Bazel, anything that ships
# Kythe extractors). One `all.kzip` covers C++/Java/Kotlin/Rust/Go.
scry build-symbols --source-root /path/to/repo \
                   --build-kzip   /path/to/all.kzip \
                   --index /path/to/scry-index

# GN (Chromium / Fuchsia / Perfetto).
scry build-symbols --source-root /path/to/chromium \
                   --build-gn /path/to/chromium/out/Default \
                   --index /path/to/scry-index

# Linux kernel.
scry build-symbols --source-root /path/to/linux \
                   --build-kbuild /path/to/linux \
                   --index /path/to/scry-index

# CMake.
scry build-symbols --source-root /path/to/proj \
                   --build-cmake /path/to/proj/build \
                   --index /path/to/scry-index

# Cargo workspace.
scry build-symbols --source-root /path/to/repo \
                   --build-cargo \
                   --index /path/to/scry-index

# Already have a .scip from somewhere else? Import directly.
scry build-symbols --source-root /path/to/repo \
                   --scip /path/to/file.scip \
                   --index /path/to/scry-index
```

Flags are mutually exclusive except for `--with-polyglot`, which
composes with any `--build-*` to also walk for Rust / Go / TS /
Python projects under `--source-root` and run rust-analyzer / gopls /
scip-typescript / scip-python on each.

## AOSP / Soong → kzip

AOSP ships Kythe extractors wired into Soong. One command produces
`all.kzip` covering C++/Java/Kotlin/Rust:

```bash
cd ~/dev/aosp
. build/envsetup.sh
lunch aosp_cf_x86_64_phone-trunk_staging-userdebug

XREF_CORPUS=android.googlesource.com/platform/superproject \
DIST_DIR=/mnt/agent/scry-kzip \
KZIP_NAME=aosp_cf_x86_64_phone \
OUT_DIR=/mnt/agent/aosp-out \
GOCACHE=/mnt/agent/tmp/go-build-cache \
build/soong/build_kzip.bash
```

`OUT_DIR` and `GOCACHE` point at a large disk because Soong's
intermediates and the Go extractor cache fill tens of GB. `KZIP_NAME`
controls the output filename (`$DIST_DIR/$KZIP_NAME.kzip`); a UUID is
used if unset.

Then:

```bash
scry build-symbols --source-root /home/zim/dev/aosp \
                   --build-kzip   /mnt/agent/scry-kzip/aosp_cf_x86_64_phone.kzip \
                   --index /mnt/agent/scry-index
```

## compile_commands.json builds

`--build-gn`, `--build-kbuild`, and `--build-cmake` all locate (and
regenerate when missing) a `compile_commands.json`, then hand it to
libclang in-process:

| Flag             | Looks for           | Regenerates via                                              |
|------------------|---------------------|--------------------------------------------------------------|
| `--build-gn DIR` | `args.gn` in DIR    | `gn gen --export-compile-commands DIR`                       |
| `--build-kbuild DIR` | `.config` in DIR | `scripts/clang-tools/gen_compile_commands.py`               |
| `--build-cmake DIR`  | `CMakeCache.txt` | `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON …`                |

The directory passed is always the BUILD OUT dir, not the source
root. Override the `gn` / `cmake` binary with `--gn-binary` /
`--cmake-binary` if it's not on PATH.

## Polyglot (Rust / Go / TS / Python)

Add `--with-polyglot` to any `--build-*` invocation, or use
`--build-cargo` for a Rust-only workspace. Per-language project
markers picked up by the walker:

| Language    | Marker                       | Indexer            |
|-------------|------------------------------|--------------------|
| Rust        | `Cargo.toml` at workspace    | `rust-analyzer scip` |
| Go          | `go.mod`                     | `gopls scip`       |
| TypeScript  | `tsconfig.json`              | `scip-typescript`  |
| Python      | `pyproject.toml` / `setup.py`| `scip-python`      |

Each per-target `.scip` is imported into the shared `scip_index.bin`.

Skip individual languages with `--no-rust` / `--no-go` /
`--no-typescript` / `--no-python`.

## Indexer binary lookup

Per-language indexers are resolved by name, in priority order:

1. Explicit `--<name>` flag (`--gn-binary`, `--cmake-binary`,
   `--rust-analyzer`, …).
2. `SCRY_INDEXER_<NAME>` env var (uppercased, dashes → underscores):
   `SCRY_INDEXER_RUST_ANALYZER=/path`, etc.
3. First match on `$PATH`.

Missing → the bare name is invoked and the kernel's
`No such file or directory` surfaces which binary is needed.

## Install every indexer in one shot

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/fiveapplesonthetable/scry/master/scripts/install_indexers.sh)
```

What it does (idempotent):

- System packages: `libclang`, JDK, Go, Node, npm.
- `scip-typescript`, `scip-python` via npm.
- `rust-analyzer` via `rustup component add`.
- `gopls` (which ships `scip` output mode) via `go install`.
- `scip-java` launcher (GitHub release).

For Kythe extraction on AOSP, the extractor binaries are already
present in the AOSP tree (`prebuilts/build-tools/linux-x86/bin/`
plus `prebuilts/build-tools/common/framework/javac_extractor.jar`) —
no install required, `build_kzip.bash` invokes them.

## Scratch dir

Per-target `.scip` shards and any per-compilation scratch land under
`$SCRY_TMP_DIR/scry-*`. Default `/mnt/agent/tmp`. Override with
`SCRY_TMP_DIR=/some/large/volume`.

## Verifying

```bash
scry health --index /path/to/scry-index
```

Reports which sidecars are present, their record counts, and sample
symbols. A working AOSP setup looks like:

```
clang_usrs.bin: N USRs / N records
scip_index.bin: 17M+ occurrences / 1.5M+ unique symbols
```

Run a strict-precision query to confirm:

```bash
scry callers bindService --index /path/to/scry-index --limit 5
# [scry] precise (scip_index): 1981 → 37 refs (1880 uncovered TU)
# 37 hits in 0.55s
```

The `1981 → 37` line shows the precision filter dropping 95% of
lexical candidates that aren't the type-resolved callers of
`bindService`. Without sidecars, `--lexical` would return all 1981.
