//! Each lazy Boolean operand is a temporary scope; pattern scrutinees still live through
//! the consequent body. The same semantic oracle also runs through the native ladder.

use core::ops::Drop;

mod case;

fn main() -> std::entry::Status {
    std::entry::Status(case::check())
}

#[test]
fn lazy_boolean_operand_drop_order() {
    assert_eq!(case::check(), 0);
}
