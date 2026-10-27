//! Version-2 location fields are required and malformed paths never reach materialization.
use blaze_wire::{BuildSpec, Encoder, ErrorKind, Limits, Sample, SourcePath, Wire};

#[test]
fn paths_are_validated_without_normalizing_bad_messages() {
    for path in
        ["", "/absolute", "../out", "a/../b", "a//b", "a/./b", "a/", "a\\b", "C:/root", "a\0b"]
    {
        let mut e = Encoder::new();
        path.to_owned().encode(&mut e);
        assert_eq!(
            blaze_wire::decode::<SourcePath>(&e.into_bytes(), &Limits::default()).unwrap_err().kind,
            ErrorKind::InvalidPath,
            "{path:?}"
        );
    }
    for path in [".", "Cargo.toml", "left/Cargo.toml", "nested/deep"] {
        let value = SourcePath::new(path.to_owned()).unwrap();
        assert_eq!(
            blaze_wire::decode::<SourcePath>(&blaze_wire::encode(&value), &Limits::default())
                .unwrap(),
            value
        );
    }
}

#[test]
fn legacy_missing_location_unknown_fields_and_unsafe_cwd_are_refused() {
    let mut value = BuildSpec::sample(&mut blaze_wire::Gen::new(1));
    let bytes = blaze_wire::encode(&value);
    assert_eq!(bytes[0], 0xad);
    // Field 12 is last; v1's twelve-field shape has no location.
    let last = blaze_wire::encode(&value.workspace_manifest).len() + 1;
    let mut legacy = bytes[..bytes.len() - last].to_vec();
    legacy[0] = 0xac;
    assert_eq!(
        blaze_wire::decode::<BuildSpec>(&legacy, &Limits::default()).unwrap_err().kind,
        ErrorKind::MissingKey(12)
    );
    let mut unknown = bytes.clone();
    unknown[0] = 0xae;
    unknown.extend_from_slice(&[13, 0xf6]);
    assert_eq!(
        blaze_wire::decode::<BuildSpec>(&unknown, &Limits::default()).unwrap_err().kind,
        ErrorKind::UnknownKey(13)
    );
    value.workspace_manifest = SourcePath::new("left/src".to_owned()).unwrap();
    assert_eq!(
        blaze_wire::decode::<BuildSpec>(&blaze_wire::encode(&value), &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(blaze_wire::VERSION, 3);
}

#[test]
fn configuration_tags_are_required_and_relative_paths_are_validated() {
    use blaze_wire::ConfigEnvValue;
    let literal = ConfigEnvValue::Literal("data".to_owned());
    let relative = ConfigEnvValue::WorkspaceRelative(SourcePath::new("data".to_owned()).unwrap());
    assert_ne!(blaze_wire::encode(&literal), blaze_wire::encode(&relative));
    assert_eq!(
        blaze_wire::decode::<ConfigEnvValue>(&blaze_wire::encode(&relative), &Limits::default())
            .unwrap(),
        relative
    );
    // Schema 1's bare string is not interpreted as a literal in schema 2.
    assert!(
        blaze_wire::decode::<ConfigEnvValue>(
            &blaze_wire::encode(&"data".to_owned()),
            &Limits::default()
        )
        .is_err()
    );
    let mut bad = Encoder::new();
    bad.array(2);
    bad.uint(1);
    "../outside".to_owned().encode(&mut bad);
    assert_eq!(
        blaze_wire::decode::<ConfigEnvValue>(&bad.into_bytes(), &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
    assert!(blaze_wire::decode::<ConfigEnvValue>(&[0x82, 2, 0xf6], &Limits::default()).is_err());
}

#[cfg(unix)]
#[test]
fn byte_components_are_lossless_distinct_and_canonical() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    for bytes in [
        b"my:proj".to_vec(),
        b"literal\\name".to_vec(),
        b"C:".to_vec(),
        vec![b'n', 0xff],
        "n\u{fffd}".as_bytes().to_vec(),
        b"percent%FF".to_vec(),
    ] {
        let native = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()));
        let logical = SourcePath::from_native(&native).unwrap();
        let back =
            blaze_wire::decode::<SourcePath>(&blaze_wire::encode(&logical), &Limits::default())
                .unwrap();
        assert_eq!(back.to_native().unwrap().as_os_str().as_bytes(), bytes);
    }
    let raw = SourcePath::from_native(std::path::Path::new(&std::ffi::OsString::from_vec(vec![
        b'n', 0xff,
    ])))
    .unwrap();
    let replacement = SourcePath::from_native(std::path::Path::new("n\u{fffd}")).unwrap();
    assert_ne!(raw, replacement);
    for malformed in ["%41", "%2f", "%2F", "%00", "%2E%2E", "%ff", "a/%", "a/%GG", "/a"] {
        assert!(SourcePath::new(malformed.to_owned()).is_err(), "{malformed}");
    }
}
