//! Run a complete native grant against the code store. The content and its immutable recipe
//! binding are present before a row can be returned for publication.

use super::{CodeKey, Fold, Function, Hasher, Symbol, Task};
use crate::code_records::{CodeClass, CodeStore, StoreError, content_key};
use blaze_code::record::{CodeView, DecodeError};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, BTreeMap, Vec};

const BINDING_DOMAIN: &str = "blaze-ladder backend output binding 1";

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
    pub source: Source,
}

#[derive(Debug)]
pub enum Error<E> {
    Store(StoreError),
    Renderer(E),
    Binding { recipe: CodeKey },
    Content { recipe: CodeKey, expected: CodeKey },
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
            if let Some((content, _)) = read::<B::Error>(store, output, recipe)? {
                rows.push(Compiled { output, recipe, content, source: Source::Store });
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
            let row = Compiled {
                output,
                recipe,
                content: content_key(&emitted.record),
                source: Source::Emitted,
            };
            if held.get(&output).is_some_and(|previous| *previous != row.content) {
                return Err(Error::Store(StoreError::TwoValues { key: binding(recipe) }));
            }
            if checked.insert(output, (row, emitted.record)).is_some() {
                return Err(Error::Repeated(output));
            }
        }
        if let Some(output) = expected.keys().find(|output| !checked.contains_key(*output)) {
            return Err(Error::Missing(*output));
        }
        for (_, (row, bytes)) in checked {
            store.put(CodeClass::Program, row.content, &bytes).map_err(Error::Store)?;
            store
                .put(CodeClass::Program, binding(row.recipe), &row.content.0.0)
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
) -> Result<Option<(CodeKey, CodeView<'_>)>, Error<E>> {
    read_with(|key| store.get(key), output, recipe)
}

/// The same verification against exactly one committed publication, before admitting any
/// of its values to the local store.
pub fn read_with<'s, E>(
    mut get: impl FnMut(CodeKey) -> Result<Option<&'s [u8]>, StoreError>,
    output: Output,
    recipe: CodeKey,
) -> Result<Option<(CodeKey, CodeView<'s>)>, Error<E>> {
    let Some(bytes) = get(binding(recipe)).map_err(Error::Store)? else { return Ok(None) };
    let key = bytes.try_into().map_err(|_| Error::Binding { recipe })?;
    let content = CodeKey(ContentId(key));
    let bytes =
        get(content).map_err(Error::Store)?.ok_or(Error::Content { recipe, expected: content })?;
    if content_key(bytes) != content {
        return Err(Error::Content { recipe, expected: content });
    }
    let record = CodeView::read(bytes).map_err(|why| Error::InvalidRecord { output, why })?;
    Ok(Some((content, record)))
}
