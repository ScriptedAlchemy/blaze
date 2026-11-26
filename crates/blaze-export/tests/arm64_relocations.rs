//! Run separately scheduled instruction relocations on their target, through both exporters.
#![cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]

mod host;

use blaze_code::record::{CodeRecord, CodeView, OWN_CODE, OWN_RELRO, Site, SiteKind, Space, Symbol, Target};
use blaze_export::{Entry, Function, Function02, Panic, Platform, ProgramIn, ProgramIn02, Tier, export, export_optimized, v1_to_v2};
use blaze_own::{AVec, Alloc, Vec};

fn bytes(record: CodeRecord<'static>, alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut bytes = Vec::new_in(alloc);
    record.seal().expect("valid relocations").encode(&mut bytes);
    bytes
}

#[test]
fn native_exports_call_an_indirect_veneer_and_read_a_scaled_constant() {
    let alloc = blaze_own::testing::heap().alloc();
    let symbols = ["main", "callee"].map(Symbol::of_name);
    let mut main = CodeRecord::new_in(alloc);
    let callee = main.target(Target { space: Space::Func, symbol: symbols[1] });
    for inst in [
        0xa9bf_7bfdu32, // stp x29, x30, [sp, #-16]!
        0x9100_03fd,    // mov x29, sp
        0x9400_0000,    // bl veneer
        0x9000_0008,    // adrp x8, pool
        0x1100_0400,    // add w0, w0, #1
        0xb940_0109,    // ldr w9, [x8, pool@pageoff]
        0x0b09_0000,    // add w0, w0, w9
        0xa8c1_7bfd,    // ldp x29, x30, [sp], #16
        0xd65f_03c0,    // ret
        0x9000_0010,    // veneer: adrp x16, callee@gotpage
        0xd503_201f,    // nop
        0xf940_0210,    // ldr x16, [x16, callee@gotpageoff]
        0xd61f_0200,    // br x16 (preserve the link from main)
    ] {
        main.code.extend_from_slice(&inst.to_le_bytes());
    }
    main.relro_align = 6;
    main.relro.resize(20, 0);
    main.relro.extend_from_slice(&24u32.to_le_bytes());
    main.sites.extend_from_slice(&[
        Site { at: 8, kind: SiteKind::Branch26, target: OWN_CODE, addend: 36 },
        Site { at: 12, kind: SiteKind::Page21, target: OWN_RELRO, addend: 20 },
        Site { at: 20, kind: SiteKind::LdstLo12_4, target: OWN_RELRO, addend: 20 },
        Site { at: 36, kind: SiteKind::GotPage21, target: callee, addend: 0 },
        Site { at: 44, kind: SiteKind::GotLo12, target: callee, addend: 0 },
    ]);
    let mut callee = CodeRecord::new_in(alloc);
    for inst in [0x1400_0000u32, 0xd420_0000, 0x5280_0220, 0xd65f_03c0] {
        callee.code.extend_from_slice(&inst.to_le_bytes());
    }
    callee.sites.push(Site { at: 0, kind: SiteKind::Branch26, target: OWN_CODE, addend: 8 });
    let records = [bytes(main, alloc), bytes(callee, alloc)];
    let functions = [0, 1].map(|i| Function { symbol: symbols[i], record: CodeView::read(&records[i]).unwrap(), tier: Tier::Fast });
    let platform = if cfg!(target_os = "macos") { Platform::MacosArm64 } else { Platform::LinuxArm64 };
    let run = |name, bytes: &[u8]| {
        assert_eq!(host::run(name, bytes, |path| std::process::Command::new(path)).code(), Some(42));
    };
    let entry = Entry::CMain { main: symbols[0] };
    let program = ProgramIn { name: "arm64-fixups", functions: &functions, data: &[], entry, panic: Panic::Abort, personality: None, tls: None, profile: None, natives: None, exports: &[] };
    let mut file = Vec::new_in(alloc);
    export(&program, platform, &mut file, alloc).unwrap();
    run("arm64-object-fixups", &file);

    let converted = records.map(|record| {
        let mut out = Vec::new_in(alloc);
        v1_to_v2(CodeView::read(&record).unwrap(), &blaze_export::optimized::StartBound, &mut out, alloc).unwrap();
        out
    });
    let functions = [0, 1].map(|i| Function02 { symbol: symbols[i], record: blaze_tier::record::read(&converted[i]).unwrap(), tier: Tier::Fast });
    let program = ProgramIn02 { name: "arm64-fixups", functions: &functions, data: &[], entry, panic: Panic::Abort, personality: None, tls: None, profile: None };
    file.clear();
    export_optimized(&program, platform, &mut file, alloc).unwrap();
    run("arm64-optimized-fixups", &file);
}
