//! Run a complete native grant against the code store. The content and its immutable recipe
//! binding are present before a row can be returned for publication.

use super::{CodeKey, Fold, Function, Hasher, Symbol, Task};
use crate::code_records::{CodeClass, CodeStore, StoreError, content_key};
use blaze_code::record::{CodeView, DecodeError};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, BTreeMap, Vec};

const BINDING_DOMAIN: &str = "blaze-ladder backend output binding 2";
const RECEIPT_MAGIC: [u8; 8] = *b"bznout01";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum OutputKind {
    Standalone,
    ReleaseBlob,
    FoldedBody,
    Thunk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Output {
    pub symbol: Symbol,
    pub kind: OutputKind,
}

pub struct Emitted<'a> {
    pub output: Output,
    /// A complete blaze-code record, including the target and debug tables the renderer made.
    pub record: AVec<'a, u8>,
}

/// Implemented by the owner of the prepared modules; each entry point receives the whole
/// decision its compiler must implement. The driver checks what the renderer returned.
pub trait Emit {
    type Error;

    fn standalone<'a>(
        &mut self,
        function: &Function<'_>,
        alloc: Alloc<'a>,
    ) -> Result<Emitted<'a>, Self::Error>;

    fn release<'a>(
        &mut self,
        function: &Function<'_>,
        imports: &[&Function<'_>],
        alloc: Alloc<'a>,
    ) -> Result<Emitted<'a>, Self::Error>;

    fn fold<'a>(
        &mut self,
        group: &Fold<'_>,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, Emitted<'a>>, Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Store,
    Emitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Compiled {
    pub output: Output,
    pub recipe: CodeKey,
    pub content: CodeKey,
    /// Content address of the actual output's receipt, distinct for different recipes
    /// even when LLVM makes byte-identical machine code.
    pub receipt: CodeKey,
    pub source: Source,
}

#[derive(Debug)]
pub enum Error<E> {
    Store(StoreError),
    Renderer(E),
    Binding { recipe: CodeKey },
    Content { recipe: CodeKey, expected: CodeKey },
    Receipt { recipe: CodeKey, expected: CodeKey, output: Output },
    InvalidRecord { output: Output, why: DecodeError },
    Missing(Output),
    Unexpected(Output),
    Repeated(Output),
}

impl<E: core::fmt::Display> core::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(why) => core::fmt::Display::fmt(why, f),
            Self::Renderer(why) => core::fmt::Display::fmt(why, f),
            Self::Binding { recipe } => {
                write!(f, "native recipe {recipe} has an invalid content binding")
            }
            Self::Content { recipe, expected } => {
                write!(f, "recipe {recipe} requires missing or changed record {expected}")
            }
            Self::Receipt { recipe, expected, output } => write!(
                f,
                "receipt {expected} does not bind recipe {recipe} to {:?} for {}",
                output.kind, output.symbol,
            ),
            Self::InvalidRecord { output, why } => {
                write!(f, "invalid {:?} record for {}: {why}", output.kind, output.symbol)
            }
            Self::Missing(output) => {
                write!(f, "LLVM omitted {:?} for {}", output.kind, output.symbol)
            }
            Self::Unexpected(output) => {
                write!(f, "LLVM emitted unrequested {:?} for {}", output.kind, output.symbol)
            }
            Self::Repeated(output) => {
                write!(f, "LLVM emitted {:?} twice for {}", output.kind, output.symbol)
            }
        }
    }
}

/// Kept apart from the Reference writer's mutable pre-key pointers. The CAS refuses two
/// different content keys written under this same immutable binding.
fn binding(recipe: CodeKey) -> CodeKey {
    CodeKey(Hasher::new(BINDING_DOMAIN).id(&recipe.0).finish())
}

/// The immutable output record: version, emitter role, pre-emission recipe, actual output
/// symbol and the digest of the raw machine record. Its encoding is identical in a pack.
pub fn receipt(output: Output, recipe: CodeKey, content: CodeKey) -> [u8; 105] {
    let role = match output.kind {
        OutputKind::Standalone => 0,
        OutputKind::ReleaseBlob => 1,
        OutputKind::FoldedBody => 2,
        OutputKind::Thunk => 3,
    };
    let mut bytes = [0; 105];
    bytes[..8].copy_from_slice(&RECEIPT_MAGIC);
    bytes[8] = role;
    bytes[9..41].copy_from_slice(&recipe.0.0);
    bytes[41..73].copy_from_slice(&output.symbol.0);
    bytes[73..].copy_from_slice(&content.0.0);
    bytes
}

/// A receipt and machine record verified against the output the local plan expects.
pub struct Bound<'s> {
    pub receipt: CodeKey,
    pub content: CodeKey,
    pub record: CodeView<'s>,
}

impl Task<'_> {
    pub fn outputs(&self, mut visit: impl FnMut(Output, CodeKey)) {
        match self {
            Task::Standalone { function } => visit(
                Output { symbol: function.symbol, kind: OutputKind::Standalone },
                function.template,
            ),
            Task::Release { function, blob, .. } => {
                visit(Output { symbol: function.symbol, kind: OutputKind::ReleaseBlob }, *blob)
            }
            Task::Fold(group) => {
                visit(
                    Output { symbol: Symbol(group.body.0.0), kind: OutputKind::FoldedBody },
                    group.body,
                );
                for member in &group.members {
                    visit(
                        Output { symbol: member.function.symbol, kind: OutputKind::Thunk },
                        member.thunk,
                    );
                }
            }
        }
    }

    pub fn compile<'a, B: Emit>(
        &self,
        backend: &mut B,
        store: &mut dyn CodeStore,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, Compiled>, Error<B::Error>> {
        let mut expected = BTreeMap::new_in(alloc);
        self.outputs(|output, recipe| {
            expected.insert(output, recipe);
        });
        let mut rows = Vec::with_capacity_in(expected.len(), alloc);
        for (&output, &recipe) in &expected {
            if let Some(held) = read::<B::Error>(store, output, recipe)? {
                rows.push(Compiled {
                    output,
                    recipe,
                    content: held.content,
                    receipt: held.receipt,
                    source: Source::Store,
                });
            }
        }
        if rows.len() == expected.len() {
            return Ok(rows);
        }
        let mut held = BTreeMap::new_in(alloc);
        for row in rows.drain(..) {
            held.insert(row.output, row.content);
        }
        // Any missing folded member requires one merge of the complete group.
        let single = |record| {
            let mut one = Vec::with_capacity_in(1, alloc);
            one.push(record);
            one
        };
        let rendered = match self {
            Task::Fold(group) => backend.fold(group, alloc),
            Task::Standalone { function } => backend.standalone(function, alloc).map(single),
            Task::Release { function, imports, .. } => {
                backend.release(function, imports, alloc).map(single)
            }
        }
        .map_err(Error::Renderer)?;
        let mut checked = BTreeMap::new_in(alloc);
        for emitted in rendered {
            let output = emitted.output;
            let &recipe = expected.get(&output).ok_or(Error::Unexpected(output))?;
            CodeView::read(&emitted.record).map_err(|why| Error::InvalidRecord { output, why })?;
            let content = content_key(&emitted.record);
            let written = receipt(output, recipe, content);
            let row = Compiled {
                output,
                recipe,
                content,
                receipt: content_key(&written),
                source: Source::Emitted,
            };
            if held.get(&output).is_some_and(|previous| *previous != row.content) {
                return Err(Error::Store(StoreError::TwoValues { key: binding(recipe) }));
            }
            if checked.insert(output, (row, emitted.record, written)).is_some() {
                return Err(Error::Repeated(output));
            }
        }
        if let Some(output) = expected.keys().find(|output| !checked.contains_key(*output)) {
            return Err(Error::Missing(*output));
        }
        for (_, (row, bytes, written)) in checked {
            store.put(CodeClass::Program, row.content, &bytes).map_err(Error::Store)?;
            store.put(CodeClass::Program, row.receipt, &written).map_err(Error::Store)?;
            store
                .put(CodeClass::Program, binding(row.recipe), &row.receipt.0.0)
                .map_err(Error::Store)?;
            rows.push(row);
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests;

/// Read the bound, content-checked code the exporter places under this output's symbol.
pub fn read<E>(
    store: &dyn CodeStore,
    output: Output,
    recipe: CodeKey,
) -> Result<Option<Bound<'_>>, Error<E>> {
    read_with(|key| store.get(key), output, recipe)
}

/// The same verification against exactly one committed publication, before admitting any
/// of its values to the local store.
pub fn read_with<'s, E>(
    mut get: impl FnMut(CodeKey) -> Result<Option<&'s [u8]>, StoreError>,
    output: Output,
    recipe: CodeKey,
) -> Result<Option<Bound<'s>>, Error<E>> {
    let Some(bytes) = get(binding(recipe)).map_err(Error::Store)? else { return Ok(None) };
    let key = bytes.try_into().map_err(|_| Error::Binding { recipe })?;
    let output_receipt = CodeKey(ContentId(key));
    let wrong = || Error::Receipt { recipe, expected: output_receipt, output };
    let written = get(output_receipt).map_err(Error::Store)?.ok_or_else(wrong)?;
    let (_, raw) = written.split_last_chunk::<32>().ok_or_else(wrong)?;
    let content = CodeKey(ContentId(*raw));
    if content_key(written) != output_receipt || written != receipt(output, recipe, content).as_slice() {
        return Err(wrong());
    }
    let bytes =
        get(content).map_err(Error::Store)?.ok_or(Error::Content { recipe, expected: content })?;
    if content_key(bytes) != content {
        return Err(Error::Content { recipe, expected: content });
    }
    let record = CodeView::read(bytes).map_err(|why| Error::InvalidRecord { output, why })?;
    Ok(Some(Bound { receipt: output_receipt, content, record }))
}
