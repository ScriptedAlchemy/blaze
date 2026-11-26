//! Bind item-relative trap sources before LLVM can move, duplicate, inline, or delete a trap.
//! LLVM's side-effecting AArch64 break intrinsic carries the ID in its immediate operand.
//! Only the object emitted for this task uses these IDs. The reader replaces each live ID
//! with the semantic unreachable immediate and writes its ordinary trap row.

use blaze_code::{
    record::{Symbol, TrapSource},
    trap::UNREACHABLE,
};
use blaze_own::{AVec, Alloc, FixedString};
use core::{fmt, ops::Range};

use super::LlvmModule;

pub(super) struct Operand(pub Range<usize>);

const FIRST: u32 = UNREACHABLE + 1;
const CAPACITY: usize = 0x1_0000 - FIRST as usize;
const SEMANTIC_DIGITS: &[u8; 5] = b"65282";
const _: () = assert!(UNREACHABLE == 65282);

#[derive(Clone, Copy)]
pub(super) struct Tag(u32);

impl Tag {
    pub(super) fn of_immediate(immediate: u32) -> Option<Self> {
        (FIRST..=u32::from(u16::MAX)).contains(&immediate).then_some(Self(immediate))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TrapError {
    Ordinal { symbol: Symbol, ordinal: u32 },
    Repeated { symbol: Symbol, ordinal: u32 },
    Operand { symbol: Symbol, ordinal: u32 },
    Capacity { symbol: Symbol, limit: usize },
}

impl fmt::Display for TrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ordinal { symbol, ordinal } => write!(f, "{symbol} has a trap source for missing unreachable {ordinal}"),
            Self::Repeated { symbol, ordinal } => write!(f, "{symbol} has two trap sources for unreachable {ordinal}"),
            Self::Operand { symbol, ordinal } => write!(f, "{symbol} lost the LLVM break operand for unreachable {ordinal}"),
            Self::Capacity { symbol, limit } => write!(f, "linking {symbol} exceeds {limit} item-relative trap sources in one LLVM emission"),
        }
    }
}

impl core::error::Error for TrapError {}

pub(super) struct Tags<'a> {
    sources: AVec<'a, TrapSource>,
    alloc: Alloc<'a>,
}

impl<'a> Tags<'a> {
    pub(super) fn new(alloc: Alloc<'a>) -> Self {
        Self { sources: AVec::new_in(alloc), alloc }
    }

    /// Returns a copy with provenance operands replaced only for modules carrying sources.
    /// Each original immediate and every temporary ID occupy exactly five decimal digits.
    pub(super) fn prepare(&mut self, module: &LlvmModule<'_>) -> Result<Option<AVec<'a, u8>>, TrapError> {
        if module.traps.is_empty() {
            return Ok(None);
        }
        let symbol = module.symbol;
        if module.traps.len() > CAPACITY - self.sources.len() {
            return Err(TrapError::Capacity { symbol, limit: CAPACITY });
        }
        let mut sources = AVec::from_slice_in(&module.traps, self.alloc);
        sources.sort_unstable_by_key(|source| source.ordinal);
        if let Some(pair) = sources.windows(2).find(|pair| pair[0].ordinal == pair[1].ordinal) {
            return Err(TrapError::Repeated { symbol, ordinal: pair[0].ordinal });
        }
        let mut ir = AVec::from_slice_in(&module.ir, self.alloc);
        for source in sources {
            let ordinal = source.ordinal;
            let operand = usize::try_from(ordinal)
                .ok()
                .and_then(|index| module.unreachable_operands.get(index))
                .ok_or(TrapError::Ordinal { symbol, ordinal })?;
            let digits = ir.get_mut(operand.0.clone()).ok_or(TrapError::Operand { symbol, ordinal })?;
            if &*digits != SEMANTIC_DIGITS {
                return Err(TrapError::Operand { symbol, ordinal });
            }
            let id = FIRST + u32::try_from(self.sources.len()).expect("bounded trap source count fits in u32");
            let mut spelling = FixedString::<5>::new();
            use fmt::Write as _;
            write!(&mut spelling, "{id}").expect("a BRK immediate has at most five decimal digits");
            digits.copy_from_slice(spelling.as_bytes());
            self.sources.push(source);
        }
        Ok(Some(ir))
    }

    pub(super) fn source(&self, tag: Tag) -> Option<&TrapSource> {
        self.sources.get(usize::try_from(tag.0 - FIRST).ok()?)
    }
}
