use blaze_wire::{DIST_PACK_FORMAT, PackRef, pack_list};

fn pack() -> PackRef {
    PackRef { pack: [0xab; 32], size: 80, keys: [0xcd; 32], packed: None, format: DIST_PACK_FORMAT }
}

#[test]
fn the_native_three_field_layout_round_trips_including_empty() {
    assert_eq!(pack_list::decode(b"").unwrap(), Vec::<PackRef>::new());
    assert_eq!(pack_list::encode(&[]), b"");
    let packs = vec![pack()];
    let expected = format!("{} 80 {}\n", "ab".repeat(32), "cd".repeat(32));
    assert_eq!(pack_list::encode(&packs), expected.as_bytes());
    assert_eq!(pack_list::decode(expected.as_bytes()).unwrap(), packs);
}

#[test]
fn malformed_lists_never_become_empty_cache_entries() {
    let good = String::from_utf8(pack_list::encode(&[pack()])).unwrap();
    let cases = [
        "\n".to_owned(),
        good.trim_end().to_owned(),
        good.replace(" 80 ", " +80 "),
        good.replace(" 80 ", " 080 "),
        good.replace(" 80 ", " 0 "),
        good.replace(" 80 ", " 18446744073709551616 "),
        good.replace(" 80 ", " 80  "),
        good.replace('\n', " extra\n"),
        good.replace('a', "g"),
        good.replace('a', "A"),
        good.replacen('a', "", 1),
        good.clone() + &good,
    ];
    for bytes in cases {
        assert!(pack_list::decode(bytes.as_bytes()).is_err(), "accepted {bytes:?}");
    }
    assert!(pack_list::decode(&[0xff]).is_err());
    assert!(
        pack_list::decode(&vec![b'x'; blaze_wire::Limits::DOCUMENTED.request as usize + 1])
            .is_err()
    );
}
