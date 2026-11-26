//! Portable, validated constant parameterization. Original function indices never move.
//! This core produces wasm bytes, not Wasmtime's private native compiler objects.

use blaze_format::cursor::Put;
use blaze_own::{AVec, Alloc};
use wasmtime::wasmparser::{self, FuncType, Parser, Payload, TypeRef, Validator};

mod body;
use body::Candidate;
mod records;
pub use records::{Record, RecordView, Work};
#[cfg(test)]
mod tests;

/// A function's position in the complete wasm function index space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function(pub u32);

#[derive(Debug)]
pub enum Error {
    Invalid(wasmparser::BinaryReaderError),
    Limit(&'static str),
    Metadata,
    Unsupported(&'static str),
    Record(&'static str),
    Cursor(blaze_format::cursor::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "portable wasm folding: {error}"),
            Self::Limit(why) | Self::Unsupported(why) | Self::Record(why) => write!(f, "portable wasm folding: {why}"),
            Self::Metadata => f.write_str("portable wasm folding: custom metadata may contain code offsets"),
            Self::Cursor(error) => write!(f, "portable wasm body record: {error}"),
        }
    }
}
impl core::error::Error for Error {}
impl From<blaze_format::cursor::Error> for Error {
    fn from(error: blaze_format::cursor::Error) -> Self { Self::Cursor(error) }
}

impl From<wasmparser::BinaryReaderError> for Error {
    fn from(value: wasmparser::BinaryReaderError) -> Self { Self::Invalid(value) }
}

/// One equivalent family; order and identity are derived only from the original module.
pub struct Group<'a> {
    first: Candidate<'a>,
    rest: AVec<'a, Candidate<'a>>,
    varying: AVec<'a, usize>,
    key: [u8; 32],
}

impl<'a> Group<'a> {
    pub fn key(&self) -> [u8; 32] { self.key }
    pub fn members(&self) -> impl Iterator<Item = Function> + '_ {
        self.candidates().map(|member| member.function)
    }
    pub fn parameters(&self) -> usize { self.varying.len() }
    fn candidates(&self) -> impl Iterator<Item = &Candidate<'a>> { core::iter::once(&self.first).chain(self.rest.iter()) }
    fn member_count(&self) -> usize { 1 + self.rest.len() }
    fn member(&self, index: usize) -> Result<&Candidate<'a>, Error> {
        if index == 0 { Ok(&self.first) } else { self.rest.get(index - 1).ok_or(Error::Record("unknown group member")) }
    }
}

/// Validated input plus a deterministic partition. No IO, cache access or grant is implied.
pub struct Plan<'a> {
    input: &'a [u8],
    types: AVec<'a, FuncType>,
    functions: AVec<'a, u32>,
    imported: u32,
    bodies: AVec<'a, &'a [u8]>,
    groups: AVec<'a, Group<'a>>,
    ineligible: AVec<'a, Function>,
    alloc: Alloc<'a>,
}

impl<'a> Plan<'a> {
    /// Validate before interpreting instructions. Resource caps apply before any rewrite.
    pub fn new(input: &'a [u8], alloc: Alloc<'a>) -> Result<Self, Error> {
        if input.len() > 128 * 1024 * 1024 { return Err(Error::Limit("wasm module exceeds 128 MiB")); }
        Validator::new().validate_all(input)?;
        let mut plan = Self {
            input, types: AVec::new_in(alloc), functions: AVec::new_in(alloc), imported: 0,
            bodies: AVec::new_in(alloc), groups: AVec::new_in(alloc),
            ineligible: AVec::new_in(alloc), alloc,
        };
        let mut candidates = AVec::new_in(alloc);
        let mut metadata_safe = true;
        for payload in Parser::new(0).parse_all(input) {
            match payload? {
                Payload::Version { encoding: wasmparser::Encoding::Module, .. } => {}
                Payload::Version { .. } => return Err(Error::Unsupported("expected a core wasm module")),
                Payload::TypeSection(reader) => {
                    for ty in reader.into_iter_err_on_gc_types() { plan.types.push(ty?); }
                }
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        if matches!(import?.ty, TypeRef::Func(_) | TypeRef::FuncExact(_)) {
                            plan.imported = plan.imported.checked_add(1).ok_or(Error::Limit("function index"))?;
                        }
                    }
                }
                Payload::FunctionSection(reader) => {
                    if reader.count() > 100_000 { return Err(Error::Limit("over 100000 wasm functions")); }
                    for index in reader { plan.functions.push(index?); }
                }
                Payload::CodeSectionEntry(body) => {
                    let defined = count(plan.bodies.len())?;
                    let function = Function(plan.imported.checked_add(defined).ok_or(Error::Limit("function index"))?);
                    let ty = *plan.functions.get(plan.bodies.len()).ok_or(Error::Record("missing function type"))?;
                    let signature = plan.types.get(index(ty)?).ok_or(Error::Record("unknown function type"))?;
                    match Candidate::read(function, ty, &body, alloc)? {
                        Some(candidate) if numeric(signature) => candidates.push(candidate),
                        _ => plan.ineligible.push(function),
                    }
                    plan.bodies.push(body.as_bytes());
                }
                // These standard sections carry no code offsets. Unknown custom metadata may
                // carry offsets (DWARF, relocation and source maps); preserving it would lie.
                Payload::CustomSection(reader) => {
                    // `.rustc` is Rust's embedded crate metadata, not code-offset debug data.
                    metadata_safe &= matches!(reader.name(), ".rustc" | "name" | "producers" | "target_features");
                }
                _ => {}
            }
        }
        candidates.sort_unstable_by(|a, b| a.shape.cmp(&b.shape).then(a.function.0.cmp(&b.function.0)));
        let mut pending = candidates.into_iter().peekable();
        let input_key = blaze_sha256::digest(input);
        while let Some(first) = pending.next() {
            let mut rest = AVec::new_in(alloc);
            while pending.peek().is_some_and(|next| next.shape == first.shape) {
                if let Some(next) = pending.next() { rest.push(next); }
            }
            if rest.is_empty() { continue; }
            let mut varying = AVec::new_in(alloc);
            for (slot, value) in first.constants.iter().enumerate() {
                if rest.iter().any(|member| member.constants.get(slot) != Some(value)) {
                    varying.push(slot);
                }
            }
            let mut key = blaze_sha256::Sha256::new();
            key.update(b"blaze portable wasm fold 1\0");
            key.update(&input_key);
            for member in core::iter::once(&first).chain(rest.iter()) { key.update(&member.function.0.to_le_bytes()); }
            plan.groups.push(Group { first, rest, varying, key: key.finish() });
        }
        plan.groups.sort_unstable_by_key(|group| group.first.function.0);
        if !plan.groups.is_empty() && !metadata_safe { return Err(Error::Metadata); }
        Ok(plan)
    }

    pub fn groups(&self) -> &[Group<'a>] { &self.groups }
    /// Explicitly report bodies outside the straight-line numeric proof domain.
    pub fn ineligible(&self) -> &[Function] { &self.ineligible }

    /// Assemble the pure transform. This is not a distributed publication receipt.
    pub fn assemble(&self) -> Result<AVec<'a, u8>, Error> {
        let mut records = AVec::new_in(self.alloc);
        for work in self.work()? { records.push(self.emit(&work)?); }
        records.sort_unstable_by_key(|record| record.key);
        let views = AVec::from_iter_in(records.iter().map(Record::view), self.alloc);
        self.assemble_records(&views)
    }

    /// Assemble only a complete, canonical set of records of this input and recipe.
    /// The transport must authenticate and digest-check the records before calling this.
    pub fn assemble_records(&self, records: &[RecordView<'_>]) -> Result<AVec<'a, u8>, Error> {
        let expected = self.groups.iter().try_fold(0usize, |count, group| {
            count.checked_add(group.member_count() + 1).ok_or(Error::Limit("fold output count"))
        })?;
        if records.len() != expected || records.iter().zip(records.iter().skip(1)).any(|(a, b)| a.key >= b.key) {
            return Err(Error::Record("output set is missing, duplicated, extraneous or unsorted"));
        }
        if self.groups.is_empty() { return Ok(AVec::from_slice_in(self.input, self.alloc)); }
        let mut out = AVec::from_slice_in(self.input.get(..8).ok_or(Error::Record("wasm header"))?, self.alloc);
        let mut extra_types = AVec::new_in(self.alloc);
        let mut extra_functions = AVec::new_in(self.alloc);
        let mut extra_bodies = AVec::new_in(self.alloc);
        let mut replacements = AVec::from_iter_in((0..self.bodies.len()).map(|_| None), self.alloc);
        for (number, group) in self.groups.iter().enumerate() {
            let first = &group.first;
            let ty = self.types.get(index(first.ty)?).ok_or(Error::Record("template type"))?;
            extra_types.push(0x60);
            extra_types.put_uleb(u64::from(count(ty.params().len() + group.varying.len())?));
            for param in ty.params() { extra_types.push(numeric_byte(*param)?); }
            for &slot in &group.varying { extra_types.push(first.constant(slot)?.ty()); }
            extra_types.put_uleb(u64::from(count(ty.results().len())?));
            for result in ty.results() { extra_types.push(numeric_byte(*result)?); }
            extra_functions.put_uleb(u64::from(count(self.types.len() + number)?));
            let template = records::body(records, &self.template_work(number)?)?;
            extra_bodies.put_bytes_uleb(template);
            for (member_index, member) in group.candidates().enumerate() {
                let thunk = records::body(records, &self.member_work(number, member_index)?)?;
                let defined = member.function.0.checked_sub(self.imported).ok_or(Error::Record("imported fold member"))?;
                *replacements.get_mut(index(defined)?).ok_or(Error::Record("fold member body"))? = Some(thunk);
            }
        }
        for payload in Parser::new(0).parse_all(self.input) {
            let payload = payload?;
            if let Some((id, range)) = payload.as_section() {
                let raw = self.input.get(offset(range.start)?..offset(range.end)?).ok_or(Error::Record("section range"))?;
                let changed = match id {
                    1 => Some(append(raw, &extra_types, self.groups.len(), self.alloc)?),
                    3 => Some(append(raw, &extra_functions, self.groups.len(), self.alloc)?),
                    10 => {
                        let mut code = AVec::new_in(self.alloc);
                        code.put_uleb(u64::from(count(self.bodies.len() + self.groups.len())?));
                        for (replacement, raw) in replacements.iter().zip(self.bodies.iter()) {
                            code.put_bytes_uleb(replacement.unwrap_or(raw));
                        }
                        code.put_bytes(&extra_bodies);
                        Some(code)
                    }
                    _ => None,
                };
                out.push(id);
                out.put_bytes_uleb(changed.as_deref().unwrap_or(raw));
            }
        }
        Validator::new().validate_all(&out)?;
        Ok(out)
    }
}

fn append<'a>(raw: &[u8], suffix: &[u8], extra: usize, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Error> {
    let mut reader = wasmparser::BinaryReader::new(raw, 0);
    let old = reader.read_var_u32()?;
    let total = old.checked_add(count(extra)?).ok_or(Error::Limit("section entries"))?;
    let mut out = AVec::new_in(alloc);
    out.put_uleb(u64::from(total));
    out.put_bytes(reader.read_bytes(reader.bytes_remaining())?);
    out.put_bytes(suffix);
    Ok(out)
}

fn count(value: usize) -> Result<u32, Error> { u32::try_from(value).map_err(|_| Error::Limit("wasm count exceeds u32")) }
fn offset(value: u64) -> Result<usize, Error> { usize::try_from(value).map_err(|_| Error::Limit("wasm offset")) }
fn index(value: u32) -> Result<usize, Error> { usize::try_from(value).map_err(|_| Error::Limit("wasm index")) }
fn numeric(ty: &FuncType) -> bool { ty.params().iter().chain(ty.results()).all(|ty| numeric_byte(*ty).is_ok()) }
fn numeric_byte(ty: wasmparser::ValType) -> Result<u8, Error> {
    match ty {
        wasmparser::ValType::I32 => Ok(0x7f), wasmparser::ValType::I64 => Ok(0x7e),
        _ => Err(Error::Unsupported("folded signature must use integer values")),
    }
}
