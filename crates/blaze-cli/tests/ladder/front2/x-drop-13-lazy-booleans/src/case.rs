//! Executable oracle for the Reference's "Temporary scopes": each operand of a lazy
//! Boolean expression is a scope. The log records actual destructor calls, not lowered
//! instruction shapes. Global counters keep this independent of borrow-check acceptance.

use crate::Drop;

static mut LOG: u32 = 0;
static mut MADE: u32 = 0;

struct Probe {
    id: u32,
    flag: bool,
}

enum Maybe<T> {
    Some(T),
    None,
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe { LOG = LOG * 10 + self.id };
    }
}

impl Probe {
    fn test(&self) -> bool {
        self.flag
    }

    fn peek(&self) -> Maybe<bool> {
        Maybe::Some(self.flag)
    }
}

fn make(id: u32, flag: bool) -> Probe {
    unsafe { MADE = MADE * 10 + id };
    Probe { id, flag }
}

fn reset() {
    unsafe {
        LOG = 0;
        MADE = 0;
    }
}

fn log() -> u32 {
    unsafe { LOG }
}

fn made() -> u32 {
    unsafe { MADE }
}

fn pair(value: bool, dropped: u32) -> bool {
    value && dropped == 2
}

pub fn check() -> u8 {
    // The left operand dies before the evaluated right operand, for both operators.
    reset();
    let left_or = make(1, false).test() || log() == 1;
    if !left_or || log() != 1 || made() != 1 {
        return 1;
    }
    reset();
    let left_and = make(1, true).test() && log() == 1;
    if !left_and || log() != 1 || made() != 1 {
        return 2;
    }

    // The evaluated right operand dies before a later argument of the enclosing call.
    reset();
    if !pair(false || make(2, true).test(), log()) || log() != 2 {
        return 3;
    }

    // A skipped operand is never constructed or dropped.
    reset();
    let short_or = make(1, true).test() || make(2, false).test();
    if !short_or || made() != 1 || log() != 1 {
        return 4;
    }
    reset();
    let short_and = make(1, false).test() && make(2, true).test();
    if short_and || made() != 1 || log() != 1 {
        return 5;
    }

    // Nested operands end in evaluation order, rather than at the outer statement.
    reset();
    let nested = (make(1, false).test() || make(2, true).test()) && log() == 12;
    if !nested || log() != 12 || made() != 12 {
        return 6;
    }

    // Copy the operand's Boolean result before dropping its enclosing temporary.
    reset();
    let field = make(4, false).flag || log() == 4;
    if !field || log() != 4 {
        return 7;
    }

    // Pattern scrutinees live through the then arm, while ordinary Boolean links do not.
    reset();
    let inside = if let Maybe::Some(value) = make(5, true).peek()
        && value
        && make(6, true).test()
    {
        log()
    } else {
        99
    };
    if inside != 6 || log() != 65 || made() != 56 {
        return 8;
    }
    reset();
    let in_else = if let Maybe::Some(value) = make(7, true).peek()
        && value
        && make(8, false).test()
    {
        99
    } else {
        log()
    };
    if in_else != 87 || log() != 87 || made() != 78 {
        return 9;
    }

    // An operand scope never takes ownership of a named local from the surrounding block.
    reset();
    {
        let held = make(9, false);
        let result = held.test() || log() == 0;
        if !result || log() != 0 {
            return 10;
        }
    }
    if log() != 9 {
        return 11;
    }

    // The same separation applies on a loop back edge and on a failed Boolean link.
    reset();
    let mut turn = 0;
    while let Maybe::Some(value) = make(3, turn == 0).peek()
        && value
        && make(4, true).test()
    {
        if log() != 4 {
            return 12;
        }
        turn = 1;
    }
    if turn != 1 || log() != 433 || made() != 343 {
        return 13;
    }
    0
}
