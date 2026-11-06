//! Where code is written, as rustc counts it: the line, column and file that `line!()`,
//! `column!()` and `file!()` expand to, which rust-analyzer expands to placeholders, and that
//! `#[track_caller]` and panics report. The evaluator of constants and the code generator
//! both read them.

use super::MirSpan;
use crate::InferBodyId;
use hir_def::expr_store::ExpressionStore;
use hir_def::{DefWithBodyId, expr_store::Body, hir::ExprId};
use hir_expand::{
    FileRange, HirFileId, InFile, MacroDefKind,
    builtin::{BuiltinFnLikeExpander, EagerExpander},
};
use rustc_hash::FxHashMap;
use syntax::{AstNode, AstToken, SyntaxElement, SyntaxKind, TextRange, ast};
use syntax::{SyntaxNode, TextSize};
use unicode_width::UnicodeWidthChar;

use crate::db::HirDatabase;

/// The range of an item or a macro call without the attributes and comments on top of it,
/// which rustc's span of it leaves out.
pub fn written_range(node: &SyntaxNode) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| {
            !matches!(it.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
                && !it.kind().is_trivia()
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// The literals of a body that `line!()`, `column!()` or `file!()` expanded to. The source map
/// changes with every edit above the body, and this keeps code without such literals from
/// changing with it.
#[salsa::tracked(returns(ref))]
pub fn location_macros<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
) -> FxHashMap<ExprId, (BuiltinFnLikeExpander, HirFileId)> {
    let (body, source_map) = (Body::of(db, owner), Body::unmoved_source_map(db, owner));
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let MacroDefKind::BuiltIn(_, expander) = file_id.macro_file()?.loc(db).def.kind else {
                return None;
            };
            matches!(
                expander,
                BuiltinFnLikeExpander::Line
                    | BuiltinFnLikeExpander::Column
                    | BuiltinFnLikeExpander::File
            )
            .then_some((id, (expander, file_id)))
        })
        .collect()
}

/// The literals of a body that `concat!` made of arguments with `line!()`, `column!()` or
/// `file!()` among them, each with the expansion it is in.
#[salsa::tracked(returns(ref))]
pub fn located_concats<'db>(
    db: &'db dyn HirDatabase,
    owner: DefWithBodyId,
) -> FxHashMap<ExprId, HirFileId> {
    let (body, source_map) = (Body::of(db, owner), Body::unmoved_source_map(db, owner));
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let loc = file_id.macro_file()?.loc(db);
            let concat =
                matches!(loc.def.kind, MacroDefKind::BuiltInEager(_, EagerExpander::Concat));
            (concat && names_location(&loc.to_node(db).value)).then_some((id, file_id))
        })
        .collect()
}

/// What `line!()`, `column!()`, `file!()` or a `concat!` of them is where it is written.
pub enum Placed {
    Number(u32),
    Text(String),
    /// A location literal was recognized, but its source could not be resolved.
    Unavailable,
}

/// A trait implementation inherits `#[track_caller]` from its declaration, as rustc's
/// `Instance::requires_caller_location` does.
#[salsa::tracked(returns(copy))]
pub fn tracks_caller<'db>(db: &'db dyn HirDatabase, function: hir_def::FunctionId) -> bool {
    use hir_def::{ItemContainerId, Lookup, attrs::AttrFlags, signatures::FunctionSignature};
    let has = |function: hir_def::FunctionId| {
        AttrFlags::query(db, function.into()).contains(AttrFlags::TRACK_CALLER)
    };
    if has(function) {
        return true;
    }
    let ItemContainerId::ImplId(implementation) = function.lookup(db).container else {
        return false;
    };
    let Some(trait_ref) = db.impl_trait(implementation) else {
        return false;
    };
    let name = &FunctionSignature::of(db, function).name;
    trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name).is_some_and(has)
}

/// The build's source spelling. Missing mappings fail closed, including before a build
/// has installed its names. Fixtures install their own VFS provider explicitly.
pub fn source_name(db: &dyn HirDatabase, file: base_db::FileId) -> Option<String> {
    use salsa::Accumulator as _;
    let name = SourceNames::try_get(db).and_then(|names| (names.provider(db).0)(db, file));
    base_db::CurrentSourceName { file, name: name.clone() }.accumulate(db);
    name
}

type SourcePaths = dyn Fn(&dyn HirDatabase, base_db::FileId) -> Option<String> + Send + Sync;

#[derive(Clone)]
struct SourceNameProvider(std::sync::Arc<SourcePaths>);

impl PartialEq for SourceNameProvider {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for SourceNameProvider {}
impl std::fmt::Debug for SourceNameProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceNameProvider")
    }
}

#[salsa::input(singleton)]
struct SourceNames {
    #[returns(ref)]
    provider: SourceNameProvider,
}

/// Installs this database's source names. Updating the provider invalidates constants that
/// read filenames, as rustc's source map belongs to its compilation context.
pub fn name_sources(
    db: &mut dyn HirDatabase,
    paths: impl Fn(&dyn HirDatabase, base_db::FileId) -> Option<String> + Send + Sync + 'static,
) {
    use salsa::{Durability, Setter};
    let paths = SourceNameProvider(std::sync::Arc::new(paths));
    match SourceNames::try_get(db) {
        Some(names) => {
            names.set_provider(db).with_durability(Durability::MEDIUM).to(paths);
        }
        None => {
            _ = SourceNames::builder(paths).durability(Durability::MEDIUM).new(db);
        }
    }
}

/// The value of the literal `expr` of `owner` when it is what `line!()`, `column!()`,
/// `file!()` or a `concat!` of them expanded to: rust-analyzer expands them to placeholders,
/// and a constant is evaluated from what it expanded them to.
pub fn placed(db: &dyn HirDatabase, owner: DefWithBodyId, expr: ExprId) -> Option<Placed> {
    let path = |file| source_name(db, file);
    if let Some(file_id) = located_concats(db, owner).get(&expr) {
        let value = || {
            let call = file_id.macro_file()?.loc(db).to_node(db);
            let mut text = String::new();
            for part in concat_parts(&call.value)? {
                match part {
                    ConcatPart::Text(part) => text.push_str(&part),
                    ConcatPart::Location(expander, range) => {
                        let (file, line, col) = location_of(db, call.file_id, range)?;
                        match expander {
                            BuiltinFnLikeExpander::Line => text.push_str(&line.to_string()),
                            BuiltinFnLikeExpander::Column => text.push_str(&col.to_string()),
                            _ => text.push_str(&path(file)?),
                        }
                    }
                }
            }
            Some(Placed::Text(text))
        };
        return Some(value().unwrap_or(Placed::Unavailable));
    }
    let &(expander, file_id) = location_macros(db, owner).get(&expr)?;
    let value = || {
        let site = written_call(db, file_id)?;
        let range = InFile::new(site.file_id, written_range(&site.value));
        let FileRange { file_id, range } = range.original_node_file_range_rooted(db);
        let (file, line, col) = location_of(db, file_id.into(), range)?;
        Some(match expander {
            BuiltinFnLikeExpander::Line => Placed::Number(line),
            BuiltinFnLikeExpander::Column => Placed::Number(col),
            _ => Placed::Text(path(file)?),
        })
    };
    Some(value().unwrap_or(Placed::Unavailable))
}

/// The macro of `name` that expands to a place in the source.
fn location_expander(name: &str) -> Option<BuiltinFnLikeExpander> {
    match name {
        "line" => Some(BuiltinFnLikeExpander::Line),
        "column" => Some(BuiltinFnLikeExpander::Column),
        "file" => Some(BuiltinFnLikeExpander::File),
        _ => None,
    }
}

/// Whether a macro call is passed a call of `line!`, `column!` or `file!`.
fn names_location(call: &SyntaxNode) -> bool {
    call.descendants_with_tokens().filter_map(|it| it.into_token()).any(|token| {
        token.kind() == SyntaxKind::IDENT
            && location_expander(token.text()).is_some()
            && token
                .next_token()
                .and_then(|next| syntax::algo::skip_trivia_token(next, syntax::Direction::Next))
                .is_some_and(|next| next.kind() == SyntaxKind::BANG)
    })
}

/// An argument of a `concat!`.
pub enum ConcatPart {
    Text(String),
    /// A call of `line!`, `column!` or `file!`, and where it is.
    Location(BuiltinFnLikeExpander, TextRange),
}

/// The arguments of a `concat!` call, when each is a literal or a call of `line!`, `column!`
/// or `file!`.
pub fn concat_parts(call: &SyntaxNode) -> Option<Vec<ConcatPart>> {
    let tree = ast::MacroCall::cast(call.clone())?.token_tree()?;
    let mut inside: Vec<SyntaxElement> =
        tree.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    inside.pop()?;
    let inside = inside.get(1..)?;
    inside
        .split(|it| it.kind() == SyntaxKind::COMMA)
        .filter(|argument| !argument.is_empty())
        .map(concat_part)
        .collect()
}

fn concat_part(argument: &[SyntaxElement]) -> Option<ConcatPart> {
    if let [SyntaxElement::Token(token)] = argument {
        let text = match token.kind() {
            SyntaxKind::STRING => ast::String::cast(token.clone())?.value().ok()?.into_owned(),
            SyntaxKind::INT_NUMBER => {
                ast::IntNumber::cast(token.clone())?.value().ok()?.to_string()
            }
            SyntaxKind::CHAR => ast::Char::cast(token.clone())?.value().ok()?.to_string(),
            SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => token.text().to_owned(),
            _ => return None,
        };
        return Some(ConcatPart::Text(text));
    }
    let [.., SyntaxElement::Token(name), bang, SyntaxElement::Node(arguments)] = argument else {
        return None;
    };
    let expander = location_expander(name.text())?;
    let empty = arguments.children_with_tokens().filter(|it| !it.kind().is_trivia()).count() == 2;
    (bang.kind() == SyntaxKind::BANG && empty).then(|| {
        let range = argument[0].text_range().cover(arguments.text_range());
        ConcatPart::Location(expander, range)
    })
}

/// The macro call an expansion comes from that is itself written out, in the source or in
/// what a macro was passed, and not by another macro.
pub fn written_call(db: &dyn HirDatabase, file_id: HirFileId) -> Option<InFile<SyntaxNode>> {
    let mut call = file_id.macro_file()?.loc(db).to_node(db);
    while let Some(outer) = call.file_id.macro_file() {
        let start = TextRange::empty(call.value.text_range().start());
        if call.file_id.span_map(db).span_for_range(start).ctx.is_root() {
            break;
        }
        call = outer.loc(db).to_node(db);
    }
    Some(call)
}

pub fn location_of(
    db: &dyn HirDatabase,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    line_and_column(db, original_location_range(db, file_id, range))
}

/// `Location` uses rustc's display columns; `column!()` uses Unicode scalar columns.
pub fn caller_location_of(
    db: &dyn HirDatabase,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let site = original_location_range(db, file_id, range);
    let (file, line, _) = line_and_column(db, site)?;
    let text = db.file_text(file);
    let starts = line_starts(db, text);
    let start = starts[line as usize - 1] as usize;
    let prefix = text.text(db).get(start..u32::from(site.range.start()) as usize)?;
    Some((file, line, caller_column(prefix)))
}

fn original_location_range(
    db: &dyn HirDatabase,
    file_id: HirFileId,
    range: TextRange,
) -> FileRange {
    // What a macro writes itself is reported from where the outermost macro is called;
    // what it is passed keeps its own place.
    let own_tokens = file_id.macro_file().is_some()
        && !file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root();
    let FileRange { file_id, range } = match written_call(db, file_id) {
        Some(call) if own_tokens => InFile::new(call.file_id, written_range(&call.value))
            .original_node_file_range_rooted(db),
        _ => InFile::new(file_id, range).original_node_file_range_rooted(db),
    };
    FileRange { file_id, range }
}

/// rustc_span::char_width: tabs occupy four columns, and bidi/control characters get a
/// visible replacement. This intentionally differs from the scalar count of `column!()`.
pub fn caller_column(prefix: &str) -> u32 {
    prefix.chars().map(column_width).sum::<usize>() as u32 + 1
}

fn column_width(ch: char) -> usize {
    match ch {
        '\t' => 4,
        '\0'..='\u{8}'
        | '\u{b}'..='\u{1f}'
        | '\u{7f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}' => 1,
        _ => UnicodeWidthChar::width(ch).unwrap_or(1),
    }
}

/// File, line and column of where a range of a file begins.
pub fn line_and_column(
    db: &dyn HirDatabase,
    FileRange { file_id, range }: FileRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let starts = line_starts(db, text);
    let at = u32::from(range.start());
    let line = starts.partition_point(|&start| start <= at);
    let line_start = *starts.get(line.checked_sub(1)?)?;
    let written = text.text(db).get(line_start as usize..at as usize)?;
    Some((file_id, line as u32, written.chars().count() as u32 + 1))
}

/// Where each line of a file begins, the first of them at 0.
#[salsa::tracked(returns(ref))]
fn line_starts(db: &dyn HirDatabase, text: base_db::FileText) -> Vec<u32> {
    let ends = text.text(db).bytes().enumerate().filter(|&(_, byte)| byte == b'\n');
    std::iter::once(0).chain(ends.map(|(at, _)| at as u32 + 1)).collect()
}

pub fn source_range<'db>(
    db: &'db dyn HirDatabase,
    owner: InferBodyId<'db>,
    span: MirSpan,
) -> Option<(HirFileId, TextRange)> {
    let source_map = match owner {
        InferBodyId::DefWithBodyId(owner) => &*Body::with_source_map(db, owner).1,
        InferBodyId::AnonConstId(owner) => {
            ExpressionStore::with_source_map(db, owner.loc(db).owner).1
        }
    };
    let source = match span {
        MirSpan::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
        // The block an `async fn` is taken to wrap its body in is not written anywhere, and
        // ends where the body does.
        MirSpan::ExprEnd(expr) => match source_map.expr_syntax(expr) {
            Ok(written) => written,
            Err(_) => {
                let InferBodyId::DefWithBodyId(owner) = owner else {
                    return None;
                };
                let hir_def::hir::Expr::Block { tail: Some(tail), .. } = Body::of(db, owner)[expr]
                else {
                    return None;
                };
                source_map.expr_syntax(tail).ok()?
            }
        },
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
            let root = source.file_id.parse_or_expand(db);
            let node = source.value.syntax_node_ptr().to_node(&root);
            reported_range(db, owner, expr, source.file_id, &node)?
        }
        _ => source.value.syntax_node_ptr().text_range(),
    };
    Some((source.file_id, range))
}

/// The part of an expression that rustc's span of it covers where it panics: a method call
/// from the method's name on, an index through `Index` from its bracket on, and any other
/// expression together with the parentheses it is written in.
pub fn reported_range(
    db: &dyn HirDatabase,
    owner: InferBodyId<'_>,
    expr: ExprId,
    file_id: HirFileId,
    node: &SyntaxNode,
) -> Option<TextRange> {
    let end = node.text_range().end();
    if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
        return Some(TextRange::new(call.name_ref()?.syntax().text_range().start(), end));
    }
    if let Some(written) = ast::IndexExpr::cast(node.clone())
        && let InferBodyId::DefWithBodyId(owner) = owner
        && let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner)[expr]
        && !crate::mir::is_builtin_index(crate::InferenceResult::of(db, owner), base, index)
    {
        return Some(TextRange::new(written.l_brack_token()?.text_range().start(), end));
    }
    Some(parenthesized(db, file_id, node))
}

/// The range of an expression with the parentheses around it. rustc's span of an expression
/// includes them when both are written in one place, and not when a macro put the
/// parentheses around what it was passed.
fn parenthesized(db: &dyn HirDatabase, file_id: HirFileId, node: &SyntaxNode) -> TextRange {
    let passed_in = |range: TextRange| {
        file_id.macro_file().is_none()
            || file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root()
    };
    let inner = passed_in(node.text_range());
    node.ancestors()
        .skip(1)
        .take_while(|it| it.kind() == SyntaxKind::PAREN_EXPR && passed_in(it.text_range()) == inner)
        .last()
        .map_or(node.text_range(), |it| it.text_range())
}
