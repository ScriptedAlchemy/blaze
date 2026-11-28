//! Ordered evidence from the final unit's checked walk. A committed publication
//! carries this beside the exact entries it checked. The unit's identity binds the
//! checked input and selected roots, and the committed pack binds the bytes.

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
    ) -> Option<Step> {
        let (rows, _) = self.bytes[32..].as_chunks::<ROW>();
        let row = rows.get(index)?;
        if row[1..17] != tabled.0.to_le_bytes() || row[17..49] != symbol.0 {
            return None;
        }
        let step = step(row)?;
        if let Step::Keyed { recipe, .. } = step && recipe_of() != Some(recipe) {
            return None;
        }
        Some(step)
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
        assert_eq!(closure.at(0, owner, symbol, || Some(recipe)), Some(keyed));
        assert_eq!(closure.at(1, ItemKey(5), Symbol([6; 32]), || None), Some(Step::Stub));
        assert_eq!(closure.at(2, ItemKey(8), Symbol([8; 32]), || None), Some(Step::Unkeyed(content)));
        assert_eq!(closure.at(0, owner, symbol, || Some(other_recipe)), None);
        assert_eq!(closure.at(0, ItemKey(5), symbol, || Some(recipe)), None);
        assert_eq!(closure.at(0, owner, Symbol([6; 32]), || Some(recipe)), None);
        assert_eq!(closure.at(1, owner, symbol, || Some(recipe)), None);
        assert_eq!(closure.at(3, owner, symbol, || Some(recipe)), None);

        assert!(Closure::read([9; 32], &mut bundle(), heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&receipt.key).unwrap().1[0] = 9;
        assert!(Closure::read(unit, &mut altered, heap).is_err());
        let mut altered = bundle();
        altered.get_mut(&receipt.key).unwrap().1.pop();
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
}
