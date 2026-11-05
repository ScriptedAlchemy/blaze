#!/usr/bin/env python3
"""Native std callees introduced by an edit are materialized again after a revert.

Run with RA_JIT_BIN naming the compiler. Reuse unreached.py's strict output and splice
checks, including its initially dead-stripped i128 formatting callee. Every later patch
is relative to the original link, so it cannot import code a preceding patch appended.
"""
import unreached


def main():
    original = list(unreached.STEPS)
    wide = 0x123456789abcdef0123456789abcdef
    start = "fn main() {\n"
    printed = start + f'    println!("{{:?}} {{:x}}", std::hint::black_box(-{wide}i128), std::hint::black_box({wide}i128));\n'
    add = [('"{} {} {}"', '"{} {} {} {}"'),
           ("<u16>()", "<u16>(), ushow::show::<i128>()")]
    remove = [(new, old) for old, new in reversed(add)]
    unreached.STEPS = original + [
        ("exercise wide native arithmetic", [(start, printed)], f"-{wide} {wide:x}\nb 0 0 0"),
        ("take wide arithmetic back", [(printed, start)], "b 0 0 0"),
        ("take the new std callee back", remove, "b 0 0"),
        ("introduce the std callee again", add, "b 0 0 0"),
        ("take every edit back", remove + [
            ('"{} {} {}"', '"{} {}"'),
            ("<u8>(), ushow::show::<u16>()", "<u8>()"),
            ('"b"', '"a"'),
        ], "a 0"),
    ]
    try:
        unreached.main()
    finally:
        unreached.STEPS = original


if __name__ == "__main__":
    main()
