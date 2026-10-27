//! The names of today's layout, and the one thing a name has to say: which object it is.
//!
//! `docs/cache.md` names the objects of a remote, and the `Dir` place names them the same way.
//! A blob's name carries the digest it is named by, and one part the digest does not: whether
//! it is a pack, a pack sent compressed, a pack's keys, or cargo's units. [`ObjectName`] reads
//! that out of the name, and refuses a name that is not one of the four, so that no request
//! reaches a file outside the directory.

use std::path::{Path, PathBuf};

use blaze_wire::Digest;

/// Which object of today's layout a name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `packs/<sha256>.pack`: a sealed pack, as the store has it.
    Pack,
    /// `packs/<sha256>.pack.zst`: a pack sent compressed whole, named by the pack it holds.
    Packed,
    /// `keys/<sha256>`: the keys of one pack.
    Keys,
    /// `units/<sha256>`: cargo's build of one dependency.
    Unit,
    /// `manifests/<project>`: the packs a project's builds were served from.
    Manifest,
    /// `deps/<project>`: the units a project's builds were served from.
    Deps,
}

/// A name of today's layout, read apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectName {
    /// Which object it is.
    pub kind: Kind,
    /// The name as the layout writes it, such as `packs/<sha256>.pack`.
    pub name: String,
    /// The digest it is named by, for the four kinds that have one.
    pub digest: Option<Digest>,
    /// The project, for a list.
    pub project: Option<String>,
}

impl ObjectName {
    /// The name `text` is, or `None` when it is not one of today's layout.
    pub fn parse(text: &str) -> Option<ObjectName> {
        let (dir, rest) = text.split_once('/')?;
        let plain = !rest.is_empty()
            && !rest.contains(['/', '\\'])
            && !rest.starts_with('.')
            && !rest.contains('\0');
        if !plain {
            return None;
        }
        let digest = |hex: &str| -> Option<Digest> {
            if hex.len() != 64 || !hex.bytes().all(|it| it.is_ascii_hexdigit()) {
                return None;
            }
            let mut out = [0u8; 32];
            for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
                *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
            }
            Some(out)
        };
        let (kind, digest) = match dir {
            "packs" => match rest.strip_suffix(".pack.zst") {
                Some(hex) => (Kind::Packed, digest(hex)?),
                None => (Kind::Pack, digest(rest.strip_suffix(".pack")?)?),
            },
            "keys" => (Kind::Keys, digest(rest)?),
            "units" => (Kind::Unit, digest(rest)?),
            "manifests" => return project(Kind::Manifest, text, rest),
            "deps" => return project(Kind::Deps, text, rest),
            _ => return None,
        };
        Some(ObjectName { kind, name: text.to_owned(), digest: Some(digest), project: None })
    }

    /// The name the object of `kind` and `digest` has, for the four kinds that are a blob.
    pub fn of(kind: Kind, digest: &Digest) -> Option<String> {
        let hex = hex(digest);
        Some(match kind {
            Kind::Pack => format!("packs/{hex}.pack"),
            Kind::Packed => format!("packs/{hex}.pack.zst"),
            Kind::Keys => format!("keys/{hex}"),
            Kind::Unit => format!("units/{hex}"),
            Kind::Manifest | Kind::Deps => return None,
        })
    }

    /// Whether this is a blob, which is every kind but a list.
    pub fn is_blob(&self) -> bool {
        !matches!(self.kind, Kind::Manifest | Kind::Deps)
    }

    /// Where the object is, under the cache directory.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.name)
    }
}

fn project(kind: Kind, text: &str, rest: &str) -> Option<ObjectName> {
    Some(ObjectName { kind, name: text.to_owned(), digest: None, project: Some(rest.to_owned()) })
}

/// A digest as its name writes it: 64 lower-case hex digits.
pub(crate) fn hex(digest: &Digest) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The bytes a `Range` header asks for, as section 8's `Range` on a blob: `bytes=<first>-[<last>]`
/// or `bytes=-<suffix>`. `None` is a range the object cannot answer, which is answered 416.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    /// From `first` to `last`, both included, `last` past the end meaning the end.
    Span {
        /// The first byte, included.
        first: u64,
        /// The last byte, included, or the object's end.
        last: Option<u64>,
    },
    /// The last `suffix` bytes.
    Suffix(u64),
}

/// What a `Range` header says, or an error naming what is not a range. A header of several
/// ranges is refused: section 8 asks for one.
pub fn parse_range(text: &str) -> Result<Range, String> {
    let rest = text
        .strip_prefix("bytes=")
        .ok_or_else(|| format!("`{text}` is not `bytes=<first>-<last>`"))?;
    if rest.contains(',') {
        return Err("more than one range is asked for".to_owned());
    }
    let (first, last) = rest.split_once('-').ok_or_else(|| format!("`{text}` has no `-`"))?;
    let number = |text: &str, what: &str| -> Result<u64, String> {
        text.parse().map_err(|_| format!("`{text}` is not the {what} of a range"))
    };
    match (first.is_empty(), last.is_empty()) {
        (true, true) => Err("a range of `bytes=-` asks for nothing".to_owned()),
        (true, false) => Ok(Range::Suffix(number(last, "suffix")?)),
        (false, true) => Ok(Range::Span { first: number(first, "first")?, last: None }),
        (false, false) => {
            let (first, last) = (number(first, "first")?, number(last, "last")?);
            if last < first {
                return Err(format!("`{text}` ends before it starts"));
            }
            Ok(Range::Span { first, last: Some(last) })
        }
    }
}

impl Range {
    /// The bytes of an object of `len` bytes this range asks for: `first` and `last` included,
    /// or `None` when it asks for nothing of it.
    pub fn of(self, len: u64) -> Option<(u64, u64)> {
        if len == 0 {
            return None;
        }
        match self {
            Range::Span { first, last } => {
                if first >= len {
                    return None;
                }
                let last = last.unwrap_or(len - 1).min(len - 1);
                Some((first, last))
            }
            Range::Suffix(suffix) => {
                if suffix == 0 {
                    return None;
                }
                Some((len.saturating_sub(suffix), len - 1))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_of_each_kind_is_read() {
        let digest = hex(&[0xab; 32]);
        for (text, kind) in [
            (format!("packs/{digest}.pack"), Kind::Pack),
            (format!("packs/{digest}.pack.zst"), Kind::Packed),
            (format!("keys/{digest}"), Kind::Keys),
            (format!("units/{digest}"), Kind::Unit),
        ] {
            let name = ObjectName::parse(&text).unwrap();
            assert_eq!(name.kind, kind);
            assert_eq!(name.digest, Some([0xab; 32]));
            assert_eq!(ObjectName::of(kind, &[0xab; 32]).unwrap(), text);
        }
        let list = ObjectName::parse("manifests/default").unwrap();
        assert_eq!((list.kind, list.project.as_deref()), (Kind::Manifest, Some("default")));
        assert!(ObjectName::parse("deps/std-aarch64-apple-darwin").is_some());
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
            format!("packs/{}.pack", &digest[..62]),
            format!("blobs/{digest}"),
            "manifests/.hidden".to_owned(),
            "manifests/a/b".to_owned(),
            "keys/x".to_owned(),
        ] {
            assert!(ObjectName::parse(&text).is_none(), "{text} is a name of the layout");
        }
    }

    #[test]
    fn a_range_asks_for_a_part_of_an_object() {
        let len = 100;
        assert_eq!(parse_range("bytes=0-").unwrap().of(len), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19").unwrap().of(len), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-200").unwrap().of(len), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10").unwrap().of(len), Some((90, 99)));
        assert_eq!(parse_range("bytes=-200").unwrap().of(len), Some((0, 99)));
        assert_eq!(parse_range("bytes=99-").unwrap().of(len), Some((99, 99)));
        // Past the end, or nothing asked for.
        assert_eq!(parse_range("bytes=100-").unwrap().of(len), None);
        assert_eq!(parse_range("bytes=100-200").unwrap().of(len), None);
        assert_eq!(parse_range("bytes=-0").unwrap().of(len), None);
        assert_eq!(parse_range("bytes=0-").unwrap().of(0), None);
        // Not a range.
        for text in ["0-10", "bytes=10", "bytes=10-9", "bytes=-", "bytes=a-b", "bytes=0-10,20-30"] {
            assert!(parse_range(text).is_err(), "{text} is a range");
        }
    }
}
