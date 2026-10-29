//! Whether rustc accepts a build-script compiler probe, decided for a closed fragment
//! of Rust whose every acceptance rule is checked here.
//!
//! A probe such as proc-macro2's `proc_macro_span_file.rs` asks whether the release
//! has a library API: it imports standard-library items and calls their methods. The
//! answer selects cfgs, so a wrong "yes" or "no" builds a different program than rustc.
//! IDE diagnostics are not a proof that rustc accepts a crate, so this module proves
//! acceptance only for a crate whose items and bodies stay inside a fragment where it
//! checks everything rustc's analysis would reject: name resolution of every path
//! segment, library stability (`#[stable]` on each segment and method, read from the
//! release's own library source), privacy, exact types, moves, borrows and lifetimes.
//! Anything outside the fragment is [`ProbeVerdict::Undecided`], never a guess.
//!
//! The fragment, after rustc's crate-level `cfg_attr` (only `no_std` may be gated):
//! - inner attributes `#![no_std]` or `#![cfg_attr(<name>, no_std)]`;
//! - `extern crate` of `core`, `alloc`, `std` or `proc_macro`, without rename;
//! - `use` of plain paths into those crates, without globs, renames or `self`;
//! - `fn` items without generics, attributes or qualifiers, with identifier parameters
//!   and an explicit owned return type, whose body is one expression: a parameter or
//!   an inherent method call on such an expression, with arguments of the same form.
//!
//! Methods must be stable, visible, safe, non-generic inherent methods of non-generic
//! impls whose parameter and return types are owned and free of lifetimes, so no
//! temporary borrows anything and no value escapes a borrow.

use hir_def::{
    AttrDefId,
    attrs::{AttrFlags, stability::Stability},
};
use rustc_hash::FxHashMap;
use syntax::{
    AstNode,
    ast::{self, HasArgList, HasAttrs, HasGenericArgs, HasModuleItem, HasName, HasVisibility as _},
};

use crate::{
    Access, Adt, AsAssocItem, AssocItemContainer, Crate, Function, GenericDef, GenericParam,
    HasSource, HasVisibility, Impl, Module, ModuleDef, PathResolution, Semantics,
    Type, db::HirDatabase,
};
use base_db::{CrateOrigin, LangCrateOrigin};

/// What rustc does with a probe crate, as far as this module can prove it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// rustc's analysis accepts the crate, given the named standard-library crates
    /// are installed for the target.
    Accepted { sysroot_crates: Vec<&'static str> },
    /// rustc rejects the crate with a hard error.
    Rejected(String),
    /// The crate is outside the fragment this module decides.
    Undecided(String),
}

impl Crate {
    /// Decides rustc's acceptance of this probe crate, built with `--cap-lints=allow`
    /// and without `#![feature]` (the caller rejects feature gates on stable first).
    pub fn probe_verdict(self, db: &dyn HirDatabase) -> ProbeVerdict {
        hir_ide::attach_db(db, || {
            let mut proof = Proof {
                sema: Semantics::new_dyn(db),
                db,
                krate: self,
                root: self.root_module(db),
                sysroot: Vec::new(),
                names: FxHashMap::default(),
            };
            match proof.prove() {
                Ok(()) => {
                    let mut sysroot_crates = std::mem::take(&mut proof.sysroot);
                    sysroot_crates.push("core");
                    let no_std = AttrFlags::query(db, AttrDefId::ModuleId(proof.root.id))
                        .contains(AttrFlags::IS_NO_STD);
                    if !no_std {
                        sysroot_crates.push("std");
                    }
                    sysroot_crates.sort_unstable();
                    sysroot_crates.dedup();
                    ProbeVerdict::Accepted { sysroot_crates }
                }
                Err(verdict) => verdict,
            }
        })
    }
}

fn undecided<T>(why: impl Into<String>) -> Result<T, ProbeVerdict> {
    Err(ProbeVerdict::Undecided(why.into()))
}

struct Proof<'db> {
    sema: Semantics<'db, dyn HirDatabase>,
    db: &'db dyn HirDatabase,
    krate: Crate,
    root: Module,
    sysroot: Vec<&'static str>,
    /// Every name the crate root declares or imports, in any namespace.
    names: FxHashMap<String, ()>,
}

/// How an expression's value is used by what contains it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// Moved or copied: an argument, a by-value receiver, the function's result.
    Value,
    /// Borrowed shared for the duration of one call.
    Borrow,
}

/// The parameters of one probe function and how its body uses them.
struct Params<'db> {
    types: FxHashMap<String, Type<'db>>,
    /// Per parameter: how many uses, and whether any of them moves a non-Copy value.
    uses: FxHashMap<String, (usize, bool)>,
}

impl<'db> Proof<'db> {
    fn prove(&mut self) -> Result<(), ProbeVerdict> {
        let db = self.db;
        if self.krate.modules(db).len() != 1 {
            return undecided("the probe declares modules");
        }
        let file = self.krate.base().root_file_id(db);
        if file.parse_errors(db).is_some_and(|it| !it.is_empty()) {
            return undecided("the probe does not parse");
        }
        let source = self.sema.parse(file);
        for attr in source.attrs() {
            self.crate_attr(&attr)?;
        }
        let mut functions = Vec::new();
        for item in source.items() {
            match item {
                ast::Item::ExternCrate(it) => self.extern_crate(&it)?,
                ast::Item::Use(it) => {
                    plain_item(&it)?;
                    let tree = it.use_tree().map_or_else(|| undecided("empty use"), Ok)?;
                    self.use_tree(&tree)?;
                }
                ast::Item::Fn(it) => {
                    let name = it.name().map_or_else(|| undecided("unnamed fn"), Ok)?;
                    self.declare(name.text().as_str())?;
                    functions.push(it);
                }
                _ => return undecided("the probe has items outside the decided fragment"),
            }
        }
        for function in &functions {
            self.function(function)?;
        }
        Ok(())
    }

    fn declare(&mut self, name: &str) -> Result<(), ProbeVerdict> {
        // Duplicate definitions and conflicting imports are errors in some namespace
        // combinations and not others; none are in the fragment.
        if self.names.insert(name.to_owned(), ()).is_some() {
            return undecided(format!("the name `{name}` is declared twice"));
        }
        Ok(())
    }

    fn crate_attr(&self, attr: &ast::Attr) -> Result<(), ProbeVerdict> {
        if attr.excl_token().is_none() {
            return undecided("outer attribute at the crate root");
        }
        let no_std = |meta: &ast::Meta| {
            matches!(meta, ast::Meta::PathMeta(it) if it.path().is_some_and(|path| {
                path.as_single_name_ref().is_some_and(|name| name.text() == "no_std")
            }))
        };
        match attr.meta() {
            Some(meta) if no_std(&meta) => Ok(()),
            Some(ast::Meta::CfgAttrMeta(meta)) => {
                // Either value of a single-name predicate leaves a crate in the fragment,
                // so the predicate need not be evaluated here; it must only be valid.
                let predicate = meta.cfg_predicate();
                let simple = matches!(&predicate, Some(ast::CfgPredicate::CfgAtom(atom))
                    if atom.ident_token().is_some() && atom.eq_token().is_none());
                let metas: Vec<_> = meta.metas().collect();
                if simple && !metas.is_empty() && metas.iter().all(no_std) {
                    Ok(())
                } else {
                    undecided("crate attribute outside the decided fragment")
                }
            }
            _ => undecided("crate attribute outside the decided fragment"),
        }
    }

    fn extern_crate(&mut self, item: &ast::ExternCrate) -> Result<(), ProbeVerdict> {
        plain_item(item)?;
        if item.rename().is_some() {
            return undecided("renamed extern crate");
        }
        let name = item.name_ref().map_or_else(|| undecided("unnamed extern crate"), Ok)?;
        let (name, origin) = match name.text().as_str() {
            "core" => ("core", LangCrateOrigin::Core),
            "alloc" => ("alloc", LangCrateOrigin::Alloc),
            "std" => ("std", LangCrateOrigin::Std),
            "proc_macro" => ("proc_macro", LangCrateOrigin::ProcMacro),
            _ => return undecided("extern crate outside the standard library"),
        };
        let decl = self.sema.to_def(item).map_or_else(|| undecided("extern crate"), Ok)?;
        let krate = decl.resolved_crate(self.db).map_or_else(|| undecided("extern crate"), Ok)?;
        if krate.origin(self.db) != CrateOrigin::Lang(origin) {
            return undecided("extern crate is not the release's standard library crate");
        }
        self.require_stable(ModuleDef::Module(krate.root_module(self.db)), name)?;
        self.declare(name)?;
        self.sysroot.push(name);
        Ok(())
    }

    fn use_tree(&mut self, tree: &ast::UseTree) -> Result<(), ProbeVerdict> {
        if tree.star_token().is_some() || tree.rename().is_some() {
            return undecided("glob or renamed import");
        }
        if let Some(list) = tree.use_tree_list() {
            if let Some(prefix) = tree.path() {
                self.path(&prefix, false)?;
            }
            for tree in list.use_trees() {
                self.use_tree(&tree)?;
            }
            return Ok(());
        }
        let path = tree.path().map_or_else(|| undecided("empty import"), Ok)?;
        let leaf = self.path(&path, false)?;
        if matches!(leaf, ModuleDef::BuiltinType(_)) {
            return undecided("import of a builtin type");
        }
        let segment = path.segment().and_then(|it| it.name_ref());
        let name = segment.map_or_else(|| undecided("import without a name"), Ok)?;
        self.declare(name.text().as_str())
    }

    /// Resolves every segment of `path` to a stable, visible standard-library item.
    /// Only the final segment of a type path may carry generic arguments.
    fn path(&self, path: &ast::Path, generic_tail: bool) -> Result<ModuleDef, ProbeVerdict> {
        if let Some(qualifier) = path.qualifier() {
            // Only the named item's own mark is certain to be checked; an unstable
            // module on the way is not proof of an error.
            match self.path(&qualifier, false) {
                Ok(ModuleDef::Module(_)) => {}
                Ok(_) => return undecided("path through an item that is not a module"),
                Err(ProbeVerdict::Rejected(why)) => return undecided(why),
                Err(other) => return Err(other),
            }
        } else if path.coloncolon_token().is_some() {
            return undecided("global path");
        }
        let segment = path.segment().map_or_else(|| undecided("path segment"), Ok)?;
        if !matches!(segment.kind(), Some(ast::PathSegmentKind::Name(_))) {
            return undecided("path segment that is not a name");
        }
        if segment.generic_arg_list().is_some() && !generic_tail
            || segment.parenthesized_arg_list().is_some()
            || segment.ret_type().is_some()
        {
            return undecided("generic arguments in a path segment");
        }
        let text = segment.syntax().text().to_string();
        let def = match self.sema.resolve_path(path) {
            Some(PathResolution::Def(def)) => def,
            _ => return undecided(format!("`{text}` does not resolve to an item")),
        };
        self.require_stable(def, &text)?;
        Ok(def)
    }

    /// The item is a visible item of the standard library marked `#[stable]`. An item
    /// marked hard-unstable rejects the probe: its stability is checked on every path
    /// that names it (E0658), and the probe enables no feature.
    fn require_stable(&self, def: ModuleDef, text: &str) -> Result<(), ProbeVerdict> {
        let db = self.db;
        let owner = match def {
            ModuleDef::BuiltinType(_) => return Ok(()),
            ModuleDef::Module(it) => AttrDefId::ModuleId(it.id),
            ModuleDef::Adt(it) => AttrDefId::AdtId(it.into()),
            _ => return undecided(format!("`{text}` is not a module or a type")),
        };
        let Some(module) = def.module(db) else { return undecided("item without a module") };
        if !matches!(module.krate(db).origin(db), CrateOrigin::Lang(_)) {
            return undecided(format!("`{text}` is not a standard library item"));
        }
        let visible = match def {
            // A crate root is reachable from every crate that depends on it.
            ModuleDef::Module(it) if it.is_crate_root(db) => true,
            _ => def.is_visible_from(db, self.root),
        };
        if !visible {
            return undecided(format!("`{text}` is not visible from the probe"));
        }
        match Stability::query(db, owner) {
            Stability::Stable => Ok(()),
            Stability::Unstable { soft: false } => {
                Err(ProbeVerdict::Rejected(format!("use of unstable library item `{text}`")))
            }
            Stability::Unstable { soft: true } | Stability::Unmarked => {
                undecided(format!("`{text}` has no stable mark"))
            }
        }
    }

    fn function(&self, item: &ast::Fn) -> Result<(), ProbeVerdict> {
        let db = self.db;
        plain_item(item)?;
        if item.generic_param_list().is_some()
            || item.where_clause().is_some()
            || item.const_token().is_some()
            || item.async_token().is_some()
            || item.unsafe_token().is_some()
            || item.safe_token().is_some()
            || item.abi().is_some()
            || item.default_token().is_some()
            || item.gen_token().is_some()
        {
            return undecided("fn with generics or qualifiers");
        }
        let def: Function = self.sema.to_def(item).map_or_else(|| undecided("fn"), Ok)?;
        let list = item.param_list().map_or_else(|| undecided("fn without parameters"), Ok)?;
        if list.self_param().is_some() {
            return undecided("free fn with a self parameter");
        }
        let mut params = Params { types: FxHashMap::default(), uses: FxHashMap::default() };
        for (param, hir) in list.params().zip(def.params_without_self(db)) {
            if param.attrs().next().is_some() || param.dotdotdot_token().is_some() {
                return undecided("parameter attributes or variadics");
            }
            let Some(ast::Pat::IdentPat(pat)) = param.pat() else {
                return undecided("parameter pattern outside the decided fragment");
            };
            if pat.ref_token().is_some() || pat.mut_token().is_some() || pat.pat().is_some() {
                return undecided("parameter pattern outside the decided fragment");
            }
            let name = pat.name().map_or_else(|| undecided("unnamed parameter"), Ok)?;
            let ty = param.ty().map_or_else(|| undecided("parameter without a type"), Ok)?;
            self.signature_type(&ty, true)?;
            if !self.param_type(hir.ty()) {
                return undecided("parameter type outside the decided fragment");
            }
            let name = name.text().to_string();
            if params.types.insert(name, hir.ty().clone()).is_some() {
                return undecided("parameter bound twice");
            }
        }
        if list.params().count() != def.params_without_self(db).len() {
            return undecided("parameter count");
        }
        let ret = item.ret_type().and_then(|it| it.ty());
        let ret = ret.map_or_else(|| undecided("fn without a return type"), Ok)?;
        self.signature_type(&ret, false)?;
        let ret_ty = def.ret_type(db);
        if !owned(db, &ret_ty) {
            return undecided("return type outside the decided fragment");
        }
        let body = item.body().map_or_else(|| undecided("fn without a body"), Ok)?;
        if body.attrs().next().is_some()
            || body.modifier().is_some()
            || body.label().is_some()
            || body.stmt_list().is_none_or(|it| it.statements().next().is_some())
        {
            return undecided("fn body is not a single expression");
        }
        let tail = body.tail_expr().map_or_else(|| undecided("fn body without a value"), Ok)?;
        let ty = self.expr(&tail, Use::Value, def, &mut params)?;
        if ty != ret_ty {
            return Err(ProbeVerdict::Undecided("the body's type is not the return type".into()));
        }
        for (name, (count, moved)) in &params.uses {
            if *moved && *count > 1 {
                return undecided(format!("parameter `{name}` is moved and used again"));
            }
        }
        Ok(())
    }

    /// A type written in a probe signature: a path to a stable standard-library type or
    /// builtin, with only type arguments, each of them owned; or `()`; or, at the top of
    /// a parameter type, a reference without a named lifetime.
    fn signature_type(&self, ty: &ast::Type, reference: bool) -> Result<(), ProbeVerdict> {
        match ty {
            ast::Type::RefType(it) if reference && it.lifetime().is_none() => {
                let inner = it.ty().map_or_else(|| undecided("reference type"), Ok)?;
                self.signature_type(&inner, false)
            }
            ast::Type::TupleType(it) if it.fields().next().is_none() => Ok(()),
            ast::Type::PathType(it) => {
                let path = it.path().map_or_else(|| undecided("path type"), Ok)?;
                let def = self.path(&path, true)?;
                let args = path.segment().and_then(|it| it.generic_arg_list());
                let args: Vec<_> = args.iter().flat_map(|it| it.generic_args()).collect();
                let adt = match def {
                    ModuleDef::BuiltinType(it) if args.is_empty() && !it.is_str() => return Ok(()),
                    ModuleDef::Adt(it) => it,
                    _ => return undecided("type outside the decided fragment"),
                };
                let params = adt_type_params(self.db, adt)?;
                if args.len() > params.len() {
                    return undecided("too many type arguments");
                }
                for (index, bounded) in params.iter().enumerate() {
                    match args.get(index) {
                        Some(ast::GenericArg::TypeArg(arg)) if !bounded => {
                            let arg = arg.ty().map_or_else(|| undecided("type argument"), Ok)?;
                            self.signature_type(&arg, false)?;
                        }
                        // A defaulted parameter's default is well-formed in the library.
                        None => {}
                        _ => return undecided("type argument outside the decided fragment"),
                    }
                }
                Ok(())
            }
            _ => undecided("type outside the decided fragment"),
        }
    }

    /// A parameter type: owned, or one shared or mutable reference to an owned type.
    fn param_type(&self, ty: &Type<'db>) -> bool {
        match ty.as_reference() {
            Some((inner, _)) => owned(self.db, &inner),
            None => owned(self.db, ty),
        }
    }

    /// Checks a body expression and returns its type, which has no coercion applied.
    fn expr(
        &self,
        expr: &ast::Expr,
        usage: Use,
        caller: Function,
        params: &mut Params<'db>,
    ) -> Result<Type<'db>, ProbeVerdict> {
        let db = self.db;
        let info = self.sema.type_of_expr(expr).map_or_else(|| undecided("untyped"), Ok)?;
        if info.adjusted.is_some() && usage == Use::Value {
            return undecided("coerced expression");
        }
        let ty = info.original;
        if ty.contains_unknown() {
            return undecided("expression of unknown type");
        }
        match expr {
            ast::Expr::PathExpr(it) => {
                let path = it.path().map_or_else(|| undecided("path"), Ok)?;
                if path.qualifier().is_some() || path.coloncolon_token().is_some() {
                    return undecided("qualified value path");
                }
                let segment = path.segment().and_then(|it| it.name_ref());
                let name = segment.map_or_else(|| undecided("path"), Ok)?.text().to_string();
                if path.segment().is_some_and(|it| it.generic_arg_list().is_some()) {
                    return undecided("value path with generic arguments");
                }
                let Some(declared) = params.types.get(&name) else {
                    return undecided(format!("`{name}` is not a parameter"));
                };
                match self.sema.resolve_path(&path) {
                    Some(PathResolution::Local(local)) if local.is_param(db) => {}
                    _ => return undecided(format!("`{name}` does not resolve to its parameter")),
                }
                if *declared != ty {
                    return undecided("parameter's type differs from its use");
                }
                let moves = usage == Use::Value && !ty.is_reference() && !ty.is_copy(db);
                let entry = params.uses.entry(name).or_default();
                entry.0 += 1;
                entry.1 |= moves;
                Ok(ty)
            }
            ast::Expr::MethodCallExpr(call) => {
                self.method_call(call, caller, params)?;
                Ok(ty)
            }
            _ => undecided("expression outside the decided fragment"),
        }
    }

    fn method_call(
        &self,
        call: &ast::MethodCallExpr,
        caller: Function,
        params: &mut Params<'db>,
    ) -> Result<(), ProbeVerdict> {
        let db = self.db;
        if call.generic_arg_list().is_some() {
            return undecided("method call with generic arguments");
        }
        let name = call.name_ref().map_or_else(|| undecided("method name"), Ok)?;
        let name = name.text().to_string();
        let method = self.sema.resolve_method_call(call);
        let method = method.map_or_else(|| undecided(format!("`{name}` does not resolve")), Ok)?;
        let imp = match method.as_assoc_item(db).map(|it| it.container(db)) {
            Some(AssocItemContainer::Impl(imp)) if imp.trait_(db).is_none() => imp,
            _ => return undecided(format!("`{name}` is not an inherent method")),
        };
        if !matches!(method.module(db).krate(db).origin(db), CrateOrigin::Lang(_)) {
            return undecided(format!("`{name}` is not a standard library method"));
        }
        if !method.is_visible_from(db, self.root) {
            return undecided(format!("`{name}` is not visible from the probe"));
        }
        // rustc's method probe skips an unstable candidate when a stable one applies,
        // so an unstable pick is not proof of an error.
        let Some(owner) = method_owner(method) else { return undecided("builtin method") };
        if Stability::query(db, owner) != Stability::Stable {
            return undecided(format!("method `{name}` has no stable mark"));
        }
        self.plain_method(method, imp, caller)?;
        let receiver = call.receiver().map_or_else(|| undecided("receiver"), Ok)?;
        let self_param = method.self_param(db);
        let self_param = self_param.map_or_else(|| undecided("associated fn call"), Ok)?;
        let usage = match self_param.access(db) {
            Access::Shared => Use::Borrow,
            Access::Owned => Use::Value,
            Access::Exclusive => return undecided("method taking `&mut self`"),
        };
        if !matches!(receiver, ast::Expr::PathExpr(_) | ast::Expr::MethodCallExpr(_)) {
            return undecided("receiver outside the decided fragment");
        }
        // Autoderef and autoref adjust the receiver: that is the call, not a coercion.
        let receiver_ty = self.expr(&receiver, Use::Borrow, caller, params)?;
        if usage == Use::Value {
            let self_ty = imp.self_ty(db);
            match receiver_ty.as_reference() {
                // `(*receiver).method()`: a copy out of the borrow.
                Some((inner, _)) if inner == self_ty && self_ty.is_copy(db) => {}
                Some(_) => return undecided("by-value method on a borrowed receiver"),
                None if receiver_ty == self_ty => {
                    if !self_ty.is_copy(db)
                        && let ast::Expr::PathExpr(it) = &receiver
                        && let Some(name) = it.path().and_then(|it| it.segment()?.name_ref())
                        && let Some(entry) = params.uses.get_mut(name.text().as_str())
                    {
                        entry.1 = true;
                    }
                }
                None => return undecided("by-value method on an adjusted receiver"),
            }
        }
        let args = call.arg_list().map_or_else(|| undecided("arguments"), Ok)?;
        let args: Vec<_> = args.args().collect();
        let declared = method.params_without_self(db);
        if args.len() != declared.len() {
            return undecided("argument count");
        }
        for (arg, param) in args.iter().zip(&declared) {
            let ty = self.expr(arg, Use::Value, caller, params)?;
            if ty != *param.ty() {
                return undecided("argument type differs from the parameter");
            }
        }
        Ok(())
    }

    /// A safe, non-generic inherent method of a non-generic impl whose parameter and
    /// return types are owned and lifetime-free.
    fn plain_method(&self, method: Function, imp: Impl, caller: Function) -> Result<(), ProbeVerdict> {
        let db = self.db;
        if !GenericDef::Function(method).params(db).is_empty()
            || !GenericDef::Impl(imp).params(db).is_empty()
        {
            return undecided("generic method or impl");
        }
        let (Some(method_source), Some(impl_source)) = (method.source(db), imp.source(db)) else {
            return undecided("method without source");
        };
        if method_source.value.where_clause().is_some() || impl_source.value.where_clause().is_some()
        {
            return undecided("method or impl with a where clause");
        }
        if method.is_async(db)
            || method.is_unsafe_to_call(db, Some(caller), self.krate.edition(db))
        {
            return undecided("async or unsafe method");
        }
        if !owned(db, &method.ret_type(db))
            || !method.params_without_self(db).iter().all(|it| owned(db, it.ty()))
        {
            return undecided("method signature outside the decided fragment");
        }
        Ok(())
    }
}

/// An item with no attributes and at most plain `pub` visibility.
fn plain_item(item: &(impl HasAttrs + ast::HasVisibility)) -> Result<(), ProbeVerdict> {
    if item.attrs().next().is_some() {
        return undecided("item attributes");
    }
    if let Some(visibility) = item.visibility()
        && (visibility.in_token().is_some() || visibility.path().is_some())
    {
        return undecided("restricted visibility");
    }
    Ok(())
}

fn method_owner(method: Function) -> Option<AttrDefId> {
    match method.id {
        crate::AnyFunctionId::FunctionId(id) => Some(AttrDefId::FunctionId(id)),
        crate::AnyFunctionId::BuiltinDeriveImplMethod { .. } => None,
    }
}

/// Owned and free of lifetimes: `()`, a builtin other than `str`, or an ADT with only
/// type parameters whose arguments are owned.
fn owned(db: &dyn HirDatabase, ty: &Type<'_>) -> bool {
    if ty.contains_unknown() || ty.is_reference() {
        return false;
    }
    if ty.is_unit() {
        return true;
    }
    if let Some(builtin) = ty.as_builtin() {
        return !builtin.is_str();
    }
    let Some(adt) = ty.as_adt() else { return false };
    let only_types = GenericDef::Adt(adt)
        .params(db)
        .iter()
        .all(|it| matches!(it, GenericParam::TypeParam(_)));
    only_types && ty.type_arguments().all(|it| owned(db, &it))
}

/// For each type parameter of `adt`: whether it has bounds, which a supplied argument
/// would have to be proven to satisfy. Lifetime and const parameters are undecided.
fn adt_type_params(db: &dyn HirDatabase, adt: Adt) -> Result<Vec<bool>, ProbeVerdict> {
    let (params, where_clause) = match adt {
        Adt::Struct(it) => {
            let it = it.source(db).map_or_else(|| undecided("type source"), Ok)?.value;
            (it.generic_param_list(), it.where_clause())
        }
        Adt::Enum(it) => {
            let it = it.source(db).map_or_else(|| undecided("type source"), Ok)?.value;
            (it.generic_param_list(), it.where_clause())
        }
        Adt::Union(it) => {
            let it = it.source(db).map_or_else(|| undecided("type source"), Ok)?.value;
            (it.generic_param_list(), it.where_clause())
        }
    };
    if where_clause.is_some() {
        return undecided("type with a where clause");
    }
    let mut bounded = Vec::new();
    for param in params.iter().flat_map(|it| it.generic_params()) {
        match param {
            ast::GenericParam::TypeParam(it) => bounded.push(it.type_bound_list().is_some()),
            _ => return undecided("type with lifetime or const parameters"),
        }
    }
    Ok(bounded)
}
