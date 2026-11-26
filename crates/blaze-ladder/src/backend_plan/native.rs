//! Plan native source work from the actual LLVM modules, with the pinned release import
//! budget applied to the instruction count and attributes of their grounded LIR.

use super::{
    Backend, Binding, CodeKey, Constant, Function, Mask, Recipe, Reference, Release, Static,
    Summary,
};
use crate::body_ids::RowKind;
use crate::code_records::{EntryError, statics};
use crate::run::make_one::source_unit::{Prepared, Reached, Row};
use blaze_code::datum::{DatumError, DatumView};
use blaze_code::record::Symbol;
use blaze_emit::llvm::LlvmModule;
use blaze_ids::{ContentId, Hasher};
use blaze_int::Int;
use blaze_lir::Scalar;
use blaze_lir::func::{FnAttrs, Inline};
use blaze_own::{ABTreeMap, AVec, Alloc, BTreeMap, BTreeSet, CollectIn, Vec, VecDeque};

pub mod render;

const SOURCE_DOMAIN: &str = "blaze-ladder native LLVM module 1";
const STATIC_DOMAIN: &str = "blaze-ladder native static entry 1";
const MASK_DOMAIN: &str = "blaze-ladder native LLVM mask 1";
const SUMMARY_DOMAIN: &str = "blaze-ladder native LLVM summary 1";

#[derive(Debug)]
pub enum Error {
    Literal { symbol: Symbol },
    Instructions { symbol: Symbol, count: usize },
    Entry { symbol: Symbol, why: EntryError },
    Datum { symbol: Symbol, why: DatumError },
    Repeated(Symbol),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Literal { symbol } => {
                write!(f, "{symbol} has an invalid LLVM definition or literal range")
            }
            Self::Instructions { symbol, count } => {
                write!(f, "{symbol} has {count} instructions, beyond the native summary's limit")
            }
            Self::Entry { symbol, why } => write!(f, "static entry {symbol}: {why}"),
            Self::Datum { symbol, why } => write!(f, "static datum {symbol}: {why}"),
            Self::Repeated(symbol) => write!(f, "two prepared source rows define {symbol}"),
        }
    }
}

pub type Module<'a> = Prepared<'a, LlvmModule<'a>>;

struct Form<'a> {
    shape: ContentId,
    size: u32,
    constants: AVec<'a, Constant>,
}

struct Body<'a, 'r> {
    module: &'r Module<'a>,
    prepared: ContentId,
    summary: ContentId,
    encoded: AVec<'a, u8>,
    references: AVec<'a, Reference>,
    form: Form<'a>,
    imports: AVec<'a, Symbol>,
}

struct Datum<'a> {
    value: ContentId,
    references: AVec<'a, Reference>,
}

pub struct Native<'a, 'r> {
    bodies: ABTreeMap<'a, Symbol, Body<'a, 'r>>,
    data: ABTreeMap<'a, Symbol, Datum<'a>>,
    /// The actual target compiler, target machine, optimization pipeline and their options.
    settings: ContentId,
}

fn form<'a>(module: &LlvmModule<'_>, alloc: Alloc<'a>) -> Result<Form<'a>, Error> {
    let symbol = module.symbol;
    let bad = || Error::Literal { symbol };
    let size = u32::try_from(module.instructions)
        .map_err(|_| Error::Instructions { symbol, count: module.instructions })?;
    let mut hash = Hasher::new(MASK_DOMAIN);
    let own = &module.definition_name;
    if module.ir.get(own.start..own.end).ok_or_else(bad)?.is_empty() {
        return Err(bad());
    }
    hash.part(module.ir.get(..own.start).ok_or_else(bad)?);
    let mut from = own.end;
    let mut constants = Vec::with_capacity_in(module.literals.len(), alloc);
    for literal in &module.literals {
        let range = &literal.operand;
        let field = module.ir.get(range.start..range.end).ok_or_else(bad)?;
        if field.is_empty() {
            return Err(bad());
        }
        hash.part(module.ir.get(from..range.start).ok_or_else(bad)?)
            .update(&literal.ty.0.to_le_bytes());
        from = range.end;
        let value = match literal.ty.elem() {
            Scalar::F32 => Constant::F32(u32::try_from(literal.bits).map_err(|_| bad())?),
            Scalar::F64 => Constant::F64(u64::try_from(literal.bits).map_err(|_| bad())?),
            ty => Constant::Int {
                bits: u8::try_from(ty.bits(64)).map_err(|_| bad())?,
                value: literal.bits,
            },
        };
        constants.push(value);
    }
    hash.part(module.ir.get(from..).ok_or_else(bad)?);
    Ok(Form { shape: hash.finish(), size, constants })
}

fn body<'a, 'r>(input: &'r Module<'a>, alloc: Alloc<'a>) -> Result<Body<'a, 'r>, Error> {
    let module = &input.module;
    let form = form(module, alloc)?;
    let mut hash = Hasher::new(SOURCE_DOMAIN);
    hash.update(&module.symbol.0).part(&module.ir).id(&input.side);
    hash.update(&module.references.len().widen_to::<u64>().to_le_bytes());
    for reference in &module.references {
        match reference {
            Some(target) => {
                hash.update(&[1, target.space.byte()]).update(&target.symbol.0);
            }
            None => {
                hash.update(&[0]);
            }
        }
    }
    hash.update(&module.implicit_references.len().widen_to::<u64>().to_le_bytes());
    for target in &module.implicit_references {
        hash.update(&[target.space.byte()]).update(&target.symbol.0);
    }
    hash.update(&module.positions.len().widen_to::<u64>().to_le_bytes());
    for (data, position) in &module.positions {
        hash.update(&data.to_le_bytes())
            .update(&position.item)
            .update(&position.offset.to_le_bytes())
            .update(&position.site.to_le_bytes())
            .update(&position.field.to_le_bytes());
    }
    let references = module
        .references
        .iter()
        .flatten()
        .chain(&module.implicit_references)
        .map(|&target| Reference { target, binding: Binding::Direct })
        .collect_in(alloc);
    let mut encoded = Vec::new_in(alloc);
    encoded.extend_from_slice(&module.symbol.0);
    encoded.extend_from_slice(&input.attrs.0.to_le_bytes());
    encoded.extend_from_slice(&form.size.to_le_bytes());
    encoded.extend_from_slice(&module.calls.len().widen_to::<u64>().to_le_bytes());
    for call in &module.calls {
        encoded.extend_from_slice(&call.0);
    }
    let summary = ContentId::of(SUMMARY_DOMAIN, &encoded);
    Ok(Body {
        module: input,
        prepared: hash.finish(),
        summary,
        encoded,
        references,
        form,
        imports: Vec::new_in(alloc),
    })
}

/// Breadth first as in the historical release planner, preserving direct call order while
/// deciding the budget and ordering the resulting imported modules by symbol. LLVM receives
/// those definitions and decides actual inlining during optimization.
fn imports<'a>(
    root: &Body<'_, '_>,
    all: &ABTreeMap<'_, Symbol, Body<'_, '_>>,
    alloc: Alloc<'a>,
) -> AVec<'a, Symbol> {
    let mut result = Vec::new_in(alloc);
    if root.module.attrs.0 & FnAttrs::OPT_NONE != 0 {
        return result;
    }
    let mut seen = BTreeSet::new_in(alloc);
    seen.insert(root.module.module.symbol);
    let mut queue = VecDeque::new_in(alloc);
    for &callee in &root.module.module.calls {
        queue.push_back((callee, 1f64, 0u8));
    }
    let mut spent = 0u64;
    while let Some((symbol, share, depth)) = queue.pop_front() {
        if !seen.insert(symbol) {
            continue;
        }
        let Some(callee) = all.get(&symbol) else { continue };
        let attrs = callee.module.attrs;
        if attrs.0 & FnAttrs::OPT_NONE != 0 {
            continue;
        }
        let room = match attrs.inline() {
            Inline::Never => continue,
            Inline::Always => 40_000,
            Inline::Hint => 300,
            Inline::None => 100,
        };
        let size = callee.form.size;
        if f64::from(size) > f64::from(room) * share || spent + u64::from(size) > 40_000 {
            continue;
        }
        spent += u64::from(size);
        result.push(symbol);
        if depth + 1 < 8 {
            for &called in &callee.module.module.calls {
                queue.push_back((called, share * 0.7, depth + 1));
            }
        }
    }
    result.sort_unstable();
    result
}

impl<'a, 'r> Native<'a, 'r> {
    pub fn of(
        reached: &'r Reached<'a, LlvmModule<'a>>,
        settings: ContentId,
        alloc: Alloc<'a>,
    ) -> Result<Self, Error> {
        let mut bodies = BTreeMap::new_in(alloc);
        let mut data = BTreeMap::new_in(alloc);
        for (at, row) in reached.rows.iter().enumerate() {
            let symbol = reached.walk.symbol(at);
            match row {
                Row::Function(module) => {
                    if bodies.insert(symbol, body(module, alloc)?).is_some() {
                        return Err(Error::Repeated(symbol));
                    }
                }
                Row::Datum(entry) => {
                    let bytes =
                        statics::entry_record(entry).map_err(|why| Error::Entry { symbol, why })?;
                    let view =
                        DatumView::read(bytes).map_err(|why| Error::Datum { symbol, why })?;
                    let references = (0..view.targets())
                        .map(|i| Reference { target: view.target(i), binding: Binding::Direct })
                        .collect_in(alloc);
                    let mut value = Hasher::new(STATIC_DOMAIN);
                    value
                        .update(&[u8::from(reached.walk.kind(at) == RowKind::ThreadLocal)])
                        .part(entry);
                    if data.insert(symbol, Datum { value: value.finish(), references }).is_some() {
                        return Err(Error::Repeated(symbol));
                    }
                }
            }
        }
        let closures = bodies.values().map(|item| imports(item, &bodies, alloc)).collect_in(alloc);
        for (item, closure) in bodies.values_mut().zip(closures) {
            item.imports = closure;
        }
        Ok(Self { bodies, data, settings })
    }

    pub fn module(&self, symbol: Symbol) -> Option<&'r Module<'a>> {
        self.bodies.get(&symbol).map(|body| body.module)
    }

    pub fn functions<'s>(&'s self, alloc: Alloc<'s>) -> AVec<'s, Function<'s>> {
        self.bodies
            .iter()
            .map(|(&symbol, body)| {
                let recipe =
                    Recipe { prepared: body.prepared, settings: self.settings, built_in: &[] };
                let release = Release {
                    recipe,
                    summary: Summary { key: body.summary, bytes: &body.encoded },
                    decisions: &[],
                    imports: &body.imports,
                };
                let backend = if body.module.module.fold_abi.is_none()
                    || !body.module.module.traps.is_empty()
                {
                    Backend::Release(release)
                } else {
                    Backend::ReleaseFold {
                        release,
                        mask: Mask {
                            shape: body.form.shape,
                            instructions: body.form.size,
                            constants: &body.form.constants,
                        },
                    }
                };
                Function {
                    symbol,
                    template: CodeKey(body.prepared),
                    reads: body.module.side,
                    references: &body.references,
                    backend,
                }
            })
            .collect_in(alloc)
    }

    pub fn statics<'s>(&'s self, alloc: Alloc<'s>) -> AVec<'s, Static<'s>> {
        self.data
            .iter()
            .map(|(&symbol, datum)| Static {
                symbol,
                value: datum.value,
                references: &datum.references,
            })
            .collect_in(alloc)
    }
}
