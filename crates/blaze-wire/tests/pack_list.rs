#![no_std]
#![feature(allocator_api)]

use blaze_own::{String, Vec, format_in};
use blaze_wire::{DIST_PACK_FORMAT, PackRef, WireAlloc, pack_list};

fn heap() -> WireAlloc {
    blaze_own::testing::heap().shared()
}

fn pack() -> PackRef {
    PackRef { pack: [0xab; 32], size: 80, keys: [0xcd; 32], packed: None, format: DIST_PACK_FORMAT }
}

fn repeat_str(unit: &str, count: usize, alloc: WireAlloc) -> String<WireAlloc> {
    let mut out = String::with_capacity_in(unit.len() * count, alloc);
    for _ in 0..count {
        out.push_str(unit);
    }
    out
}

fn replace_all(text: &str, from: &str, to: &str, alloc: WireAlloc) -> String<WireAlloc> {
    let mut out = String::new_in(alloc);
    let mut rest = text;
    while let Some(idx) = rest.find(from) {
        out.push_str(&rest[..idx]);
        out.push_str(to);
        rest = &rest[idx + from.len()..];
    }
    out.push_str(rest);
    out
}

fn replace_first(text: &str, from: &str, to: &str, alloc: WireAlloc) -> String<WireAlloc> {
    let mut out = String::new_in(alloc);
    if let Some(idx) = text.find(from) {
        out.push_str(&text[..idx]);
        out.push_str(to);
        out.push_str(&text[idx + from.len()..]);
    } else {
        out.push_str(text);
    }
    out
}

#[test]
fn the_native_three_field_layout_round_trips_including_empty() {
    let alloc = heap();
    assert_eq!(pack_list::decode(b"", alloc).unwrap().as_slice(), &[]);
    assert_eq!(pack_list::encode(&[], alloc).as_slice(), b"");
    let packs = [pack()];
    let expected = format_in!(alloc, "{} 80 {}\n", repeat_str("ab", 32, alloc), repeat_str("cd", 32, alloc));
    assert_eq!(pack_list::encode(&packs, alloc).as_slice(), expected.as_bytes());
    assert_eq!(pack_list::decode(expected.as_bytes(), alloc).unwrap().as_slice(), &packs);
}

#[test]
fn malformed_lists_never_become_empty_cache_entries() {
    let alloc = heap();
    let good = String::from_utf8(pack_list::encode(&[pack()], alloc)).unwrap();
    let cases = [
        String::from_str_in("\n", alloc),
        String::from_str_in(good.trim_end(), alloc),
        replace_all(&good, " 80 ", " +80 ", alloc),
        replace_all(&good, " 80 ", " 080 ", alloc),
        replace_all(&good, " 80 ", " 0 ", alloc),
        replace_all(&good, " 80 ", " 18446744073709551616 ", alloc),
        replace_all(&good, " 80 ", " 80  ", alloc),
        replace_all(&good, "\n", " extra\n", alloc),
        replace_all(&good, "a", "g", alloc),
        replace_all(&good, "a", "A", alloc),
        replace_first(&good, "a", "", alloc),
        format_in!(alloc, "{good}{good}"),
        format_in!(alloc, "{good}{}", replace_all(&good, "ab", "aa", alloc)),
    ];
    for bytes in cases {
        assert!(pack_list::decode(bytes.as_bytes(), alloc).is_err(), "accepted {bytes:?}");
    }
    assert!(pack_list::decode(&[0xff], alloc).is_err());
    let mut oversize = Vec::new_in(alloc);
    oversize.resize(usize::try_from(blaze_wire::Limits::DOCUMENTED.request).unwrap() + 1, b'x');
    assert!(pack_list::decode(&oversize, alloc).is_err());
}
