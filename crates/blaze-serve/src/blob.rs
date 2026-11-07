//! The names of the objects the server keeps, and the one thing a name has to say: which it is.
//!
//! [`ObjectName`] reads that out of the name, and refuses a name that is not of the layout, so
//! that no request reaches a file outside the directory.

use std::path::{Path, PathBuf};

use blaze_wire::Digest;

/// Which object a name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `packs/<sha256>.pack`: a sealed pack, named by the hash of its bytes.
    Pack,
    /// `manifests/<sha256>`: the packs a unit is made of, named by the unit's digest.
    Manifest,
}

/// A name of the layout, read apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectName {
    /// Which object it is.
    pub kind: Kind,
    /// The name as the layout writes it, such as `packs/<sha256>.pack`.
    pub name: String,
    /// The digest it is named by.
    pub digest: Digest,
}

impl ObjectName {
    /// The name `text` is, or `None` when it is not of the layout.
    pub fn parse(text: &str) -> Option<ObjectName> {
        let (dir, rest) = text.split_once('/')?;
        let digest = |hex: &str| -> Option<Digest> {
            if hex.len() != 64
                || !hex.bytes().all(|it| it.is_ascii_digit() || (b'a'..=b'f').contains(&it))
            {
                return None;
            }
            let mut out = [0u8; 32];
            for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
                *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
            }
            Some(out)
        };
        let (kind, digest) = match dir {
            "packs" => (Kind::Pack, digest(rest.strip_suffix(".pack")?)?),
            "manifests" => (Kind::Manifest, digest(rest)?),
            _ => return None,
        };
        Some(ObjectName { kind, name: text.to_owned(), digest })
    }

    /// Whether this is a blob, which is what a pack is.
    pub fn is_blob(&self) -> bool {
        self.kind == Kind::Pack
    }

    /// Where the object is, under the cache directory.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.name)
    }
}

/// A digest as its name writes it: 64 lower-case hex digits.
pub(crate) fn hex(digest: &Digest) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_of_each_kind_is_read() {
        let digest = hex(&[0xab; 32]);
        for (text, kind) in [
            (format!("packs/{digest}.pack"), Kind::Pack),
            (format!("manifests/{digest}"), Kind::Manifest),
        ] {
            let name = ObjectName::parse(&text).unwrap();
            assert_eq!(name.kind, kind);
            assert_eq!(name.digest, [0xab; 32]);
        }
    }

    #[test]
    fn a_name_that_is_not_of_the_layout_is_refused() {
        let digest = hex(&[0xab; 32]);
        for text in [
            "".to_owned(),
            "packs".to_owned(),
            "packs/".to_owned(),
            "packs/../packs".to_owned(),
            "packs//etc/passwd".to_owned(),
            "packs/abc.pack".to_owned(),
            format!("packs/{digest}"),
            format!("packs/{digest}.pack.zst"),
            format!("packs/{}.pack", &digest[..62]),
            format!("blobs/{digest}"),
            format!("keys/{digest}"),
            format!("units/{digest}"),
            format!("deps/{digest}"),
            "manifests/default".to_owned(),
            "manifests/.hidden".to_owned(),
            format!("manifests/{digest}/b"),
            format!("manifests/{}", digest.to_uppercase()),
        ] {
            assert!(ObjectName::parse(&text).is_none(), "{text} is a name of the layout");
        }
    }
}
