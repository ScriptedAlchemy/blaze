//! AAPCS64 6.8.2 C.10/C.11 and C.13..C.17, 6.9: a 128-bit integer is one value in
//! two general registers, or wholly in a 16-aligned stack slot; its result is x0:x1.
//! Apple's "Pass arguments to functions correctly" permits an odd starting register.
//!
//! Every body here goes through type checking and walk.rs `pass`, including the direct and
//! indirect caller signatures. Restoring the old `Class::Int { size: 16 }` refusal fails the
//! build before emission. The native controls independently cross the host ABI in each
//! direction, so matching mistakes in a generated caller and callee cannot cancel out.

use super::*;

const U128: PNode = PNode::Uint(UintTy::U128);
const I128: PNode = PNode::Int(IntTy::I128);
const U64: PNode = PNode::Uint(UintTy::U64);
const FOREIGN: ItemKey = ItemKey(3);
const WIDE: u128 = 0x9234_5678_9abc_def0_fedc_ba98_7654_3210;
const NEGATIVE: i128 = -0x2345_6789_abcd_ef01_0123_4567_89ab_cdef;

#[derive(Clone, Copy, Debug)]
enum Shape {
    Odd,
    LastPair,
    SpilledPair,
    AlignedStack,
}

impl Shape {
    fn inputs(self) -> &'static [PNode] {
        match self {
            Shape::Odd => &[U64, U128, I128, U64],
            Shape::LastPair => &[U64, U64, U64, U64, U64, U64, U128, U64],
            Shape::SpilledPair => &[U64, U64, U64, U64, U64, U64, U64, U128, U64],
            Shape::AlignedStack => &[U64, U64, U64, U64, U64, U64, U64, U64, U8, U128, U8],
        }
    }

    fn output(self) -> PNode {
        match self {
            Shape::Odd => I128,
            _ => U128,
        }
    }

    fn values(self, wide: u128) -> [u128; 11] {
        match self {
            Shape::Odd => [11, wide, u128::from_le_bytes(NEGATIVE.to_le_bytes()), 99, 0, 0, 0, 0, 0, 0, 0],
            Shape::LastPair => [11, 22, 33, 44, 55, 66, wide, 99, 0, 0, 0],
            Shape::SpilledPair => [11, 22, 33, 44, 55, 66, 77, wide, 99, 0, 0],
            Shape::AlignedStack => [11, 22, 33, 44, 55, 66, 77, 88, 0x9a, wide, 0x9b],
        }
    }
}

fn digest(values: &[u128]) -> u128 {
    values.iter().enumerate().fold(0, |out, (i, &value)| {
        let shift = (u32::try_from(i).expect("at most eleven arguments") * 17) % 128;
        out ^ if shift == 0 { value } else { value ^ (value << shift) }
    })
}

/// The same operation as `digest`, written as a source body: casts preserve the signed
/// argument's bits, and the different shifts make every argument and both halves count.
/// Each written read and cast has its own ExprId: the checker's body is a tree, even when
/// two occurrences read the same parameter. Sharing a cast below both XOR and shift would
/// give the promotion walk more occurrences than the body's node-count bound permits.
fn digest_body(b: &mut BodyBuf, params: &[PatId], output: PNode) -> ExprId {
    let wide = b.types.intern(U128).expect("u128 interns");
    let mut out = b.int(0, Some(IntSuffix::Uint(UintTy::U128)));
    for (i, &param) in params.iter().enumerate() {
        let arg = b.local(param);
        let value = b.e(Expr::Cast { operand: arg, ty: wide });
        let shift = (u32::try_from(i).expect("at most eleven arguments") * 17) % 128;
        let mixed = if shift == 0 {
            value
        } else {
            let left = b.int(u128::from(shift), Some(IntSuffix::Uint(UintTy::U32)));
            let arg = b.local(param);
            let shifted_value = b.e(Expr::Cast { operand: arg, ty: wide });
            let shifted = b.bin(BinOp::Shl, shifted_value, left);
            b.bin(BinOp::BitXor, value, shifted)
        };
        out = b.bin(BinOp::BitXor, out, mixed);
    }
    let output = b.types.intern(output).expect("the return type interns");
    b.e(Expr::Cast { operand: out, ty: output })
}

fn build_fixture(world: &TestWorld, body: &BodyBuf, root: ExprId, alloc: Alloc<'static>) -> Result<Built<Alloc<'static>>, String> {
    let mut ground = PortableTys::new_in(alloc);
    build_instance_in(world, body, MAIN, root, &[], &mut ground, &[], alloc)
}

fn callee(shape: Shape, alloc: Alloc<'static>) -> Built<Alloc<'static>> {
    let mut world = TestWorld::new();
    world.func(MAIN.0, "main", shape.inputs(), shape.output());
    world.c_abi.push(MAIN);
    let mut body = BodyBuf::new();
    let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"];
    let mut params = [PatId(0); 11];
    for (i, name) in names[..shape.inputs().len()].iter().enumerate() {
        let param = body.bind(name);
        body.params.push(param);
        params[i] = param;
    }
    let root = digest_body(&mut body, &params[..shape.inputs().len()], shape.output());
    build_fixture(&world, &body, root, alloc).unwrap_or_else(|error| panic!("{shape:?} callee: {error}"))
}

fn caller(shape: Shape, values: &[u128], indirect: bool, alloc: Alloc<'static>) -> Built<Alloc<'static>> {
    let mut world = TestWorld::new();
    world.func(FOREIGN.0, "foreign", shape.inputs(), shape.output());
    let mut signature = [PTy(0); 12];
    for (to, &ty) in signature.iter_mut().zip(shape.inputs()) {
        *to = world.ty(ty);
    }
    signature[shape.inputs().len()] = world.ty(shape.output());
    let signature = world.types.intern_list(&signature[..shape.inputs().len() + 1]).expect("the fn pointer's signature interns");
    let pointer = PNode::FnPtr(signature, blaze_typeck::FnPtrHeader {
        abi: blaze_typeck::Abi::C, safety: blaze_typeck::Safety::Safe, c_variadic: false,
    });
    let pointer_input = [pointer];
    world.func(MAIN.0, "main", if indirect { &pointer_input } else { &[] }, shape.output());
    world.c_abi.extend_from_slice(&[MAIN, FOREIGN]);
    let mut body = BodyBuf::new();
    let mut args = [ExprId(0); 11];
    for (i, (&bits, &ty)) in values.iter().zip(shape.inputs()).enumerate() {
        let literal = body.int(bits, Some(IntSuffix::Uint(UintTy::U128)));
        let ty = body.types.intern(ty).expect("the argument type interns");
        args[i] = body.e(Expr::Cast { operand: literal, ty });
    }
    let args = &args[..values.len()];
    let root = if indirect {
        let pointer = body.bind("callee");
        body.params.push(pointer);
        let callee = body.local(pointer);
        let args = body.exprs_list(args);
        body.e(Expr::Call { callee, args })
    } else {
        body.call("foreign", args)
    };
    build_fixture(&world, &body, root, alloc).unwrap_or_else(|error| panic!("{shape:?} caller (indirect={indirect}): {error}"))
}

#[test]
fn c_i128_signatures_keep_wide_scalars() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    for shape in [Shape::Odd, Shape::LastPair, Shape::SpilledPair, Shape::AlignedStack] {
        let built = callee(shape, alloc);
        let mut expected = [Ty::NONE; 11];
        for (to, ty) in expected.iter_mut().zip(shape.inputs()) {
            *to = match ty {
                PNode::Uint(UintTy::U128) | PNode::Int(IntTy::I128) => Ty::I128,
                PNode::Uint(UintTy::U8) => Ty::I8,
                _ => Ty::I64,
            };
        }
        let expected = &expected[..shape.inputs().len()];
        let (params, returns) = own_sig(&built);
        assert_eq!(params.as_slice(), expected, "{shape:?}");
        assert_eq!(returns.as_slice(), &[Ty::I128], "{shape:?}");
        let all_values = shape.values(WIDE);
        let args = all_values.map(Val::Int);
        let values = &all_values[..shape.inputs().len()];
        assert_eq!(one(&built, &args[..values.len()]), digest(values), "{shape:?}");
        for indirect in [false, true] {
            let built = caller(shape, values, indirect, alloc);
            assert!(has_call_sig(&built, expected, &[Ty::I128]), "{shape:?}, indirect={indirect}");
            assert_eq!(own_sig(&built).1.as_slice(), &[Ty::I128]);
        }
    }
    // `repr(transparent)` keeps its field's C ABI, even though the walk stores the wrapper
    // as an aggregate place. Both incoming and outgoing adaptations must copy all 16 bytes.
    let mut world = TestWorld::new();
    let wrapper = ItemKey(9);
    world.strukt(wrapper.0, "Wide", &[("bits", U128)]);
    world.reprs.push((wrapper, ReprAttrs { transparent: true, ..ReprAttrs::RUST }));
    let wide = PNode::Adt(wrapper, List::EMPTY);
    world.func(MAIN.0, "main", &[wide], wide);
    world.c_abi.push(MAIN);
    let mut body = BodyBuf::new();
    let param = body.bind("value");
    body.params.push(param);
    let root = body.local(param);
    let built = build_fixture(&world, &body, root, alloc).expect("a transparent u128 wrapper keeps its scalar C ABI");
    let (params, returns) = own_sig(&built);
    assert_eq!(params.as_slice(), &[Ty::I128]);
    assert_eq!(returns.as_slice(), &[Ty::I128]);
    for bits in [WIDE, WIDE ^ (1 << 79), WIDE ^ 1] {
        assert_eq!(one(&built, &[Val::Int(bits)]), bits);
    }
}

#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
mod native {
    use super::*;
    use core::alloc::Allocator;
    use blaze_code::image::ImageError;
    use blaze_code::place::{At, Words};
    use blaze_code::record::{Space, Symbol, Target};
    use blaze_inproc::exec::Executable;
    use blaze_tier::{Machine, Names, Named, Outside, RoutinePlaces, Tier};

    #[derive(Clone, Copy, Debug)]
    enum Backend {
        Reference,
        Sota,
    }

    extern "C" fn host_odd(a: u64, b: u128, c: i128, d: u64) -> i128 {
        i128::from_le_bytes(digest(&[u128::from(a), b, u128::from_le_bytes(c.to_le_bytes()), u128::from(d)]).to_le_bytes())
    }

    extern "C" fn host_last(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, wide: u128, tail: u64) -> u128 {
        digest(&[a.into(), b.into(), c.into(), d.into(), e.into(), f.into(), wide, tail.into()])
    }

    extern "C" fn host_spilled(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, wide: u128, tail: u64) -> u128 {
        digest(&[a.into(), b.into(), c.into(), d.into(), e.into(), f.into(), g.into(), wide, tail.into()])
    }

    extern "C" fn host_aligned(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, byte: u8, wide: u128, tail: u8) -> u128 {
        digest(&[a.into(), b.into(), c.into(), d.into(), e.into(), f.into(), g.into(), h.into(), byte.into(), wide, tail.into()])
    }

    fn host(shape: Shape) -> *const () {
        match shape {
            Shape::Odd => host_odd as *const (),
            Shape::LastPair => host_last as *const (),
            Shape::SpilledPair => host_spilled as *const (),
            Shape::AlignedStack => host_aligned as *const (),
        }
    }

    fn place_reference<A: Allocator>(built: &Built<A>, target: *const (), alloc: Alloc<'_>) -> Executable {
        let vias = [blaze_emit::Via::Address];
        let mut code = blaze_emit::Code::new_in(alloc);
        let os = if cfg!(target_os = "macos") { blaze_emit::Os::MacOs } else { blaze_emit::Os::Linux };
        blaze_emit::emit(built.func.verified(), &vias[..built.refs.len()], blaze_emit::Isa { lse: false, os }, &mut code).expect("the host's C ABI emits");
        let target = u64::try_from(target.addr()).expect("an AArch64 address fits u64");
        let resolve = |place: u32| {
            let place = usize::try_from(place).expect("the reference index fits usize");
            assert_eq!(built.refs[place].item, FOREIGN, "only the declared host function is bound");
            target
        };
        Executable::written(code.bytes.len(), 0, |out, _| {
            let base = u64::try_from(out.as_ptr().addr()).expect("an AArch64 address fits u64");
            out.copy_from_slice(&code.bytes);
            for fix in code.fixups.iter() {
                let at = usize::try_from(fix.at).expect("a code offset fits usize");
                out[at..at + 8].copy_from_slice(&resolve(fix.place).to_le_bytes());
            }
            for reloc in code.relocs.iter() {
                let to = match reloc.to {
                    blaze_emit::RelocTo::Code(off) => base + u64::from(off),
                    blaze_emit::RelocTo::Func(place) => resolve(place),
                    other => panic!("the scalar ABI control needs no {other:?}"),
                };
                let at = usize::try_from(reloc.at).expect("a code offset fits usize");
                out[at..at + 8].copy_from_slice(&to.wrapping_add_signed(reloc.addend).to_le_bytes());
            }
        }).expect("the emitted body maps")
    }

    struct HostName<'a, A: Allocator>(&'a Built<A>);

    impl<A: Allocator> Names for HostName<'_, A> {
        fn name(&self, of: Outside) -> Option<Named<'_>> {
            let Outside::Func(place) = of else { return None };
            let instance = self.0.refs.get(usize::try_from(place).ok()?)?;
            (instance.item == FOREIGN).then_some(Named::C("i128_abi_host"))
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    struct HostWord {
        slot: u64,
        function: u64,
    }

    impl Words for HostWord {
        fn word(&mut self, target: Target, _: bool) -> Result<u64, ImageError> {
            match target == (Target { space: Space::Outside, symbol: Symbol::of_name("i128_abi_host") }) {
                true => Ok(self.slot),
                false => Err(ImageError::Unbound { target }),
            }
        }

        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            match target == (Target { space: Space::Outside, symbol: Symbol::of_name("i128_abi_host") }) {
                true => Ok(self.function),
                false => Err(ImageError::Unbound { target }),
            }
        }
    }

    fn place_sota<A: Allocator>(built: &Built<A>, target: *const (), alloc: Alloc<'_>) -> Executable {
        let os = if cfg!(target_os = "macos") { blaze_tier::Os::MacOs } else { blaze_tier::Os::Linux };
        let machine = Machine { arch: blaze_tier::Arch::Arm64, os, features: 0 };
        let emitted = blaze_emit::Sota::standalone().emit(built.func.verified(), &HostName(built), machine, alloc)
            .expect("Sota must emit the source C signature directly, without fallback");
        let sealed = blaze_tier::record::seal(emitted).expect("the Sota record seals");
        let mut bytes = blaze_own::Vec::new_in(alloc);
        blaze_tier::record::encode(&sealed, &mut bytes);
        let view = blaze_tier::record::read(&bytes).expect("the Sota record reads back");
        let align = 8usize.max(1usize << view.relro_align());
        let function = u64::try_from(target.addr()).expect("an AArch64 address fits u64");
        let mut placed = Ok(());
        let entry = Executable::written(view.code().len(), view.relro().len() + align + 16, |code, data| {
            let skip = (align - data.as_ptr().addr() % align) % align;
            let (relro, rest) = data[skip..].split_at_mut(view.relro().len());
            let slot_skip = (8 - rest.as_ptr().addr() % 8) % 8;
            let slot = &mut rest[slot_skip..slot_skip + 8];
            slot.copy_from_slice(&function.to_le_bytes());
            let at = At {
                code: u64::try_from(code.as_ptr().addr()).expect("the code address fits"),
                relro: u64::try_from(relro.as_ptr().addr()).expect("the relro address fits"),
            };
            let mut words = HostWord { slot: u64::try_from(slot.as_ptr().addr()).expect("the slot address fits"), function };
            placed = blaze_tier::record::place(&view, Symbol::of_name("i128_abi_control"), at, &mut words, code, relro);
        }).expect("the Sota body maps");
        placed.expect("the exact Sota record binds and places");
        entry
    }

    fn place<A: Allocator>(built: &Built<A>, target: *const (), backend: Backend, alloc: Alloc<'_>) -> Executable {
        match backend {
            Backend::Reference => place_reference(built, target, alloc),
            Backend::Sota => place_sota(built, target, alloc),
        }
    }

    fn call_callee(shape: Shape, entry: &Executable, wide: u128) -> u128 {
        // SAFETY: `callee(shape, alloc)` has exactly the corresponding C signature below, and the
        // executable mapping remains live through the host's call and its complete return.
        unsafe {
            match shape {
                Shape::Odd => {
                    let f: extern "C" fn(u64, u128, i128, u64) -> i128 = entry.as_fn();
                    u128::from_le_bytes(f(11, wide, NEGATIVE, 99).to_le_bytes())
                }
                Shape::LastPair => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u128, u64) -> u128 = entry.as_fn();
                    f(11, 22, 33, 44, 55, 66, wide, 99)
                }
                Shape::SpilledPair => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u128, u64) -> u128 = entry.as_fn();
                    f(11, 22, 33, 44, 55, 66, 77, wide, 99)
                }
                Shape::AlignedStack => {
                    let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u8, u128, u8) -> u128 = entry.as_fn();
                    f(11, 22, 33, 44, 55, 66, 77, 88, 0x9a, wide, 0x9b)
                }
            }
        }
    }

    fn call_caller(shape: Shape, entry: &Executable, indirect: bool) -> u128 {
        // SAFETY: a direct caller has no parameters; an indirect caller's sole parameter is
        // the C fn pointer (one LIR PTR, the same ABI as *const ()). Its target is the live
        // host function with the exact signature used to build this shape's indirect call.
        unsafe {
            match (shape, indirect) {
                (Shape::Odd, false) => {
                    let f: extern "C" fn() -> i128 = entry.as_fn();
                    u128::from_le_bytes(f().to_le_bytes())
                }
                (Shape::Odd, true) => {
                    let f: extern "C" fn(*const ()) -> i128 = entry.as_fn();
                    u128::from_le_bytes(f(host(shape)).to_le_bytes())
                }
                (_, false) => {
                    let f: extern "C" fn() -> u128 = entry.as_fn();
                    f()
                }
                (_, true) => {
                    let f: extern "C" fn(*const ()) -> u128 = entry.as_fn();
                    f(host(shape))
                }
            }
        }
    }

    fn exercise(shape: Shape) {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let built = callee(shape, alloc);
        let baseline = shape.values(WIDE);
        let baseline = digest(&baseline[..shape.inputs().len()]);
        for backend in [Backend::Reference, Backend::Sota] {
            let entry = place(&built, core::ptr::null(), backend, alloc);
            // High-half and low-half changes must both reach the result in every direction.
            for wide in [WIDE, WIDE ^ (1 << 79), WIDE ^ 1] {
                let values = shape.values(wide);
                let values = &values[..shape.inputs().len()];
                let want = digest(values);
                if matches!(shape, Shape::Odd) {
                    assert_ne!(want & (1 << 127), 0, "the signed return is negative, not just a small positive i128");
                }
                if wide != WIDE {
                    assert_ne!(want, baseline, "the control observes the changed half");
                }
                assert_eq!(call_callee(shape, &entry, wide), want, "host to built {shape:?}, {backend:?}");
                for indirect in [false, true] {
                    let built = caller(shape, values, indirect, alloc);
                    let caller = place(&built, host(shape), backend, alloc);
                    assert_eq!(call_caller(shape, &caller, indirect), want, "built to host {shape:?}, {backend:?}, indirect={indirect}");
                }
            }
        }
    }

    #[test]
    fn c_i128_register_pairs_match_host_abi() {
        exercise(Shape::Odd);
    }

    #[test]
    fn c_i128_last_register_pair_matches_host_abi() {
        exercise(Shape::LastPair);
    }

    #[test]
    fn c_i128_spilled_pair_matches_host_abi() {
        exercise(Shape::SpilledPair);
    }

    #[test]
    fn c_i128_aligned_stack_pair_matches_host_abi() {
        exercise(Shape::AlignedStack);
    }
}
