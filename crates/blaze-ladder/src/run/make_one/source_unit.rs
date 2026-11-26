//! Root-based source discovery for a whole-unit backend. Function declarations come from
//! grounded instances, static and TLS declarations from their real encoded entries.

use super::{Grounded, Maker, RecordsProgram, Typing};
use crate::body_ids::{DeclaredList, EntryForm, Form, Grant, Make, RowKind, Walk, WalkError};
use crate::code_records::{Callee, CodeRecordError, EntryError};
use crate::run::{RunError, special::ProductRoots};
use blaze_code::record::Symbol;
use blaze_emit::llvm::{self, LlvmModule, LowerError};
use blaze_ids::{ContentId, Hasher};
use blaze_int::Int;
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;

const SIDE_DOMAIN: &str = "blaze-ladder grounded backend side data 1";

pub struct Vtable<'a> {
    pub symbol: Symbol,
    pub bytes: AVec<'a, u8>,
}

pub struct Prepared<'a, M> {
    pub module: M,
    pub side: ContentId,
    pub attrs: blaze_lir::func::FnAttrs,
    pub vtables: AVec<'a, Vtable<'a>>,
}

pub enum Row<'a, M> {
    Function(Prepared<'a, M>),
    /// The ordinary static or TLS entry including relocations and declarations.
    Datum(AVec<'a, u8>),
}

pub struct Reached<'a, M> {
    pub walk: Walk<'a>,
    pub rows: AVec<'a, Row<'a, M>>,
}

#[derive(Debug)]
pub enum Error<G, L> {
    Grant { tabled: ItemKey, why: G },
    Make { symbol: Symbol, why: RunError },
    Walk(WalkError<RunError>),
    Entry { symbol: Symbol, why: EntryError },
    Routine { symbol: Symbol, why: CodeRecordError },
    Vtable { symbol: Symbol, table: Symbol },
    Lower { symbol: Symbol, why: L },
}

impl<G: core::fmt::Display, L: core::fmt::Display> core::fmt::Display for Error<G, L> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Grant { tabled, why } => {
                write!(f, "source item {:#x} has no runnable proof: {why}", tabled.0)
            }
            Self::Make { symbol, why } => write!(f, "preparing {symbol}: {why}"),
            Self::Walk(why) => core::fmt::Display::fmt(why, f),
            Self::Entry { symbol, why } => write!(f, "declarations in entry {symbol}: {why}"),
            Self::Routine { symbol, why } => write!(f, "runtime routine of {symbol}: {why}"),
            Self::Vtable { symbol, table } => write!(f, "{symbol} carries two different data records for vtable {table}"),
            Self::Lower { symbol, why } => write!(f, "lowering {symbol}: {why}"),
        }
    }
}

fn side_data<'a, M>(body: &Grounded<'_>, module: M, alloc: Alloc<'a>) -> Result<Prepared<'a, M>, Symbol> {
    let mut hash = Hasher::new(SIDE_DOMAIN);
    let count = |hash: &mut Hasher, n: usize| {
        hash.update(&n.widen_to::<u64>().to_le_bytes());
    };
    count(&mut hash, body.reads.len());
    for (question, answer) in body.reads.rows() {
        hash.part(question).id(&answer);
    }
    let mut declared = Vec::from_iter_in(body.declared.iter().copied(), alloc);
    declared.sort_unstable_by(|a, b| a.tabled.cmp(&b.tabled).then_with(|| a.name.cmp(b.name)));
    declared.dedup_by(|a, b| a.tabled == b.tabled && a.name == b.name);
    count(&mut hash, declared.len());
    for row in &declared {
        hash.update(&row.tabled.0.to_le_bytes()).part(row.name);
    }
    let mut carried = Vec::from_iter_in(body.vtables.iter().copied(), alloc);
    carried.sort_unstable_by_key(|table| table.symbol);
    for [a, b] in carried.array_windows::<2>() {
        if a.symbol == b.symbol && a.datum != b.datum { return Err(a.symbol); }
    }
    carried.dedup_by(|a, b| a.symbol == b.symbol);
    count(&mut hash, carried.len());
    let mut vtables = Vec::with_capacity_in(carried.len(), alloc);
    for datum in &carried {
        hash.update(&datum.symbol.0).part(datum.datum);
        let mut bytes = Vec::with_capacity_in(datum.datum.len(), alloc);
        bytes.extend_from_slice(datum.datum);
        vtables.push(Vtable { symbol: datum.symbol, bytes });
    }
    count(&mut hash, body.traps.len());
    for trap in body.traps {
        hash.update(&trap.ordinal.to_le_bytes()).update(&[trap.kind.byte()]);
        if let Some((item, offset)) = trap.site {
            hash.update(&[1]).update(&item.to_le_bytes()).update(&offset.to_le_bytes());
        } else {
            hash.update(&[0]);
        }
        if let Some(subject) = trap.subject {
            hash.update(&[1])
                .update(&subject.item.to_le_bytes())
                .update(&subject.count.to_le_bytes());
            for discr in &subject.discriminants {
                hash.update(&discr.to_le_bytes());
            }
        } else {
            hash.update(&[0]);
        }
    }
    let view = body.body.view();
    Ok(Prepared {
        module,
        side: hash.finish(),
        attrs: blaze_lir::func::FnAttrs(view.head.attrs),
        vtables,
    })
}

fn declare(
    body: &Grounded<'_>,
    called: &[Symbol],
    rows: &mut DeclaredList<'_>,
) -> Result<(), CodeRecordError> {
    for target in body.declared {
        rows.push(target.tabled, target.name);
    }
    for &item in body.names.statics {
        if !body.names.c_statics.iter().any(|&(foreign, _)| foreign == item) {
            rows.push_static(item);
        }
    }
    for &item in body.names.tls {
        rows.push_thread_local(item);
    }
    for routine in blaze_lir::legalize::SoftFloat::ALL {
        if let Some(place) = body.routines.places().place(routine)
            && let Some(Callee::Symbol(symbol)) = body.names.funcs.get(place.widen_to::<usize>())
            && called.contains(symbol)
        {
            let target = body.routines.declared(routine)?;
            rows.push(target.tabled, target.name);
        }
    }
    Ok(())
}

/// `lower` owns everything that has to survive the next source build. `calls` returns the
/// actual internal calls of that module, including any runtime routine introduced by lowering.
pub fn collect<'a, 'n, G, T, M, Denied, Lower>(
    roots: &ProductRoots<'_>,
    maker: &mut Maker<'n, G, T>,
    mut grant: impl FnMut(ItemKey) -> Result<G, Denied>,
    mut lower: impl FnMut(Symbol, &Grounded<'_>, Alloc<'a>) -> Result<M, Lower>,
    calls: impl Fn(&M) -> &[Symbol],
    alloc: Alloc<'a>,
) -> Result<Reached<'a, M>, Error<Denied, Lower>>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    let mut walk = Walk::new_in(alloc);
    for at in 0..roots.len() {
        walk.root(roots.row(at), super::MOST_WALKED).map_err(Error::Walk)?;
    }
    let mut rows = Vec::new_in(alloc);
    let mut declared = DeclaredList::new_in(alloc);
    while rows.len() < walk.len() {
        let at = rows.len();
        let tabled = walk.tabled(at);
        let symbol = walk.symbol(at);
        let proof = grant(tabled).map_err(|why| Error::Grant { tabled, why })?;
        declared.clear();
        let row = match walk.kind(at) {
            RowKind::Instance => {
                let prepared = maker
                    .prepare(&proof, tabled, walk.name(at), |body| {
                        let module = lower(symbol, &body, alloc)
                            .map_err(|why| Error::Lower { symbol, why })?;
                        declare(&body, calls(&module), &mut declared)
                            .map_err(|why| Error::Routine { symbol, why })?;
                        side_data(&body, module, alloc).map_err(|table| Error::Vtable { symbol, table })
                    })
                    .map_err(|why| Error::Make { symbol, why })??;
                Row::Function(prepared)
            }
            kind => {
                let mut entry = Vec::new_in(alloc);
                if kind == RowKind::Static {
                    maker.make_static(&proof, tabled, &mut entry)
                } else {
                    maker.make_thread_local(&proof, tabled, &mut entry)
                }
                .map_err(|why| Error::Make { symbol, why })?;
                EntryForm
                    .declared(&entry, &mut declared)
                    .map_err(|why| Error::Entry { symbol, why })?;
                Row::Datum(entry)
            }
        };
        walk.discover(at, super::MOST_WALKED, &declared).map_err(Error::Walk)?;
        rows.push(row);
    }
    Ok(Reached { walk, rows })
}

/// The source export's native producer. Use the same bound soft-float routine places as the
/// ordinary emitter when resolving references introduced by target lowering.
pub fn native<'a, 'n, G, T, Denied>(
    roots: &ProductRoots<'_>,
    maker: &mut Maker<'n, G, T>,
    grant: impl FnMut(ItemKey) -> Result<G, Denied>,
    alloc: Alloc<'a>,
) -> Result<Reached<'a, LlvmModule<'a>>, Error<Denied, LowerError>>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    collect(
        roots,
        maker,
        grant,
        |symbol, body, alloc| {
            let names = crate::code_records::Routed {
                names: body.names,
                routines: *body.routines.places(),
            };
            let mut module = llvm::prepare(symbol, body.body, &names, alloc)?;
            module.traps.extend_from_slice(body.traps);
            Ok(module)
        },
        |module| &module.calls,
        alloc,
    )
}
