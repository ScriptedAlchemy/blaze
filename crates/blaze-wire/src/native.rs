//! Private native compiler descriptor transport. Environment values travel only over stdin.

use std::{collections::BTreeMap, ffi::OsString, io};

/// Private descriptor field; remove it before writing a public source description.
pub const UNIT_FIELD: &str = "blaze_native_units";
/// Descriptor field containing native root paths and content identities, without environments.
pub const BINDINGS_FIELD: &str = "blaze_native_bindings";

/// Exact native environment and compiler flags of a canonical source unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unit {
    /// Complete process environment, including non-Unicode names and values.
    pub env: BTreeMap<OsString, OsString>,
    /// Ordered compiler flags before translation to native linker arguments.
    pub flags: Vec<OsString>,
}

type UnitRecord = (u32, Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>);

/// Encode native units without converting native names or values to diagnostic text.
pub fn encode_units(units: &BTreeMap<u32, Unit>) -> Vec<u8> {
    let units: Vec<UnitRecord> = units
        .iter()
        .map(|(path, unit)| {
            (
                *path,
                unit.env
                    .iter()
                    .map(|(key, value)| {
                        (key.as_encoded_bytes().to_vec(), value.as_encoded_bytes().to_vec())
                    })
                    .collect(),
                unit.flags.iter().map(|flag| flag.as_encoded_bytes().to_vec()).collect(),
            )
        })
        .collect();
    crate::encode(&(1_u32, units))
}

/// Decode private units, rejecting invalid process values and duplicate unit identities.
pub fn decode_units(bytes: &[u8]) -> io::Result<BTreeMap<u32, Unit>> {
    let (version, units): (u32, Vec<UnitRecord>) =
        crate::decode(bytes, &crate::Limits::DOCUMENTED).map_err(io::Error::other)?;
    if version != 1 {
        return Err(invalid("unknown native units version"));
    }
    let mut out = BTreeMap::new();
    for (path, env, flags) in units {
        let mut values = BTreeMap::new();
        for (key, value) in env {
            if key.is_empty() || key.contains(&b'=') {
                return Err(invalid("invalid native environment name"));
            }
            let key = native_value(key)?;
            if values.insert(key, native_value(value)?).is_some() {
                return Err(invalid("duplicate native environment name"));
            }
        }
        let flags = flags.into_iter().map(native_value).collect::<io::Result<_>>()?;
        if out.insert(path, Unit { env: values, flags }).is_some() {
            return Err(invalid("duplicate native unit identity"));
        }
    }
    Ok(out)
}

/// Remove stdin-only native environments before any public descriptor is persisted.
/// Callers must also remove crate environment fields already private in their descriptor.
pub fn redact_transport(project: &mut serde_json::Value) -> io::Result<()> {
    project
        .as_object_mut()
        .ok_or_else(|| invalid("compiler description is not an object"))?
        .remove(UNIT_FIELD);
    Ok(())
}

fn native_value(bytes: Vec<u8>) -> io::Result<OsString> {
    if bytes.contains(&0) {
        return Err(invalid("native process value contains a NUL"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        Ok(String::from_utf8(bytes)
            .map_err(|_| invalid("native bytes cannot be represented on this host"))?
            .into())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_units_can_share_a_source_file_and_keep_native_settings() {
        let units = BTreeMap::from([
            (
                0,
                Unit {
                    env: BTreeMap::from([("CARGO_PKG_NAME".into(), "a".into())]),
                    flags: vec!["-Copt-level=1".into()],
                },
            ),
            (
                1,
                Unit {
                    env: BTreeMap::from([("CARGO_PKG_NAME".into(), "b".into())]),
                    flags: vec!["-Copt-level=2".into()],
                },
            ),
        ]);
        let encoded = encode_units(&units);
        assert_eq!(decode_units(&encoded).unwrap(), units);
        let mut private =
            serde_json::json!({"crates": [], UNIT_FIELD: encoded, BINDINGS_FIELD: [1, 2]});
        let stdin = private.clone();
        redact_transport(&mut private).unwrap();
        assert!(stdin.get(UNIT_FIELD).is_some());
        assert!(private.get(UNIT_FIELD).is_none());
        assert!(private.get(BINDINGS_FIELD).is_some());
    }

    #[test]
    #[cfg(unix)]
    fn native_values_and_flags_preserve_raw_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let raw = |bytes: &[u8]| OsString::from_vec(bytes.to_vec());
        let units = BTreeMap::from([(
            7,
            Unit {
                env: BTreeMap::from([(raw(b"NAME-\xfe"), raw(b"value-\xff"))]),
                flags: vec![raw(b"-L/producer-\xff")],
            },
        )]);
        assert_eq!(decode_units(&encode_units(&units)).unwrap(), units);
    }

    #[test]
    fn invalid_native_units_fail_decoding() {
        let record = |id: u32, env: Vec<(Vec<u8>, Vec<u8>)>, flags: Vec<Vec<u8>>| (id, env, flags);
        for (version, units) in [
            (2_u32, vec![]),
            (1, vec![record(0, vec![(b"A=B".to_vec(), vec![])], vec![])]),
            (1, vec![record(0, vec![(vec![], vec![])], vec![])]),
            (1, vec![record(0, vec![(b"A".to_vec(), b"x\0".to_vec())], vec![])]),
            (1, vec![record(0, vec![(b"A".to_vec(), vec![]), (b"A".to_vec(), vec![])], vec![])]),
            (1, vec![record(0, vec![], vec![b"x\0".to_vec()])]),
            (1, vec![record(0, vec![], vec![]), record(0, vec![], vec![])]),
        ] {
            assert!(decode_units(&crate::encode(&(version, units))).is_err());
        }
    }
}
