//! A JSON rendering of a wire value, for people: `Accept: application/json`, `--json`.
//!
//! This is never the wire and nothing's identity is taken from it. A struct renders as an object
//! whose keys are its field numbers as text, an enum as the two-element array the wire carries
//! (`[discriminant, payload]`, `Namespace::Main` as `[0,null]`), a set as an array in the set's
//! order, a byte string or a digest as lowercase hex, and an integer as a JSON number. The
//! rendering is made by reading the value's canonical CBOR back, so it is a view of the same bytes
//! the encoder writes and not a second schema.

use std::alloc::Allocator;
use std::fmt::{self, Write as _};

use crate::cbor::{Encoder, Named, Wire};

/// Renders one wire value as JSON, with its type's name as the outer key.
///
/// The rendering is built by reading the value's own canonical CBOR, so a byte string is hex and
/// not an array of integers. It ends without a newline.
pub fn to_json<T: Wire + Named, A: Allocator + Clone>(
    value: &T,
    out: &mut dyn fmt::Write,
    alloc: A,
) -> fmt::Result {
    write!(out, "{{\"{}\":", T::NAME)?;
    render(value, out, alloc)?;
    out.write_char('}')
}

/// Renders one wire value as its JSON form, without its type's name around it.
pub fn render<T: Wire, A: Allocator + Clone>(
    value: &T,
    out: &mut dyn fmt::Write,
    alloc: A,
) -> fmt::Result {
    render_cbor(&Encoder::of(value, alloc), out)
}

/// Renders one value's canonical CBOR as JSON, which is what a message's bytes are.
pub fn render_cbor(cbor: &[u8], out: &mut dyn fmt::Write) -> fmt::Result {
    render_item(cbor, out)
}

/// A borrowed lowercase hexadecimal rendering. Formatting writes directly to its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hex<'a> {
    bytes: &'a [u8],
}

impl Hex<'_> {
    /// The number of ASCII digits in this rendering.
    pub fn len(&self) -> usize {
        self.bytes.len() * 2
    }

    /// Whether there are no digits.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The digits, without allocating a string.
    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        self.bytes.iter().flat_map(|byte| {
            [DIGITS[(byte >> 4) as usize] as char, DIGITS[(byte & 15) as usize] as char]
        })
    }
}

impl fmt::Display for Hex<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for digit in self.chars() {
            out.write_char(digit)?;
        }
        Ok(())
    }
}

/// `bytes` as lowercase hex, borrowed until its destination formats it.
pub fn hex(bytes: &[u8]) -> Hex<'_> {
    Hex { bytes }
}

fn render_item(bytes: &[u8], out: &mut dyn fmt::Write) -> fmt::Result {
    let mut at = 0;
    render_at(bytes, &mut at, out)
}

fn push_hex(bytes: &[u8], out: &mut dyn fmt::Write) -> fmt::Result {
    write!(out, "\"{}\"", hex(bytes))
}

fn escaped(text: &str, out: &mut dyn fmt::Write) -> fmt::Result {
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\""),
            '\\' => out.write_str("\\\\"),
            '\n' => out.write_str("\\n"),
            '\r' => out.write_str("\\r"),
            '\t' => out.write_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32),
            c => out.write_char(c),
        }?;
    }
    Ok(())
}

fn push_string(text: &str, out: &mut dyn fmt::Write) -> fmt::Result {
    out.write_char('"')?;
    escaped(text, out)?;
    out.write_char('"')
}

struct Escaped<'a>(&'a mut dyn fmt::Write);

impl fmt::Write for Escaped<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        escaped(text, self.0)
    }
}

/// Reads one head and its argument, advancing `at` past both.
fn head(bytes: &[u8], at: &mut usize) -> (u8, u64) {
    let ib = bytes[*at];
    *at += 1;
    let major = ib >> 5;
    let ai = ib & 0x1f;
    let arg = match ai {
        0..=23 => u64::from(ai),
        24 => {
            let v = u64::from(bytes[*at]);
            *at += 1;
            v
        }
        25 => {
            let v = u64::from(u16::from_be_bytes([bytes[*at], bytes[*at + 1]]));
            *at += 2;
            v
        }
        26 => {
            let v = u64::from(u32::from_be_bytes([
                bytes[*at],
                bytes[*at + 1],
                bytes[*at + 2],
                bytes[*at + 3],
            ]));
            *at += 4;
            v
        }
        27 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[*at..*at + 8]);
            *at += 8;
            u64::from_be_bytes(b)
        }
        _ => 0,
    };
    (major, arg)
}

fn render_at(bytes: &[u8], at: &mut usize, out: &mut dyn fmt::Write) -> fmt::Result {
    let (major, arg) = head(bytes, at);
    match major {
        0 => write!(out, "{arg}"),
        // `-1 - arg` in an `i128`: `-2^64` is a well-formed CBOR item the validator accepts, and
        // `arg + 1` would overflow a `u64` in debug and render `-0` in release.
        1 => write!(out, "{}", -1 - i128::from(arg)),
        2 => {
            let end = *at + arg as usize;
            push_hex(&bytes[*at..end], out)?;
            *at = end;
            Ok(())
        }
        3 => {
            let end = *at + arg as usize;
            push_string(std::str::from_utf8(&bytes[*at..end]).unwrap_or(""), out)?;
            *at = end;
            Ok(())
        }
        4 => {
            out.write_char('[')?;
            for i in 0..arg {
                if i > 0 {
                    out.write_char(',')?;
                }
                render_at(bytes, at, out)?;
            }
            out.write_char(']')
        }
        5 => {
            out.write_char('{')?;
            for i in 0..arg {
                if i > 0 {
                    out.write_char(',')?;
                }
                // A schema map's key is a field number, which JSON writes as text.
                out.write_char('"')?;
                render_at(bytes, at, &mut Escaped(out))?;
                out.write_str("\":")?;
                render_at(bytes, at, out)?;
            }
            out.write_char('}')
        }
        _ => match arg {
            20 => out.write_str("false"),
            21 => out.write_str("true"),
            22 => out.write_str("null"),
            _ => out.write_str("null"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed<const N: usize> {
        bytes: [u8; N],
        len: usize,
    }

    impl<const N: usize> fmt::Write for Fixed<N> {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
            let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
            target.copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    #[test]
    fn rendering_uses_the_destination_and_reports_its_capacity() {
        // A real canonical map with a field number and text requiring all three escape paths.
        let cbor = [0xa1, 0x00, 0x65, b'a', b'"', b'\\', b'\n', 1];
        let mut enough = Fixed { bytes: [0; 64], len: 0 };
        render_cbor(&cbor, &mut enough).unwrap();
        assert_eq!(
            std::str::from_utf8(&enough.bytes[..enough.len]).unwrap(),
            r#"{"0":"a\"\\\n\u0001"}"#,
        );
        // Control: the same renderer and bytes with the destination capacity removed.
        let mut short = Fixed { bytes: [0; 8], len: 0 };
        assert_eq!(render_cbor(&cbor, &mut short), Err(fmt::Error));
    }
}
