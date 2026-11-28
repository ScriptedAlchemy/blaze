//! Executable oracle for the Reference's "Temporary scopes": each operand of a lazy
//! Boolean expression is a scope. The log records actual destructor calls, not lowered
//! instruction shapes. Stack-owned Cells let the strict source gate check the oracle.

use core::cell::Cell;
use crate::Drop;

struct Counters {
    log: Cell<u32>,
    made: Cell<u32>,
}

struct Probe<'a> {
    counters: &'a Counters,
    id: u32,
    flag: bool,
}

enum Maybe<T> {
    Some(T),
    None,
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        self.counters.log.set(self.counters.log.get() * 10 + self.id);
    }
}

impl Probe<'_> {
    fn test(&self) -> bool {
        self.flag
    }

    fn peek(&self) -> Maybe<bool> {
        Maybe::Some(self.flag)
    }
}

fn make(counters: &Counters, id: u32, flag: bool) -> Probe<'_> {
    counters.made.set(counters.made.get() * 10 + id);
    Probe { counters, id, flag }
}

fn reset(counters: &Counters) {
    counters.log.set(0);
    counters.made.set(0);
}

fn log(counters: &Counters) -> u32 {
    counters.log.get()
}

fn made(counters: &Counters) -> u32 {
    counters.made.get()
}

fn pair(value: bool, dropped: u32) -> bool {
    value && dropped == 2
}

pub fn check() -> u8 {
    let counters = Counters { log: Cell::new(0), made: Cell::new(0) };

    // The left operand dies before the evaluated right operand, for both operators.
    reset(&counters);
    let left_or = make(&counters, 1, false).test() || log(&counters) == 1;
    if !left_or || log(&counters) != 1 || made(&counters) != 1 {
        return 1;
    }
    reset(&counters);
    let left_and = make(&counters, 1, true).test() && log(&counters) == 1;
    if !left_and || log(&counters) != 1 || made(&counters) != 1 {
        return 2;
    }

    // The evaluated right operand dies before a later argument of the enclosing call.
    reset(&counters);
    if !pair(false || make(&counters, 2, true).test(), log(&counters)) || log(&counters) != 2 {
        return 3;
    }

    // A skipped operand is never constructed or dropped.
    reset(&counters);
    let short_or = make(&counters, 1, true).test() || make(&counters, 2, false).test();
    if !short_or || made(&counters) != 1 || log(&counters) != 1 {
        return 4;
    }
    reset(&counters);
    let short_and = make(&counters, 1, false).test() && make(&counters, 2, true).test();
    if short_and || made(&counters) != 1 || log(&counters) != 1 {
        return 5;
    }

    // Nested operands end in evaluation order, rather than at the outer statement.
    reset(&counters);
    let nested = (make(&counters, 1, false).test() || make(&counters, 2, true).test()) && log(&counters) == 12;
    if !nested || log(&counters) != 12 || made(&counters) != 12 {
        return 6;
    }

    // Copy the operand's Boolean result before dropping its enclosing temporary.
    reset(&counters);
    let field = make(&counters, 4, false).flag || log(&counters) == 4;
    if !field || log(&counters) != 4 {
        return 7;
    }

    // Pattern scrutinees live through the then arm, while ordinary Boolean links do not.
    reset(&counters);
    let inside = if let Maybe::Some(value) = make(&counters, 5, true).peek()
        && value
        && make(&counters, 6, true).test()
    {
        log(&counters)
    } else {
        99
    };
    if inside != 6 || log(&counters) != 65 || made(&counters) != 56 {
        return 8;
    }
    reset(&counters);
    let in_else = if let Maybe::Some(value) = make(&counters, 7, true).peek()
        && value
        && make(&counters, 8, false).test()
    {
        99
    } else {
        log(&counters)
    };
    if in_else != 87 || log(&counters) != 87 || made(&counters) != 78 {
        return 9;
    }

    // An operand scope never takes ownership of a named local from the surrounding block.
    reset(&counters);
    {
        let held = make(&counters, 9, false);
        let result = held.test() || log(&counters) == 0;
        if !result || log(&counters) != 0 {
            return 10;
        }
    }
    if log(&counters) != 9 {
        return 11;
    }

    // The same separation applies on a loop back edge and on a failed Boolean link.
    reset(&counters);
    let mut turn = 0;
    while let Maybe::Some(value) = make(&counters, 3, turn == 0).peek()
        && value
        && make(&counters, 4, true).test()
    {
        if log(&counters) != 4 {
            return 12;
        }
        turn = 1;
    }
    if turn != 1 || log(&counters) != 433 || made(&counters) != 343 {
        return 13;
    }
    0
}
