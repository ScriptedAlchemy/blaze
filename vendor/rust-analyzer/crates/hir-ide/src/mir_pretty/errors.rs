//! Pretty-printing MIR and consteval errors through their shared typed renderer.

use std::fmt::Write;

use hir_ty::{
    consteval::ConstEvalError,
    db::HirDatabase,
    mir::{MirEvalError, MirLowerError},
};
use macros::extension;
use span::{FileId, TextRange};

use crate::display::{ClosureStyle, DisplayTarget, HirDisplay};

#[extension(pub trait MirLowerErrorPretty)]
impl MirLowerError<'_> {
    fn pretty_print(
        &self,
        f: &mut String,
        db: &dyn HirDatabase,
        span_formatter: impl Fn(FileId, TextRange) -> String,
        display_target: DisplayTarget,
    ) -> std::result::Result<(), std::fmt::Error> {
        writeln!(
            f,
            "{}",
            self.format_diagnostic(
                db,
                span_formatter,
                |ty| ty
                    .display(db, display_target)
                    .with_closure_style(ClosureStyle::ClosureWithId)
                    .to_string(),
                |arg| arg.display(db, display_target).to_string(),
            )
        )
    }
}

#[extension(pub trait MirEvalErrorPretty)]
impl MirEvalError<'_> {
    fn pretty_print(
        &self,
        f: &mut String,
        db: &dyn HirDatabase,
        span_formatter: impl Fn(FileId, TextRange) -> String,
        display_target: DisplayTarget,
    ) -> std::result::Result<(), std::fmt::Error> {
        writeln!(
            f,
            "{}",
            self.format_diagnostic(
                db,
                span_formatter,
                |ty| ty
                    .display(db, display_target)
                    .with_closure_style(ClosureStyle::ClosureWithId)
                    .to_string(),
                |arg| arg.display(db, display_target).to_string(),
            )
        )
    }

    fn is_panic(&self) -> Option<&str> {
        let mut err = self;
        while let MirEvalError::InFunction(e, _) = err {
            err = e;
        }
        match err {
            MirEvalError::Panic(msg) => Some(msg),
            _ => None,
        }
    }
}

#[extension(pub trait ConstEvalErrorPretty)]
impl ConstEvalError<'_> {
    fn pretty_print(
        &self,
        f: &mut String,
        db: &dyn HirDatabase,
        span_formatter: impl Fn(FileId, TextRange) -> String,
        display_target: DisplayTarget,
    ) -> std::result::Result<(), std::fmt::Error> {
        writeln!(
            f,
            "{}",
            self.format_diagnostic(
                db,
                span_formatter,
                |ty| ty
                    .display(db, display_target)
                    .with_closure_style(ClosureStyle::ClosureWithId)
                    .to_string(),
                |arg| arg.display(db, display_target).to_string(),
            )
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        next_solver::{DbInterner, Ty},
        test_db::TestDB,
    };
    use hir_def::{ModuleDefId, expr_store::Body, signatures::FunctionSignature};
    use rustc_type_ir::inherent::Ty as _;
    use test_fixture::WithFixture;

    #[test]
    fn shared_errors_keep_ide_source_locations_and_type_display() {
        let (db, _) = TestDB::with_single_file("pub fn alpha() { missing(); }");
        let krate = base_db::all_crates(&db)[0];
        let func = hir_def::nameres::crate_def_map(&db, krate)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations())
            .find_map(|item| match item {
                ModuleDefId::FunctionId(id)
                    if FunctionSignature::of(&db, id).name.as_str() == "alpha" =>
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
        let error = MirLowerError::MissingFunctionDefinition(func.into(), expr);
        let mut text = String::new();
        let target = DisplayTarget::from_crate(&db, krate);
        error.pretty_print(&mut text, &db, |_, _| "source.rs:1:18".into(), target).unwrap();
        assert!(text.contains("alpha") && text.contains("missing"), "{text}");
        assert!(text.contains("source.rs:1:18"), "{text}");
        assert!(!text.contains("FunctionId(") && !text.contains("Idx::<"), "{text}");
        crate::attach_db(&db, || {
            let interner = DbInterner::new_no_crate(&db);
            let ty = Ty::new_slice(interner, Ty::new_bool(interner));
            let error = MirEvalError::LayoutError(crate::layout::LayoutError::Unknown, ty.store());
            let mut text = String::new();
            error.pretty_print(&mut text, &db, |_, _| unreachable!(), target).unwrap();
            assert!(text.contains("[bool]"), "{text}");
        });
    }

    #[test]
    fn shared_errors_keep_nested_panic_detection() {
        let (db, _) = TestDB::with_single_file("fn main() {}");
        let target = DisplayTarget::from_crate(&db, base_db::all_crates(&db)[0]);
        let error =
            MirEvalError::InFunction(Box::new(MirEvalError::Panic("boom".into())), Vec::new());
        assert_eq!(error.is_panic(), Some("boom"));
        let mut text = String::new();
        error.pretty_print(&mut text, &db, |_, _| unreachable!(), target).unwrap();
        assert!(text.contains("boom"));
    }
}
