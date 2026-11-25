use super::*;
use blaze_lir::{
    BlockKind, Builder, Flags, Full,
    builder::{Param, Variadic},
    func::Conv,
};
use std::alloc::Global;

struct ByPlace;
impl Names for ByPlace {
    fn name(&self, outside: Outside) -> Option<Named<'_>> {
        match outside {
            Outside::Func(0) => Some(Named::Symbol(Symbol([7; 32]))),
            _ => None,
        }
    }
    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// Both arms target the same LIR block with different arguments. The result must be
/// 44 for input 0 and 19 otherwise. The amounts and addends are eligible fold cells.
/// The markers let the ARM64 admission command run these exact bytes through LLVM.
#[test]
fn parallel_edges_and_wrapping_shift() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    let join = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let n = b.param(Ty::I32);
    let zero = b.iconst(Ty::I32, 0);
    let cmp = b.icmp(blaze_lir::IntCond::Eq, n, zero);
    let a = b.iconst(Ty::I32, 11);
    let amount = b.iconst(Ty::I32, 34);
    let shifted = b.binary(Op::Shl, Flags::NONE, a, amount);
    let other = b.iconst(Ty::I32, 19);
    b.br(Flags::NONE, cmp, (join, &[shifted]), (join, &[other]));
    b.begin(join);
    let ret = b.param(Ty::I32);
    b.ret(&[ret]);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([3; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains("phi i32 [ %v5, %e7_0 ], [ %v6, %e7_1 ]"));
    assert!(ir.contains("%mask5 = and i32 %v4, 31"));
    let values: std::vec::Vec<_> =
        module.literals.iter().map(|l| (&ir[l.operand.clone()], l.bits)).collect();
    assert_eq!(values, [("0", 0), ("11", 11), ("34", 34), ("19", 19)]);
    let second = prepare(Symbol([3; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    assert_eq!(&*module.ir, &*second.ir);
    println!("BEGIN BLAZE LLVM CONTROL\n{ir}END BLAZE LLVM CONTROL");
}

#[test]
fn resolved_direct_call_and_missing_import() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let p = Param::of(Ty::I32);
    let sig = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I32);
    let r = b.reference(RefKind::Func, 0);
    let y = b.call(Flags::NONE, r, sig, &[x]);
    b.ret(&[y]);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([2; 32]), func.verified(), &ByPlace, Alloc::global()).unwrap();
    assert_eq!(&*module.calls, &[Symbol([7; 32])]);
    let target = Target { space: Space::Func, symbol: Symbol([7; 32]) };
    assert_eq!(&*module.references, &[Some(target)]);
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains(&std::format!("declare i32 @{}(i32)", LinkName(target))));
    assert!(ir.contains(&std::format!("call i32 @{}(i32 %v0)", LinkName(target))));
    struct Absent;
    impl Names for Absent {
        fn name(&self, _: Outside) -> Option<Named<'_>> {
            None
        }
        fn routines(&self) -> crate::RoutinePlaces {
            crate::RoutinePlaces::NONE
        }
    }
    assert_eq!(
        prepare(Symbol([2; 32]), func.verified(), &Absent, Alloc::global()).err().unwrap().reason,
        "unresolved outside reference in verified LIR"
    );
}
