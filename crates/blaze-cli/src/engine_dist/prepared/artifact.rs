//! Inputs the native artifact reads after all holders' accepted packs have arrived.

use super::{DistError, refused};
use blaze_code::record::{Space, Symbol, Target};
use blaze_ladder::backend_plan::{WorkSet, execute};
use blaze_ladder::body_ids::RowKind;
use blaze_ladder::code_records::{CodeKey, CodeStore, content_key};
use blaze_ladder::run::make_one::source_unit::{Reached, Row};
use blaze_own::{AVec, Alloc, BTreeMap, BTreeSet, Heap, Vec};

pub(crate) struct Datum {
    pub(crate) target: Target,
    pub(crate) content: CodeKey,
    /// Static and TLS rows carry an encoded entry around their datum; a vtable is a bare datum.
    pub(crate) entry: bool,
}

pub(crate) struct Artifact<'a> {
    pub(crate) functions: AVec<'a, (Symbol, CodeKey)>,
    pub(crate) data: AVec<'a, Datum>,
    pub(crate) has_tls: bool,
}

impl<'a> Artifact<'a> {
    pub(crate) fn read<M>(
        reached: &Reached<'_, M>,
        work: &WorkSet<'_>,
        store: &dyn CodeStore,
        alloc: Alloc<'a>,
        heap: &'static Heap,
    ) -> Result<Self, DistError> {
        let mut functions = Vec::new_in(alloc);
        let mut defined = BTreeSet::new_in(alloc);
        let getter = blaze_ladder::symbols::tls_getter_symbol();
        let descriptor = blaze_ladder::symbols::tls_desc_symbol();
        let mut has_tls = false;
        for (_, task) in work.tasks() {
            let mut outputs = Vec::new_in(alloc);
            task.outputs(|output, recipe| outputs.push((output, recipe)));
            for (output, recipe) in outputs {
                let held =
                    execute::read::<core::convert::Infallible>(store, output, recipe)
                        .map_err(|why| {
                            refused(
                                heap,
                                format_args!("placing native output {}: {why}", output.symbol),
                            )
                        })?
                        .ok_or_else(|| {
                            refused(
                                heap,
                                format_args!("native grant omitted {} ({recipe})", output.symbol),
                            )
                        })?;
                if !defined.insert(output.symbol) {
                    return Err(refused(
                        heap,
                        format_args!("two native definitions of {}", output.symbol),
                    ));
                }
                has_tls |= crate::engine_export::code_touches_tls(&held.record, getter, descriptor);
                functions.push((output.symbol, held.content));
            }
        }
        let mut targets = BTreeMap::new_in(alloc);
        for (index, row) in reached.rows.iter().enumerate() {
            let symbol = reached.walk.symbol(index);
            match row {
                Row::Function(body) => {
                    if !defined.contains(&symbol) {
                        return Err(refused(
                            heap,
                            format_args!("no native record defines {symbol}"),
                        ));
                    }
                    for table in &body.vtables {
                        let target = Target { space: Space::Vtable, symbol: table.symbol };
                        let key = content_key(&table.bytes);
                        if targets.insert(target, key).is_some_and(|prior| prior != key) {
                            return Err(refused(
                                heap,
                                format_args!(
                                    "two native data records define vtable {}",
                                    table.symbol
                                ),
                            ));
                        }
                    }
                }
                Row::Datum(bytes) => {
                    let space = match reached.walk.kind(index) {
                        RowKind::Static => Space::Static,
                        RowKind::ThreadLocal => Space::Tls,
                        RowKind::Instance => {
                            return Err(refused(
                                heap,
                                format_args!("function {symbol} owns a static entry"),
                            ));
                        }
                    };
                    targets.insert(Target { space, symbol }, content_key(bytes));
                }
            }
        }
        let mut data = Vec::with_capacity_in(targets.len(), alloc);
        for (target, content) in targets {
            let bytes = store.get(content).map_err(DistError::Store)?.ok_or_else(|| {
                refused(
                    heap,
                    format_args!("native unit omitted data {} ({content})", target.symbol),
                )
            })?;
            if content_key(bytes) != content {
                return Err(refused(
                    heap,
                    format_args!("wrong native data at {content} for {}", target.symbol),
                ));
            }
            has_tls |= target.space == Space::Tls;
            data.push(Datum { target, content, entry: target.space != Space::Vtable });
        }
        Ok(Self { functions, data, has_tls })
    }
}
