use super::{Error, Function, count, offset};
use blaze_format::cursor::Put;
use blaze_own::{AVec, Alloc};
use wasmtime::wasmparser::{FuncType, FunctionBody, Operator};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Constant { I32(i32), I64(i64) }

impl Constant {
    pub(super) fn ty(self) -> u8 { match self { Self::I32(_) => 0x7f, Self::I64(_) => 0x7e } }
    fn write(self, out: &mut AVec<'_, u8>) {
        match self {
            Self::I32(value) => { out.push(0x41); out.put_sleb(i64::from(value)); }
            Self::I64(value) => { out.push(0x42); out.put_sleb(value); }
        }
    }
}

enum Piece<'a> {
    Raw(&'a [u8]),
    Local { opcode: u8, index: u32 },
    Constant(usize),
}

pub(super) struct Candidate<'a> {
    pub function: Function,
    pub ty: u32,
    pub shape: AVec<'a, u8>,
    pub constants: AVec<'a, Constant>,
    locals: &'a [u8],
    pieces: AVec<'a, Piece<'a>>,
}

impl<'a> Candidate<'a> {
    pub fn read(function: Function, ty: u32, body: &FunctionBody<'a>, alloc: Alloc<'a>) -> Result<Option<Self>, Error> {
        if body.as_bytes().len() > 65536 { return Ok(None); }
        let mut reader = body.get_operators_reader()?;
        let start = body.range().start;
        let locals = body.as_bytes().get(..offset(reader.original_position() - start)?).ok_or(Error::Record("locals range"))?;
        let mut out = Self {
            function, ty, shape: AVec::new_in(alloc), constants: AVec::new_in(alloc),
            locals, pieces: AVec::new_in(alloc),
        };
        out.shape.put_u32(ty);
        out.shape.put_bytes_uleb(locals);
        while !reader.eof() {
            let begin = reader.original_position();
            let op = reader.read()?;
            let raw = body.as_bytes().get(offset(begin - start)?..offset(reader.original_position() - start)?).ok_or(Error::Record("operator range"))?;
            let constant = match op {
                Operator::I32Const { value } => Some(Constant::I32(value)),
                Operator::I64Const { value } => Some(Constant::I64(value)),
                _ => None,
            };
            if let Some(value) = constant {
                if out.constants.len() == 128 { return Ok(None); }
                out.shape.push(1);
                out.shape.push(value.ty());
                out.pieces.push(Piece::Constant(out.constants.len()));
                out.constants.push(value);
                continue;
            }
            let piece = match op {
                Operator::LocalGet { local_index } | Operator::LocalSet { local_index }
                | Operator::LocalTee { local_index } => Piece::Local { opcode: *raw.first().ok_or(Error::Record("local opcode"))?, index: local_index },
                Operator::End if reader.eof() => Piece::Raw(raw),
                // No memory, globals, calls, branches, exceptions or reference instructions:
                // substituting a constant by an immutable parameter preserves operand values.
                Operator::Nop | Operator::Drop | Operator::Select
                | Operator::I32Eqz | Operator::I32Eq | Operator::I32Ne
                | Operator::I32LtS | Operator::I32LtU | Operator::I32GtS | Operator::I32GtU
                | Operator::I32LeS | Operator::I32LeU | Operator::I32GeS | Operator::I32GeU
                | Operator::I64Eqz | Operator::I64Eq | Operator::I64Ne
                | Operator::I64LtS | Operator::I64LtU | Operator::I64GtS | Operator::I64GtU
                | Operator::I64LeS | Operator::I64LeU | Operator::I64GeS | Operator::I64GeU
                | Operator::I32Clz | Operator::I32Ctz | Operator::I32Popcnt
                | Operator::I32Add | Operator::I32Sub | Operator::I32Mul
                | Operator::I32DivS | Operator::I32DivU | Operator::I32RemS | Operator::I32RemU
                | Operator::I32And | Operator::I32Or | Operator::I32Xor
                | Operator::I32Shl | Operator::I32ShrS | Operator::I32ShrU | Operator::I32Rotl | Operator::I32Rotr
                | Operator::I64Clz | Operator::I64Ctz | Operator::I64Popcnt
                | Operator::I64Add | Operator::I64Sub | Operator::I64Mul
                | Operator::I64DivS | Operator::I64DivU | Operator::I64RemS | Operator::I64RemU
                | Operator::I64And | Operator::I64Or | Operator::I64Xor
                | Operator::I64Shl | Operator::I64ShrS | Operator::I64ShrU | Operator::I64Rotl | Operator::I64Rotr
                | Operator::I32WrapI64 | Operator::I64ExtendI32S | Operator::I64ExtendI32U
                | Operator::I32Extend8S | Operator::I32Extend16S
                | Operator::I64Extend8S | Operator::I64Extend16S | Operator::I64Extend32S => Piece::Raw(raw),
                _ => return Ok(None),
            };
            out.shape.push(0);
            out.shape.put_bytes_uleb(raw);
            out.pieces.push(piece);
        }
        Ok(Some(out))
    }

    pub fn template(&self, ty: &FuncType, varying: &[usize], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Error> {
        let params = count(ty.params().len())?;
        let extra = count(varying.len())?;
        let mut out = AVec::from_slice_in(self.locals, alloc);
        for piece in &self.pieces {
            match piece {
                Piece::Raw(raw) => out.put_bytes(raw),
                Piece::Local { opcode, index } => {
                    let index = if *index < params { *index } else {
                        index.checked_add(extra).ok_or(Error::Limit("template local index"))?
                    };
                    out.push(*opcode);
                    out.put_uleb(u64::from(index));
                }
                Piece::Constant(slot) => match varying.iter().position(|other| other == slot) {
                    Some(parameter) => {
                        let index = params.checked_add(count(parameter)?).ok_or(Error::Limit("constant parameter index"))?;
                        out.push(0x20);
                        out.put_uleb(u64::from(index));
                    }
                    None => self.constant(*slot)?.write(&mut out),
                },
            }
        }
        Ok(out)
    }

    pub fn thunk(&self, ty: &FuncType, varying: &[usize], template: u32, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Error> {
        // Keep original locals so local-name indices stay meaningful even on the thunk.
        let mut out = AVec::from_slice_in(self.locals, alloc);
        for parameter in 0..count(ty.params().len())? {
            out.push(0x20);
            out.put_uleb(u64::from(parameter));
        }
        for &slot in varying { self.constant(slot)?.write(&mut out); }
        out.push(0x10);
        out.put_uleb(u64::from(template));
        out.push(0x0b);
        Ok(out)
    }

    pub fn constant(&self, slot: usize) -> Result<Constant, Error> {
        self.constants.get(slot).copied().ok_or(Error::Record("constant parameter slot"))
    }
}
