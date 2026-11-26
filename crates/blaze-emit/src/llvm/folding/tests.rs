use super::*;
use crate::{Named, Names, Outside, RoutinePlaces, llvm::prepare};
use blaze_lir::{
    BlockKind, Builder, Flags, Full, Ty,
    builder::{Param, Variadic},
    func::Conv,
};

struct NoReferences;

impl Names for NoReferences {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }
    fn routines(&self) -> RoutinePlaces {
        RoutinePlaces::NONE
    }
}

#[test]
fn generalized_definition_and_each_original_symbol_preserve_their_argument_values() {
    let alloc = blaze_own::testing::heap().alloc();
    let body_symbol = Symbol([11; 32]);
    let build = |member: u8| {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let p = Param::of(Ty::I32);
        let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I32);
        let literal = b.iconst(Ty::I32, u128::from(member));
        let add = b.binary(Op::Add, Flags::NONE, x, literal);
        b.ret(&[add]);
        let f = b.finish_verified().unwrap();
        prepare(Symbol([member; 32]), f.verified(), &NoReferences, alloc).unwrap()
    };
    let first = build(7);
    let second = build(9);
    let places = [(0, 0)];
    let body = module(&first, body_symbol, Role::Body, places.into_iter(), alloc).unwrap();
    let body_ir = core::str::from_utf8(&body.ir).unwrap();
    assert!(body_ir.contains("(i32 %v0, i32 %fold0)"));
    assert!(body_ir.contains("add i32 0, %fold0"));
    println!("BEGIN BLAZE LLVM FOLD BODY\n{body_ir}END BLAZE LLVM FOLD BODY");
    for (member, value) in [(&first, 7), (&second, 9)] {
        let thunk = module(member, body_symbol, Role::Thunk, places.into_iter(), alloc).unwrap();
        let ir = core::str::from_utf8(&thunk.ir).unwrap();
        assert_eq!(thunk.symbol, member.symbol);
        assert_eq!(&thunk.calls[..], &[body.symbol]);
        assert!(ir.contains(format_in!(alloc, "i32 %v0, i32 {value})\n").as_str()));
        assert!(ir.contains("ret i32 %result"));
        println!("BEGIN BLAZE LLVM FOLD THUNK {value}\n{ir}END BLAZE LLVM FOLD THUNK {value}");
    }
    assert!(module(&first, body_symbol, Role::Body, [(0, 1)].into_iter(), alloc).is_err());
}
