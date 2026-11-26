//! One portable body per granted work name. A group consists of one template and its thunks.
use super::{Error, Plan, count};
use blaze_format::cursor::Put;
use blaze_own::AVec;

const MAGIC: &[u8; 4] = b"BWF1";
const HEADER: usize = 41;

/// An immutable recipe tied to one canonical group; callers cannot invent its indices.
pub struct Work {
    group: usize,
    member: Option<usize>,
    proof: [u8; 32],
    key: [u8; 32],
    function: u32,
}

impl Work {
    pub fn key(&self) -> [u8; 32] { self.key }
}

/// Actual produced portable body bytes, not a plan or a cache-hit receipt.
pub struct Record<'a> {
    pub key: [u8; 32],
    pub bytes: AVec<'a, u8>,
}

impl Record<'_> {
    pub fn view(&self) -> RecordView<'_> { RecordView { key: self.key, bytes: &self.bytes } }
}

#[derive(Clone, Copy)]
pub struct RecordView<'a> {
    pub key: [u8; 32],
    pub bytes: &'a [u8],
}

impl<'a> Plan<'a> {
    /// Sorted work names. Templates and thunks are genuinely separate outputs and grants.
    pub fn work(&self) -> Result<AVec<'a, Work>, Error> {
        let mut out = AVec::new_in(self.alloc);
        for (group, family) in self.groups.iter().enumerate() {
            out.push(self.template_work(group)?);
            for member in 0..family.members.len() { out.push(self.member_work(group, member)?); }
        }
        out.sort_unstable_by_key(Work::key);
        Ok(out)
    }

    pub(super) fn template_work(&self, group: usize) -> Result<Work, Error> { self.make_work(group, None) }
    pub(super) fn member_work(&self, group: usize, member: usize) -> Result<Work, Error> { self.make_work(group, Some(member)) }

    fn make_work(&self, group: usize, member: Option<usize>) -> Result<Work, Error> {
        let family = self.groups.get(group).ok_or(Error::Record("unknown fold group"))?;
        let function = match member {
            Some(member) => family.members.get(member).ok_or(Error::Record("unknown fold member"))?.function.0,
            None => self.imported.checked_add(count(self.functions.len())?)
                .and_then(|index| index.checked_add(u32::try_from(group).ok()?))
                .ok_or(Error::Limit("template function index"))?,
        };
        let mut key = blaze_sha256::Sha256::new();
        key.update(b"blaze portable wasm body 1\0");
        key.update(&family.key);
        key.update(&[u8::from(member.is_some())]);
        key.update(&function.to_le_bytes());
        Ok(Work { group, member, proof: family.key, key: key.finish(), function })
    }

    /// Called only for work held by this executor. No other body is rendered by this call.
    pub fn emit(&self, work: &Work) -> Result<Record<'a>, Error> {
        let expected = self.make_work(work.group, work.member)?;
        if expected.key != work.key { return Err(Error::Record("work belongs to another module or recipe")); }
        let group = &self.groups[work.group];
        let first = &group.members[0];
        let ty = &self.types[first.ty as usize];
        let body = match work.member {
            None => first.template(ty, &group.varying, self.alloc)?,
            Some(member) => group.members[member].thunk(ty, &group.varying, self.template_work(work.group)?.function, self.alloc)?,
        };
        let mut bytes = AVec::new_in(self.alloc);
        bytes.put_bytes(MAGIC);
        bytes.put_bytes(&work.proof);
        bytes.push(u8::from(work.member.is_some()));
        bytes.put_u32(work.function);
        bytes.put_bytes(&body);
        Ok(Record { key: work.key, bytes })
    }
}

pub(super) fn body<'r>(records: &[RecordView<'r>], work: &Work) -> Result<&'r [u8], Error> {
    let found = records.binary_search_by_key(&work.key, |record| record.key)
        .map_err(|_| Error::Record("required template or member thunk is missing"))?;
    let bytes = records[found].bytes;
    if bytes.len() <= HEADER || &bytes[..4] != MAGIC || bytes[4..36] != work.proof
        || bytes[36] != u8::from(work.member.is_some()) || bytes[37..41] != work.function.to_le_bytes()
    { return Err(Error::Record("body record does not name its expected group, role and function")); }
    Ok(&bytes[HEADER..])
}
