//! Native values have one semantic encoding; environment maps reject invalid names/origins.
use blaze_wire::{BuildSpec, ConfigEnvValue, InputText, Limits, NativeValue, Sample, Set, Wire};
#[test]
fn native_utf8_and_nul_are_rejected_literal_nul_and_bad_names_rejected() {
    for bytes in [b"valid utf8".to_vec(), vec![0xff, 0]] {
        assert!(
            blaze_wire::decode::<NativeValue>(
                &blaze_wire::encode(&NativeValue(bytes)),
                &Limits::default()
            )
            .is_err()
        );
    }
    assert!(
        blaze_wire::decode::<NativeValue>(
            &blaze_wire::encode(&NativeValue(vec![0xff])),
            &Limits::default()
        )
        .is_ok()
    );
    let mut value = BuildSpec::sample(&mut blaze_wire::Gen::new(1));
    value.script_env = Set::new(vec![]);
    for name in ["", "BAD=NAME", "BAD\0NAME"] {
        value.config_env =
            Set::new(vec![(name.into(), ConfigEnvValue::Literal("x".to_owned().into()))]);
        assert!(
            blaze_wire::decode::<BuildSpec>(&blaze_wire::encode(&value), &Limits::default())
                .is_err()
        );
    }
    value.config_env =
        Set::new(vec![("A".into(), ConfigEnvValue::Literal(blaze_wire::EnvText("x\0y".into())))]);
    assert!(
        blaze_wire::decode::<BuildSpec>(&blaze_wire::encode(&value), &Limits::default()).is_err()
    );
    value.config_env = Set::new(vec![
        ("A".into(), ConfigEnvValue::Literal("one".to_owned().into())),
        ("A".into(), ConfigEnvValue::Literal("two".to_owned().into())),
    ]);
    assert!(
        blaze_wire::decode::<BuildSpec>(&blaze_wire::encode(&value), &Limits::default()).is_err()
    );
}
