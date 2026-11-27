//! Pure `#![no_std]` string transformation utilities:
//!
//! - Word splitting and case conversions ([`split_words`], [`to_snake_case_in`],
//!   [`to_screaming_snake_case_in`], [`to_kebab_case_in`], [`to_camel_case_in`],
//!   [`to_pascal_case_in`], [`to_title_case_in`]) with acronym and digit boundary awareness
//!   (`HTTPServer` -> `http_server`, `parseHTML5Doc` -> `parse_html5_doc`).
//! - URL slugification ([`slugify_in`]) using Unicode NFD decomposition (`blaze_unicode::nfd_chars`)
//!   to fold Latin diacritics cleanly.
//! - Zero-alloc natural sort comparison ([`natural_cmp`]) forming a strict total order over all
//!   `&str` with arbitrary-length decimal digit runs (never overflows `u64`).
//! - Indentation utilities ([`dedent_in`], [`indent_in`]) preserving line endings and blank lines.
//! - ANSI- and Unicode-display-width-aware truncation ([`truncate_width_in`]) over `crate::ansi`
//!   that never splits escape sequences or wide characters and closes active ANSI styles on
//!   truncation.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::cmp::Ordering;

use blaze_own::{AllocFailure, String};
use blaze_unicode::{combining_class, nfd_chars};

use crate::ansi::{Segment, char_width, has_ansi, segments, visible_width};

// ---------------------------------------------------------------------------
// Word splitting & case conversions
// ---------------------------------------------------------------------------

/// Zero-alloc iterator splitting an identifier or phrase into constituent words.
///
/// Splits on non-alphanumeric separators (`_`, `-`, whitespace, punctuation), camelCase
/// transitions (`fooBar` -> `foo`, `Bar`), digit-to-uppercase transitions (`parseHTML5Doc` ->
/// `parse`, `HTML5`, `Doc`), and acronym endings (`HTTPServer` -> `HTTP`, `Server`).
#[derive(Clone, Debug)]
pub struct Words<'a> {
    rest: &'a str,
}

/// Iterate over the words of `input` without allocating.
#[inline]
#[must_use]
pub const fn split_words(input: &str) -> Words<'_> {
    Words { rest: input }
}

impl<'a> Iterator for Words<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let mut iter = self.rest.char_indices();
        let start = loop {
            match iter.next() {
                Some((idx, ch)) if ch.is_alphanumeric() => break idx,
                Some(_) => {}
                None => {
                    self.rest = "";
                    return None;
                }
            }
        };
        let (_, tail) = self.rest.split_at_checked(start)?;
        self.rest = tail;

        let mut chars = self.rest.char_indices().peekable();
        let (_, mut prev) = chars.next()?;
        let mut end = prev.len_utf8();

        while let Some((idx, curr)) = chars.next() {
            if !curr.is_alphanumeric() {
                end = idx;
                break;
            }
            let next_ch = chars.peek().map(|&(_, c)| c);
            let boundary = (prev.is_lowercase() && curr.is_uppercase())
                || (prev.is_numeric() && curr.is_uppercase())
                || (prev.is_uppercase()
                    && curr.is_uppercase()
                    && next_ch.is_some_and(char::is_lowercase));
            if boundary {
                end = idx;
                break;
            }
            prev = curr;
            end = idx + curr.len_utf8();
        }

        let (word, rest) = self.rest.split_at_checked(end)?;
        self.rest = rest;
        Some(word)
    }
}

fn push_lowercase<A: Allocator>(word: &str, out: &mut String<A>) -> Result<(), AllocFailure> {
    for ch in word.chars() {
        for lc in ch.to_lowercase() {
            out.try_push(lc)?;
        }
    }
    Ok(())
}

fn push_uppercase<A: Allocator>(word: &str, out: &mut String<A>) -> Result<(), AllocFailure> {
    for ch in word.chars() {
        for uc in ch.to_uppercase() {
            out.try_push(uc)?;
        }
    }
    Ok(())
}

fn push_capitalized<A: Allocator>(word: &str, out: &mut String<A>) -> Result<(), AllocFailure> {
    let mut chars = word.chars();
    if let Some(first) = chars.next() {
        for uc in first.to_uppercase() {
            out.try_push(uc)?;
        }
        for ch in chars {
            for lc in ch.to_lowercase() {
                out.try_push(lc)?;
            }
        }
    }
    Ok(())
}

/// Convert `input` to `snake_case` in `alloc`.
pub fn to_snake_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push('_')?;
        }
        push_lowercase(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `SCREAMING_SNAKE_CASE` in `alloc`.
pub fn to_screaming_snake_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push('_')?;
        }
        push_uppercase(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `kebab-case` in `alloc`.
pub fn to_kebab_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push('-')?;
        }
        push_lowercase(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `camelCase` in `alloc`.
pub fn to_camel_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx == 0 {
            push_lowercase(word, &mut out)?;
        } else {
            push_capitalized(word, &mut out)?;
        }
    }
    Ok(out)
}

/// Convert `input` to `PascalCase` in `alloc`.
pub fn to_pascal_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for word in split_words(input) {
        push_capitalized(word, &mut out)?;
    }
    Ok(out)
}

/// Convert `input` to `Title Case` in `alloc`.
pub fn to_title_case_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (idx, word) in split_words(input).enumerate() {
        if idx > 0 {
            out.try_push(' ')?;
        }
        push_capitalized(word, &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// URL Slugify
// ---------------------------------------------------------------------------

#[inline]
fn is_combining_mark(ch: char) -> bool {
    if combining_class(ch) != 0 {
        return true;
    }
    matches!(
        u32::from(ch),
        0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

#[inline]
const fn fold_latin_special(ch: char) -> Option<&'static str> {
    match ch {
        'ß' => Some("ss"),
        'æ' | 'Æ' => Some("ae"),
        'œ' | 'Œ' => Some("oe"),
        'ø' | 'Ø' => Some("o"),
        'đ' | 'Đ' | 'ð' | 'Ð' => Some("d"),
        'ł' | 'Ł' => Some("l"),
        'þ' | 'Þ' => Some("th"),
        _ => None,
    }
}

/// Turn `input` into a clean ASCII URL slug using `separator` in `alloc`.
///
/// Decomposes characters via Unicode NFD (`blaze_unicode::nfd_chars`), strips combining marks,
/// folds common non-decomposing Latin letters (`ß`, `æ`, `œ`, `ø`, `đ`, `ł`, `þ`), lowercases
/// ASCII alphanumerics, collapses runs of separators, and trims leading/trailing separators.
pub fn slugify_in<A: Allocator + Copy>(
    input: &str,
    separator: char,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    let mut pending_sep = false;

    for ch in nfd_chars(input) {
        if is_combining_mark(ch) {
            continue;
        }
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.try_push(separator)?;
            }
            pending_sep = false;
            out.try_push(ch.to_ascii_lowercase())?;
        } else if let Some(folded) = fold_latin_special(ch) {
            if pending_sep && !out.is_empty() {
                out.try_push(separator)?;
            }
            pending_sep = false;
            out.try_push_str(folded)?;
        } else {
            pending_sep = true;
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Natural Sorting Comparison (strict total order, arbitrary-length digits)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum NaturalToken<'a> {
    Byte(u8),
    Digits(&'a [u8]),
}

fn next_natural_token(bytes: &[u8]) -> Option<(NaturalToken<'_>, &[u8])> {
    let first = *bytes.first()?;
    if first.is_ascii_digit() {
        let mut end = 1usize;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        let (run, rest) = bytes.split_at_checked(end)?;
        Some((NaturalToken::Digits(run), rest))
    } else {
        let (_, rest) = bytes.split_at_checked(1)?;
        Some((NaturalToken::Byte(first), rest))
    }
}

#[inline]
fn strip_leading_zeros(digits: &[u8]) -> &[u8] {
    let mut i = 0usize;
    while digits.get(i) == Some(&b'0') {
        i += 1;
    }
    digits.get(i..).unwrap_or(&[])
}

fn cmp_natural_tokens(
    a: NaturalToken<'_>,
    b: NaturalToken<'_>,
    plant_lexicographic_digits: bool,
) -> Ordering {
    match (a, b) {
        (NaturalToken::Byte(x), NaturalToken::Byte(y)) => x.cmp(&y),
        (NaturalToken::Byte(x), NaturalToken::Digits(_)) => {
            if x < b'0' {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (NaturalToken::Digits(_), NaturalToken::Byte(y)) => {
            if y < b'0' {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (NaturalToken::Digits(da), NaturalToken::Digits(db)) => {
            if plant_lexicographic_digits {
                da.cmp(db)
            } else {
                let ta = strip_leading_zeros(da);
                let tb = strip_leading_zeros(db);
                ta.len()
                    .cmp(&tb.len())
                    .then_with(|| ta.cmp(tb))
                    .then_with(|| da.len().cmp(&db.len()))
            }
        }
    }
}

fn natural_cmp_planted(a: &str, b: &str, plant_lexicographic_digits: bool) -> Ordering {
    let mut rem_a = a.as_bytes();
    let mut rem_b = b.as_bytes();

    loop {
        match (next_natural_token(rem_a), next_natural_token(rem_b)) {
            (Some((ta, next_a)), Some((tb, next_b))) => {
                let ord = cmp_natural_tokens(ta, tb, plant_lexicographic_digits);
                if ord != Ordering::Equal {
                    return ord;
                }
                rem_a = next_a;
                rem_b = next_b;
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Compare two strings in natural order without allocating.
///
/// Maximal runs of ASCII digits are compared by exact numeric value (stripping leading zeros,
/// comparing significant length then digits, and breaking numeric ties by original digit-run
/// length), so arbitrarily long numbers never overflow and the relation is a strict total order
/// (reflexive, antisymmetric, transitive) with `natural_cmp(a, b) == Equal` iff `a == b`.
#[must_use]
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_cmp_planted(a, b, false)
}

// ---------------------------------------------------------------------------
// Indentation Utilities
// ---------------------------------------------------------------------------

struct LinesWithEndings<'a> {
    rest: &'a str,
}

#[inline]
const fn lines_with_endings(input: &str) -> LinesWithEndings<'_> {
    LinesWithEndings { rest: input }
}

impl<'a> Iterator for LinesWithEndings<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        if let Some(nl) = self.rest.as_bytes().iter().position(|&b| b == b'\n') {
            let (line_with_nl, after) = self.rest.split_at_checked(nl + 1)?;
            self.rest = after;
            if let Some(content) = line_with_nl.strip_suffix("\r\n") {
                Some((content, "\r\n"))
            } else if let Some(content) = line_with_nl.strip_suffix('\n') {
                Some((content, "\n"))
            } else {
                Some((line_with_nl, ""))
            }
        } else {
            let content = self.rest;
            self.rest = "";
            Some((content, ""))
        }
    }
}

#[inline]
fn is_blank_line(content: &str) -> bool {
    content.as_bytes().iter().all(|&b| b == b' ' || b == b'\t')
}

#[inline]
fn leading_ws(content: &str) -> &str {
    let bytes = content.as_bytes();
    let mut n = 0usize;
    while bytes.get(n).is_some_and(|&b| b == b' ' || b == b'\t') {
        n += 1;
    }
    content.split_at_checked(n).map_or("", |(ws, _)| ws)
}

#[inline]
fn common_ws_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let mut n = 0usize;
    for (&x, &y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        if x == y {
            n += 1;
        } else {
            break;
        }
    }
    a.split_at_checked(n).map_or("", |(p, _)| p)
}

/// Strip the common leading space/tab indentation across all non-empty lines in `alloc`.
///
/// Preserves line endings (`\n` and `\r\n`) and keeps blank lines empty.
pub fn dedent_in<A: Allocator + Copy>(
    input: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut common: Option<&str> = None;
    for (content, _) in lines_with_endings(input) {
        if is_blank_line(content) {
            continue;
        }
        let ws = leading_ws(content);
        common = Some(match common {
            None => ws,
            Some(prev) => common_ws_prefix(prev, ws),
        });
    }
    let strip_len = common.map_or(0, str::len);

    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (content, ending) in lines_with_endings(input) {
        if !is_blank_line(content) {
            let trimmed = content.split_at_checked(strip_len).map_or(content, |(_, tail)| tail);
            out.try_push_str(trimmed)?;
        }
        out.try_push_str(ending)?;
    }
    Ok(out)
}

/// Prepend `prefix` to every non-blank line of `input` in `alloc`, preserving line endings and
/// blank lines.
pub fn indent_in<A: Allocator + Copy>(
    input: &str,
    prefix: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(input.len(), alloc)?;
    for (content, ending) in lines_with_endings(input) {
        if !is_blank_line(content) {
            out.try_push_str(prefix)?;
        }
        out.try_push_str(content)?;
        out.try_push_str(ending)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ANSI / Unicode Display-Width Truncation
// ---------------------------------------------------------------------------

fn push_width_prefix_in<A: Allocator>(
    src: &str,
    budget: usize,
    out: &mut String<A>,
) -> Result<bool, AllocFailure> {
    if budget == 0 {
        return Ok(false);
    }
    let mut used = 0usize;
    let mut emitted_escape = false;
    let mut stopped = false;

    for seg in segments(src) {
        if stopped {
            break;
        }
        match seg {
            Segment::Escape(esc) => {
                if used < budget {
                    out.try_push_str(esc)?;
                    emitted_escape = true;
                }
            }
            Segment::Text(txt) => {
                for ch in txt.chars() {
                    let cw = char_width(ch);
                    if used.saturating_add(cw) > budget {
                        stopped = true;
                        break;
                    }
                    out.try_push(ch)?;
                    used += cw;
                }
            }
        }
    }

    Ok(emitted_escape)
}

/// Truncate `input` so its visible terminal display width is at most `max_width`, appending
/// `ellipsis` when truncated.
///
/// Uses `crate::ansi::{segments, char_width, visible_width}`: never splits inside an ANSI escape
/// sequence or a wide East Asian / emoji scalar, and appends `\x1b[0m` if any escape sequence was
/// emitted before truncating so styling never bleeds.
pub fn truncate_width_in<A: Allocator + Copy>(
    input: &str,
    max_width: usize,
    ellipsis: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    if visible_width(input) <= max_width {
        return String::try_from_str_in(input, alloc);
    }
    if max_width == 0 {
        return Ok(String::new_in(alloc));
    }

    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width > max_width {
        let mut out = String::new_in(alloc);
        let esc = push_width_prefix_in(ellipsis, max_width, &mut out)?;
        if esc && !out.as_str().ends_with("\x1b[0m") {
            out.try_push_str("\x1b[0m")?;
        }
        return Ok(out);
    }

    let budget = max_width - ellipsis_width;
    let cap = input
        .len()
        .min(max_width.saturating_mul(4))
        .saturating_add(ellipsis.len())
        .saturating_add(8);
    let mut out = String::try_with_capacity_in(cap, alloc)?;

    let esc_in = push_width_prefix_in(input, budget, &mut out)?;
    let esc_el = has_ansi(ellipsis);
    out.try_push_str(ellipsis)?;
    if (esc_in || esc_el) && !out.as_str().ends_with("\x1b[0m") {
        out.try_push_str("\x1b[0m")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
