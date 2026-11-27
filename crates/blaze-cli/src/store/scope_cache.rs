//! Portable read witnesses for the signed toolchain set. The drive payload stays under its
//! existing physical key. Only this entry links that key to the pre-work task key.
//! After the form are the toolchain id, task key, scope subject and payload key, then the
//! byte-length-prefixed target triple and the fixed-width witness rows in cell order.

use core::fmt;

use blaze_cas::plain::{Lent, Namespace};
use blaze_core::Driver;
use blaze_drive::record::Stored;
use blaze_drive::{Cell, Key, Kind, Name};
use blaze_engine::{CellId, Row};
use blaze_format::cursor::{self, Reader};
use blaze_ids::ContentId;
use blaze_keyed::kind::ReservedTag;
use blaze_own::{AVec, Alloc};
use blaze_remote::published::set::SetName;
use blaze_scope::expansion_records::Directory;
use blaze_toolchain::ToolchainId;
use bstd::Cx;

use super::{CommandStore, PlaceError, Sharing, Store, StoreAlloc};
use crate::CliError;
use crate::engine_check::EngineCheckError;
use crate::engine_tasks::{EXPORTS, WHOLE};

const MAGIC: &[u8; 4] = b"BSW1";
const DOMAIN: &str = "blaze-cli signed scope candidate 1";
const HEAD_BYTES: usize = MAGIC.len() + 4 * 32 + 1;
const WITNESS_BYTES: usize = 32 + 16 + 32;
const WITNESSES_MOST: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ScopeError {
    Wire(cursor::Error),
    Form,
    Target,
    WitnessLength { bytes: usize },
    Witness { at: usize, owner: Name },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(why) => write!(f, "a signed scope candidate: {why}"),
            Self::Form => f.write_str("a signed scope candidate has an unknown form"),
            Self::Target => f.write_str("a signed scope candidate has an invalid target triple"),
            Self::WitnessLength { bytes } => write!(f, "a signed scope candidate has {bytes} witness bytes outside the supported row geometry"),
            Self::Witness { at, owner } => write!(f, "witness {at} of a signed scope candidate is invalid or out of order at {owner:?}"),
        }
    }
}

impl core::error::Error for ScopeError {}

impl From<cursor::Error> for ScopeError {
    fn from(error: cursor::Error) -> Self { Self::Wire(error) }
}

/// A checked entry body. Both the entry's name and witness owners are `CrateScope` names.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry<'b> {
    pub task: Key,
    pub name: Name,
    pub payload: Key,
    pub toolchain: ToolchainId,
    pub target: &'b str,
    witnesses: &'b [[u8; WITNESS_BYTES]],
}

impl<'b> Entry<'b> {
    pub(crate) fn open(bytes: &'b [u8]) -> Result<Self, ScopeError> {
        let mut reader = Reader::new(bytes);
        if reader.array::<4>()? != MAGIC {
            return Err(ScopeError::Form);
        }
        let toolchain = ToolchainId(*reader.array()?);
        let task = Key(ContentId(*reader.array()?));
        let name = Name::new(Kind::CrateScope, ContentId(*reader.array()?));
        let payload = Key(ContentId(*reader.array()?));
        let length = usize::from(reader.u8()?);
        let target = core::str::from_utf8(reader.take(length)?).map_err(|_| ScopeError::Target)?;
        SetName::new("0", target).map_err(|_| ScopeError::Target)?;
        let witness_bytes = reader.take_rest();
        let (witnesses, tail) = witness_bytes.as_chunks::<WITNESS_BYTES>();
        if !tail.is_empty() || witnesses.len() > WITNESSES_MOST {
            return Err(ScopeError::WitnessLength { bytes: witness_bytes.len() });
        }
        let entry = Self { task, name, payload, toolchain, target, witnesses };
        let mut previous = None;
        for (at, (id, digest)) in entry.reads().enumerate() {
            if !scope_witness(name, id, digest) || previous.is_some_and(|previous| previous >= id) {
                return Err(ScopeError::Witness { at, owner: id.of });
            }
            previous = Some(id);
        }
        Ok(entry)
    }

    pub(crate) fn reads(&self) -> impl ExactSizeIterator<Item = (CellId, ContentId)> + 'b {
        self.witnesses.iter().map(read_witness)
    }
}

fn read_witness(bytes: &[u8; WITNESS_BYTES]) -> (CellId, ContentId) {
    let (subject_bytes, rest) = bytes.split_at(32);
    let (cell_bytes, digest_bytes) = rest.split_at(16);
    let (mut subject, mut cell, mut digest) = ([0; 32], [0; 16], [0; 32]);
    subject.copy_from_slice(subject_bytes);
    cell.copy_from_slice(cell_bytes);
    digest.copy_from_slice(digest_bytes);
    (CellId { of: Name::new(Kind::CrateScope, ContentId(subject)), cell: Cell(cell) }, ContentId(digest))
}

fn scope_witness(scope: Name, id: CellId, digest: ContentId) -> bool {
    id.of.kind == Kind::CrateScope && id.of != scope && matches!(id.cell, WHOLE | EXPORTS) && digest != blaze_drive::key::ABSENT
}

pub(crate) fn entry_key(bytes: &[u8]) -> Key {
    Key(ContentId::of(DOMAIN, bytes))
}

pub(super) fn write<'a>(
    toolchain: ToolchainId,
    target: &str,
    task: Key,
    name: Name,
    payload: Key,
    reads: impl ExactSizeIterator<Item = (CellId, ContentId)>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, ScopeError> {
    let target_length = u8::try_from(target.len()).map_err(|_| ScopeError::Target)?;
    let mut bytes = AVec::with_capacity_in(HEAD_BYTES + target.len() + reads.len() * WITNESS_BYTES, alloc);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&toolchain.0);
    bytes.extend_from_slice(&task.0.0);
    bytes.extend_from_slice(&name.subject.0);
    bytes.extend_from_slice(&payload.0.0);
    bytes.push(target_length);
    bytes.extend_from_slice(target.as_bytes());
    for (input, digest) in reads {
        bytes.extend_from_slice(&input.of.subject.0);
        bytes.extend_from_slice(&input.cell.0);
        bytes.extend_from_slice(&digest.0);
    }
    Ok(bytes)
}

/// Files a candidate beside its verified public owner in the save that keeps the toolchain's
/// records. The namespace check is against held records, including the declared expansions.
#[expect(clippy::too_many_arguments, reason = "a saved scope and the store and request handles that certify it")]
pub(crate) fn keep<D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'_, '_, D, S>,
    lent: &Lent<D, StoreAlloc>,
    toolchain: ToolchainId,
    name: Name,
    row: &Row<'_>,
    payload: Key,
    alloc: Alloc<'_>,
) -> Result<(), CliError> {
    if name.kind != Kind::CrateScope || row.findings != 0 || row.reads.len() > WITNESSES_MOST
        || !row.reads.iter().all(|&(id, digest)| scope_witness(name, id, digest) && store.is_toolchain(id.of))
    {
        return Ok(());
    }
    let Some(held) = lent.get(&blaze_cas::Key(payload.0)).map_err(PlaceError::from)? else { return Ok(()) };
    if held.namespace != Namespace::PUBLIC || !ReservedTag::Drive.is(held.tag) {
        return Ok(());
    }
    let invalid = |what| EngineCheckError::CellTable { task: name, what };
    let Ok(Stored::Whole(view)) = blaze_drive::record::open(held.bytes) else {
        return Err(invalid("the public scope's drive record is unreadable").into());
    };
    if view.name != name || view.source != row.source || view.out != row.out {
        return Err(invalid("the public scope's drive record disagrees with its answer row").into());
    }
    let parts = crate::names::sections::sections(view.payload).map_err(|_| invalid("the public scope's payload sections are unreadable"))?;
    let expansions = Directory::open(parts.expansions).map_err(|_| invalid("the public scope's expansion directory is unreadable"))?;
    let mut previous = None;
    for key in expansions.record_keys() {
        if previous.is_some_and(|previous| previous >= key) {
            return Err(invalid("the public scope's expansion keys are out of order").into());
        }
        previous = Some(key);
        let is_public_expansion = lent.with_bytes(&blaze_cas::Key(key), |tag, namespace, _| {
            namespace == Namespace::PUBLIC && ReservedTag::Expansion.is(tag)
        }).map_err(PlaceError::from)?;
        if is_public_expansion != Some(true) {
            return Ok(());
        }
    }
    let reads = row.reads.iter().copied();
    if reads.clone().zip(reads.clone().skip(1)).any(|(first, second)| first.0 >= second.0) {
        return Err(invalid("the public scope's read witnesses are out of order").into());
    }
    let bytes = write(toolchain, crate::graph::TRIPLE, row.key, name, payload, reads, alloc)
        .map_err(|_| invalid("the public scope's target does not fit the entry"))?;
    let key = entry_key(&bytes);
    store.put_record(cx, key, ReservedTag::ScopeCandidate.tag(), &[held.bytes], &bytes)?;
    store.keep_with_toolchain(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_candidate_round_trips_in_read_order() {
        let frame = blaze_own::testing::bump();
        let toolchain = ToolchainId([1; 32]);
        let target = "aarch64-unknown-linux-gnu";
        let task = Key(ContentId([2; 32]));
        let name = Name::new(Kind::CrateScope, ContentId([3; 32]));
        let payload = Key(ContentId([4; 32]));
        let owner = Name::new(Kind::CrateScope, ContentId([5; 32]));
        let reads = [
            (CellId { of: owner, cell: WHOLE }, ContentId([6; 32])),
            (CellId { of: owner, cell: EXPORTS }, ContentId([7; 32])),
        ];
        let bytes = write(toolchain, target, task, name, payload, reads.into_iter(), frame.into()).expect("write the scope candidate");
        let entry = Entry::open(&bytes).expect("the written scope candidate");
        assert_eq!((entry.toolchain, entry.target, entry.task, entry.name, entry.payload), (toolchain, target, task, name, payload));
        assert!(entry.reads().eq(reads));
        assert!(matches!(Entry::open(&bytes[..bytes.len() - 1]), Err(ScopeError::WitnessLength { .. })));
        let reversed = [reads[1], reads[0]];
        let invalid = write(toolchain, target, task, name, payload, reversed.into_iter(), frame.into()).expect("encode the order control");
        assert!(matches!(Entry::open(&invalid), Err(ScopeError::Witness { at: 1, .. })));
    }
}
