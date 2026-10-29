//! Local freshness records compare keyed identities, never captured variable values.
use sha2::{Digest as _, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read as _, Write as _},
};

pub(crate) struct Identity([u8; 32]);

impl Identity {
    pub(crate) fn load() -> anyhow::Result<Self> {
        let home = crate::cargo::daemon::home()?;
        crate::socket::private_dir(&home)?;
        Self::at(&home.join("env-identity-key"))
    }

    fn at(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut file = crate::socket::private_file(
            OpenOptions::new().read(true).write(true).create(true).truncate(false),
            path,
        )?;
        file.lock()?;
        let mut bytes = Vec::new();
        (&mut file).take(33).read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            let mut key = [0; 32];
            getrandom::fill(&mut key)
                .map_err(|_| anyhow::anyhow!("environment identity entropy"))?;
            file.write_all(&key)?;
            file.sync_all()?;
            return Ok(Self(key));
        }
        bytes.try_into().map(Self).map_err(|_| anyhow::anyhow!("invalid environment identity key"))
    }

    pub(crate) fn value(&self, value: Option<&str>) -> Option<String> {
        value.map(|value| {
            // HMAC-SHA256 with one private per-home key for the whole freshness
            // judgment. No file I/O or fallible lookup inside freshness callbacks.
            let mut inner = Sha256::new();
            let mut pad = [0x36; 64];
            for (to, from) in pad.iter_mut().zip(self.0) {
                *to ^= from;
            }
            inner.update(pad);
            inner.update(b"blaze-env-identity-v1\0");
            inner.update(value.as_bytes());
            let mut outer = Sha256::new();
            let mut pad = [0x5c; 64];
            for (to, from) in pad.iter_mut().zip(self.0) {
                *to ^= from;
            }
            outer.update(pad);
            outer.update(inner.finalize());
            format!("hmac-sha256-v1:{:x}", outer.finalize())
        })
    }
}

pub(crate) fn value(value: Option<&str>) -> anyhow::Result<Option<String>> {
    match value {
        Some(value) => Ok(Identity::load()?.value(Some(value))),
        None => Ok(None),
    }
}

pub(crate) fn name(name: &std::ffi::OsStr) -> String {
    name.to_str().map(str::to_owned).unwrap_or_else(|| {
        format!(
            "native-name:{}",
            name.as_encoded_bytes().iter().map(|it| format!("{it:02x}")).collect::<String>()
        )
    })
}

/// Persistent compiler records retain environment field names only. Values
/// are delivered to daemon memory over the existing socket and restored only
/// while that daemon lives; a lost snapshot makes the record unreadable.
pub(crate) fn disk_json(
    path: &std::path::Path,
    value: &serde_json::Value,
) -> anyhow::Result<Vec<u8>> {
    let mut kept = value.clone();
    let mut removed = std::collections::BTreeMap::new();
    fn strip(
        value: &mut serde_json::Value,
        prefix: String,
        removed: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        match value {
            serde_json::Value::Object(fields) => {
                for (name, field) in fields.iter_mut() {
                    let at = format!("{prefix}/{}", name.replace('~', "~0").replace('/', "~1"));
                    if matches!(name.as_str(), "env" | "native_env" | "crate_env" | "configured")
                        && !field.is_null()
                    {
                        removed.insert(at, field.take());
                        *field = serde_json::Value::Null;
                    } else {
                        strip(field, at, removed);
                    }
                }
            }
            serde_json::Value::Array(fields) => {
                for (index, field) in fields.iter_mut().enumerate() {
                    strip(field, format!("{prefix}/{index}"), removed);
                }
            }
            _ => {}
        }
    }
    strip(&mut kept, String::new(), &mut removed);
    if !removed.is_empty() {
        let snapshot = crate::process::Native::of([(
            "fields".into(),
            serde_json::to_string(&removed)?.into(),
        )]);
        crate::cargo::client::ask(
            crate::cargo::daemon::Request::RememberScript { dir: path.to_owned(), env: snapshot },
            crate::cargo::client::Start::Never,
        )?;
        kept["_memory_environment_fields"] = serde_json::json!(removed.keys().collect::<Vec<_>>());
    }
    serde_json::to_vec(&kept).map_err(Into::into)
}

pub(crate) fn read_json(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context as _;
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if value.get("_memory_environment_fields").is_some() {
        let snapshot = crate::cargo::script::environment(path)
            .context("record environment is no longer in daemon memory")?;
        let text = snapshot
            .vars()
            .find(|(name, _)| name == "fields")
            .context("record environment fields")?
            .1;
        let fields: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_str(text.to_str().context("record environment encoding")?)?;
        for (path, field) in fields {
            *value.pointer_mut(&path).context("record environment field missing")? = field;
        }
        value.as_object_mut().context("record object")?.remove("_memory_environment_fields");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn private_identities_keep_the_existing_hmac_format() {
        assert_eq!(
            super::Identity([11; 32]).value(Some("hello")).as_deref(),
            Some("hmac-sha256-v1:fca1cee17e4f97d87ff30c25c67828ca4cda8b5ce316bdd86509eac5d35db876")
        );
    }

    #[test]
    fn a_corrupt_key_is_an_error_and_an_active_judgment_keeps_its_key() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("key");
        let identity = super::Identity::at(&path).unwrap();
        let before = identity.value(Some("credential"));
        assert_eq!(before, super::Identity::at(&path).unwrap().value(Some("credential")));
        for bytes in [vec![1], vec![2; 33], vec![3; 4096]] {
            std::fs::write(&path, &bytes).unwrap();
            let error = super::Identity::at(&path).err().unwrap();
            assert!(error.to_string().contains("invalid environment identity key"));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(identity.value(Some("credential")), before);
        }
    }

    #[test]
    fn nested_record_fields_are_kept_only_in_memory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("record.json");
        let secret = "synthetic-env-disk-secret";
        let value = serde_json::json!({
            "checkouts": [{"env": {"TOKEN": secret}, "native_env": null}],
            "configured": [{"name": "TOKEN", "value": secret}],
            "key": "ordinary"
        });
        // A missing daemon is harmless for serialization and loses read eligibility.
        let bytes = super::disk_json(&path, &value).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
        let fields = std::collections::BTreeMap::from([
            ("/checkouts/0/env", value["checkouts"][0]["env"].clone()),
            ("/configured", value["configured"].clone()),
        ]);
        crate::cargo::script::remember(
            path.clone(),
            crate::process::Native::of([(
                "fields".into(),
                serde_json::to_string(&fields).unwrap().into(),
            )]),
        );
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(super::read_json(&path).unwrap(), value);
    }

    #[test]
    fn identities_preserve_absence_and_empty_without_values() {
        let secret = "c4-synthetic-credential-for-identity";
        let first = super::value(Some(secret)).unwrap();
        assert!(!first.as_ref().unwrap().contains(secret));
        assert_eq!(first, super::value(Some(secret)).unwrap());
        let prefix_value = "hmac-sha256-v1:source-value";
        assert_ne!(super::value(Some(prefix_value)).unwrap().as_deref(), Some(prefix_value));
        assert_ne!(first, super::value(Some("another")).unwrap());
        assert_ne!(super::value(None).unwrap(), super::value(Some("")).unwrap());
    }
}
