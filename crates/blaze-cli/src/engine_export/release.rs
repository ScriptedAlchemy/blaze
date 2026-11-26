use super::{
    CliError, CodeSymbol, Data, ExportCliError, Function, Target, Tier, box_err,
    materialize_positionless, owned_fmt, push_datum, statics,
};
use crate::code_store::HomeCodeStore;
use crate::engine_dist::{
    DistConfig,
    prepared::{self, Request, ResultImage, artifact::Artifact},
};
use crate::run_ids::{RunIds, Runnable};
use blaze_code::record::CodeView;
use blaze_core::Driver;
use blaze_ids::ContentId;
use blaze_ladder::backend_plan::{
    Unit, WorkSet,
    native::{
        Native,
        render::{Compiler, Renderer},
    },
};
use blaze_ladder::code_records::{CodeKey, CodeStore};
use blaze_ladder::run::make_one::NamesRecords;
use blaze_ladder::run::{
    make_one::{Maker, RecordsProgram, Typing, source_unit},
    special::ProductRoots,
};
use blaze_own::{AVec, Bump, Heap};
use bstd::mem::Frame;

#[expect(
    clippy::too_many_arguments,
    reason = "grounded source compilation and export capabilities"
)]
pub(super) fn build<'a, 'n, D: Driver, T: Typing<RecordsProgram<'n>>>(
    request: &Request<'_, 'n>,
    compiler: Compiler,
    roots: &ProductRoots<'_>,
    ids: &RunIds<'n>,
    maker: &mut Maker<'n, Runnable, T>,
    codes: &mut HomeCodeStore<'_, D>,
    dist: Option<&DistConfig>,
    frame: &Frame,
    alloc: blaze_own::Alloc<'a>,
) -> Result<(ResultImage<'a>, Option<blaze_wire::Digest>), CliError> {
    let heap = request.heap;
    let refused = |why: core::fmt::Arguments<'_>| {
        box_err(ExportCliError::Native { shown: owned_fmt(why, frame, heap.alloc()) }, heap)
    };
    let reached = source_unit::native(roots, maker, |item| ids.runnable(item), alloc)
        .map_err(|why| refused(format_args!("preparing source for {}: {why}", request.program)))?;
    let native = Native::of(&reached, request.compiler, alloc).map_err(|why| {
        refused(format_args!("collecting LLVM modules for {}: {why}", request.program))
    })?;
    let functions = native.functions(alloc);
    let statics = native.statics(alloc);
    let work = WorkSet::plan(
        Unit {
            product: ContentId(request.product),
            functions: &functions,
            statics: &statics,
            roots: roots.symbols(),
        },
        alloc,
    )
    .map_err(|why| {
        refused(format_args!("planning native backend for {}: {why}", request.program))
    })?;
    let mut renderer = Renderer { catalog: &native, compiler };
    prepared::run(request, &reached, &work, &mut renderer, codes, dist, alloc)
        .map_err(|why| box_err(ExportCliError::Dist(why), heap))
}

fn stored<'s>(
    codes: &'s dyn CodeStore,
    symbol: CodeSymbol,
    key: CodeKey,
    frame: &Frame,
    heap: &'static Heap,
) -> Result<&'s [u8], CliError> {
    codes
        .get(key)
        .map_err(|why| {
            box_err(
                ExportCliError::Store {
                    symbol: symbol.0,
                    shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()),
                },
                heap,
            )
        })?
        .ok_or_else(|| box_err(ExportCliError::MissingEntry { symbol: symbol.0 }, heap))
}

#[expect(
    clippy::too_many_arguments,
    reason = "code records, placement inputs and artifact output columns"
)]
pub(super) fn place<'a>(
    image: &Artifact<'_>,
    names: NamesRecords<'_>,
    codes: &'a dyn CodeStore,
    frame: &Frame,
    bump: Bump<'a>,
    heap: &'static Heap,
    functions: &mut AVec<'_, Function<'a>>,
    data: &mut AVec<'a, Data<'a>>,
    seen_data: &mut AVec<'a, Target>,
    tls_count: &mut usize,
) -> Result<(), CliError> {
    for &(symbol, key) in &image.functions {
        let bytes = stored(codes, symbol, key, frame, heap)?;
        let raw = CodeView::read(bytes).map_err(|why| {
            box_err(
                ExportCliError::Decode {
                    symbol: symbol.0,
                    what: "optimized code",
                    shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()),
                },
                heap,
            )
        })?;
        let record = materialize_positionless(
            symbol,
            raw,
            |item, site| names.records.source_position(item, site, bump.alloc()),
            frame,
            bump,
            heap,
        )
        .map_err(|why| box_err(why, heap))?;
        functions.push(Function { symbol, record, tier: Tier::Optimized });
    }
    for datum in &image.data {
        let target = datum.target;
        let bytes = stored(codes, target.symbol, datum.content, frame, heap)?;
        let raw = if datum.entry {
            statics::entry_record(bytes).map_err(|why| {
                box_err(
                    ExportCliError::Decode {
                        symbol: target.symbol.0,
                        what: "static entry",
                        shown: owned_fmt(format_args!("{why}"), frame, heap.alloc()),
                    },
                    heap,
                )
            })?
        } else {
            bytes
        };
        push_datum(data, seen_data, tls_count, target, raw, frame, bump, heap)?;
    }
    Ok(())
}
