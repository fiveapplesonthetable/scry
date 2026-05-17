//! Reader for the `clang_usrs.bin` sidecar produced by
//! `scry-clang-index`. Optional; missing → `ClangUsrIndex::open`
//! returns `Ok(None)`. Present → mmaps the packed sidecar (see
//! [`crate::precision_packed`] for the on-disk layout) and exposes
//! `(abs_path, byte_offset)` lookups.
//!
//! The on-disk shape is owned by `precision_packed`; this module
//! invokes [`crate::precision_sidecar_wrapper!`] to attach the
//! USR-flavoured public API names (`usr_for`, `usr_for_window`,
//! `usr_count`, `iter_usrs`) so callers keep their C / C++ / ObjC
//! naming.

crate::precision_sidecar_wrapper! {
    index = ClangUsrIndex,
    lookup = ByFileLookup,
    record = UsrRecord,
    record_doc = "One symbol decl / ref / call site, keyed by absolute path + byte \
                   offset within the file. Writer-side carrier — the on-disk shape \
                   interns `usr_table[usr_id]` into a single symbol blob.",
    symid = usr_id,
    kind = kind,
    kind_doc = "0 = decl, 1 = ref, 2 = call.",
    lookup_fn = usr_for_window,
    exact_fn = usr_for,
    count_fn = usr_count,
    iter_fn = iter_usrs,
    table_arg = usr_table,
    magic = precision_packed::MAGIC_CLANG_USR,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-empty-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        write(&tmp, &[], &[]).unwrap();
        let opened = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        assert_eq!(opened.len(), 0);
        assert_eq!(opened.usr_count(), 0);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn lookup_by_loc() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-lookup-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let usr_table = vec!["c:@F@foo".to_string(), "c:@F@bar".to_string()];
        let records = vec![
            UsrRecord { abs_path: "/x/y.cc".to_string(), byte_offset: 42, usr_id: 0, kind: 0 },
            UsrRecord { abs_path: "/x/z.cc".to_string(), byte_offset: 100, usr_id: 1, kind: 2 },
        ];
        write(&tmp, &usr_table, &records).unwrap();
        let idx = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        assert_eq!(idx.usr_for("/x/y.cc", 42), Some("c:@F@foo"));
        assert_eq!(idx.usr_for("/x/z.cc", 100), Some("c:@F@bar"));
        assert_eq!(idx.usr_for("/x/y.cc", 99), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn missing_returns_none() {
        let nope = crate::scry_tmp_dir().join(format!(
            "scry-cusr-missing-{}", std::process::id(),
        ));
        let _ = std::fs::remove_file(&nope);
        assert!(ClangUsrIndex::open(&nope).unwrap().is_none());
    }

    #[test]
    fn window_lookup_finds_closest_within_range() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-win-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let usr_table = vec!["c:@S@A".into(), "c:@S@B".into(), "c:@S@C".into()];
        let records = vec![
            UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 100, usr_id: 0, kind: 0 },
            UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 110, usr_id: 1, kind: 0 },
            UsrRecord { abs_path: "/x/a.cc".into(), byte_offset: 500, usr_id: 2, kind: 0 },
        ];
        write(&tmp, &usr_table, &records).unwrap();
        let idx = ClangUsrIndex::open(&tmp).unwrap().unwrap();
        // Exact hits still work via window.
        assert_eq!(idx.usr_for_window("/x/a.cc", 100, 64), Some("c:@S@A"));
        // Within window: 105 is closer to 100 than 110.
        assert_eq!(idx.usr_for_window("/x/a.cc", 105, 64), Some("c:@S@A"));
        // 107 is closer to 110 than 100 (dist 3 vs 7).
        assert_eq!(idx.usr_for_window("/x/a.cc", 107, 64), Some("c:@S@B"));
        // Outside window from 500: nothing.
        assert_eq!(idx.usr_for_window("/x/a.cc", 200, 64), None);
        // Unknown path → None.
        assert_eq!(idx.usr_for_window("/x/none.cc", 100, 64), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn rejects_wrong_magic() {
        let tmp = crate::scry_tmp_dir().join(format!("scry-cusr-badmagic-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        // Write a SCIP-magic sidecar, try to open it as a USR sidecar → error.
        precision_packed::write(&tmp, precision_packed::MAGIC_SCIP, &[]).unwrap();
        let err = ClangUsrIndex::open(&tmp).unwrap_err();
        assert!(format!("{err}").contains("bad magic"));
        std::fs::remove_file(&tmp).ok();
    }
}
