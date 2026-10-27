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
    value.cwd = "../out".to_owned();
    assert_eq!(
        blaze_wire::decode::<BuildSpec>(&blaze_wire::encode(&value), &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(blaze_wire::VERSION, 2);
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
