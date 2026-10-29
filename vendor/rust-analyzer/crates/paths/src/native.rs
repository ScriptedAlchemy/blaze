//! A reversible UTF-8 namespace for native compiler paths. The namespace is
//! logical: filesystem reads resolve it to the authoritative native path.

use std::{
    borrow::Cow,
    io,
    path::{Component, Path, PathBuf},
    sync::RwLock,
};

use crate::{AbsPath, AbsPathBuf};

static PROJECTION: RwLock<Option<Projection>> = RwLock::new(None);

#[derive(Clone, Debug)]
pub struct Projection {
    prefix: AbsPathBuf,
}

impl Projection {
    pub fn new(prefix: AbsPathBuf) -> io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = prefix;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native path namespace is not available on this platform",
            ))
        }
        #[cfg(unix)]
        {
            if prefix.parent().is_none() || prefix.as_path().normalize() != prefix {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid native path namespace",
                ));
            }
            Ok(Self { prefix })
        }
    }

    pub fn prefix(&self) -> &AbsPath {
        &self.prefix
    }

    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(AsRef::<Path>::as_ref(&self.prefix))
    }

    #[cfg(unix)]
    pub fn encode(&self, native: &Path) -> io::Result<AbsPathBuf> {
        use std::os::unix::ffi::OsStrExt as _;
        if !native.is_absolute() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "native path is not absolute"));
        }
        let mut names = Vec::new();
        for component in native.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    names.pop();
                }
                Component::Normal(name) => names.push(encode_component(name.as_bytes())),
                Component::Prefix(_) => unreachable!(),
            }
        }
        let mut logical = self.prefix.clone();
        for name in names {
            logical.push(name);
        }
        Ok(logical)
    }

    #[cfg(not(unix))]
    pub fn encode(&self, _native: &Path) -> io::Result<AbsPathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native path namespace is not available on this platform",
        ))
    }

    #[cfg(unix)]
    pub fn decode(&self, logical: &Path) -> io::Result<Option<PathBuf>> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
        let Ok(relative) = logical.strip_prefix(AsRef::<Path>::as_ref(&self.prefix)) else {
            return Ok(None);
        };
        let mut native = PathBuf::from("/");
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    let text = name.to_str().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 logical path")
                    })?;
                    let bytes = decode_component(text)?;
                    if encode_component(&bytes) != text {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "noncanonical native path component",
                        ));
                    }
                    native.push(OsString::from_vec(bytes));
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    native.pop();
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid native path component",
                    ));
                }
            }
        }
        Ok(Some(native))
    }

    #[cfg(not(unix))]
    pub fn decode(&self, _logical: &Path) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }

    pub fn join(&self, base: &AbsPath, literal: &str) -> io::Result<AbsPathBuf> {
        let native = self.decode(base.as_ref())?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path is outside native namespace")
        })?;
        self.encode(&native.join(literal))
    }
}

/// Install the compiler descriptor's namespace before loading its source graph.
pub fn install(projection: Option<Projection>) {
    *PROJECTION.write().unwrap_or_else(|poison| poison.into_inner()) = projection;
}

/// Resolve a logical filename for an actual filesystem operation.
pub fn resolve(path: &Path) -> io::Result<Cow<'_, Path>> {
    let projection = PROJECTION.read().unwrap_or_else(|poison| poison.into_inner());
    match projection.as_ref().map(|projection| projection.decode(path)).transpose()?.flatten() {
        Some(native) => Ok(Cow::Owned(native)),
        None => Ok(Cow::Borrowed(path)),
    }
}

/// Encode a name discovered while traversing a projected source root.
pub fn encode(path: &Path) -> io::Result<AbsPathBuf> {
    PROJECTION
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no native path namespace"))?
        .encode(path)
}

pub fn is_projected(path: &Path) -> bool {
    PROJECTION
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .is_some_and(|projection| projection.contains(path))
}

/// Source path strings are native names, not encoded namespace components.
pub fn absolutize(base: &AbsPath, literal: &str) -> Option<AbsPathBuf> {
    let projection = PROJECTION.read().unwrap_or_else(|poison| poison.into_inner());
    if let Some(projection) =
        projection.as_ref().filter(|projection| projection.contains(base.as_ref()))
    {
        return projection.join(base, literal).ok();
    }
    Some(base.absolutize(literal))
}

#[cfg(unix)]
fn encode_component(mut bytes: &[u8]) -> String {
    let mut out = String::new();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                for ch in text.chars() {
                    if ch == '~' {
                        out.push_str("~7e");
                    } else {
                        out.push(ch);
                    }
                }
                break;
            }
            Err(error) => {
                let valid = &bytes[..error.valid_up_to()];
                // The UTF-8 validator returned this exact valid prefix.
                let valid = std::str::from_utf8(valid).expect("valid UTF-8 prefix");
                for ch in valid.chars() {
                    if ch == '~' {
                        out.push_str("~7e");
                    } else {
                        out.push(ch);
                    }
                }
                let invalid = error.error_len().unwrap_or(bytes.len() - valid.len());
                let start = valid.len();
                for byte in &bytes[start..start + invalid] {
                    use std::fmt::Write as _;
                    write!(out, "~{byte:02x}").expect("writing to String");
                }
                bytes = &bytes[start + invalid..];
            }
        }
    }
    out
}

#[cfg(unix)]
fn decode_component(text: &str) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut bytes = text.as_bytes();
    while !bytes.is_empty() {
        if bytes[0] == b'~' {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            let byte = bytes
                .get(1)
                .copied()
                .and_then(digit)
                .zip(bytes.get(2).copied().and_then(digit))
                .map(|(hi, lo)| (hi << 4) | lo)
                .filter(|byte| !matches!(byte, 0 | b'/'))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid native path escape")
                })?;
            out.push(byte);
            bytes = &bytes[3..];
        } else {
            out.push(bytes[0]);
            bytes = &bytes[1..];
        }
    }
    if out.is_empty() || out == b"." || out == b".." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid native path name"));
    }
    Ok(out)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    fn projection() -> Projection {
        Projection::new(AbsPathBuf::assert("/compiler/native-paths".into())).unwrap()
    }

    #[test]
    fn every_native_component_and_relative_ancestor_round_trips() {
        let native = PathBuf::from(OsString::from_vec(
            b"/source-\xff/a/nested-\xfe/src/main-\xfd.rs".to_vec(),
        ));
        let projection = projection();
        let encoded = projection.encode(&native).unwrap();
        assert_eq!(projection.decode(encoded.as_ref()).unwrap(), Some(native.clone()));
        let sibling = projection.join(encoded.parent().unwrap(), "../../../peer/data").unwrap();
        assert_eq!(
            projection.decode(sibling.as_ref()).unwrap(),
            Some(PathBuf::from(OsString::from_vec(b"/source-\xff/peer/data".to_vec())))
        );
        assert!(encoded.as_str().ends_with("main-~fd.rs"));
    }

    #[test]
    fn literal_escape_like_names_do_not_alias_invalid_bytes() {
        let projection = projection();
        let raw = PathBuf::from(OsString::from_vec(b"/src/raw-\xff.rs".to_vec()));
        let encoded = projection.encode(&raw).unwrap();
        let literal = projection.join(encoded.parent().unwrap(), "raw-~ff.rs").unwrap();
        assert_ne!(literal, encoded);
        assert_eq!(
            projection.decode(literal.as_ref()).unwrap(),
            Some(PathBuf::from("/src/raw-~ff.rs"))
        );
        assert!(projection.decode(Path::new("/compiler/native-paths/a/~2f")).is_err());
        assert!(projection.decode(Path::new("/compiler/native-paths/a/~00")).is_err());
        assert!(projection.decode(Path::new("/compiler/native-paths/a/~61")).is_err());
    }
}
