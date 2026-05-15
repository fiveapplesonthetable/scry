# AOSP scale on this machine (snapshot 2026-05-15)

Source root: `/home/zim/dev/aosp`

## Disk

| Path                  | Size  | Indexed? |
|-----------------------|-------|----------|
| total                 | 350 G |          |
| `out/`                | 184 G | **no** (build artifacts) |
| `prebuilts/`          |  48 G | **no** (binary blobs, SDK jars) |
| `external/`           |  32 G | yes      |
| `kernel/`             | 3.9 G | yes      |
| `packages/`           | 3.3 G | yes      |
| `device/`             | 2.9 G | yes      |
| `frameworks/`         | 2.4 G | yes      |
| `cts/`                | 2.3 G | yes      |
| `toolchain/`          | 700 M | partial (sources only) |
| `system/`             | 726 M | yes      |
| `kernel/` minus build | ~3 G  | yes      |

Subtracting `out/` + `prebuilts/` leaves **~118 GB to index**.

Host disk: 466 G total, **42 G free on `/`** — too tight for a meaningful
index there. `/mnt/agent` has **1.1 TB free** — index goes here.

## File counts (excluding `out/`, `prebuilts/`, `.repo/`, `.git/`)

### Source code
| Ext   | Count   | Notes                              |
|-------|---------|------------------------------------|
| .java | 204,715 |                                    |
| .h    | 141,950 |                                    |
| .cpp  | 116,770 |                                    |
| .c    |  75,103 |                                    |
| .cc   |  47,130 |                                    |
| .py   |  38,213 |                                    |
| .kt   |  25,949 |                                    |
| .rs   |  19,666 |                                    |
| .aidl |  17,452 | AOSP IPC; cross-language pivot     |
| .hpp  |  13,754 |                                    |
| .proto|  11,587 |                                    |
| .go   |   3,739 |                                    |

### Build / project / config
| Type             | Count  | Notes                                     |
|------------------|--------|-------------------------------------------|
| `.xml`           | 89,029 | layouts, configs, manifests              |
| `.json`          | 15,680 | configs, metadata                         |
| `.bp`            | 13,715 | **Soong / Blueprint** (the build truth)   |
| AndroidManifest.xml | 4,726 | App/component manifests — special-cased |
| BUILD / BUILD.bazel | 4,502 | Bazel; AOSP partially on it              |
| `.mk`            |  4,364 | Android.mk + make includes                |
| `.cfg`           |  2,782 | aconfig / runtime configs                 |
| `.toml`          |  1,914 | crate manifests etc.                      |
| `.properties`    |  1,290 |                                           |
| `.te`            |  1,081 | **SELinux** type enforcement              |
| `.rc`            |    920 | **Android init** scripts                  |
| Kconfig          |    890 | kernel config                             |
| `.aconfig`       |    441 | **AOSP feature flags** (high-value)       |
| `.policy`        |    302 | SELinux policy                            |
| `.flags`         |    161 | compiler/linker flag fragments            |
| `.jarjar`        |      2 |                                           |

### Scripts
| Ext      | Count |
|----------|-------|
| `.sh`    | 7,615 |
| `.bash`  |    50 |

### Ownership
- OWNERS files in top 3 levels: 547; full tree probably > 5,000.

### Totals (AOSP only)
**~830,000 files indexed** once configs/scripts/build files are included
(up from ~734k counting only source code). The XML population dominates
by raw count but most are layouts/strings — we index identifiers and
references lightly (see DESIGN.md §7).

---

## Linux kernel root (added)

Path: `/mnt/agent/dev/linux` — Linux 7.0-rc7, **37 GB on disk**.

| Ext        | Count  | Notes                              |
|------------|--------|------------------------------------|
| .c         | 42,974 |                                    |
| .h         | 26,652 |                                    |
| .S         |  1,358 | assembly — symbol-only indexing    |
| .rs        |    345 | Rust-for-Linux                     |
| Kconfig    |  1,822 | option defs/deps                   |
| Makefile / Kbuild | 3,184 | shallow: `obj-$(CONFIG_*)`, `ccflags-y` |

Kernel adds **~76,000 files** to the total → combined target indexed
across AOSP + Linux: **~906,000 files** / **~155 GB of source**.

## Implication for budgets

If we average 0.5 ms parse + index per file across 72 cores:
- 734k files / 72 cores * 0.5 ms = **~5 s** raw parse wall time.
- Realistic with I/O + serialization: target **< 10 min cold full index**, **< 30 s
  incremental** (single-file or module-level change).

Index size budget: aim for **< 5% of source = < 6 GB**. Trigram index on the
identifier corpus alone will dominate; symbol + ref tables should be tiny by
comparison.
