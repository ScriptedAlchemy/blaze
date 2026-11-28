//! Ordered evidence from the final unit's checked walk. A committed publication
//! carries this beside the exact entries it checked. The unit's identity binds the
//! checked input and selected roots, and the committed pack binds the bytes.

use core::fmt;

use blaze_code::record::Symbol;
use blaze_dist_local::pack::Value;
use blaze_ids::ContentId;
use blaze_ladder::code_records::{CodeKey, content_key};
use blaze_ladder::run::make_one::MOST_WALKED;
use blaze_own::{BTreeMap, Heap};
use blaze_typeck::ItemKey;
use blaze_wire::{Digest, WireAlloc, WireVec};

use super::{DistError, KIND_CODE, KIND_POINTER};

const KIND_REACH: u8 = 2;
const ROW: usize = 1 + 16 + 32 + 32 + 32 + 32;
const DOMAIN: &str = "blaze distributed dev reachability 1";
const ZERO: Digest = [0; 32];

pub(super) fn protocol() -> Digest {
    ContentId::of(DOMAIN, &[]).0
}

fn key(unit: Digest) -> Digest {
    ContentId::of(DOMAIN, &unit).0
}

pub(super) fn refused(heap: &'static Heap, unit: Digest, why: &str) -> DistError {
    let shown = blaze_own::format_in!(heap.alloc(), "distributed reachability for {} {why}", blaze_wire::json::hex(&unit));
    DistError::Compile { shown }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    Keyed { recipe: CodeKey, key: CodeKey, content: CodeKey },
    Unkeyed(CodeKey),
    Stub,
}

/// A placement refusal carries only fixed-width identities and row counts. Formatting
/// writes directly to its caller; it does not collect names or an unbounded read list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mismatch {
    MissingRow { row: usize, rows: usize },
    Item { expected: ItemKey, actual: ItemKey },
    Symbol { expected: Symbol, actual: Symbol },
    MalformedRow,
    MissingRecipe { expected: CodeKey },
    Recipe { expected: CodeKey, actual: CodeKey },
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::MissingRow { row, rows } => write!(f, "row {row} is absent from the publication's {rows} rows"),
            Self::Item { expected, actual } => write!(f, "item differs: published {:032x}, current {:032x}", expected.0, actual.0),
            Self::Symbol { expected, actual } => write!(f, "symbol differs: published {:x}, current {:x}", ContentId(expected.0), ContentId(actual.0)),
            Self::MalformedRow => f.write_str("the stored reachability row is malformed"),
            Self::MissingRecipe { expected } => write!(f, "current inputs have no recipe; published {:x}", expected.0),
            Self::Recipe { expected, actual } => write!(f, "recipe differs: published {:x}, current {:x}", expected.0, actual.0),
        }
    }
}

pub(super) struct Writer(Value);

impl Writer {
    pub(super) fn new(unit: Digest, alloc: WireAlloc) -> Self {
        Self(Value { key: key(unit), kind: KIND_REACH, bytes: super::copy_wire_bytes(&unit, alloc) })
    }

    pub(super) fn push(&mut self, tabled: ItemKey, symbol: Symbol, step: Step) {
        let (tag, recipe, key, content) = match step {
            Step::Keyed { recipe, key, content } => (0, recipe.0.0, key.0.0, content.0.0),
            Step::Unkeyed(content) => (1, ZERO, ZERO, content.0.0),
            Step::Stub => (2, ZERO, ZERO, ZERO),
        };
        let bytes = &mut self.0.bytes;
        bytes.push(tag);
        bytes.extend_from_slice(&tabled.0.to_le_bytes());
        bytes.extend_from_slice(&symbol.0);
        bytes.extend_from_slice(&recipe);
        bytes.extend_from_slice(&key);
        bytes.extend_from_slice(&content);
    }

    pub(super) fn finish(self) -> Value {
        self.0
    }
}

fn step(row: &[u8; ROW]) -> Option<Step> {
    let recipe = CodeKey(ContentId(*row[49..81].as_array()?));
    let key = CodeKey(ContentId(*row[81..113].as_array()?));
    let content = CodeKey(ContentId(*row[113..].as_array()?));
    match row[0] {
        0 if recipe == key || key == content => Some(Step::Keyed { recipe, key, content }),
        1 if recipe.0.0 == ZERO && key.0.0 == ZERO => Some(Step::Unkeyed(content)),
        2 if recipe.0.0 == ZERO && key.0.0 == ZERO && content.0.0 == ZERO => Some(Step::Stub),
        _ => None,
    }
}

/// An ordered closure whose keyed entries were supplied by this same publication
/// and match the content identities observed by its completing worker.
pub(crate) struct Closure {
    pub(super) digest: Digest,
    bytes: WireVec<u8>,
}

impl Closure {
    pub(super) fn read(
        unit: Digest,
        values: &mut BTreeMap<Digest, (u8, WireVec<u8>), WireAlloc>,
        heap: &'static Heap,
    ) -> Result<Self, DistError> {
        let refuse = |why| refused(heap, unit, why);
        let (kind, bytes) = values.remove(&key(unit)).ok_or_else(|| refuse("was omitted"))?;
        if kind != KIND_REACH || bytes.get(..32) != Some(unit.as_slice()) {
            return Err(refuse("has the wrong kind or unit"));
        }
        let (rows, trailing) = bytes[32..].as_chunks::<ROW>();
        if rows.len() > MOST_WALKED || !trailing.is_empty() {
            return Err(refuse("has an invalid row count"));
        }
        for row in rows {
            if let Step::Keyed { recipe, key, content } = step(row).ok_or_else(|| refuse("has an invalid row"))? {
                if recipe != key {
                    let (kind, target) = values.get(&recipe.0.0).ok_or_else(|| refuse("omits a checked instance pointer"))?;
                    if *kind != KIND_POINTER || target.as_slice() != key.0.0 {
                        return Err(refuse("changed a checked instance pointer"));
                    }
                }
                let (kind, entry) = values.get(&key.0.0).ok_or_else(|| refuse("omits a checked entry"))?;
                if *kind != KIND_CODE || content_key(entry) != content {
                    return Err(refuse("changed a checked entry"));
                }
            }
        }
        let digest = ContentId::of(DOMAIN, bytes.as_slice()).0;
        Ok(Self { digest, bytes })
    }

    pub(crate) fn len(&self) -> usize {
        (self.bytes.len() - 32) / ROW
    }

    pub(crate) fn at(
        &self,
        index: usize,
        tabled: ItemKey,
        symbol: Symbol,
        recipe_of: impl FnOnce() -> Option<CodeKey>,
    ) -> Result<Step, Mismatch> {
        let (rows, _) = self.bytes[32..].as_chunks::<ROW>();
        let row = rows.get(index).ok_or(Mismatch::MissingRow { row: index, rows: rows.len() })?;
        if row[1..17] != tabled.0.to_le_bytes() {
            let expected = ItemKey(u128::from_le_bytes(*row[1..17].first_chunk().ok_or(Mismatch::MalformedRow)?));
            return Err(Mismatch::Item { expected, actual: tabled });
        }
        if row[17..49] != symbol.0 {
            let expected = Symbol(*row[17..49].first_chunk().ok_or(Mismatch::MalformedRow)?);
            return Err(Mismatch::Symbol { expected, actual: symbol });
        }
        let step = step(row).ok_or(Mismatch::MalformedRow)?;
        if let Step::Keyed { recipe, .. } = step {
            let actual = recipe_of().ok_or(Mismatch::MissingRecipe { expected: recipe })?;
            if actual != recipe {
                return Err(Mismatch::Recipe { expected: recipe, actual });
            }
        }
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_binds_ordered_rows_and_their_exact_recipes_and_bytes() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let unit = [1; 32];
        let recipe = CodeKey(ContentId([2; 32]));
        let owner = ItemKey(3);
        let symbol = Symbol([4; 32]);
        let bytes = b"the checked entry";
        let content = content_key(bytes);
        let other_recipe = CodeKey(ContentId([7; 32]));
        let mut writer = Writer::new(unit, alloc);
        let keyed = Step::Keyed { recipe, key: content, content };
        writer.push(owner, symbol, keyed);
        writer.push(ItemKey(5), Symbol([6; 32]), Step::Stub);
        writer.push(ItemKey(8), Symbol([8; 32]), Step::Unkeyed(content));
        let receipt = writer.finish();
        let bundle = || {
            let mut values = BTreeMap::new_in(alloc);
            values.insert(receipt.key, (receipt.kind, super::super::copy_wire_bytes(&receipt.bytes, alloc)));
            values.insert(recipe.0.0, (KIND_POINTER, super::super::copy_wire_bytes(&content.0.0, alloc)));
            values.insert(other_recipe.0.0, (KIND_POINTER, super::super::copy_wire_bytes(&content.0.0, alloc)));
            values.insert(content.0.0, (KIND_CODE, super::super::copy_wire_bytes(bytes, alloc)));
            values
        };
        let closure = Closure::read(unit, &mut bundle(), heap).unwrap();
        assert_eq!(closure.len(), 3);
        assert_eq!(closure.at(0, owner, symbol, || Some(recipe)), Ok(keyed));
        assert_eq!(closure.at(1, ItemKey(5), Symbol([6; 32]), || None), Ok(Step::Stub));
        assert_eq!(closure.at(2, ItemKey(8), Symbol([8; 32]), || None), Ok(Step::Unkeyed(content)));
        assert_eq!(closure.at(0, owner, symbol, || Some(other_recipe)), Err(Mismatch::Recipe { expected: recipe, actual: other_recipe }));
        assert_eq!(closure.at(0, owner, symbol, || None), Err(Mismatch::MissingRecipe { expected: recipe }));
        assert_eq!(closure.at(0, ItemKey(5), symbol, || Some(recipe)), Err(Mismatch::Item { expected: owner, actual: ItemKey(5) }));
        assert_eq!(closure.at(0, owner, Symbol([6; 32]), || Some(recipe)), Err(Mismatch::Symbol { expected: symbol, actual: Symbol([6; 32]) }));
        assert_eq!(closure.at(1, owner, symbol, || Some(recipe)), Err(Mismatch::Item { expected: ItemKey(5), actual: owner }));
        assert_eq!(closure.at(3, owner, symbol, || Some(recipe)), Err(Mismatch::MissingRow { row: 3, rows: 3 }));

        let recipes = core::cell::Cell::new(0);
        let recipe_of = || { recipes.set(recipes.get() + 1); Some(recipe) };
        assert!(closure.at(0, ItemKey(5), symbol, recipe_of).is_err());
        assert!(closure.at(0, owner, Symbol([6; 32]), recipe_of).is_err());
        assert!(closure.at(3, owner, symbol, recipe_of).is_err());
        assert_eq!(closure.at(1, ItemKey(5), Symbol([6; 32]), recipe_of), Ok(Step::Stub));
        assert_eq!(closure.at(2, ItemKey(8), Symbol([8; 32]), recipe_of), Ok(Step::Unkeyed(content)));
        assert_eq!(recipes.get(), 0, "only an identity-matched keyed row asks for its current recipe");
        assert_eq!(closure.at(0, owner, symbol, recipe_of), Ok(keyed));
        assert_eq!(recipes.get(), 1);

        assert!(Closure::read([9; 32], &mut bundle(), heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&receipt.key).unwrap().1[0] = 9;
        assert!(Closure::read(unit, &mut altered, heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&receipt.key).unwrap().1.pop();
        assert!(Closure::read(unit, &mut altered, heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&receipt.key).unwrap().1[32] = 255;
        assert!(Closure::read(unit, &mut altered, heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&recipe.0.0).unwrap().1[0] ^= 1;
        assert!(Closure::read(unit, &mut altered, heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&content.0.0).unwrap().1[0] ^= 1;
        assert!(Closure::read(unit, &mut altered, heap).is_err());
        let mut altered = bundle();
        altered.remove(&content.0.0);
        assert!(Closure::read(unit, &mut altered, heap).is_err());
    }

    #[test]
    fn mismatch_diagnostics_keep_full_identities_in_bounded_stack_space() {
        struct Text {
            bytes: [u8; 256],
            len: usize,
        }
        impl fmt::Write for Text {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
                self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(text.as_bytes());
                self.len = end;
                Ok(())
            }
        }
        let expected = CodeKey(ContentId([255; 32]));
        let actual = CodeKey(ContentId([254; 32]));
        assert!(core::mem::size_of::<Mismatch>() <= 80, "a refusal owns no variable-length data");
        for mismatch in [
            Mismatch::MissingRow { row: usize::MAX, rows: usize::MAX },
            Mismatch::Item { expected: ItemKey(u128::MAX), actual: ItemKey(u128::MAX) },
            Mismatch::Symbol { expected: Symbol(expected.0.0), actual: Symbol(actual.0.0) },
            Mismatch::MalformedRow,
            Mismatch::MissingRecipe { expected },
            Mismatch::Recipe { expected, actual },
        ] {
            let mut shown = Text { bytes: [0; 256], len: 0 };
            fmt::write(&mut shown, format_args!("{mismatch}")).expect("every cause fits the fixed stack buffer");
            let text = core::str::from_utf8(&shown.bytes[..shown.len]).expect("diagnostics use ASCII names and identities");
            assert!(!text.is_empty());
            if matches!(mismatch, Mismatch::Symbol { .. } | Mismatch::MissingRecipe { .. } | Mismatch::Recipe { .. }) {
                assert!(text.contains("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"), "the published identity is full-width");
            }
            if matches!(mismatch, Mismatch::Symbol { .. } | Mismatch::Recipe { .. }) {
                assert!(text.contains("fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefe"), "the current identity is full-width");
            }
        }
        let mut shown = Text { bytes: [0; 256], len: 0 };
        let zeros = Mismatch::Recipe { expected: CodeKey(ContentId([0; 32])), actual: CodeKey(ContentId([1; 32])) };
        fmt::write(&mut shown, format_args!("{zeros}")).expect("leading-zero identities fit the same stack buffer");
        assert_eq!(
            &shown.bytes[..shown.len],
            b"recipe differs: published 0000000000000000000000000000000000000000000000000000000000000000, current 0101010101010101010101010101010101010101010101010101010101010101",
            "leading zero bytes are retained in both complete identities",
        );
    }
}
