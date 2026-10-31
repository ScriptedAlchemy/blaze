//! The runtime inputs of one successful daemon build, carried in its private reply.

use crate::process;
use anyhow::{Context as _, ensure};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

const FRAME: &str = "\truntime-v1=";
type Wire = ((u32, u8), blaze_wire::Bytes, (Vec<blaze_wire::Bytes>, (blaze_wire::Bytes, String)));

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Context {
    pub(crate) env: process::Native,
    pub(crate) libraries: Vec<PathBuf>,
    pub(crate) sysroot_lib: PathBuf,
    pub(crate) triple: String,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeContext")
            .field("env", &self.env)
            .field("library_count", &self.libraries.len())
            .field("triple", &self.triple)
            .finish()
    }
}

pub(crate) fn package_variable(name: &str) -> bool {
    matches!(
        name,
        "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_PKG_NAME"
            | "CARGO_PKG_VERSION"
            | "CARGO_PKG_VERSION_MAJOR"
            | "CARGO_PKG_VERSION_MINOR"
            | "CARGO_PKG_VERSION_PATCH"
            | "CARGO_PKG_VERSION_PRE"
            | "CARGO_PKG_AUTHORS"
            | "CARGO_PKG_DESCRIPTION"
            | "CARGO_PKG_HOMEPAGE"
            | "CARGO_PKG_REPOSITORY"
            | "CARGO_PKG_LICENSE"
            | "CARGO_PKG_LICENSE_FILE"
            | "CARGO_PKG_RUST_VERSION"
            | "CARGO_PKG_README"
    )
}

impl Context {
    fn encode(&self) -> String {
        let units = BTreeMap::from([(
            0,
            blaze_wire::native::Unit { env: self.env.vars().collect(), flags: Vec::new() },
        )]);
        let bytes = |path: &Path| blaze_wire::Bytes::new(process::os_bytes(path.as_os_str()));
        let wire: Wire = (
            (1, u8::from(cfg!(windows))),
            blaze_wire::Bytes::new(blaze_wire::native::encode_units(&units)),
            (
                self.libraries.iter().map(|it| bytes(it)).collect(),
                (bytes(&self.sysroot_lib), self.triple.clone()),
            ),
        );
        blaze_wire::encode(&wire).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn decode(text: &str) -> anyhow::Result<Self> {
        let fail = || anyhow::anyhow!("invalid private runtime context");
        ensure!(
            text.len() % 2 == 0 && text.len() <= crate::socket::LONGEST_REQUEST as usize,
            "invalid private runtime context length"
        );
        let bytes = text
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte| match byte {
                    b'0'..=b'9' => Ok(byte - b'0'),
                    b'a'..=b'f' => Ok(byte - b'a' + 10),
                    _ => Err(fail()),
                };
                Ok(digit(pair[0])? * 16 + digit(pair[1])?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let ((version, platform), units, (libraries, (sysroot, triple))): Wire =
            blaze_wire::decode(&bytes, &blaze_wire::Limits::DOCUMENTED).map_err(|_| fail())?;
        ensure!(
            version == 1 && platform == u8::from(cfg!(windows)),
            "unknown private runtime context version or platform"
        );
        let mut units = blaze_wire::native::decode_units(&units.bytes).map_err(|_| fail())?;
        ensure!(
            units.len() == 1 && units.get(&0).is_some_and(|unit| unit.flags.is_empty()),
            "invalid private runtime environment shape"
        );
        let path = |bytes: blaze_wire::Bytes| -> anyhow::Result<PathBuf> {
            let path = PathBuf::from(process::from_os_bytes(bytes.bytes).map_err(|_| fail())?);
            ensure!(
                path.is_absolute() && !path.as_os_str().as_encoded_bytes().contains(&0),
                "invalid private runtime path"
            );
            Ok(path)
        };
        ensure!(
            !triple.is_empty() && !triple.contains(['\n', '\r', '\0']),
            "invalid runtime target"
        );
        Ok(Self {
            env: process::Native::of(units.remove(&0).unwrap().env),
            libraries: libraries.into_iter().map(path).collect::<anyhow::Result<_>>()?,
            sysroot_lib: path(sysroot)?,
            triple,
        })
    }

    pub(crate) fn reply(&self, status: &str) -> String {
        format!("ok {status}{FRAME}{}", self.encode())
    }

    pub(crate) fn environment(
        &self,
        profile: &Path,
        inherited: &process::Native,
    ) -> anyhow::Result<process::Native> {
        let mut env: BTreeMap<_, _> = inherited.vars().chain(self.env.vars()).collect();
        let path = super::loader::path(
            &self.libraries,
            profile,
            &self.sysroot_lib,
            &self.triple,
            |name| env.get(OsStr::new(name)).map(OsString::as_os_str),
        )?;
        env.insert(super::loader::variable(&self.triple).into(), path);
        Ok(process::Native::of(env))
    }
}

impl serde::Serialize for Context {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encode())
    }
}
impl<'de> serde::Deserialize<'de> for Context {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::decode(&<String as serde::Deserialize>::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

pub(crate) fn split_reply(reply: &str) -> anyhow::Result<(String, Option<Context>)> {
    let Some((status, encoded)) = reply.split_once(FRAME) else {
        return Ok((reply.to_owned(), None));
    };
    ensure!(status.starts_with("ok ") && !status.contains('\t'), "invalid runtime reply status");
    Ok((status.to_owned(), Some(Context::decode(encoded)?)))
}

pub(crate) fn cargo_frontend(compiler: &Path, work: &Path) -> anyhow::Result<PathBuf> {
    let sibling = compiler.with_file_name(format!("blaze{}", std::env::consts::EXE_SUFFIX));
    if sibling.is_file() {
        return Ok(sibling);
    }
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};
        let shell = std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join("sh"))
                    .find(|path| path.is_file())
            })
            .context("the owned Cargo frontend needs the installed blaze sibling or a shell")?;
        let shell = shell.canonicalize()?;
        ensure!(
            !shell.as_os_str().as_bytes().contains(&b'\n')
                && !shell.as_os_str().as_bytes().contains(&b' '),
            "owned Cargo launcher interpreter cannot be named in a shebang"
        );
        crate::socket::private_dir(work)?;
        let path = work.join("blaze");
        let mut source = b"#!".to_vec();
        source.extend_from_slice(shell.as_os_str().as_bytes());
        source.extend_from_slice(b"\nexec '");
        for &byte in compiler.as_os_str().as_bytes() {
            if byte == b'\'' {
                source.extend_from_slice(b"'\\''");
            } else {
                source.push(byte);
            }
        }
        source.extend_from_slice(b"' cargo \"$@\"\n");
        match std::fs::read(&path) {
            Ok(existing) => {
                ensure!(existing == source, "owned Cargo launcher has different contents");
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(&source)?;
        file.sync_all()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500))?;
        Ok(path)
    }
    #[cfg(not(unix))]
    anyhow::bail!("the owned Cargo frontend blaze is not installed beside the compiler")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_overlay_does_not_copy_arbitrary_or_compile_only_process_controls() {
        for name in [
            "CARGO_PRIMARY_PACKAGE",
            "CARGO_BIN_NAME",
            "CARGO_CRATE_NAME",
            "CARGO_PKG_CALLER_SECRET",
            "RA_JIT_INPUTS_KEY",
            "OUT_DIR",
            "PATH",
        ] {
            assert!(!package_variable(name), "{name}");
        }
        assert!(package_variable("CARGO_PKG_NAME"));
        assert!(package_variable("CARGO_MANIFEST_DIR"));
    }

    #[test]
    fn runtime_reply_is_versioned_and_redacts_values() {
        let context = Context {
            env: process::Native::of([("RUNTIME_SECRET".into(), "do-not-log".into())]),
            libraries: vec![PathBuf::from("/generated/lib")],
            sysroot_lib: "/sysroot/lib".into(),
            triple: "x86_64-unknown-linux-gnu".into(),
        };
        let (status, decoded) = split_reply(&context.reply("unchanged")).unwrap();
        assert_eq!(status, "ok unchanged");
        assert_eq!(decoded, Some(context.clone()));
        assert!(!format!("{context:?}").contains("do-not-log"));
        assert!(split_reply("ok unchanged\truntime-v1=00").is_err());
        assert_eq!(split_reply("ok unchanged").unwrap(), ("ok unchanged".into(), None));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_loader_keeps_native_bytes_and_caller_ambient_values() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = OsString::from_vec(b"/generated-\xff/lib".to_vec());
        let context = Context {
            env: process::Native::of([(
                "OUT_DIR".into(),
                OsString::from_vec(b"/out-\xfe".to_vec()),
            )]),
            libraries: vec![PathBuf::from(&raw)],
            sysroot_lib: "/sysroot/lib".into(),
            triple: "x86_64-unknown-linux-gnu".into(),
        };
        let context = split_reply(&context.reply("built")).unwrap().1.unwrap();
        let inherited = process::Native::of([
            ("CALLER".into(), "kept".into()),
            ("LD_LIBRARY_PATH".into(), "/prior".into()),
        ]);
        let env: BTreeMap<_, _> =
            context.environment(Path::new("/profile"), &inherited).unwrap().vars().collect();
        assert_eq!(env[OsStr::new("CALLER")], "kept");
        assert_eq!(
            std::env::split_paths(&env[OsStr::new("LD_LIBRARY_PATH")]).collect::<Vec<_>>(),
            [
                PathBuf::from(raw),
                "/profile/deps".into(),
                "/profile".into(),
                "/sysroot/lib".into(),
                "/prior".into()
            ]
        );
    }
}
