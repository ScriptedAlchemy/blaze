//! A JSON rendering of a wire value, for people: `Accept: application/json`, `--json`.
//!
//! This is never the wire and nothing's identity is taken from it. A struct renders as an object
//! whose keys are its field numbers as text, an enum as the two-element array the wire carries
//! (`[discriminant, payload]`, `Namespace::Main` as `[0,null]`), a set as an array in the set's
//! order, a byte string or a digest as lowercase hex, and an integer as a JSON number. The
//! rendering is made by reading the value's canonical CBOR back, so it is a view of the same bytes
//! the encoder writes and not a second schema.

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use crate::cbor::{Encoder, Named, Wire};

/// Renders one wire value as JSON, with its type's name as the outer key.
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

const fn hex_nibble(nibble: u8) -> char {
    match nibble & 0x0f {
        0 => '0',
        1 => '1',
        2 => '2',
        3 => '3',
        4 => '4',
        5 => '5',
        6 => '6',
        7 => '7',
        8 => '8',
        9 => '9',
        10 => 'a',
        11 => 'b',
        12 => 'c',
        13 => 'd',
        14 => 'e',
        _ => 'f',
    }
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
        self.bytes.iter().flat_map(|byte| [hex_nibble(byte >> 4), hex_nibble(*byte)])
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
            c if u32::from(c) < 0x20 => write!(out, "\\u{:04x}", u32::from(c)),
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

struct MultiEscaped<'a> {
    out: &'a mut dyn fmt::Write,
    depth: usize,
}

impl MultiEscaped<'_> {
    fn write_char_escaped(&mut self, c: char) -> fmt::Result {
        if self.depth == 0 {
            return self.out.write_char(c);
        }
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let encoded = c.encode_utf8(&mut a);
        let mut len = encoded.len();
        for _ in 0..self.depth {
            let mut dst = SliceBuf { buf: &mut b, len: 0 };
            let src = core::str::from_utf8(a.get(..len).ok_or(fmt::Error)?).map_err(|_| fmt::Error)?;
            escaped(src, &mut dst)?;
            len = dst.len;
            let dst_slice = a.get_mut(..len).ok_or(fmt::Error)?;
            let src_slice = b.get(..len).ok_or(fmt::Error)?;
            dst_slice.copy_from_slice(src_slice);
        }
        let final_str = core::str::from_utf8(a.get(..len).ok_or(fmt::Error)?).map_err(|_| fmt::Error)?;
        self.out.write_str(final_str)
    }
}

struct SliceBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl fmt::Write for SliceBuf<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let target = self.buf.get_mut(self.len..end).ok_or(fmt::Error)?;
        target.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl fmt::Write for MultiEscaped<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.depth == 0 {
            return self.out.write_str(text);
        }
        if self.depth == 1 {
            return escaped(text, self.out);
        }
        for c in text.chars() {
            self.write_char_escaped(c)?;
        }
        Ok(())
    }
}

/// Reads one head and its argument, advancing `at` past both.
fn head(bytes: &[u8], at: &mut usize) -> Result<(u8, u64), fmt::Error> {
    let ib = *bytes.get(*at).ok_or(fmt::Error)?;
    *at += 1;
    let major = ib >> 5;
    let ai = ib & 0x1f;
    let arg = match ai {
        0..=23 => u64::from(ai),
        24 => {
            let v = u64::from(*bytes.get(*at).ok_or(fmt::Error)?);
            *at += 1;
            v
        }
        25 => {
            let end = at.checked_add(2).ok_or(fmt::Error)?;
            let chunk: [u8; 2] =
                bytes.get(*at..end).and_then(|s| s.try_into().ok()).ok_or(fmt::Error)?;
            *at = end;
            u64::from(u16::from_be_bytes(chunk))
        }
        26 => {
            let end = at.checked_add(4).ok_or(fmt::Error)?;
            let chunk: [u8; 4] =
                bytes.get(*at..end).and_then(|s| s.try_into().ok()).ok_or(fmt::Error)?;
            *at = end;
            u64::from(u32::from_be_bytes(chunk))
        }
        27 => {
            let end = at.checked_add(8).ok_or(fmt::Error)?;
            let chunk: [u8; 8] =
                bytes.get(*at..end).and_then(|s| s.try_into().ok()).ok_or(fmt::Error)?;
            *at = end;
            u64::from_be_bytes(chunk)
        }
        _ => 0,
    };
    Ok((major, arg))
}

fn render_at(bytes: &[u8], at: &mut usize, out: &mut dyn fmt::Write) -> fmt::Result {
    #[derive(Clone, Copy)]
    enum Frame {
        Array { remaining: u64 },
        MapVal { remaining: u64 },
        MapKey { remaining: u64 },
    }
    const CAP: usize = 33;
    let mut stack: [Option<Frame>; CAP] = [None; CAP];
    let mut sp: usize = 0;
    let mut esc_depth: usize = 0;

    'item: loop {
        let (major, arg) = head(bytes, at)?;
        {
            let mut dst = MultiEscaped { out, depth: esc_depth };
            match major {
                0 => write!(dst, "{arg}")?,
                1 => write!(dst, "{}", -1 - i128::from(arg))?,
                2 => {
                    let len = usize::try_from(arg).map_err(|_| fmt::Error)?;
                    let end = at.checked_add(len).ok_or(fmt::Error)?;
                    let slice = bytes.get(*at..end).ok_or(fmt::Error)?;
                    push_hex(slice, &mut dst)?;
                    *at = end;
                }
                3 => {
                    let len = usize::try_from(arg).map_err(|_| fmt::Error)?;
                    let end = at.checked_add(len).ok_or(fmt::Error)?;
                    let slice = bytes.get(*at..end).ok_or(fmt::Error)?;
                    push_string(core::str::from_utf8(slice).unwrap_or(""), &mut dst)?;
                    *at = end;
                }
                4 => {
                    dst.write_char('[')?;
                    if arg > 0 {
                        let slot = stack.get_mut(sp).ok_or(fmt::Error)?;
                        *slot = Some(Frame::Array { remaining: arg - 1 });
                        sp += 1;
                        continue 'item;
                    }
                    dst.write_char(']')?;
                }
                5 => {
                    dst.write_char('{')?;
                    if arg > 0 {
                        dst.write_char('"')?;
                        let slot = stack.get_mut(sp).ok_or(fmt::Error)?;
                        *slot = Some(Frame::MapVal { remaining: arg - 1 });
                        sp += 1;
                        esc_depth += 1;
                        continue 'item;
                    }
                    dst.write_char('}')?;
                }
                _ => match arg {
                    20 => dst.write_str("false")?,
                    21 => dst.write_str("true")?,
                    22 => dst.write_str("null")?,
                    _ => dst.write_str("null")?,
                },
            }
        }

        loop {
            if sp == 0 {
                return Ok(());
            }
            let Some(frame) = stack.get_mut(sp - 1).and_then(Option::take) else {
                return Ok(());
            };
            match frame {
                Frame::Array { remaining } => {
                    let mut dst = MultiEscaped { out, depth: esc_depth };
                    if remaining > 0 {
                        dst.write_char(',')?;
                        if let Some(slot) = stack.get_mut(sp - 1) {
                            *slot = Some(Frame::Array { remaining: remaining - 1 });
                        }
                        continue 'item;
                    }
                    dst.write_char(']')?;
                    sp -= 1;
                }
                Frame::MapVal { remaining } => {
                    esc_depth = esc_depth.saturating_sub(1);
                    let mut dst = MultiEscaped { out, depth: esc_depth };
                    dst.write_str("\":")?;
                    if let Some(slot) = stack.get_mut(sp - 1) {
                        *slot = Some(Frame::MapKey { remaining });
                    }
                    continue 'item;
                }
                Frame::MapKey { remaining } => {
                    let mut dst = MultiEscaped { out, depth: esc_depth };
                    if remaining > 0 {
                        dst.write_str(",\"")?;
                        if let Some(slot) = stack.get_mut(sp - 1) {
                            *slot = Some(Frame::MapVal { remaining: remaining - 1 });
                        }
                        esc_depth += 1;
                        continue 'item;
                    }
                    dst.write_char('}')?;
                    sp -= 1;
                }
            }
        }
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
        let cbor = [0xa1, 0x00, 0x65, b'a', b'"', b'\\', b'\n', 1];
        let mut enough = Fixed { bytes: [0; 64], len: 0 };
        render_cbor(&cbor, &mut enough).unwrap();
        assert_eq!(
            core::str::from_utf8(&enough.bytes[..enough.len]).unwrap(),
            r#"{"0":"a\"\\\n\u0001"}"#,
        );
        let mut short = Fixed { bytes: [0; 8], len: 0 };
        assert_eq!(render_cbor(&cbor, &mut short), Err(fmt::Error));
    }
}
