//! Portable, validated constant parameterization. Original function indices never move.
//! This core produces wasm bytes, not Wasmtime's private native compiler objects.

use blaze_format::cursor::Put;
use blaze_own::{AVec, Alloc};
use wasmparser::{FuncType, Parser, Payload, TypeRef, Validator};

mod body;
use body::Candidate;
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
}

impl From<wasmparser::BinaryReaderError> for Error {
    fn from(value: wasmparser::BinaryReaderError) -> Self { Self::Invalid(value) }
}

/// One equivalent family; order and identity are derived only from the original module.
pub struct Group<'a> {
    members: AVec<'a, Candidate<'a>>,
    varying: AVec<'a, usize>,
    key: [u8; 32],
}

impl Group<'_> {
    pub fn key(&self) -> [u8; 32] { self.key }
    pub fn members(&self) -> impl Iterator<Item = Function> + '_ {
        self.members.iter().map(|member| member.function)
    }
    pub fn parameters(&self) -> usize { self.varying.len() }
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
                    let ty = plan.functions[plan.bodies.len()];
                    match Candidate::read(function, ty, &body, alloc)? {
                        Some(candidate) if numeric(&plan.types[ty as usize]) => candidates.push(candidate),
                        _ => plan.ineligible.push(function),
                    }
                    plan.bodies.push(body.as_bytes());
                }
                // These standard sections carry no code offsets. Unknown custom metadata may
                // carry offsets (DWARF, relocation and source maps); preserving it would lie.
                Payload::CustomSection(reader) => {
                    metadata_safe &= matches!(reader.name(), "name" | "producers" | "target_features");
                }
                _ => {}
            }
        }
        candidates.sort_unstable_by(|a, b| a.shape.cmp(&b.shape).then(a.function.0.cmp(&b.function.0)));
        let mut pending = candidates.into_iter().peekable();
        let input_key = blaze_sha256::digest(input);
        while let Some(first) = pending.next() {
            let mut members = AVec::new_in(alloc);
            members.push(first);
            while pending.peek().is_some_and(|next| next.shape == members[0].shape) {
                if let Some(next) = pending.next() { members.push(next); }
            }
            if members.len() < 2 { continue; }
            let mut varying = AVec::new_in(alloc);
            for slot in 0..members[0].constants.len() {
                if members.iter().any(|member| member.constants[slot] != members[0].constants[slot]) {
                    varying.push(slot);
                }
            }
            let mut key = blaze_sha256::Sha256::new();
            key.update(b"blaze portable wasm fold 1\0");
            key.update(&input_key);
            for member in &members { key.update(&member.function.0.to_le_bytes()); }
            plan.groups.push(Group { members, varying, key: key.finish() });
        }
        plan.groups.sort_unstable_by_key(|group| group.members[0].function.0);
        if !plan.groups.is_empty() && !metadata_safe { return Err(Error::Metadata); }
        Ok(plan)
    }

    pub fn groups(&self) -> &[Group<'a>] { &self.groups }
    /// Explicitly report bodies outside the straight-line numeric proof domain.
    pub fn ineligible(&self) -> &[Function] { &self.ineligible }

    /// Assemble the pure transform. This is not a distributed publication receipt.
    pub fn assemble(&self) -> Result<AVec<'a, u8>, Error> {
        if self.groups.is_empty() { return Ok(AVec::from_slice_in(self.input, self.alloc)); }
        let mut out = AVec::from_slice_in(&self.input[..8], self.alloc);
        let mut extra_types = AVec::new_in(self.alloc);
        let mut extra_functions = AVec::new_in(self.alloc);
        let mut extra_bodies = AVec::new_in(self.alloc);
        let mut replacements = AVec::from_iter_in((0..self.bodies.len()).map(|_| None), self.alloc);
        for (number, group) in self.groups.iter().enumerate() {
            let first = &group.members[0];
            let ty = &self.types[first.ty as usize];
            let template_index = self.imported.checked_add(count(self.functions.len())?)
                .and_then(|index| index.checked_add(u32::try_from(number).ok()?))
                .ok_or(Error::Limit("template function index"))?;
            extra_types.push(0x60);
            extra_types.put_uleb(u64::from(count(ty.params().len() + group.varying.len())?));
            for param in ty.params() { extra_types.push(numeric_byte(*param)?); }
            for &slot in &group.varying { extra_types.push(first.constants[slot].ty()); }
            extra_types.put_uleb(u64::from(count(ty.results().len())?));
            for result in ty.results() { extra_types.push(numeric_byte(*result)?); }
            extra_functions.put_uleb(u64::from(count(self.types.len() + number)?));
            let template = first.template(ty, &group.varying, self.alloc)?;
            extra_bodies.put_bytes_uleb(&template);
            for member in &group.members {
                let thunk = member.thunk(ty, &group.varying, template_index, self.alloc)?;
                replacements[(member.function.0 - self.imported) as usize] = Some(thunk);
            }
        }
        for payload in Parser::new(0).parse_all(self.input) {
            let payload = payload?;
            if let Some((id, range)) = payload.as_section() {
                let raw = &self.input[offset(range.start)?..offset(range.end)?];
                let changed = match id {
                    1 => Some(append(raw, &extra_types, self.groups.len(), self.alloc)?),
                    3 => Some(append(raw, &extra_functions, self.groups.len(), self.alloc)?),
                    10 => {
                        let mut code = AVec::new_in(self.alloc);
                        code.put_uleb(u64::from(count(self.bodies.len() + self.groups.len())?));
                        for (index, raw) in self.bodies.iter().enumerate() {
                            code.put_bytes_uleb(replacements[index].as_deref().unwrap_or(raw));
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
    out.put_bytes(reader.remaining_buffer());
    out.put_bytes(suffix);
    Ok(out)
}

fn count(value: usize) -> Result<u32, Error> { u32::try_from(value).map_err(|_| Error::Limit("wasm count exceeds u32")) }
fn offset(value: u64) -> Result<usize, Error> { usize::try_from(value).map_err(|_| Error::Limit("wasm offset")) }
fn numeric(ty: &FuncType) -> bool { ty.params().iter().chain(ty.results()).all(|ty| numeric_byte(*ty).is_ok()) }
fn numeric_byte(ty: wasmparser::ValType) -> Result<u8, Error> {
    match ty {
        wasmparser::ValType::I32 => Ok(0x7f), wasmparser::ValType::I64 => Ok(0x7e),
        _ => Err(Error::Unsupported("folded signature must use integer values")),
    }
}
