//! Slot contents above a narrow value's width must not become ABI operand bits.
//!
//! The observers deliberately declare the transported word as i64, as `ext_seen` does:
//! this checks our canonicalization, not an ABI promise about unspecified upper bits.
//! Every subject and observer is stencil code; typed calls need no Cranelift trampoline.

use cranelift_codegen::ir::{self, AbiParam, ArgumentExtension, InstBuilder, types};
use cranelift_frontend::FunctionBuilder;

use super::super::{FUNCS, Mk, call, import, mem, world::host_mk};
use crate::{
    code::Code,
    tests::exec::{self, Exec, Link, Unit},
};

use types::{I8, I16, I32, I64};

fn param(ty: ir::Type, extension: ArgumentExtension) -> AbiParam {
    let mut param = AbiParam::new(ty);
    param.extension = extension;
    param
}

/// Force a real 8-byte local slot, free it, then reuse it for the narrow load. Two uses
/// keep the poison load from being accumulator-only; the final store frees its slot.
fn poisoned(b: &mut FunctionBuilder<'_>, area: ir::Value, ty: ir::Type) -> ir::Value {
    let poison = b.ins().load(I64, mem(), area, 0);
    b.ins().store(mem(), poison, area, 8);
    b.ins().store(mem(), poison, area, 16);
    b.ins().load(ty, mem(), area, 0)
}

fn link(mk: &Mk, funcs: &[ir::Function]) -> Exec {
    // alloc: compiled test subjects, kept until the native executor copies them.
    let codes: Vec<Code> = funcs
        .iter()
        .map(|func| {
            crate::compile(func, &mk.target)
                .unwrap_or_else(|e| panic!("{e}\n{}", func.display()))
        })
        .collect();
    // alloc: the test executor's borrowed unit descriptions.
    let units: Vec<Unit<'_>> = codes
        .iter()
        .zip(funcs)
        .map(|(code, func)| Unit::Stencil(code, &func.signature))
        .collect();
    let isa = exec::host_isa();
    Exec::link_typed(&Link { isa: &*isa, units: &units, data: &[], host: &[] })
}

/// The oracle is the typed operand, independent of the slot's high bits and of the
/// generated instructions: default/uext zero-extend; sext replicates the type's sign bit.
fn extended(word: u64, ty: ir::Type, extension: ArgumentExtension) -> u64 {
    let shift = 64 - ty.bits();
    match extension {
        ArgumentExtension::Sext => ((word << shift) as i64 >> shift) as u64,
        ArgumentExtension::None | ArgumentExtension::Uext => (word << shift) >> shift,
    }
}

fn words(ty: ir::Type) -> [u64; 12] {
    let sign = 1u64 << (ty.bits() - 1);
    let mask = (sign << 1) - 1;
    let lows = [0, 1, 3, sign - 1, sign, mask];
    std::array::from_fn(|k| {
        let poison = [0x60b6_a59c_d37e_f180, 0x562b_ffff_a581_7e03][k / lows.len()];
        (poison & !mask) | lows[k % lows.len()]
    })
}

#[test]
fn narrow_call_slots_respect_operand_width() {
    let Some(mk) = host_mk() else { return };
    // The third integer is mprotect's protection argument (rdx); the seventh is on
    // the stack. An eight-word entry also reserves the incoming tail-call stack area.
    for position in [2, 6] {
        for tail in [false, true] {
            for indirect in [false, true] {
                for ty in [I8, I16, I32] {
                    for extension in [
                        ArgumentExtension::None,
                        ArgumentExtension::Uext,
                        ArgumentExtension::Sext,
                    ] {
                        let wide = mk.sig_of(&[I64; 8], &[I64]);
                        let mut narrow = wide.clone();
                        narrow.params[position] = param(ty, extension);
                        let caller = mk.func(0, wide.clone(), |b, v| {
                            let callee = import(b, FUNCS, 1, narrow.clone(), true);
                            let value = poisoned(b, v[0], ty);
                            // alloc: one mutable argument list for this test subject.
                            let mut args = v.to_vec();
                            args[position] = value;
                            if indirect {
                                let addr = b.ins().func_addr(I64, callee);
                                let sig = b.import_signature(narrow.clone());
                                if tail {
                                    b.ins().return_call_indirect(sig, addr, &args);
                                } else {
                                    let inst = b.ins().call_indirect(sig, addr, &args);
                                    let out = b.inst_results(inst)[0];
                                    b.ins().return_(&[out]);
                                }
                            } else if tail {
                                b.ins().return_call(callee, &args);
                            } else {
                                let out = call(b, callee, &args)[0];
                                b.ins().return_(&[out]);
                            }
                        });
                        let observer = mk.func(1, wide, |b, v| {
                            for (k, value) in v.iter().enumerate() {
                                b.ins().store(mem(), *value, v[0], 32 + 8 * k as i32);
                            }
                            b.ins().return_(&[v[position]]);
                        });
                        let exec = link(&mk, &[caller, observer]);
                        for word in words(ty) {
                            let mut area = [0u64; 12];
                            area[0] = word;
                            let ptr = area.as_mut_ptr();
                            let sentinels = [
                                ptr as u64,
                                16 * 1024 * 1024,
                                0xfedc_ba98_7654_3210,
                                0x0123_4567_89ab_cdef,
                                0xa5a5_5a5a_dead_beef,
                                0x8000_0000_0000_0001,
                                0xffff_ffff_ffff_ffff,
                                0x562b_0000_0000_0003,
                            ];
                            let got: u64 = exec.call(
                                0,
                                (
                                    ptr,
                                    sentinels[1],
                                    sentinels[2],
                                    sentinels[3],
                                    sentinels[4],
                                    sentinels[5],
                                    sentinels[6],
                                    sentinels[7],
                                ),
                            );
                            let want = extended(word, ty, extension);
                            assert_eq!(
                                got, want,
                                "{ty} {extension:?}, argument {position}, tail={tail}, \
                                 indirect={indirect}, poison={word:#x}"
                            );
                            assert_eq!(&area[1..3], &[word; 2], "the poison was materialized");
                            let mut expected = sentinels;
                            expected[position] = want;
                            assert_eq!(&area[4..], &expected, "all eight ABI words, including i64s");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn narrow_return_slots_respect_operand_width() {
    let Some(mk) = host_mk() else { return };
    for ty in [I8, I16, I32] {
        for extension in [
            ArgumentExtension::None,
            ArgumentExtension::Uext,
            ArgumentExtension::Sext,
        ] {
            let wide = mk.sig_of(&[I64], &[I64]);
            let caller = mk.func(0, wide.clone(), |b, v| {
                let callee = import(b, FUNCS, 1, wide.clone(), true);
                let out = call(b, callee, v)[0];
                b.ins().return_(&[out]);
            });
            let narrow = mk.sig(&[AbiParam::new(I64)], &[param(ty, extension)]);
            let callee = mk.func(1, narrow, |b, v| {
                let value = poisoned(b, v[0], ty);
                b.ins().return_(&[value]);
            });
            let exec = link(&mk, &[caller, callee]);
            for word in words(ty) {
                let mut area = [word, 0, 0];
                let got: u64 = exec.call(0, (area.as_mut_ptr(),));
                assert_eq!(got, extended(word, ty, extension), "{ty} {extension:?}, {word:#x}");
                assert_eq!(area, [word; 3], "the poison was materialized");
            }
        }
    }
}
