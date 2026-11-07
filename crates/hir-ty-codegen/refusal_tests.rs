use super::*;
use hir_def::expr_store::Body;
use ide_db::RootDatabase;
use test_fixture::WithFixture;

struct Symbols;
impl SymbolResolver for Symbols {
    fn function(&self, _: FunctionId) -> Option<String> {
        None
    }
    fn static_(&self, _: StaticId) -> Option<String> {
        None
    }
    fn path(&self, _: &str) -> Option<String> {
        None
    }
    fn item(&self, _: hir_def::ModuleDefId) -> Option<String> {
        None
    }
    fn source_path(&self, _: base_db::FileId) -> Option<String> {
        None
    }
    fn stable_crate_id(&self, _: Crate) -> Option<u64> {
        None
    }
}

fn refusal(noise_first: bool, name: &str, nested: bool) -> (String, String, Vec<u8>) {
    let db = RootDatabase::with_files(
        r#"
//- /noise.rs crate:noise
pub fn noise() {}
//- /main.rs crate:fixture
pub fn alpha() { missing(); }
pub fn beta() { missing(); }
"#,
    );
    hir::attach_db(&db, || {
        let crates = base_db::all_crates(&db);
        let krate =
            |name: &str| *crates.iter().find(|id| fncache::crate_name(&db, **id) == name).unwrap();
        if noise_first {
            let map = hir_def::nameres::crate_def_map(&db, krate("noise"));
            for (_, module) in map.modules(&db) {
                let _ = module.scope(&db).declarations().count();
            }
        }
        let func = hir_def::nameres::crate_def_map(&db, krate("fixture"))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations())
            .find_map(|item| match item {
                hir_def::ModuleDefId::FunctionId(id)
                    if FunctionSignature::of(&db, id).name.as_str() == name =>
                {
                    Some(id)
                }
                _ => None,
            })
            .unwrap();
        let body = Body::of(&db, func.into());
        let expr = body
            .exprs()
            .find_map(|(_, expr)| match expr {
                hir_def::hir::Expr::Call { callee, .. } => Some(*callee),
                _ => None,
            })
            .unwrap();
        let mut error = hir_ty::mir::MirLowerError::MissingFunctionDefinition(func.into(), expr);
        if nested {
            error = hir_ty::mir::MirLowerError::ConstEvalError(
                "VALUE".into(),
                Box::new(hir_ty::consteval::ConstEvalError::MirEvalError(
                    hir_ty::mir::MirEvalError::InFunction(
                        Box::new(hir_ty::mir::MirEvalError::MirLowerError(func, error)),
                        vec![(either::Either::Left(func), MirSpan::ExprId(expr), func.into())],
                    ),
                )),
            );
        }
        let rendered = mir_error(&db, &error).to_string();
        let (rejected, on_demand) = (FxHashMap::default(), FxHashSet::default());
        let mut cg = ObjectCodegen::new(
            &db,
            &Symbols,
            &rejected,
            OwnStatics::none(),
            &on_demand,
            None,
            false,
        )
        .unwrap();
        cg.module
            .declare_function("fixture_refused", Linkage::Import, &cg.module.make_signature())
            .unwrap();
        cg.define_refused("fixture_refused", &rendered).unwrap();
        let object = cg.finish().unwrap().0;
        (format!("{func:?}"), rendered, object)
    })
}

#[test]
fn refusal_bytes_ignore_database_interning_order() {
    let (a_id, a_reason, a_object) = refusal(false, "alpha", false);
    let (b_id, b_reason, b_object) = refusal(true, "alpha", false);
    assert_ne!(a_id, b_id, "fixture must change actual function interning order");
    assert_eq!(a_reason, b_reason);
    assert_eq!(a_object, b_object);
    assert!(a_reason.contains("alpha") && a_reason.contains("missing"));
}

#[test]
fn refusal_bytes_distinguish_function_identity() {
    let (_, alpha, a_object) = refusal(false, "alpha", false);
    let (_, beta, b_object) = refusal(false, "beta", false);
    assert_ne!(alpha, beta);
    assert_ne!(a_object, b_object);
}

#[test]
fn refusal_bytes_ignore_ids_in_nested_consteval_errors() {
    let (a_id, a_reason, a_object) = refusal(false, "alpha", true);
    let (b_id, b_reason, b_object) = refusal(true, "alpha", true);
    assert_ne!(a_id, b_id);
    assert_eq!(a_reason, b_reason);
    assert_eq!(a_object, b_object);
    assert!(a_reason.contains("VALUE") && a_reason.contains("In function fixture::alpha"));
}
