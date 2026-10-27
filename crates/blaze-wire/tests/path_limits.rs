//! Paths have a bounded message-sized codec budget; general strings retain 4 KiB.
use blaze_wire::{Limits, SourcePath, Wire};

#[test]
fn escaped_path_limit_is_message_budget_not_string_budget() {
    let native: std::path::PathBuf = std::iter::repeat_n("Üñïcödé dir", 170).collect();
    let path = SourcePath::from_native(&native).unwrap();
    let bytes = blaze_wire::encode(&path);
    assert!(path.as_str().len() > 4096);
    assert_eq!(blaze_wire::validate_as::<SourcePath>(&bytes, &Limits::default()).unwrap(), path);
    assert!(blaze_wire::decode::<String>(&bytes, &Limits::default()).is_err());
    let smaller = Limits { request: 4096, ..Limits::default() };
    assert!(blaze_wire::validate_as::<SourcePath>(&bytes, &smaller).is_err());
    let mut e = blaze_wire::Encoder::new();
    e.text(&"x".repeat(5000));
    assert!(blaze_wire::decode::<String>(&e.into_bytes(), &Limits::default()).is_err());
}
