//! Exact native settings of described source units.

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::{Context as _, ensure};
use ide_db::{RootDatabase, base_db};

pub(crate) use blaze_wire::native::Unit as NativeUnit;

static UNITS: LazyLock<Mutex<Option<Arc<BTreeMap<u32, Arc<NativeUnit>>>>>> =
    LazyLock::new(|| Mutex::new(None));

pub(crate) fn set_native_units(units: BTreeMap<u32, NativeUnit>) -> anyhow::Result<()> {
    let mut checked = BTreeMap::new();
    for (id, unit) in units {
        ensure!(
            unit.env.iter().all(|(name, value)| !name.is_empty()
                && !name.as_encoded_bytes().contains(&b'=')
                && !name.as_encoded_bytes().contains(&0)
                && !value.as_encoded_bytes().contains(&0)),
            "invalid native environment for unit {id}"
        );
        ensure!(
            unit.flags.iter().all(|flag| !flag.as_encoded_bytes().contains(&0)),
            "native unit {id} flag contains a NUL"
        );
        checked.insert(id, Arc::new(unit));
    }
    *crate::lock(&UNITS) = Some(Arc::new(checked));
    Ok(())
}

pub(crate) fn for_crate(
    db: &RootDatabase,
    krate: base_db::Crate,
) -> anyhow::Result<Option<Arc<NativeUnit>>> {
    let Some(id) = krate.extra_data(db).blaze_native_unit else { return Ok(None) };
    let units = crate::lock(&UNITS).clone().context("compiler unit has no native context")?;
    units
        .get(&id)
        .cloned()
        .map(Some)
        .with_context(|| format!("compiler native unit {id} has no captured settings"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn same_file_crate_instances_keep_their_native_environments() {
        let mut db = RootDatabase::default();
        let mut graph = base_db::CrateGraphBuilder::default();
        let mut asked = Vec::new();
        for id in 0..3 {
            let krate = graph.add_crate_root(
                vfs::FileId::from_raw(1),
                syntax::Edition::Edition2021,
                Some(base_db::CrateDisplayName::from_canonical_name("shared")),
                None,
                Default::default(),
                None,
                base_db::Env::default(),
                base_db::CrateOrigin::Local { repo: None, name: None },
                Vec::new(),
                base_db::TargetKind::Lib { is_proc_macro: false },
                triomphe::Arc::new(paths::AbsPathBuf::assert_utf8(
                    std::env::current_dir().unwrap(),
                )),
                triomphe::Arc::new(base_db::CrateWorkspaceData {
                    target: Err("fixture".into()),
                    toolchain: None,
                }),
            );
            graph.set_native_unit(krate, Some(id));
            asked.push(krate);
        }
        let mapped = graph.set_in_db(&mut db);
        assert_eq!(
            asked.iter().map(|id| mapped[id]).collect::<std::collections::HashSet<_>>().len(),
            3
        );
        let root = std::env::current_dir().unwrap();
        let mut files = vfs::file_set::FileSet::default();
        files.insert(
            vfs::FileId::from_raw(1),
            vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8(root.join("same-source.rs"))),
        );
        let mut change = ide_db::ChangeWithProcMacros::default();
        change.set_roots(vec![base_db::SourceRoot::new_local(files)]);
        change.change_file(vfs::FileId::from_raw(1), Some("pub fn value() -> u8 { 1 }".into()));
        db.apply_change(change);
        let linkage = |db: &RootDatabase| {
            let table = crate::symbols::SymbolTable::default();
            let known = hir_ty_codegen::Rejections::default();
            let listed = crate::Listed::default();
            let contents = crate::content::Contents::default();
            let mut args = crate::Args::parse(
                [root.to_str().unwrap(), "--crate", "shared", "--out", "app.o"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .unwrap();
            args.checkout = root.clone();
            let resolver = crate::Resolver::of(db, &table, &args, &known, &contents, &listed);
            asked.iter().map(|id| resolver.own_crate_id(mapped[id]).unwrap()).collect::<Vec<_>>()
        };
        let before = linkage(&db);
        assert_eq!(before.iter().collect::<std::collections::HashSet<_>>().len(), 3);
        let mut change = ide_db::ChangeWithProcMacros::default();
        change.change_file(vfs::FileId::from_raw(1), Some("pub fn value() -> u8 { 2 }".into()));
        db.apply_change(change);
        assert_eq!(linkage(&db), before, "source edits keep each instance's linkage stable");
        set_native_units(
            (0..3)
                .map(|id| {
                    (
                        id,
                        NativeUnit {
                            env: [("CARGO_PKG_NAME".into(), format!("package-{id}").into())].into(),
                            flags: vec![format!("-Copt-level={id}").into()],
                        },
                    )
                })
                .collect(),
        )
        .unwrap();
        for (id, krate) in asked.into_iter().enumerate() {
            let unit = for_crate(&db, mapped[&krate]).unwrap().unwrap();
            assert_eq!(
                unit.env[std::ffi::OsStr::new("CARGO_PKG_NAME")],
                OsString::from(format!("package-{id}"))
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn distinct_unit_identities_keep_native_values_and_absences() {
        use std::os::unix::ffi::OsStringExt;
        let value = OsString::from_vec(b"/out-\xff".to_vec());
        let a = NativeUnit {
            env: [("CARGO_PKG_NAME".into(), "a".into()), ("OUT_DIR".into(), value.clone())].into(),
            flags: vec!["-L".into(), value],
        };
        let b = NativeUnit {
            env: [("CARGO_PKG_NAME".into(), "b".into())].into(),
            flags: vec!["-Copt-level=2".into()],
        };
        let expected: BTreeMap<_, _> = [(0, a), (1, b)].into();
        let decoded =
            blaze_wire::native::decode_units(&blaze_wire::native::encode_units(&expected)).unwrap();
        assert_eq!(decoded, expected);
        assert!(!decoded[&1].env.contains_key(std::ffi::OsStr::new("OUT_DIR")));
        assert_ne!(decoded[&0].flags, decoded[&1].flags);
    }
}
