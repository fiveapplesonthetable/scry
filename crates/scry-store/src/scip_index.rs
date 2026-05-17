//! Reader for the `scip_index.bin` sidecar produced by the SCIP
//! importer. Mirrors the shape of [`super::clang_usrs`] — both wire
//! SCIP-style symbol-identity into scry's query path, but
//! `scip_index` is fed by external SCIP tools (scip-java, gopls,
//! rust-analyzer, …) while `clang_usrs` is produced by the in-tree
//! libclang walker for C / C++ / ObjC.
//!
//! On-disk layout is owned by [`crate::precision_packed`]; this
//! module invokes [`crate::precision_sidecar_wrapper!`] to attach
//! the SCIP-flavoured method names (`symbol_for`,
//! `symbol_for_window`, `symbol_count`, `iter_symbols`).

crate::precision_sidecar_wrapper! {
    index = ScipIndex,
    lookup = ByFileSymbolLookup,
    record = ScipRecord,
    record_doc = "One occurrence (decl, ref, or write/read access) at an exact source \
                   location. Writer-side carrier; the on-disk format interns \
                   `symbol_table[symbol_id]` into a single blob. `abs_path` is the \
                   absolute path the SCIP indexer recorded (resolved against the \
                   SCIP file's `project_root` at import time). `byte_offset` is \
                   computed at import from `(line, col)` by reading the source — \
                   matches tree-sitter's `byte_start` for occurrences on the same \
                   identifier.",
    symid = symbol_id,
    kind = role,
    kind_doc = "Low 8 bits of SCIP's `symbol_roles` bitmap: `0x01` = Definition, \
                 `0x02` = Import, `0x04` = WriteAccess, `0x08` = ReadAccess, \
                 `0x10` = Generated, `0x20` = Test, `0x40` = ForwardDefinition. \
                 0 = pure reference.",
    lookup_fn = symbol_for_window,
    exact_fn = symbol_for,
    count_fn = symbol_count,
    iter_fn = iter_symbols,
    table_arg = symbol_table,
    magic = precision_packed::MAGIC_SCIP,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write(&tmp, &[], &[]).unwrap();
        let opened = ScipIndex::open(&tmp).unwrap().unwrap();
        assert!(opened.is_empty());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn lookup_exact_and_window() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-look-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let symbol_table = vec![
            "scip-java . . jvm 0 com/Foo#".to_string(),
            "scip-java . . jvm 0 com/Bar#".to_string(),
        ];
        let records = vec![
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 100, symbol_id: 0, role: 1 },
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 120, symbol_id: 1, role: 0 },
        ];
        write(&tmp, &symbol_table, &records).unwrap();
        let idx = ScipIndex::open(&tmp).unwrap().unwrap();
        // Exact.
        assert_eq!(idx.symbol_for("/x/A.java", 100), Some("scip-java . . jvm 0 com/Foo#"));
        // Window prefers closer.
        assert_eq!(idx.symbol_for_window("/x/A.java", 105, 32), Some("scip-java . . jvm 0 com/Foo#"));
        assert_eq!(idx.symbol_for_window("/x/A.java", 118, 32), Some("scip-java . . jvm 0 com/Bar#"));
        // Out of window.
        assert_eq!(idx.symbol_for_window("/x/A.java", 200, 32), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn rejects_wrong_magic() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-badmagic-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        // Write a USR-magic sidecar, try to open it as SCIP → error.
        precision_packed::write(&tmp, precision_packed::MAGIC_CLANG_USR, &[]).unwrap();
        let err = ScipIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("bad magic"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn missing_returns_none() {
        let nope = crate::scry_tmp_dir().join(format!("scry-scip-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&nope);
        assert!(ScipIndex::open(&nope).unwrap().is_none());
    }

    #[test]
    fn iter_records_round_trips() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-scip-iter-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let symbol_table = vec!["s1".into(), "s2".into(), "s3".into()];
        let records = vec![
            ScipRecord { abs_path: "/x/B.java".into(), byte_offset: 50, symbol_id: 1, role: 0 },
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 10, symbol_id: 0, role: 1 },
            ScipRecord { abs_path: "/x/A.java".into(), byte_offset: 30, symbol_id: 2, role: 2 },
        ];
        write(&tmp, &symbol_table, &records).unwrap();
        let idx = ScipIndex::open(&tmp).unwrap().unwrap();
        let collected: Vec<_> = idx
            .iter_records()
            .map(|(p, bo, s, r)| (p.to_string(), bo, s.to_string(), r))
            .collect();
        assert_eq!(collected.len(), 3);
        // Verify every input row survived (order is by path_id then byte_offset;
        // writer interns in insertion order so /x/B.java has the lower path_id).
        let pairs: std::collections::HashSet<_> = collected
            .iter()
            .map(|(p, bo, s, r)| (p.as_str(), *bo, s.as_str(), *r))
            .collect();
        assert!(pairs.contains(&("/x/B.java", 50, "s2", 0)));
        assert!(pairs.contains(&("/x/A.java", 10, "s1", 1)));
        assert!(pairs.contains(&("/x/A.java", 30, "s3", 2)));
        std::fs::remove_file(&tmp).ok();
    }
}
