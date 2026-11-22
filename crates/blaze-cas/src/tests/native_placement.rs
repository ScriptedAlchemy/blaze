//! The store's actual placement protocol on both drivers. No cwd changes: Native must
//! resolve FileName through its own absolute root, including seal and strict FICLONE.
//! Native fixtures use TMPDIR, which must be on a reflink-capable file system.

use super::*;

fn place_output(
    driver: &mut impl Driver,
    bytes: &[u8],
    context: impl std::fmt::Display,
) -> ContentId {
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let drive = |driver: &mut _, store: &mut Store, effects: &mut Vec<Effect>| {
        run_until_idle(driver, effects, |done, effects| {
            store.complete(done, effects);
        })
        .expect("the placement protocol ends");
    };
    drive(driver, &mut store, &mut effects);
    assert!(store.ready(), "{context}: {:?}", store.broken());
    assert_eq!(
        store.place_by(),
        Some(PlaceBy::Clone),
        "{context}: strict placement needs block sharing; native fixtures use a reflink-capable TMPDIR"
    );
    let ticket = store.put(key(1), Kind::Output, bytes).unwrap();
    store.flush(&mut effects);
    drive(driver, &mut store, &mut effects);
    assert!(store.durable(ticket));
    let content = content_of(bytes);
    let len = bytes.len() as u64;
    for (at, to) in ["target/build/out/blaze", "target/debug/blaze"].into_iter().enumerate() {
        if at > 0 {
            // The first place writes the output and the store's copy beside it. A later
            // place must find that copy sealed before asking for a strict block clone.
            let sealed = name(&format!("store/placed/{content:x}"));
            let mut look = vec![publish::look(&sealed, Tag(1))];
            run_until_idle(driver, &mut look, |done, _| {
                let Done::Stamped(stamps) = done.done else { panic!("looking at {sealed} failed") };
                assert_eq!(stamps.len(), 1);
                assert_eq!(stamps[0].size, len);
                assert_eq!(stamps[0].mode & 0o777, PLACED_MODE, "sealed before cloning");
            })
            .expect("the source is sealed before the later place");
        }
        let tag = store.place(&key(1), name(to), &mut effects).unwrap();
        drive(driver, &mut store, &mut effects);
        let placed = store.placed();
        assert_eq!(placed.len(), 1, "{placed:?}");
        assert_eq!((placed[0].tag, &placed[0].result), (tag, &Ok(())), "{placed:?}");
        assert_eq!(
            placed[0].bytes,
            if at == 0 {
                PlacedBytes { copied: len, written: len, ..PlacedBytes::default() }
            } else {
                PlacedBytes { cloned: len, ..PlacedBytes::default() }
            },
            "{context}: the first place writes directly; the later place must use strict block sharing"
        );
    }
    content
}

fn native_placement(label: &str, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;
    let root =
        std::env::temp_dir().join(format!("blaze-native-placement-{label}-{}", std::process::id()));
    assert!(root.is_absolute());
    assert_ne!(root, std::env::current_dir().unwrap());
    // Refuse to reuse a previous run's files; this test must exercise make_file, not a hit.
    std::fs::create_dir(&root).expect("a fresh placement root on a reflink-capable file system");
    let mut native = blaze_core::native(&root, 1).unwrap();
    let content = place_output(&mut native, bytes, root.display());
    let sealed = name(&format!("store/placed/{content:x}"));
    assert_eq!(
        std::fs::metadata(root.join(sealed.as_str())).unwrap().permissions().mode() & 0o777,
        PLACED_MODE
    );
    for file in [sealed, name("target/build/out/blaze"), name("target/debug/blaze")] {
        // Map instead of allocating another full-size copy for the optional boundary case.
        let mut effects = vec![Effect::Map {
            file: file.clone(),
            extent: blaze_core::Extent::Whole,
            tag: Tag(1),
        }];
        run_until_idle(&mut native, &mut effects, |done, _| {
            let Done::Mapped(mapped) = done.done else { panic!("mapping {file} failed") };
            assert!(mapped.bytes() == bytes, "bytes differ at {file}");
            assert_eq!(content_of(mapped.bytes()), content, "{file}");
        })
        .expect("the mapped output is verified");
    }
    drop(native);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_placement_uses_the_driver_root_and_seals_before_cloning() {
    let bytes = noise(CLONE_FROM as usize + 37, 0x94);
    let mut sim = Sim::new(94, quiet());
    let content = place_output(&mut sim, &bytes, "Sim");
    assert_eq!(sim.contents(&name("target/build/out/blaze")), bytes);
    assert_eq!(sim.contents(&name("target/debug/blaze")), bytes);
    // Output renames wait for no sync. The store's own content-named source, in contrast,
    // becomes a durable home before its name is committed, so a crash preserves its bytes.
    sim.crash();
    let sealed = name(&format!("store/placed/{content:x}"));
    assert_eq!(sim.contents(&sealed), bytes);
    assert_eq!(sim.mode(&sealed), Some(PLACED_MODE));
    native_placement("small", &bytes);
}

#[test]
#[ignore = "root admits this >2 GiB allocation and native IO; run on a reflink-capable disk"]
fn native_placement_crosses_linux_max_rw_count() {
    // The failed bootstrap's size, beyond both MAX_RW_COUNT and signed 32-bit.
    let mut bytes = vec![0; 2_739_161_264];
    for at in [0, 0x7fff_efff, 0x7fff_f000, 0x7fff_ffff, 0x8000_0000, 2_739_161_263] {
        bytes[at] = (at as u8).wrapping_add(1);
    }
    native_placement("large", &bytes);
}
