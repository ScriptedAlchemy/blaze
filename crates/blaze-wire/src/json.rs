//! A JSON rendering of a wire value, for people: `Accept: application/json`, `--json`.
//!
//! This is never the wire and nothing's identity is taken from it. A struct renders as an object
//! whose keys are its field numbers as text, an enum as the two-element array the wire carries
//! (`[discriminant, payload]`, `Namespace::Main` as `[0,null]`), a set as an array in the set's
//! order, a byte string or a digest as lowercase hex, and an integer as a JSON number. The
//! rendering is made by reading the value's canonical CBOR back, so it is a view of the same bytes
//! the encoder writes and not a second schema.

use crate::cbor::{Encoder, Named, Wire};

/// Renders one wire value as JSON, with its type's name as the outer key.
///
/// The rendering is built by reading the value's own canonical CBOR, so a byte string is hex and
/// not an array of integers. It ends without a newline.
pub fn to_json<T: Wire + Named>(value: &T) -> String {
    let mut out = String::new();
    out.push_str("{\"");
    out.push_str(T::NAME);
    out.push_str("\":");
    render(value, &mut out);
    out.push('}');
    out
}

/// Renders one wire value as its JSON form, without its type's name around it.
pub fn render<T: Wire>(value: &T, out: &mut String) {
    render_cbor(&Encoder::of(value), out);
}

/// Renders one value's canonical CBOR as JSON, which is what a message's bytes are.
pub fn render_cbor(cbor: &[u8], out: &mut String) {
    render_item(cbor, out);
}

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

fn render_item(bytes: &[u8], out: &mut String) {
    let mut at = 0;
    render_at(bytes, &mut at, out);
}

fn push_hex(bytes: &[u8], out: &mut String) {
    out.push('"');
    out.push_str(&hex(bytes));
    out.push('"');
}

fn push_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
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

fn render_at(bytes: &[u8], at: &mut usize, out: &mut String) {
    let (major, arg) = head(bytes, at);
    match major {
        0 => out.push_str(&arg.to_string()),
        // `-1 - arg` in an `i128`: `-2^64` is a well-formed CBOR item the validator accepts, and
        // `arg + 1` would overflow a `u64` in debug and render `-0` in release.
        1 => out.push_str(&(-1 - i128::from(arg)).to_string()),
        2 => {
            let end = *at + arg as usize;
            push_hex(&bytes[*at..end], out);
            *at = end;
        }
        3 => {
            let end = *at + arg as usize;
            push_string(std::str::from_utf8(&bytes[*at..end]).unwrap_or(""), out);
            *at = end;
        }
        4 => {
            out.push('[');
            for i in 0..arg {
                if i > 0 {
                    out.push(',');
                }
                render_at(bytes, at, out);
            }
            out.push(']');
        }
        5 => {
            out.push('{');
            for i in 0..arg {
                if i > 0 {
                    out.push(',');
                }
                // A schema map's key is a field number, which JSON writes as text.
                let mut key = String::new();
                render_at(bytes, at, &mut key);
                push_string(&key, out);
                out.push(':');
                render_at(bytes, at, out);
            }
            out.push('}');
        }
        _ => match arg {
            20 => out.push_str("false"),
            21 => out.push_str("true"),
            22 => out.push_str("null"),
            _ => out.push_str("null"),
        },
    }
}
