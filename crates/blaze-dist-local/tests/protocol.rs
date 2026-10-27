//! Canonical framing/version vectors and exact response binding.

use blaze_dist_local::protocol::*;
use blaze_wire::{Limits, decode, encode};

#[test]
fn hello_golden_separates_transport_global_codec_and_terminal_versions() {
    let value = ClientFrame::Hello(LocalHello {
        transport: 1,
        codec: 1,
        terminal: 2,
        session: [0; 32],
        epoch: None,
    });
    let mut expected = vec![0x82, 0, 0xa5, 0, 1, 1, 1, 2, 2, 3, 0x58, 0x20];
    expected.extend([0; 32]);
    expected.extend([4, 0xf6]);
    assert_eq!(encode(&value), expected);
    assert_eq!(decode::<ClientFrame>(&expected, &Limits::DOCUMENTED).unwrap(), value);
    let framed = blaze_dist_local::server::frame(&expected);
    assert_eq!(u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize, expected.len());
    assert_eq!(&framed[4..], expected);
}

#[test]
fn carrier_envelopes_roundtrip_all_variants_without_json() {
    let mut g = blaze_wire::Gen::new(0x177);
    for _ in 0..64 {
        let value = <ClientFrame as blaze_wire::Sample>::sample(&mut g);
        let bytes = encode(&value);
        assert_eq!(decode::<ClientFrame>(&bytes, &Limits::DOCUMENTED).unwrap(), value);
        let value = <ServerFrame as blaze_wire::Sample>::sample(&mut g);
        let bytes = encode(&value);
        assert_eq!(decode::<ServerFrame>(&bytes, &Limits::DOCUMENTED).unwrap(), value);
    }
}
