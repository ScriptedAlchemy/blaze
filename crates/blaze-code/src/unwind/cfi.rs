//! Position-independent AArch64 call-frame information. Object relocations and absolute
//! addresses belong to the object reader and are excluded from this stored program.

use core::fmt;

use blaze_own::AVec;

use super::INST;

const HEAD: usize = 12;
const MAX_BYTES: usize = 64 * 1024;
const MAX_STACK: u32 = 32;

/// One FDE's original extent and the rules shared with its CIE. The return register is x30.
#[derive(Clone, Copy)]
pub struct CfiInput<'a> {
    pub code_len: u32,
    pub code_align: u8,
    pub data_align: i8,
    pub has_lsda: bool,
    pub initial: &'a [u8],
    pub instructions: &'a [u8],
}

/// Verified CFI read in place from a code record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cfi<'a> {
    bytes: &'a [u8],
    initial_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfiError {
    Header,
    Padding,
    Extent { len: u32, code: u32 },
    Operand { at: usize },
    Opcode { at: usize, op: u8 },
    Pc { at: usize },
    State { at: usize },
}

impl fmt::Display for CfiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => f.write_str("invalid machine CFI header, factors, or byte count"),
            Self::Padding => f.write_str("machine CFI has nonzero padding after its payload"),
            Self::Extent { len, code } => write!(f, "machine CFI covers {len} bytes with {code} bytes of record code"),
            Self::Operand { at } => write!(f, "invalid machine CFI operand at byte {at}"),
            Self::Opcode { at, op } => write!(f, "unsupported machine CFI opcode {op:#04x} at byte {at}"),
            Self::Pc { at } => write!(f, "machine CFI advances outside its aligned extent at byte {at}"),
            Self::State { at } => write!(f, "invalid machine CFI state stack at byte {at}"),
        }
    }
}

impl core::error::Error for CfiError {}

impl<'a> Cfi<'a> {
    pub fn read(bytes: &'a [u8], code: u32) -> Result<Self, CfiError> {
        if !(HEAD..=MAX_BYTES).contains(&bytes.len()) || bytes[10] > 1 || bytes[11] != 0 {
            return Err(CfiError::Header);
        }
        let initial_len = usize::try_from(u32::from_le_bytes(bytes[4..8].try_into().unwrap()))
            .map_err(|_| CfiError::Header)?;
        if initial_len > bytes.len() - HEAD {
            return Err(CfiError::Header);
        }
        let cfi = Self { bytes, initial_len };
        validate(CfiInput {
            code_len: cfi.code_len(),
            code_align: cfi.code_align(),
            data_align: cfi.data_align(),
            has_lsda: cfi.has_lsda(),
            initial: cfi.initial(),
            instructions: cfi.instructions(),
        }, code)?;
        Ok(cfi)
    }

    pub fn bytes(self) -> &'a [u8] { self.bytes }
    pub fn code_len(self) -> u32 { u32::from_le_bytes(self.bytes[..4].try_into().unwrap()) }
    pub fn code_align(self) -> u8 { self.bytes[8] }
    pub fn data_align(self) -> i8 { i8::from_le_bytes([self.bytes[9]]) }
    pub fn has_lsda(self) -> bool { self.bytes[10] != 0 }
    pub fn initial(self) -> &'a [u8] { &self.bytes[HEAD..HEAD + self.initial_len] }
    pub fn instructions(self) -> &'a [u8] { &self.bytes[HEAD + self.initial_len..] }
}

/// Replaces the payload only after validation. Written bytes are identical to the adopted rules.
pub fn write(input: CfiInput<'_>, code: u32, out: &mut AVec<'_, u8>) -> Result<(), CfiError> {
    validate(input, code)?;
    let initial_len = u32::try_from(input.initial.len()).map_err(|_| CfiError::Header)?;
    out.clear();
    out.extend_from_slice(&input.code_len.to_le_bytes());
    out.extend_from_slice(&initial_len.to_le_bytes());
    out.extend_from_slice(&[input.code_align, input.data_align.to_le_bytes()[0], u8::from(input.has_lsda), 0]);
    out.extend_from_slice(input.initial);
    out.extend_from_slice(input.instructions);
    Ok(())
}

fn validate(input: CfiInput<'_>, code: u32) -> Result<(), CfiError> {
    if !matches!(input.code_align, 1 | 4)
        || !matches!(input.data_align, -4 | -8)
        || input.initial.len().saturating_add(input.instructions.len()) > MAX_BYTES - HEAD
    {
        return Err(CfiError::Header);
    }
    if input.code_len == 0 || input.code_len > code || input.code_len % INST != 0 {
        return Err(CfiError::Extent { len: input.code_len, code });
    }
    program(input.initial, &input, true)?;
    program(input.instructions, &input, false)
}

struct Operands<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Operands<'_> {
    fn byte(&mut self) -> Option<u8> {
        let value = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(value)
    }

    fn fixed(&mut self, width: usize) -> Option<u32> {
        let mut bytes = [0; 4];
        for byte in &mut bytes[..width] {
            *byte = self.byte()?;
        }
        Some(u32::from_le_bytes(bytes))
    }

    fn uleb(&mut self) -> Option<u32> {
        let mut value = 0u32;
        for shift in [0, 7, 14, 21, 28] {
            let byte = self.byte()?;
            if shift == 28 && byte > 0x0f { return None; }
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 { return Some(value); }
        }
        None
    }

    fn sleb(&mut self) -> Option<i32> {
        let mut value = 0i64;
        for shift in [0, 7, 14, 21, 28] {
            let byte = self.byte()?;
            value |= i64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    value |= !0i64 << (shift + 7);
                }
                return i32::try_from(value).ok();
            }
        }
        None
    }
}

fn register(value: u32) -> bool {
    matches!(value, 0..=31 | 64..=95)
}

fn program(bytes: &[u8], input: &CfiInput<'_>, initial: bool) -> Result<(), CfiError> {
    let base = if initial { HEAD } else { HEAD + input.initial.len() };
    let mut args = Operands { bytes, at: 0 };
    let mut pc = 0u32;
    let mut depth = 0;
    // The caller's stack pointer must be known at every FDE row.
    let mut cfa = !initial;
    while let Some(op) = args.byte() {
        let at = base + args.at - 1;
        let bad = CfiError::Operand { at };
        let offset = |n| i64::from(input.data_align).checked_mul(n).and_then(|x| i32::try_from(x).ok()).is_some();
        let advance = match op {
            0x40..=0x7f => Some(u32::from(op & 0x3f)),
            0x02..=0x04 => Some(args.fixed(1 << usize::from(op - 2)).ok_or(bad)?),
            _ => None,
        };
        if let Some(delta) = advance {
            pc = delta.checked_mul(u32::from(input.code_align))
                .and_then(|n| pc.checked_add(n))
                .filter(|p| !initial && *p <= input.code_len && *p % INST == 0)
                .ok_or(CfiError::Pc { at })?;
            continue;
        }
        let valid = match op {
            0x00 => true,
            0x80..=0xbf => register(u32::from(op & 0x3f)) && offset(i64::from(args.uleb().ok_or(bad)?)),
            0xc0..=0xff => !initial && register(u32::from(op & 0x3f)),
            // offset_extended, val_offset; signed variants and def_cfa_sf.
            0x05 | 0x14 => register(args.uleb().ok_or(bad)?) && offset(i64::from(args.uleb().ok_or(bad)?)),
            0x11 | 0x15 => register(args.uleb().ok_or(bad)?) && offset(i64::from(args.sleb().ok_or(bad)?)),
            // restore_extended, undefined and same_value.
            0x06 => !initial && register(args.uleb().ok_or(bad)?),
            0x07 | 0x08 => register(args.uleb().ok_or(bad)?),
            0x09 => register(args.uleb().ok_or(bad)?) && register(args.uleb().ok_or(bad)?),
            0x0a if !initial && depth < MAX_STACK => { depth += 1; true }
            0x0b if !initial && depth > 0 => { depth -= 1; true }
            0x0a | 0x0b => return Err(CfiError::State { at }),
            0x0c => {
                cfa = register(args.uleb().ok_or(bad)?) && i32::try_from(args.uleb().ok_or(bad)?).is_ok();
                cfa
            }
            0x0d => cfa && register(args.uleb().ok_or(bad)?),
            0x0e => cfa && i32::try_from(args.uleb().ok_or(bad)?).is_ok(),
            0x12 => {
                cfa = register(args.uleb().ok_or(bad)?) && offset(i64::from(args.sleb().ok_or(bad)?));
                cfa
            }
            0x13 => cfa && offset(i64::from(args.sleb().ok_or(bad)?)),
            0x2d => true, // AArch64 negate_ra_state has no operand.
            0x2e => args.uleb().is_some(), // GNU_args_size.
            _ => return Err(CfiError::Opcode { at, op }),
        };
        if !valid { return Err(bad); }
    }
    if depth != 0 || !cfa {
        return Err(CfiError::State { at: base + args.at });
    }
    Ok(())
}
