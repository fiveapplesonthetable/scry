# scry — operations

Production knowledge for running `scry index` against a full AOSP +
Linux corpus without OOM-killing the host. This doc exists because the
indexing pipeline has many knobs and the right values depend on host
RAM, core count, and how pathological the corpus is.

Companion to `docs/DESIGN.md` (which describes the system) and the
`scripts/run_index.sh` runner (which encodes the production config).


## TL;DR — recipe for the 240G/72-core host

```sh
systemd-run --user --unit=scry-index --collect \
  -p MemoryMax=60G \
  -p MemorySwapMax=0 \
  -p Restart=on-failure -p RestartSec=3 \
  -p StartLimitBurst=500 -p StartLimitIntervalSec=0 \
  -p StandardOutput=append:/mnt/agent/scry-index.log \
  -p StandardError=append:/mnt/agent/scry-index.log \
  /mnt/agent/scry/scripts/run_index.sh
```

That's it. The runner script applies the knobs below; systemd applies
the cgroup; `--resume` + `Restart=on-failure` form a loop that grinds
through any OOM until the corpus finalizes.

Observe via:
- `systemctl --user status scry-index` — live process / memory / cgroup
- `tail -f /mnt/agent/scry-index.log` — every batch, slow file, jemalloc heartbeat
- `journalctl --user -u scry-index` — start/stop/OOM events
- `cat /mnt/agent/scry-index.tmp/progress.json` — checkpoint watermark
- hourly status email via `/mnt/agent/scry/scripts/status_email.sh` cron


## The knob table

Every value below is a CLI flag or env var. Nothing is hardcoded in the
binary; the runner is a thin shell wrapper that makes the choices
explicit.

| Knob | Default | What it bounds | When to tune |
|---|---|---|---|
| `--workers N` | all cores | concurrent in-flight tree-sitter parses | lower if you see OOMs on heavy-parse files (Cpp, generated Java) |
| `--flush-bytes N` | 1024 (MiB) | accumulated record bytes per batch | lower if record memory dominates (refs-heavy corpora) |
| `--flush-every N` | 50000 (files) | hard file cap per batch | lower if a batch's worth of in-flight ASTs is too large |
| `--mem-cap N` | 0 (off) | soft backpressure ceiling, GiB | set to 60-70% of cgroup ceiling — backpressure throttles at 80% of this |
| `--big-file-bytes N` | 65536 | files > N route serial | lower if mid-size files OOM concurrently |
| `--max-file-bytes N` | 100 MiB | refuse to open files larger | only files larger than this are binaries (.git packs, prebuilt jars) |
| `SCRY_PARSE_TIMEOUT_MS` | 0 (unlimited) | per-file tree-sitter parse | set to 60000 in production; pathological grammars then fail loudly per-file instead of OOM-killing |
| `MALLOC_CONF` | (jemalloc default) | how aggressively the allocator returns pages | set `dirty_decay_ms:100,muzzy_decay_ms:100,narenas:1` so RSS tracks workload |
| `--build-trigrams` | off | builds a trigram index (3-byte n-grams) alongside the symbol index — enables 100× faster `scry grep` for literal patterns. Doubles disk usage. Recommended for production indexes used by LLM agents. |


## What --resume does

`--resume` reads `<index>.tmp/progress.json` (a per-batch atomic
checkpoint) and skips files whose `file_id < watermark`. The walker is
sorted by relpath so file_ids are deterministic across runs.

Lifecycle of one batch:
1. Parallel parse of the batch's files (records pushed into a
   `parking_lot::Mutex` sink).
2. `flush_*_chunk` writes the sink to disk (`symbols.chunk.NNNNNN.bin`
   etc) — bincode'd Vec, plus a sorted (name, idx) side-file for the
   k-way merge at finalize.
3. `progress.json` is rewritten via tmp-then-rename (atomic) with the
   new watermark and chunk counts.

If we crash between step 2 and step 3, the chunks are on disk but
progress.json doesn't know about them. On resume, scry detects the
mismatch (`on_disk > saved`) and deletes those trailing chunks as
orphans before continuing — so no batch's records can be double-counted
in the final index.

If the source tree shifts between runs (files added/removed), the
deterministic file_id assignment can drift. The resume guard warns
loudly but no longer fails on count drift alone — small drifts in AOSP
are normal. Path mismatch still hard-fails (different roots entirely).


## What the cgroup gets us

`MemoryMax=60G` on a 157G host: the kernel OOM-kills the unit if it
crosses 60 GiB. systemd then sees `Result: 'oom-kill'`, schedules a
restart via `Restart=on-failure`, and the runner is reinvoked with
`--resume`. Worst case per OOM is one batch (≤ `--flush-every` files)
redone.

Why not just rely on the soft `--mem-cap` backpressure? Two gaps:
1. The heartbeat thread polls jemalloc's `stats.allocated` every 100 ms.
   Tree-sitter can transiently allocate gigabytes in <100 ms.
2. Backpressure pauses NEW pickups but cannot pause in-flight parses.
   Once a worker has started parsing a pathological file, the AST keeps
   growing until parse() returns.

The cgroup is the hard backstop for both gaps. The resume loop converts
the backstop into "slower forward progress" instead of "lost work".


## What SCRY_PARSE_TIMEOUT_MS gets us

`ts_parser_set_timeout_micros` lets tree-sitter abort a parse that
takes too long. When it fires, `parse()` returns `None` and scry emits:

```
[ts-TIMEOUT] /home/zim/dev/aosp/path/to/file.kt (4243 bytes) — tree-sitter parse returned None after 60012 ms (symbols query)
```

The file's symbols + refs are not added to the index. There is no
silent skip — every timeout is named in the log so you can investigate
the next morning.

Default is 0 (unlimited). The runner sets 60000 ms (60 s), which is
20-100x the legitimate parse time of even the heaviest real AOSP file.
If 60 s isn't enough, the grammar has a real bug and we want to know.

To investigate after the fact:
```sh
grep '^\[ts-' /mnt/agent/scry-index.log | sort -u
```


## Memory model — what to expect

With the default runner config:

- **Startup**: scry walks both roots (~3s for AOSP, ~0.5s for Linux),
  sorts each by relpath, allocates one Vec<RawFile> per root. ~250 MB
  baseline RSS after walks.
- **Per-batch parallel parse**: peak in-flight RAM ≈
  (workers × avg_per_file_AST) + accumulated records + walker vecs.
  For workers=8 on AOSP, expect 3-15 GiB peak per batch, 600 MB-1 GiB
  steady-state.
- **Batch flush**: drains in-RAM records to disk, returns memory to
  jemalloc. With `MALLOC_CONF` aggressive return, jemalloc's
  `dirty_decay_ms:100` returns pages within 100 ms.
- **Finalize**: external k-way merge over per-chunk sorted side-files
  to build the FST. Peak ~50 MB independent of corpus size.

If you see allocated > 80% of mem-cap for sustained periods, the soft
backpressure is engaged ("BACKPRESSURE" tag in `[jemalloc]` log line).
Workers wait at `await_memory_headroom()` for the heap to drain.

If you see frequent OOM-kills despite backpressure, the heartbeat is
losing the race to a transient burst. Either:
- lower `--workers` (smaller in-flight wave)
- lower `--big-file-bytes` (more files routed serial)
- set/lower `SCRY_PARSE_TIMEOUT_MS` (bound the worst single parse)


## Hourly status email

`scripts/status_email.sh` is a self-contained shell script that reads
the same files you'd grep by hand (`progress.json`, the journal, the
log) and emails a summary to the configured address via `msmtp`.

Install in cron:
```sh
( crontab -l 2>/dev/null ; \
  echo "0 * * * * /mnt/agent/scry/scripts/status_email.sh" ) | crontab -
```

The subject line carries enough info to read at a glance:
```
[scry] active — watermark 142000/1009166 (14.1%), 3 starts, 2 OOMs
```


## Scheduled nightly rebuild

The recommended pattern for keeping the index fresh on a long-lived
host is a single systemd timer rather than ad-hoc rebuilds. The unit
already exists (`scry-index.service` from the TL;DR recipe is reusable
by templating it as a `.service` file under `~/.config/systemd/user/`);
add a sibling `.timer` file:

```ini
# ~/.config/systemd/user/scry-index.timer
[Unit]
Description=Nightly scry index rebuild

[Timer]
# 03:17 local — off-minute so the fleet doesn't all wake at :00
OnCalendar=*-*-* 03:17:00
Persistent=true     # catch up if the host was off at fire time

[Install]
WantedBy=timers.target
```

Enable:
```sh
systemctl --user daemon-reload
systemctl --user enable --now scry-index.timer
systemctl --user list-timers --all     # confirm next-fire-time
```

`Persistent=true` matters: it makes the timer remember a missed
nightly window (the host was off / asleep / in maintenance) and
trigger the build the next time it boots, so the index can't quietly
drift weeks stale during downtime. Pair with `--incremental`
(the runner's default in `run_index.sh`) so each nightly is sub-second
on a no-change tree and bounded by the actual changeset otherwise.

The `auto_recover.sh` 5-minute cron still complements this: the timer
schedules the *intentional* rebuild; auto_recover restarts after a
crash / OOM during one.


## When the index finalizes

`scry index` exits 0. systemd marks the unit inactive. The
`<index>.tmp/` directory has been atomically renamed to `<index>/`
(prior `<index>/` moved to `<index>.old/` and then removed). The
final index contains:

```
<index>/
├── manifest.json           # version, stats, indexed-at
├── roots.bin               # Vec<RootEntry>
├── files.bin               # Vec<FileEntry>
├── symbols.bin             # Vec<SymbolRecord> (cat'd from chunks)
├── symbols_offsets.bin     # u64 byte offset per symbol (lazy reader)
├── refs.bin                # Vec<RefRecord>
├── refs_offsets.bin        # u64 byte offset per ref (lazy reader)
├── names.fst               # FST: symbol name → posting offset
├── name_postings.bin       # u32 indices into symbols.bin
├── ref_names.fst           # FST: ref name → posting offset
├── ref_postings.bin        # u32 indices into refs.bin
├── trigrams.fst            # FST: 3-byte trigram → posting offset    (--build-trigrams)
└── trigram_postings.bin    # delta+varint encoded file_id lists      (--build-trigrams)
```

## Adding optimizations to an existing index

If your index is missing offsets or trigrams (old format, or
`--build-trigrams` wasn't passed), you can retrofit them without
re-parsing:

```sh
# Lazy reader sidecars — ~30 sec at full-AOSP scale
scry build-offsets --index /mnt/agent/scry-index

# Trigram index — ~15-20 min at full-AOSP scale
scry build-trigrams --index /mnt/agent/scry-index --workers 16
```

Both are atomic — they stage into a tmp dir, then rename into the
final index. Safe to run on an index that's actively serving queries
(reader picks up the new files on next open).

Validate against real AOSP symbols:
```sh
scripts/validate.sh
```


## Troubleshooting

**"Result: 'oom-kill'" in journal but watermark advances each restart**
Normal. The resume loop is working. Per-OOM cost is one redone batch.

**Same watermark across many restarts**
A specific batch is OOMing repeatedly. Lower `--flush-every` or
`--workers`, or check the log right before the OOM for a `[slow]` or
large-record-count file.

**"resume: root[0] file count drift"**
The source tree changed between runs. The check is warn-only now —
chunks past the insertion point may reference shifted file_ids. If
queries return obviously-wrong paths, nuke `<index>.tmp/` and re-index
without `--resume`.

**"resume: root[N] path mismatch"**
You ran with different `-o` or different source roots. Resume cannot
proceed; remove the tmp dir or use a different `-o`.

**Index opens but queries return 0 hits for things you know are there**
- Confirm with `scry stats` that the language counts look plausible.
- Try `scry prefix '' --limit 5` to see ANY symbols.
- If a specific language is missing, check `[ts-TIMEOUT]` / `[ts-ABORT]`
  in the log — that language's files may have failed to parse.
- For Kotlin: scry's queries don't currently cover extension functions
  or extension properties. Known gap.

**FST queries are slow**
Names are mmapped; first query is a cold page fault, subsequent are
fast. If consistently slow, check `du -sh <index>` — the index might
have grown past what fits in page cache.
