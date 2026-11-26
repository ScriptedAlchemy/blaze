use super::*;
use blaze_lir::{
    BlockKind, Builder, Flags, Full,
    builder::{Param, Variadic},
    func::Conv,
};

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
    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
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
    let module = prepare(Symbol([3; 32]), func.verified(), &ByPlace, alloc).unwrap();
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains("phi i32 [ %v5, %e7_0 ], [ %v6, %e7_1 ]"));
    assert!(ir.contains("%mask5 = and i32 %v4, 31"));
    let values = module.literals.iter().map(|l| (&ir[l.operand.start..l.operand.end], l.bits));
    assert!(values.eq([("0", 0), ("11", 11), ("34", 34), ("19", 19)]));
    let second = prepare(Symbol([3; 32]), func.verified(), &ByPlace, alloc).unwrap();
    assert_eq!(&*module.ir, &*second.ir);
    println!("BEGIN BLAZE LLVM CONTROL\n{ir}END BLAZE LLVM CONTROL");
}

#[test]
fn resolved_direct_call_and_missing_import() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
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
    let module = prepare(Symbol([2; 32]), func.verified(), &ByPlace, alloc).unwrap();
    assert_eq!(&*module.calls, &[Symbol([7; 32])]);
    let target = Target { space: Space::Func, symbol: Symbol([7; 32]) };
    assert_eq!(&*module.references, &[Some(target)]);
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains(format_in!(alloc, "declare i32 @{}(i32)", LinkName(target)).as_str()));
    assert!(ir.contains(format_in!(alloc, "call i32 @{}(i32 %v0)", LinkName(target)).as_str()));
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
        prepare(Symbol([2; 32]), func.verified(), &Absent, alloc).err().unwrap().reason,
        "unresolved outside reference in verified LIR"
    );
}

#[test]
fn large_aapcs_aggregate_is_a_private_copy_passed_by_pointer() {
    use blaze_lir::{builder::Facts, func::ParamKind};
    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let aggregate = Param {
        kind: ParamKind::Byval,
        facts: Facts { size: 24, align: 3, ..Facts::default() },
        ..Param::of(Ty::PTR)
    };
    let sig = b.sig(Conv::C, Variadic::No, &[aggregate], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let arg = b.param(Ty::PTR);
    let callee = b.reference(RefKind::Func, 0);
    let result = b.call(Flags::NONE, callee, sig, &[arg]);
    b.ret(&[result]);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([4; 32]), func.verified(), &ByPlace, alloc).unwrap();
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains("(ptr align 8 %v0)"));
    assert!(ir.contains("%copy1_0 = alloca [24 x i8], align 8"));
    assert!(ir.contains(
        "call void @llvm.memcpy.p0.p0.i32(ptr align 8 %copy1_0, ptr %v0, i32 24, i1 false)"
    ));
    assert!(ir.contains("(ptr align 8 %copy1_0)"));
    assert!(!ir.contains("byval("));
    println!("BEGIN BLAZE LLVM AGGREGATE\n{ir}END BLAZE LLVM AGGREGATE");
}

#[test]
fn aarch64_hint_keeps_its_instruction_field() {
    let alloc = blaze_own::testing::heap().alloc();
    for imm in [127u128, 128] {
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let hint_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);
        let hint = b.intrinsic("llvm.aarch64.hint", hint_sig);
        let field = b.iconst(Ty::I32, imm);
        b.target_intrinsic(hint, &[field]);
        b.ret(&[]);
        let func = b.finish_verified().unwrap();
        let module = prepare(Symbol([8; 32]), func.verified(), &ByPlace, alloc);
        if imm == 127 {
            let module = module.unwrap();
            let ir = core::str::from_utf8(&module.ir).unwrap();
            assert!(ir.contains("call void @llvm.aarch64.hint(i32 127)"));
        } else {
            let error = module.err().unwrap();
            assert_eq!(error.instruction, Some(1));
            assert!(error.reason.contains("immediate past its field"));
        }
    }
}

#[test]
fn terminate_landing_uses_exception_filter_and_preserves_the_trap_code() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    let normal = b.create_block(BlockKind::Normal);
    let terminate = b.create_block(BlockKind::Terminate);
    let imported = b.reference(RefKind::Func, 0);
    b.begin(entry);
    b.invoke(Flags::NONE, imported, sig, &[], normal, terminate);
    b.begin(normal);
    b.ret(&[]);
    b.begin(terminate);
    b.param(Ty::PTR);
    b.trap(123);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([5; 32]), func.verified(), &ByPlace, alloc).unwrap();
    let ir = core::str::from_utf8(&module.ir).unwrap();
    assert!(ir.contains("filter [0 x ptr] zeroinitializer"));
    assert!(ir.contains("call void @llvm.aarch64.break(i32 123)"));
    println!("BEGIN BLAZE LLVM TERMINATE\n{ir}END BLAZE LLVM TERMINATE");
}

#[test]
fn indexed_tls_preserves_zero_and_resolves_its_runtime_targets() {
    struct Indexed;
    impl Names for Indexed {
        fn name(&self, outside: Outside) -> Option<Named<'_>> {
            let byte = match outside {
                Outside::Tls(9) => 9,
                Outside::TlsDesc => 10,
                Outside::TlsGetter => 11,
                _ => return None,
            };
            Some(Named::Symbol(Symbol([byte; 32])))
        }
        fn routines(&self) -> crate::RoutinePlaces {
            crate::RoutinePlaces::NONE
        }
    }
    let alloc = blaze_own::testing::heap().alloc();
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let var = b.reference(RefKind::Tls, 9);
    let p = b.tls_addr(var);
    b.ret(&[p]);
    let func = b.finish_verified().unwrap();
    let module = prepare(Symbol([6; 32]), func.verified(), &Indexed, alloc).unwrap();
    let ir = core::str::from_utf8(&module.ir).unwrap();
    let index = Target { space: Space::Tls, symbol: Symbol([9; 32]) };
    assert_eq!(module.references.as_slice(), &[Some(index)]);
    assert_eq!(
        module.implicit_references.as_slice(),
        &[
            Target { space: Space::Static, symbol: Symbol([10; 32]) },
            Target { space: Space::Func, symbol: Symbol([11; 32]) },
        ]
    );
    assert!(module.calls.is_empty());
    assert!(
        ir.contains(format_in!(alloc, "@{} = extern_weak global i8", LinkName(index)).as_str())
    );
    assert!(ir.contains("icmp ult i64 %tlsindex0, %tlsn0"));
    assert!(ir.contains("getelementptr i8, ptr %tlstable0, i32 32"));
    println!("BEGIN BLAZE LLVM TLS\n{ir}END BLAZE LLVM TLS");
}

#[test]
fn musttail_requires_the_callers_abi_and_atomics_access_whole_bytes() {
    use blaze_lir::{Ordering, builder::MemAttrs};
    let alloc = blaze_own::testing::heap().alloc();
    let symbol = Symbol([13; 32]);
    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let p = Param::of(Ty::I32);
    let own = b.sig(Conv::C, Variadic::No, &[p], &[p]);
    let other = b.sig(Conv::C, Variadic::No, &[], &[p]);
    b.set_signature(own);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b.param(Ty::I32);
    let callee = b.reference(RefKind::Func, 0);
    b.tail_call(Flags::NONE, callee, other, &[]);
    let function = b.finish_verified().unwrap();
    let err = prepare(symbol, function.verified(), &ByPlace, alloc).err().unwrap();
    assert_eq!(err.instruction, Some(1));
    assert!(err.reason.contains("musttail"));

    let mut b = Builder::<_, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I1)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let p = b.param(Ty::PTR);
    let v =
        b.atomic_load(Ty::I1, Flags::NONE.with_ordering(Ordering::SeqCst), p, MemAttrs::aligned(0));
    b.ret(&[v]);
    let function = b.finish_verified().unwrap();
    let err = prepare(symbol, function.verified(), &ByPlace, alloc).err().unwrap();
    assert_eq!(err.instruction, Some(1));
    assert!(err.reason.contains("whole bytes"));
}
