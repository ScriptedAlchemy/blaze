//! Channel gating of effective crate feature attributes after cfg expansion.

use crate::{Crate, db::HirDatabase};
use hir_def::unstable_features::UnstableFeatures;

mod release;

/// Invalid feature names after effective cfg expansion and library loading.
/// Language gates come from the pinned release; library gates come from source.
impl Crate {
    pub fn invalid_features(self, db: &dyn HirDatabase) -> Vec<(&'static str, String)> {
        crate::attach_db(db, || {
            use hir_def::attrs::LibraryFeature;
            use rustc_hash::{FxHashMap, FxHashSet};

            let mut names: Vec<_> = UnstableFeatures::query(db, self.id).iter().collect();
            names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            let mut errors = Vec::new();
            let enabled = |name: &str| names.iter().any(|feature| feature.as_str() == name);
            for &(left, right) in release::INCOMPATIBLE {
                if enabled(left) && enabled(right) {
                    errors.push((
                        "feature-gate",
                        format!("features `{left}` and `{right}` are incompatible"),
                    ));
                }
            }
            for &(parent, required) in release::REQUIRED {
                if enabled(parent) {
                    for child in required.iter().filter(|child| !enabled(child)) {
                        errors.push((
                            "feature-gate",
                            format!("feature `{parent}` requires `{child}`"),
                        ));
                    }
                }
            }
            let mut globally = false;
            let data = self.id.data(db);
            let mut flags = data.codegen_flags.iter();
            while let Some(flag) = flags.next() {
                let option = if flag == "-Z" {
                    flags.next().map(String::as_str)
                } else {
                    flag.strip_prefix("-Z")
                };
                if let Some(option) = option {
                    match option {
                        "next-solver" | "next-solver=globally" => globally = true,
                        "next-solver=no" | "next-solver=coherence" => globally = false,
                        _ => {}
                    }
                }
            }
            for feature in release::NEW_SOLVER_REQUIRED {
                if enabled(feature) && !globally {
                    errors.push((
                        "feature-gate",
                        format!("feature `{feature}` requires `-Znext-solver=globally`"),
                    ));
                }
            }
            for feature in release::NEW_SOLVER_BANNED {
                if enabled(feature) && globally {
                    errors.push((
                        "feature-gate",
                        format!(
                            "feature `{feature}` is incompatible with `-Znext-solver=globally`"
                        ),
                    ));
                }
            }
            let mut libraries = Vec::new();
            for name in names {
                if release::REMOVED.binary_search(&name.as_str()).is_ok() {
                    errors.push(("E0557", format!("feature `{name}` has been removed")));
                } else if release::ACTIVE.binary_search(&name.as_str()).is_err()
                    && release::ACCEPTED.binary_search(&name.as_str()).is_err()
                {
                    libraries.push(name);
                }
            }
            if libraries.is_empty() {
                return errors;
            }
            let mut defined = FxHashSet::default();
            let mut renamed = FxHashMap::default();
            let mut removed = FxHashSet::default();
            let staged = UnstableFeatures::query(db, self.id)
                .iter()
                .any(|feature| feature.as_str() == "staged_api");
            for krate in loaded_libraries(db, self) {
                if krate == self && !staged {
                    continue;
                }
                for owner in library_owners(db, krate) {
                    for feature in hir_def::attrs::library_features(db, owner) {
                        match feature {
                            LibraryFeature::Defined(name) => {
                                defined.insert(name.clone());
                            }
                            LibraryFeature::Renamed { old, new } => {
                                renamed.insert(old.clone(), new.clone());
                            }
                            LibraryFeature::Removed(name) => {
                                removed.insert(name.clone());
                            }
                        }
                    }
                }
                if libraries.iter().all(|name| defined.contains(name)) {
                    break;
                }
            }
            for name in libraries {
                if defined.contains(&name) {
                    continue;
                }
                let (code, message) = if let Some(new) = renamed.get(&name) {
                    ("E0635", format!("feature `{name}` has been renamed to `{new}`"))
                } else if removed.contains(&name) {
                    ("E0557", format!("feature `{name}` has been removed"))
                } else {
                    ("E0635", format!("unknown feature `{name}`"))
                };
                errors.push((code, message));
            }
            errors
        })
    }
}

fn loaded_libraries(db: &dyn HirDatabase, krate: Crate) -> impl Iterator<Item = Crate> + '_ {
    let mut pending = vec![krate];
    let mut seen = rustc_hash::FxHashSet::default();
    std::iter::from_fn(move || {
        while let Some(krate) = pending.pop() {
            if seen.insert(krate) {
                let mut dependencies: Vec<_> = direct_libraries(db, krate).into_iter().collect();
                dependencies.sort_by(|a, b| {
                    b.display_name(db)
                        .map(|name| name.to_string())
                        .cmp(&a.display_name(db).map(|name| name.to_string()))
                });
                pending.extend(dependencies);
                return Some(krate);
            }
        }
        None
    })
}

fn direct_libraries(db: &dyn HirDatabase, krate: Crate) -> rustc_hash::FxHashSet<Crate> {
    use crate::{PathResolution, Semantics};
    use hir_def::{attrs::AttrFlags, nameres::crate_def_map};
    use syntax::{AstNode as _, ast};

    let mut loaded = rustc_hash::FxHashSet::from_iter([krate]);
    let map = crate_def_map(db, krate.id);
    let implicit = if map.is_no_core() {
        None
    } else if map.is_no_std() {
        Some("core")
    } else {
        Some("std")
    };
    loaded.extend(
        krate
            .dependencies(db)
            .into_iter()
            .filter(|dep| Some(dep.name.as_str()) == implicit)
            .map(|dep| dep.krate),
    );
    let sema = Semantics::new_dyn(db);
    let cfg = krate.id.cfg_options(db);
    let mut files = rustc_hash::FxHashSet::default();
    let mut pending: Vec<_> =
        krate.modules(db).into_iter().map(|module| module.definition_source_file_id(db)).collect();
    for module in krate.modules(db) {
        loaded.extend(
            module.extern_crate_decls(db).into_iter().filter_map(|decl| decl.resolved_crate(db)),
        );
    }
    while let Some(file) = pending.pop() {
        if !files.insert(file) {
            continue;
        }
        let root = sema.parse_or_expand(file);
        let mut walk = root.preorder();
        while let Some(event) = walk.next() {
            let syntax::WalkEvent::Enter(node) = event else { continue };
            if let Some(attrs) = ast::AnyHasAttrs::cast(node.clone())
                && AttrFlags::is_cfg_enabled_for(&attrs, cfg).is_err()
            {
                walk.skip_subtree();
                continue;
            }
            if let Some(path) = ast::Path::cast(node.clone())
                && let Some(PathResolution::Def(def)) = sema.resolve_path(&path)
            {
                let owner = match def {
                    crate::ModuleDef::Module(module) => Some(module.krate(db)),
                    def => def.module(db).map(|module| module.krate(db)),
                };
                loaded.extend(owner);
            }
            if let Some(call) = ast::MacroCall::cast(node)
                && let Some(expanded) = sema.expand_macro_call(&call)
            {
                pending.push(expanded.file_id);
            }
        }
    }
    loaded.remove(&krate);
    loaded
}

fn library_owners(db: &dyn HirDatabase, krate: Crate) -> rustc_hash::FxHashSet<hir_def::AttrDefId> {
    use crate::{HasAttrs as _, attrs::AttrsOwner};
    use hir_def::expr_store::ExpressionStore;
    let mut owners = rustc_hash::FxHashSet::default();
    let mut add = |owner| {
        if let AttrsOwner::AttrDef(owner) = owner {
            owners.insert(owner);
        }
    };
    let mut modules = krate.modules(db);
    for owner in crate::input_owners::expression_owners(db, krate.id) {
        for (_, map) in ExpressionStore::of(db, owner).blocks(db) {
            modules.extend(map.modules(db).map(|(id, _)| crate::Module::from(id)));
        }
    }
    for module in modules {
        add(module.attr_id(db));
        for def in module.declarations(db) {
            match def {
                crate::ModuleDef::Module(it) => add(it.attr_id(db)),
                crate::ModuleDef::Function(it) => add(it.attr_id(db)),
                crate::ModuleDef::Adt(it) => add(it.attr_id(db)),
                crate::ModuleDef::EnumVariant(it) => add(it.attr_id(db)),
                crate::ModuleDef::Const(it) => add(it.attr_id(db)),
                crate::ModuleDef::Static(it) => add(it.attr_id(db)),
                crate::ModuleDef::Trait(it) => {
                    add(it.attr_id(db));
                    for item in it.items(db) {
                        add(item.attr_id(db));
                    }
                }
                crate::ModuleDef::TypeAlias(it) => add(it.attr_id(db)),
                crate::ModuleDef::Macro(it) => add(it.attr_id(db)),
                crate::ModuleDef::BuiltinType(_) => {}
            }
        }
        for implementation in module.impl_defs(db) {
            add(implementation.attr_id(db));
            for item in implementation.items(db) {
                add(item.attr_id(db));
            }
        }
        for it in module.legacy_macros(db) {
            add(it.attr_id(db));
        }
        for it in module.extern_blocks(db) {
            add(it.attr_id(db));
        }
        for it in module.extern_crate_decls(db) {
            add(it.attr_id(db));
        }
        for it in module.use_decls(db) {
            add(it.attr_id(db));
        }
    }
    owners
}

impl Crate {
    /// Stable and beta reject `#![feature]`, including stabilized feature names.
    /// The caller supplies the release channel and RUSTC_BOOTSTRAP policy.
    pub fn feature_attributes_on_stable(self, db: &dyn HirDatabase) -> bool {
        UnstableFeatures::query(db, self.id).iter().next().is_some()
    }
}
