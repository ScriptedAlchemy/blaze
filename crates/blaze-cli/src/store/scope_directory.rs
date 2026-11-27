//! The task-key directory of an adopted signed set. Its local key belongs to the receipt for
//! that exact head. Only the whole-set verifier makes a directory.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::Lent;
use blaze_core::Driver;
use blaze_drive::Key;
use blaze_drive::record::{Stored, WholeView};
use blaze_ids::ContentId;
use blaze_keyed::kind::ReservedTag;
use blaze_scope::expansion_records;

use super::StoreAlloc;
use super::plain_place::{PlaceError, PlainPlace};
use super::scope_cache::{Entry, entry_key};
use super::take::TakeError;

pub(super) const ENTRIES_MOST: usize = 4096;
const ROW_BYTES: usize = 64;
const DOMAIN: &str = "blaze-cli adopted scope directory 1";

#[derive(Clone, Copy)]
pub(super) struct Directory<'m> {
    rows: &'m [[u8; ROW_BYTES]],
}

impl<'m> Directory<'m> {
    /// The directory was built in sorted order by `keep` and reached through its receipt.
    pub(super) fn load<D: Driver>(lent: &'m Lent<D, StoreAlloc>, key: Key) -> Result<Self, TakeError> {
        let held = lent.get(&CasKey(key.0)).map_err(place_error)?
            .ok_or_else(|| refused(key, "the receipt's scope directory is missing"))?;
        let (rows, trailing) = held.bytes.as_chunks::<ROW_BYTES>();
        if !ReservedTag::ScopeDirectory.is(held.tag) || !trailing.is_empty() || rows.is_empty() || rows.len() > ENTRIES_MOST {
            return Err(refused(key, "the receipt's scope directory has the wrong shape"));
        }
        Ok(Self { rows })
    }

    pub(super) fn find(self, task: Key) -> Option<Key> {
        let at = self.rows.binary_search_by(|row| row.split_at(32).0.cmp(&task.0.0)).ok()?;
        Some(Key(ContentId(*self.rows.get(at)?.last_chunk()?)))
    }
}

pub(super) fn refused(key: Key, why: &'static str) -> TakeError {
    TakeError::Scope { key, why }
}

fn place_error(why: blaze_cas::plain::LentError) -> TakeError {
    TakeError::Place(PlaceError::from(why))
}

/// Opens a scope named by an authenticated entry under its existing drive record format.
pub(super) fn wire<'m, D: Driver>(lent: &'m Lent<D, StoreAlloc>, entry: &Entry<'_>) -> Result<WholeView<'m>, TakeError> {
    let held = lent.get(&CasKey(entry.payload.0)).map_err(place_error)?
        .ok_or_else(|| refused(entry.payload, "the indexed scope payload is missing"))?;
    let Ok(Stored::Whole(wire)) = blaze_drive::record::open(held.bytes) else {
        return Err(refused(entry.payload, "the indexed scope payload is not a whole drive record"));
    };
    if !ReservedTag::Drive.is(held.tag) || wire.name != entry.name || wire.finding_count() != 0 {
        return Err(refused(entry.payload, "the scope payload does not match the entry"));
    }
    Ok(wire)
}

/// Verifies closure in the selected signed generation and reconstructs the local expansion
/// reach index of each scope using the keys its payload actually names.
fn keep_reach<D: Driver>(lent: &Lent<D, StoreAlloc>, entry: &Entry<'_>, signed: &[CasKey]) -> Result<Option<CasKey>, TakeError> {
    if signed.binary_search(&CasKey(entry.payload.0)).is_err() {
        return Err(refused(entry.payload, "the scope payload is outside the signed generation"));
    }
    let held = wire(lent, entry)?;
    if crate::engine_store::record_key_from(entry.task, held.out, held.reads(), held.payload) != entry.payload {
        return Err(refused(entry.payload, "the scope payload's key does not match its task and bytes"));
    }
    let parts = crate::names::sections::sections(held.payload)
        .map_err(|_| refused(entry.payload, "the scope payload has malformed sections"))?;
    let expansions = expansion_records::Directory::open(parts.expansions)
        .map_err(|_| refused(entry.payload, "the scope payload has a malformed expansion directory"))?;
    let count = expansions.record_keys().len();
    if count > blaze_cas::plain::INDEX_KEYS_MOST {
        return Err(refused(entry.payload, "the scope's expansion directory exceeds the reach index limit"));
    }
    let mut keys = blaze_own::Vec::with_capacity_in(count, *lent.alloc());
    for expansion in expansions.record_keys().map(CasKey) {
        if keys.last().is_some_and(|last| *last >= expansion) || signed.binary_search(&expansion).is_err() {
            return Err(refused(Key(expansion.0), "the scope expansion is unordered or outside the signed generation"));
        }
        let matches = lent.with_bytes(&expansion, |tag, _, bytes| {
            ReservedTag::Expansion.is(tag) && expansion_records::record_key(bytes) == expansion.0
        }).map_err(place_error)?;
        if matches != Some(true) {
            return Err(refused(Key(expansion.0), "the scope expansion's tag or bytes do not match its key"));
        }
        keys.push(expansion);
    }
    if keys.is_empty() {
        return Ok(None);
    }
    lent.index_beside(CasKey(entry.payload.0), &keys).map(Some).map_err(place_error)
}

/// The whole set has been checked. The directory and all its references join the same toolchain
/// root and commit as the receipt.
pub(super) fn keep<D: Driver>(place: &mut PlainPlace<'_, D>, rows: &mut [(Key, Key)], kept: &mut blaze_own::Vec<CasKey, StoreAlloc>) -> Result<Option<Key>, TakeError> {
    if rows.is_empty() {
        return Ok(None);
    }
    rows.sort_unstable();
    if let Some(pair) = rows.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(refused(pair[0].0, "the signed generation contains multiple entries for this task"));
    }
    let lent = place.lent();
    let mut bytes = blaze_own::Vec::with_capacity_in(rows.len() * ROW_BYTES, *lent.alloc());
    let mut reach = blaze_own::Vec::with_capacity_in(rows.len(), *lent.alloc());
    for &(task, candidate) in rows.iter() {
        let held = lent.get(&CasKey(candidate.0)).map_err(place_error)?
            .ok_or_else(|| refused(candidate, "the verified scope entry is missing"))?;
        let entry = Entry::open(held.bytes).map_err(|_| refused(candidate, "the verified scope entry is malformed"))?;
        if let Some(index) = keep_reach(lent, &entry, kept)? {
            reach.push(index);
        }
        bytes.extend_from_slice(&task.0.0);
        bytes.extend_from_slice(&candidate.0.0);
    }
    let key = Key(ContentId::of(DOMAIN, &bytes));
    let put = place.put_record(key, false, ReservedTag::ScopeDirectory.tag(), &bytes).map_err(TakeError::Place)?;
    if put == blaze_keyed::store::Put::Other {
        return Err(TakeError::TwoValues { key: key.0 });
    }
    kept.extend_from_slice(&reach);
    kept.push(CasKey(key.0));
    kept.sort_unstable();
    kept.dedup();
    Ok(Some(key))
}

pub(super) fn read_entry<'m, D: Driver>(lent: &'m Lent<D, StoreAlloc>, key: Key) -> Result<Entry<'m>, TakeError> {
    let held = lent.get(&CasKey(key.0)).map_err(place_error)?
        .ok_or_else(|| refused(key, "the directory's scope entry is missing"))?;
    if !ReservedTag::ScopeCandidate.is(held.tag) {
        return Err(refused(key, "the directory's scope entry has the wrong tag"));
    }
    Entry::open(held.bytes).map_err(|_| refused(key, "the directory's scope entry is malformed"))
}

pub(super) fn check_entry(bytes: &[u8], key: Key, set: ContentId, toolchain: blaze_toolchain::ToolchainId) -> Result<Key, TakeError> {
    let entry = Entry::open(bytes).map_err(|_| refused(key, "the signed scope entry is malformed"))?;
    let hex = toolchain.hex();
    let named = blaze_remote::published::set::SetName::new(hex.as_str(), entry.target).map_err(TakeError::Set)?;
    if entry_key(bytes) != key || entry.toolchain != toolchain || named.id() != set {
        return Err(refused(key, "the signed scope entry has the wrong key, toolchain or target"));
    }
    Ok(entry.task)
}
