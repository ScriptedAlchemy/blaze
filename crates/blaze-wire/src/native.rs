//! Native compiler descriptors and environment-free linker plans.
//! Private environment values travel only over stdin.
//! Native strings use Unix bytes or Windows UTF-16LE, without Unicode normalization.

use std::{collections::BTreeMap, ffi::OsString, io};

/// Private descriptor field; remove it before writing a public source description.
pub const UNIT_FIELD: &str = "blaze_native_units";
/// Private environment of a program run by the owned executor.
pub const RUNTIME_FIELD: &str = "blaze_runtime_environment";
/// Descriptor field containing native root paths and content identities, without environments.
pub const BINDINGS_FIELD: &str = "blaze_native_bindings";

/// The native image that consumes the compiled objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkMode {
    /// Link an executable containing a caller entry point.
    Executable,
    /// Link a shared library.
    Shared,
}

/// Ordered native linker arguments surrounding the compiled object inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPlan {
    /// The emitted Rust target triple.
    pub target: String,
    /// Whether the consumer links an executable or a shared library.
    pub mode: LinkMode,
    /// Ordered linker arguments preceding the caller and compiled objects.
    pub before: Vec<OsString>,
    /// Ordered libraries and linker arguments following those objects.
    pub after: Vec<OsString>,
}

#[derive(Clone)]
struct LinkPlanRecord(u32, String, String, u32, Vec<Vec<u8>>, Vec<Vec<u8>>);

impl crate::Wire for LinkPlanRecord {
    fn encode(&self, encoder: &mut crate::Encoder) {
        encoder.array(6);
        self.0.encode(encoder);
        self.1.encode(encoder);
        self.2.encode(encoder);
        self.3.encode(encoder);
        self.4.encode(encoder);
        self.5.encode(encoder);
    }

    fn decode(reader: &mut crate::Reader<'_>) -> Result<Self, crate::Error> {
        reader.array_start(6)?;
        let record = Self(
            u32::decode(reader)?,
            String::decode(reader)?,
            String::decode(reader)?,
            u32::decode(reader)?,
            Vec::decode(reader)?,
            Vec::decode(reader)?,
        );
        reader.array_end()?;
        Ok(record)
    }
}

impl LinkPlan {
    /// Encode the version-one CBOR record using this host's native argument encoding.
    pub fn encode(&self) -> Vec<u8> {
        crate::encode(&LinkPlanRecord(
            1_u32,
            native_encoding().to_owned(),
            self.target.clone(),
            match self.mode {
                LinkMode::Executable => 0_u32,
                LinkMode::Shared => 1_u32,
            },
            self.before.iter().map(|arg| encoded_value(arg)).collect::<Vec<_>>(),
            self.after.iter().map(|arg| encoded_value(arg)).collect::<Vec<_>>(),
        ))
    }

    /// Decode a bounded record, rejecting foreign encodings and invalid process arguments.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let LinkPlanRecord(version, encoding, target, mode, before, after) =
            crate::decode(bytes, &crate::Limits::DOCUMENTED).map_err(io::Error::other)?;
        if version != 1 {
            return Err(invalid("unknown native link plan version"));
        }
        if encoding != native_encoding() {
            return Err(invalid("unsupported native link plan encoding"));
        }
        if target.is_empty() {
            return Err(invalid("native link plan target is empty"));
        }
        let mode = match mode {
            0 => LinkMode::Executable,
            1 => LinkMode::Shared,
            _ => return Err(invalid("unknown native link plan mode")),
        };
        Ok(Self {
            target,
            mode,
            before: before.into_iter().map(native_value).collect::<io::Result<_>>()?,
            after: after.into_iter().map(native_value).collect::<io::Result<_>>()?,
        })
    }

    /// Render the record as JSON with native argument bytes preserved as integer arrays.
    pub fn render(&self) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "encoding": native_encoding(),
            "target": self.target,
            "mode": match self.mode {
                LinkMode::Executable => "executable",
                LinkMode::Shared => "shared",
            },
            "before": self.before.iter().map(|arg| encoded_value(arg)).collect::<Vec<_>>(),
            "after": self.after.iter().map(|arg| encoded_value(arg)).collect::<Vec<_>>(),
        })
    }
}

fn native_encoding() -> &'static str {
    if cfg!(windows) {
        "windows-utf16le"
    } else if cfg!(unix) {
        "unix-bytes"
    } else {
        "utf8"
    }
}

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
                    .map(|(key, value)| (encoded_value(key), encoded_value(value)))
                    .collect(),
                unit.flags.iter().map(|flag| encoded_value(flag)).collect(),
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
            let key = native_value(key)?;
            if key.is_empty() || key.as_encoded_bytes().contains(&b'=') {
                return Err(invalid("invalid native environment name"));
            }
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
    let object =
        project.as_object_mut().ok_or_else(|| invalid("compiler description is not an object"))?;
    object.remove(UNIT_FIELD);
    object.remove(RUNTIME_FIELD);
    Ok(())
}

fn encoded_value(value: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        value.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
    #[cfg(not(windows))]
    {
        value.as_encoded_bytes().to_vec()
    }
}

fn native_value(bytes: Vec<u8>) -> io::Result<OsString> {
    #[cfg(windows)]
    let value = {
        use std::os::windows::ffi::OsStringExt;
        if bytes.len() % 2 != 0 {
            return Err(invalid("native Windows value is not UTF-16LE"));
        }
        let wide: Vec<_> =
            bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        OsString::from_wide(&wide)
    };
    #[cfg(unix)]
    let value = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    };
    #[cfg(not(any(unix, windows)))]
    let value = String::from_utf8(bytes)
        .map_err(|_| invalid("native bytes cannot be represented on this host"))?
        .into();
    if value.as_encoded_bytes().contains(&0) {
        return Err(invalid("native process value contains a NUL"));
    }
    Ok(value)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_plan_modes_roundtrip_without_environment_fields() {
        for (mode, rendered_mode) in
            [(LinkMode::Executable, "executable"), (LinkMode::Shared, "shared")]
        {
            let plan = LinkPlan {
                target: "x86_64-unknown-linux-gnu".into(),
                mode,
                before: vec!["-shared".into(), "-Lnative".into()],
                after: vec!["-lm".into(), "-lm".into()],
            };
            let decoded = LinkPlan::decode(&plan.encode()).unwrap();
            assert_eq!(decoded, plan);
            assert_eq!(decoded.render()["mode"], rendered_mode);
            assert_eq!(decoded.render().as_object().unwrap().len(), 6);
        }
    }

    #[test]
    #[cfg(unix)]
    fn link_plan_has_the_documented_cbor_shape() {
        let plan = LinkPlan {
            target: "x".into(),
            mode: LinkMode::Executable,
            before: vec!["a".into()],
            after: vec!["b".into()],
        };
        let bytes = b"\x86\x01\x6aunix-bytes\x61x\x00\x81\x81\x18a\x81\x81\x18b";
        assert_eq!(plan.encode(), bytes);
        assert_eq!(LinkPlan::decode(bytes).unwrap(), plan);
    }

    #[test]
    #[cfg(unix)]
    fn link_plan_preserves_order_and_raw_bytes_in_cbor_and_json() {
        use std::os::unix::ffi::OsStringExt;
        let plan = LinkPlan {
            target: "x86_64-unknown-linux-gnu".into(),
            mode: LinkMode::Executable,
            before: vec![OsString::from_vec(b"-L/native-\xff".to_vec()), "".into()],
            after: vec!["-lm".into(), "-lc".into(), "-lm".into()],
        };
        let before = vec![b"-L/native-\xff".to_vec(), vec![]];
        let after = vec![b"-lm".to_vec(), b"-lc".to_vec(), b"-lm".to_vec()];
        let envelope = LinkPlanRecord(
            1_u32,
            "unix-bytes".into(),
            "x86_64-unknown-linux-gnu".into(),
            0_u32,
            before.clone(),
            after.clone(),
        );
        assert_eq!(plan.encode(), crate::encode(&envelope));
        let decoded = LinkPlan::decode(&plan.encode()).unwrap();
        assert_eq!(decoded, plan);
        assert_eq!(
            decoded.render(),
            serde_json::json!({
                "version": 1,
                "encoding": "unix-bytes",
                "target": "x86_64-unknown-linux-gnu",
                "mode": "executable",
                "before": before,
                "after": after,
            })
        );
    }

    #[test]
    fn invalid_link_plan_envelopes_fail_decoding() {
        let valid = LinkPlanRecord(
            1,
            native_encoding().into(),
            "x86_64-unknown-linux-gnu".into(),
            0,
            vec![],
            vec![],
        );
        let mut bad = valid.clone();
        bad.0 = 2;
        assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
        for encoding in ["unknown", "unix-bytes", "windows-utf16le", "utf8"] {
            if encoding == native_encoding() {
                continue;
            }
            let mut bad = valid.clone();
            bad.1 = encoding.into();
            assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
        }
        let mut bad = valid.clone();
        bad.2.clear();
        assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
        let mut bad = valid.clone();
        bad.3 = 2;
        assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
        let mut bad = valid.clone();
        bad.4.push(encoded_value(std::ffi::OsStr::new("bad\0arg")));
        assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
        let mut bad = valid;
        bad.5.push(encoded_value(std::ffi::OsStr::new("bad\0arg")));
        assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
        for bytes in [vec![], vec![0xff], crate::encode(&(1_u32, "unix-bytes".to_owned()))] {
            assert!(LinkPlan::decode(&bytes).is_err());
        }
    }

    #[test]
    #[cfg(windows)]
    fn link_plan_windows_surrogates_use_utf16le() {
        use std::os::windows::ffi::OsStringExt;
        let plan = LinkPlan {
            target: "x86_64-pc-windows-msvc".into(),
            mode: LinkMode::Shared,
            before: vec![OsString::from_wide(&[0xd800, 0x013d])],
            after: vec![OsString::from_wide(&[0xdc00])],
        };
        let decoded = LinkPlan::decode(&plan.encode()).unwrap();
        assert_eq!(decoded, plan);
        assert_eq!(
            decoded.render(),
            serde_json::json!({
                "version": 1,
                "encoding": "windows-utf16le",
                "target": "x86_64-pc-windows-msvc",
                "mode": "shared",
                "before": [[0x00, 0xd8, 0x3d, 0x01]],
                "after": [[0x00, 0xdc]],
            })
        );
        let bad = LinkPlanRecord(
            1_u32,
            "windows-utf16le".into(),
            plan.target.clone(),
            1_u32,
            vec![vec![0x41_u8]],
            vec![],
        );
        assert!(LinkPlan::decode(&crate::encode(&bad)).is_err());
    }

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
        let mut private = serde_json::json!({"crates": [], UNIT_FIELD: encoded, RUNTIME_FIELD: encoded, BINDINGS_FIELD: [1, 2]});
        let stdin = private.clone();
        redact_transport(&mut private).unwrap();
        assert!(stdin.get(UNIT_FIELD).is_some());
        assert!(private.get(UNIT_FIELD).is_none());
        assert!(stdin.get(RUNTIME_FIELD).is_some());
        assert!(private.get(RUNTIME_FIELD).is_none());
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
    #[cfg(unix)]
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
    #[test]
    #[cfg(windows)]
    fn windows_surrogates_and_non_ascii_environment_names_use_utf16le() {
        use std::os::windows::ffi::OsStringExt;
        let name = OsString::from_wide(&[0x013d, 0xd800]);
        let value = OsString::from_wide(&[0xdc00, 0x0061]);
        let units =
            BTreeMap::from([(0, Unit { env: [(name, value.clone())].into(), flags: vec![value] })]);
        assert_eq!(decode_units(&encode_units(&units)).unwrap(), units);
        assert!(native_value(vec![0x41]).is_err());
        assert!(native_value(vec![0x00, 0x00]).is_err());
        let bad = Unit { env: [(OsString::from("="), OsString::new())].into(), flags: Vec::new() };
        assert!(decode_units(&encode_units(&BTreeMap::from([(0, bad)]))).is_err());
    }
}
