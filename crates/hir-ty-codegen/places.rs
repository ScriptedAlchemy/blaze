//! Where a place in a body is, found from what stays the same while edits elsewhere in the
//! file move the body.
//!
//! A panic's `core::panic::Location` and a debugger's rows name a file, a line and a column.
//! Found from where the body is in its file, they are found again for every body of a file on
//! every edit of it, and most of them come out as they were. So a place is found as a
//! [`Place`] in the anchor the body is written in ([`BodySyntax`]): its row and column there,
//! from the body lowered alone ([`Body::relative`]), which an edit elsewhere leaves as it is.
//! Only where the anchor begins ([`anchor_location`]) is found from the whole file, once per
//! body, and added when the place is asked for.

use hir_def::expr_store::body::{BodySyntax, body_syntax};
use hir_expand::{MacroCallId, MacroCallKind, map_node_range_up_rooted_span};
use span::Span;

use super::*;

/// Caller locations use display columns; debugger rows use scalar columns. Keep both
/// anchored so an edit above a body does not make every source position get lowered again.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Columns {
    Scalar,
    Display,
}

impl Columns {
    fn count(self, text: &str) -> u32 {
        match self {
            Self::Scalar => text.chars().count() as u32,
            Self::Display => hir_ty::mir::location::caller_column(text) - 1,
        }
    }

    fn location(
        self,
        db: &dyn HirDatabase,
        range: FileRange,
    ) -> Option<(base_db::FileId, u32, u32)> {
        match self {
            Self::Scalar => line_and_column(db, range),
            Self::Display => {
                hir_ty::mir::location::caller_location_of(db, range.file_id.into(), range.range)
            }
        }
    }
}

/// A place in a body.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Place {
    /// In the anchor of the body: the row from the anchor's first, and the column, counted
    /// from where the anchor begins on its first row and from the start of the line on others.
    Anchored { row: u32, column: u32 },
    /// Anywhere else: a file, line and column.
    At(base_db::FileId, u32, u32),
}

/// File, line and column of a place in a body, as rustc reports them.
pub(crate) fn source_location(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let found =
        resolve(db, owner, (*source_place(db, owner, span, Columns::Display))?, Columns::Display);
    if check() {
        compare("source", owner, span, found, *source_location_in_file(db, owner, span));
    }
    found
}

/// File, line and column of a place in a body for a debugger: [`row_location_in_file`].
pub(crate) fn row_location(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    closure: Option<ExprId>,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let found = resolve(db, owner, (*row_place(db, owner, closure, span))?, Columns::Scalar);
    if check() {
        compare("row", owner, span, found, *row_location_in_file(db, owner, closure, span));
    }
    found
}

/// Whether each place is checked against the one found from where the body is in its file
/// (`RA_JIT_CHECK_PLACES`). A place that differs is printed.
fn check() -> bool {
    static CHECK: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_CHECK_PLACES").is_some());
    *CHECK
}

fn compare<T: PartialEq + fmt::Debug>(
    what: &str,
    owner: DefWithBodyId,
    span: MirSpan,
    found: T,
    was: T,
) {
    if found != was {
        eprintln!("places: {what} place of {owner:?} {span:?} is {found:?}, in its file {was:?}");
    }
}

fn resolve(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    place: Place,
    columns: Columns,
) -> Option<(base_db::FileId, u32, u32)> {
    match place {
        Place::At(file, line, column) => Some((file, line, column)),
        Place::Anchored { row, column } => {
            let (file, line, first) = (*anchor_location(db, owner, columns))?;
            Some((file, line + row, if row == 0 { first + column } else { column + 1 }))
        }
    }
}

/// File, line and column of where the anchor of the body begins.
#[salsa::tracked]
fn anchor_location(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    columns: Columns,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = body_syntax(db, owner).as_ref()?.file_id.file_id()?;
    let start = Body::anchor_offset(db, owner)?;
    columns.location(db, FileRange { file_id, range: TextRange::empty(start) })
}

/// Where each row of the anchor of the body begins.
#[salsa::tracked(returns(ref))]
fn anchor_rows(db: &dyn HirDatabase, owner: DefWithBodyId) -> Option<Box<[u32]>> {
    let text = body_syntax(db, owner).as_ref()?.text();
    let ends = text.bytes().enumerate().filter(|&(_, byte)| byte == b'\n');
    Some(std::iter::once(0).chain(ends.map(|(at, _)| at as u32 + 1)).collect())
}

/// The place of `offset` in the anchor of the body.
fn anchored(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    offset: TextSize,
    columns: Columns,
) -> Option<Place> {
    let starts = anchor_rows(db, owner).as_ref()?;
    let text = body_syntax(db, owner).as_ref()?.text();
    let at = u32::from(offset);
    let row = starts.partition_point(|&start| start <= at).checked_sub(1)?;
    let written = text.get(starts[row] as usize..at as usize)?;
    let column = columns.count(written);
    Some(Place::Anchored { row: row as u32, column })
}

/// The place of a span: in the anchor of the body when it is relative to that anchor.
fn place_of_span(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    syntax: &BodySyntax,
    span: Span,
    columns: Columns,
) -> Option<Place> {
    if span.anchor == syntax.span_map.span_for_range(TextRange::empty(0.into())).anchor {
        return anchored(db, owner, span.range.start(), columns);
    }
    let (file, line, column) = columns.location(db, hir_expand::resolve_span(db, span))?;
    Some(Place::At(file, line, column))
}

/// The node of a macro call: in the anchor of the body, or in another file.
enum CallNode {
    Anchored(SyntaxNode),
    Other(InFile<SyntaxNode>),
}

fn call_node(db: &dyn HirDatabase, syntax: &BodySyntax, call: MacroCallId) -> CallNode {
    let loc = call.loc(db);
    if let MacroCallKind::FnLike { ast_id, .. } = loc.kind
        && ast_id.file_id == syntax.file_id
        && let Some(ptr) = syntax.ast_id_map.try_get_erased(ast_id.value.erase())
    {
        return CallNode::Anchored(ptr.to_node(&syntax.root(db)));
    }
    CallNode::Other(loc.to_node(db))
}

/// The file a place in a body is written in, and where in it rustc has the place begin: in
/// the tree of [`BodySyntax::root`] when that file is the body's own.
fn source_range(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    syntax: &BodySyntax,
    span: MirSpan,
) -> Option<(HirFileId, TextRange)> {
    let source_map = &Body::relative(db, owner).as_ref()?.1;
    let source = match span {
        MirSpan::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
        // The block an `async fn` is taken to wrap its body in is not written anywhere, and
        // ends where the body does.
        MirSpan::ExprEnd(expr) => {
            match (source_map.expr_syntax(expr), &Body::of(db, owner)[expr]) {
                (Ok(written), _) => written,
                (Err(_), hir_def::hir::Expr::Block { tail: Some(tail), .. }) => {
                    source_map.expr_syntax(*tail).ok()?
                }
                (Err(_), _) => return None,
            }
        }
        MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
        MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown => return None,
    };
    if let MirSpan::ExprEnd(_) = span {
        // The brace that closes a block, which is where rustc has a scope left.
        let range = source.value.syntax_node_ptr().text_range();
        let last = range.end().checked_sub(TextSize::new(1)).filter(|it| *it >= range.start());
        return Some((source.file_id, TextRange::new(last?, range.end())));
    }
    let range = match span {
        MirSpan::ExprId(expr) => {
            let root = match source.file_id == syntax.file_id {
                true => syntax.root(db),
                false => source.file_id.parse_or_expand(db),
            };
            let node = source.value.syntax_node_ptr().to_node(&root);
            hir_ty::mir::location::reported_range(db, owner.into(), expr, source.file_id, &node)?
        }
        _ => source.value.syntax_node_ptr().text_range(),
    };
    Some((source.file_id, range))
}

/// The [`Place`] of [`source_location_in_file`].
// Nothing in its arguments is collected by salsa (no tracked struct, no interned value with
// fewer revisions), so they stay interned and are never verified.
#[salsa::tracked(revisions = usize::MAX)]
fn source_place(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    span: MirSpan,
    columns: Columns,
) -> Option<Place> {
    let Some(syntax) = body_syntax(db, owner).as_ref() else {
        let (file, line, column) = match columns {
            Columns::Display => *source_location_in_file(db, owner, span),
            Columns::Scalar => *scalar_source_location_in_file(db, owner, span),
        }?;
        return Some(Place::At(file, line, column));
    };
    let (file_id, range) = source_range(db, owner, syntax, span)?;
    place_of(db, owner, syntax, file_id, range, columns)
}

/// The [`Place`] of [`row_location_in_file`].
// Nothing in its arguments is collected by salsa (no tracked struct, no interned value with
// fewer revisions), so they stay interned and are never verified.
#[salsa::tracked(revisions = usize::MAX)]
fn row_place(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    closure: Option<ExprId>,
    span: MirSpan,
) -> Option<Place> {
    let Some(syntax) = body_syntax(db, owner).as_ref() else {
        let (file, line, column) = (*row_location_in_file(db, owner, closure, span))?;
        return Some(Place::At(file, line, column));
    };
    let (file_id, range) = source_range(db, owner, syntax, span)?;
    if file_id.macro_file().is_none() {
        return *source_place(db, owner, span, Columns::Scalar);
    }
    let source_map = &Body::relative(db, owner).as_ref()?.1;
    let code = closure.unwrap_or_else(|| Body::of(db, owner).root_expr());
    let within = match source_map.expr_syntax(code) {
        Ok(body) => {
            let start = TextRange::empty(body.value.syntax_node_ptr().text_range().start());
            match body.file_id == syntax.file_id {
                true => syntax.span_map.span_for_range(start).ctx,
                false => body.file_id.span_map(db).span_for_range(start).ctx,
            }
        }
        Err(_) => return *source_place(db, owner, span, Columns::Scalar),
    };
    written_place(db, owner, syntax, file_id, range, Some(within))
}

/// [`super::written_place`], of a macro file, in the anchor of the body where it can be.
fn written_place(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    syntax: &BodySyntax,
    file_id: HirFileId,
    range: TextRange,
    within: Option<span::SyntaxContext>,
) -> Option<Place> {
    let mut at = file_id.span_map(db).span_for_range(TextRange::empty(range.start()));
    let mut kept = at;
    while !at.ctx.is_root() && Some(at.ctx) != within {
        let Some(call) = at.ctx.outer_expn(db) else { break };
        let call = MacroCallId::from(call);
        at = match call_node(db, syntax, call) {
            CallNode::Anchored(node) => {
                syntax.span_map.span_for_range(TextRange::empty(written_range(&node).start()))
            }
            CallNode::Other(node) => {
                let range = TextRange::empty(written_range(&node.value).start());
                node.file_id.span_map(db).span_for_range(range)
            }
        };
        let loc = call.loc(db);
        let own = loc.def.krate == loc.krate && loc.def.kind.is_declarative();
        if !own {
            kept = at;
        }
    }
    place_of_span(db, owner, syntax, kept, Columns::Scalar)
}

/// [`super::written_call`], in the anchor of the body where it is written there.
fn written_call(db: &dyn HirDatabase, syntax: &BodySyntax, file_id: HirFileId) -> Option<CallNode> {
    let mut call = call_node(db, syntax, file_id.macro_file()?);
    while let CallNode::Other(node) = &call
        && let Some(outer) = node.file_id.macro_file()
    {
        let start = TextRange::empty(node.value.text_range().start());
        if node.file_id.span_map(db).span_for_range(start).ctx.is_root() {
            break;
        }
        call = call_node(db, syntax, outer);
    }
    Some(call)
}

/// [`super::location_of`], in the anchor of the body where it can be: `range` is in the tree
/// of [`BodySyntax::root`] when `file_id` is the body's file.
fn place_of(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    syntax: &BodySyntax,
    file_id: HirFileId,
    range: TextRange,
    columns: Columns,
) -> Option<Place> {
    // What a macro writes itself is reported from where the outermost macro is called;
    // what it is passed keeps its own place.
    let own_tokens = file_id.macro_file().is_some()
        && !file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root();
    if own_tokens && let Some(call) = written_call(db, syntax, file_id) {
        return match call {
            CallNode::Anchored(node) => anchored(db, owner, written_range(&node).start(), columns),
            CallNode::Other(node) => {
                place_up(db, owner, syntax, node.file_id, written_range(&node.value), columns)
            }
        };
    }
    if file_id == syntax.file_id {
        return anchored(db, owner, range.start(), columns);
    }
    place_up(db, owner, syntax, file_id, range, columns)
}

/// The place of a range of a macro file, or of a file that is not the body's.
fn place_up(
    db: &dyn HirDatabase,
    owner: DefWithBodyId,
    syntax: &BodySyntax,
    file_id: HirFileId,
    range: TextRange,
    columns: Columns,
) -> Option<Place> {
    let range = match file_id {
        HirFileId::MacroFile(file) => {
            match map_node_range_up_rooted_span(file.expansion_span_map(db), range) {
                Some(span) => return place_of_span(db, owner, syntax, span, columns),
                None => {
                    let loc = file.loc(db);
                    loc.kind.original_call_range(db, loc.krate)
                }
            }
        }
        HirFileId::FileId(file_id) => FileRange { file_id, range },
    };
    let (file, line, column) = columns.location(db, range)?;
    Some(Place::At(file, line, column))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::SourceDatabase;
    use test_fixture::WithFixture;

    #[test]
    fn cached_caller_columns_and_debugger_columns_survive_source_moves() {
        for source in [
            "pub fn fixture() {\n\tlet _ = \"界\"; target();\n}\nfn target() {}\n",
            "\tpub fn fixture() { let _ = \"界\"; target(); }\nfn target() {}\n",
        ] {
            let (mut db, file) = ide_db::RootDatabase::with_single_file(source);
            for leading in ["", "\n\n"] {
                db.set_file_text(file.file_id(&db), &format!("{leading}{source}"));
                hir::attach_db(&db, || {
                    let krate = base_db::all_crates(&db)[0];
                    let def_map = hir_def::nameres::crate_def_map(&db, krate);
                    let name = hir_expand::name::Name::new_root("fixture");
                    let owner = def_map
                        .modules(&db)
                        .find_map(|(_, module)| {
                            match module.scope(&db).get(&name).take_values()? {
                                hir_def::ModuleDefId::FunctionId(function) => Some(function.into()),
                                _ => None,
                            }
                        })
                        .unwrap();
                    let body = Body::of(&db, owner);
                    let expr = body
                        .exprs()
                        .find_map(|(id, expr)| {
                            matches!(expr, hir_def::hir::Expr::Call { .. }).then_some(id)
                        })
                        .unwrap();
                    let span = MirSpan::ExprId(expr);
                    let caller = source_location(&db, owner, span).unwrap();
                    let debugger = row_location(&db, owner, None, span).unwrap();
                    assert_eq!(Some(caller), *source_location_in_file(&db, owner, span));
                    assert_eq!(Some(debugger), *row_location_in_file(&db, owner, None, span));
                    assert_eq!(caller.1, debugger.1);
                    assert_eq!(caller.2, debugger.2 + 4); // Tab +3, wide character +1.
                });
            }
        }
    }

    #[test]
    fn production_database_source_names_fail_closed_and_invalidate_constants() {
        let (mut db, _) = ide_db::RootDatabase::with_single_file(
            r#"
//- minicore: panic_location
const CL: &core::panic::Location<'static> = core::panic::Location::caller();
const GOAL: &str = CL.file();
"#,
        );
        let goal = hir::attach_db(&db, || {
            let name = hir_expand::name::Name::new_root("GOAL");
            base_db::all_crates(&db)
                .iter()
                .find_map(|&krate| {
                    hir_def::nameres::crate_def_map(&db, krate).modules(&db).find_map(
                        |(_, module)| match module.scope(&db).get(&name).take_values()? {
                            hir_def::ModuleDefId::ConstId(id) => Some(id),
                            _ => None,
                        },
                    )
                })
                .unwrap()
        });
        hir::attach_db(&db, || assert!(db.const_eval(goal, GenericArgs::empty(), None).is_err()));
        for expected in ["remapped/one.rs", "remapped/two.rs"] {
            hir_ty::mir::location::name_sources(&mut db, move |_, _| Some(expected.to_owned()));
            hir::attach_db(&db, || {
                let value = db.const_eval(goal, GenericArgs::empty(), None).unwrap();
                let width = value.memory.len() / 2;
                let address = usize::from_le_bytes(value.memory[..width].try_into().unwrap());
                let len = usize::from_le_bytes(value.memory[width..].try_into().unwrap());
                assert_eq!(value.memory_map.get(address, len).unwrap(), expected.as_bytes());
            });
        }
    }
}
