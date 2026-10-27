//! Deterministic CBOR, RFC 8949 section 4.2's core deterministic encoding, and the validator
//! that refuses anything else.
//!
//! One value has one encoding, so a request, a record or a test result can be hashed and kept
//! under that hash, and two daemons agree on it byte for byte. The encoder writes shortest heads,
//! definite lengths, no floats and no tags; a schema type's decoder requires every key of its map,
//! in ascending order, and refuses any other. [`validate`] checks the bytes alone: heads, lengths,
//! order, duplicates, floats, tags, depth and size. [`decode`] runs that check first and then reads
//! the schema, so the ordinary decoder never accepts a byte the validator refuses, and
//! [`validate_as`] is that same call.
//!
//! Strings are at most [`Limits::string`] bytes and byte strings at most [`Limits::blob`]; an
//! array or a map at most [`Limits::batch_items`] items; the whole item at most
//! [`Limits::request`] bytes unless the caller passes another limit ([`Limits::event`] for one
//! item of an event stream). A lookup's collections are at most [`Limits::lookup_items`] items,
//! which the bytes-only validator cannot tell apart from any other array, so a lookup's schema
//! field holds its own collection to that limit ([`Reader::lookup`]).

use std::fmt;

/// The deepest nesting a message may have, counting every item and not only containers.
///
/// An item's nesting level is the number of containers around it plus one, so a scalar inside 31
/// arrays is at level 32 and is the deepest item a message may have, and a scalar inside 32 arrays
/// is at level 33 and is refused. The validator and the reader count it the same way, so the
/// ordinary decoder never accepts a message the validator refuses.
pub const MAX_DEPTH: u32 = 32;

/// The limits a server holds every message to, as `Hello` answers them.
///
/// The fields are section 8's numbers: 16 MiB a request, 1 MiB an event, 4 GiB a blob, 4,096 items
/// in a lookup, 65,536 in a batch, 4 KiB a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Limits {
    /// 0: the bytes of one request, 16 MiB.
    pub request: u32,
    /// 1: the bytes of one event, 1 MiB.
    pub event: u32,
    /// 2: the bytes of one blob, 4 GiB.
    pub blob: u64,
    /// 3: the items of one lookup, 4,096.
    pub lookup_items: u32,
    /// 4: the items of one batch, 65,536.
    pub batch_items: u32,
    /// 5: the bytes of one string, 4 KiB.
    pub string: u32,
}

impl Limits {
    /// The limits section 8 gives.
    pub const DOCUMENTED: Limits = Limits {
        request: 16 << 20,
        event: 1 << 20,
        blob: 4 << 30,
        lookup_items: 4096,
        batch_items: 65536,
        string: 4096,
    };
}

impl Wire for Limits {
    fn encode(&self, e: &mut Encoder) {
        e.map(6);
        e.uint(0);
        e.uint(u64::from(self.request));
        e.uint(1);
        e.uint(u64::from(self.event));
        e.uint(2);
        e.uint(self.blob);
        e.uint(3);
        e.uint(u64::from(self.lookup_items));
        e.uint(4);
        e.uint(u64::from(self.batch_items));
        e.uint(5);
        e.uint(u64::from(self.string));
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.map_start()?;
        r.field(0)?;
        let request = u32::decode(r)?;
        r.field(1)?;
        let event = u32::decode(r)?;
        r.field(2)?;
        let blob = r.uint()?;
        r.field(3)?;
        let lookup_items = u32::decode(r)?;
        r.field(4)?;
        let batch_items = u32::decode(r)?;
        r.field(5)?;
        let string = u32::decode(r)?;
        r.map_end()?;
        Ok(Limits { request, event, blob, lookup_items, batch_items, string })
    }
}

impl crate::Named for Limits {
    const NAME: &'static str = "Limits";
}

impl crate::vectors::Sample for Limits {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        // Every limit is at a boundary of its own head's width, as a limit would be.
        Limits {
            request: u32::sample(g),
            event: u32::sample(g),
            blob: u64::sample(g),
            lookup_items: u32::sample(g),
            batch_items: u32::sample(g),
            string: u32::sample(g),
        }
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::DOCUMENTED
    }
}

/// The value a schema type can be written as and read back from.
///
/// A struct is a CBOR map of its field numbers, every key present, ascending. An enum is the
/// two-element array `[discriminant, payload]`. A tuple is an array of its elements, a `Vec` an
/// array, a [`Set`](crate::Set) a sorted array, a [`Bytes`](crate::Bytes) a byte string.
pub trait Wire: Sized {
    /// Writes this value in canonical CBOR.
    fn encode(&self, e: &mut Encoder);

    /// Reads one value, refusing anything the encoder would not have written.
    fn decode(r: &mut Reader<'_>) -> Result<Self, Error>;
}

/// The name a type is hashed and signed under: [`crate::digest_of`]'s `name`.
///
/// It is the type's Rust name, and it is part of the domain separation, so changing it changes
/// every digest and signature of the type. Golden vectors pin them.
pub trait Named {
    /// The type's name, as the domain separation spells it.
    const NAME: &'static str;
}

/// Writes canonical CBOR.
#[derive(Default)]
pub struct Encoder {
    out: Vec<u8>,
}

impl Encoder {
    /// An empty encoder.
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// An encoder that writes onto the end of `out`, for a prefix and then one value.
    ///
    /// The bytes are in `out` once the returned [`Appender`] is dropped, so a caller writes a
    /// prefix, encodes one value, and has the two as one buffer:
    ///
    /// ```
    /// let mut out = b"prefix".to_vec();
    /// {
    ///     let mut e = blaze_wire::Encoder::new_into(&mut out);
    ///     e.uint(1);
    /// }
    /// assert_eq!(out, b"prefix\x01");
    /// ```
    pub fn new_into(out: &mut Vec<u8>) -> Appender<'_> {
        Appender { encoder: Encoder { out: std::mem::take(out) }, back: out }
    }

    /// The bytes written so far.
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }

    /// The major type and argument as one head, in the shortest form that holds the argument.
    fn head(&mut self, major: u8, arg: u64) {
        let major = major << 5;
        if arg < 24 {
            self.out.push(major | arg as u8);
        } else if arg <= u8::MAX as u64 {
            self.out.push(major | 24);
            self.out.push(arg as u8);
        } else if arg <= u16::MAX as u64 {
            self.out.push(major | 25);
            self.out.extend_from_slice(&(arg as u16).to_be_bytes());
        } else if arg <= u32::MAX as u64 {
            self.out.push(major | 26);
            self.out.extend_from_slice(&(arg as u32).to_be_bytes());
        } else {
            self.out.push(major | 27);
            self.out.extend_from_slice(&arg.to_be_bytes());
        }
    }

    /// An unsigned integer.
    pub fn uint(&mut self, v: u64) {
        self.head(0, v);
    }

    /// A byte string.
    pub fn bytes(&mut self, b: &[u8]) {
        self.head(2, b.len() as u64);
        self.out.extend_from_slice(b);
    }

    /// A text string.
    pub fn text(&mut self, s: &str) {
        self.head(3, s.len() as u64);
        self.out.extend_from_slice(s.as_bytes());
    }

    /// An array of `len` items, which the caller then writes.
    pub fn array(&mut self, len: usize) {
        self.head(4, len as u64);
    }

    /// A map of `len` pairs, which the caller then writes.
    pub fn map(&mut self, len: usize) {
        self.head(5, len as u64);
    }

    /// `false` or `true`.
    pub fn bool(&mut self, v: bool) {
        self.out.push(if v { 0xf5 } else { 0xf4 });
    }

    /// `null`.
    pub fn null(&mut self) {
        self.out.push(0xf6);
    }

    /// The canonical CBOR of one value.
    pub fn of<T: Wire>(v: &T) -> Vec<u8> {
        let mut e = Encoder::new();
        v.encode(&mut e);
        e.into_bytes()
    }
}

/// An [`Encoder`] that writes onto the end of a caller's buffer, which it returns the bytes to
/// when it is dropped.
///
/// It dereferences to [`Encoder`], so a value encodes into it the same way:
///
/// ```
/// let mut out = b"blaze/Attempt/v1\0".to_vec();
/// let mut e = blaze_wire::Encoder::new_into(&mut out);
/// blaze_wire::Attempt(24).encode(&mut e);
/// drop(e);
/// assert_eq!(out.last(), Some(&24));
/// ```
pub struct Appender<'a> {
    encoder: Encoder,
    back: &'a mut Vec<u8>,
}

impl std::ops::Deref for Appender<'_> {
    type Target = Encoder;

    fn deref(&self) -> &Encoder {
        &self.encoder
    }
}

impl std::ops::DerefMut for Appender<'_> {
    fn deref_mut(&mut self) -> &mut Encoder {
        &mut self.encoder
    }
}

impl Drop for Appender<'_> {
    fn drop(&mut self) {
        // The encoder's buffer began as the caller's, and `take` leaves it empty rather than
        // moving it, so putting the bytes back is one move and no copy.
        let mut bytes = std::mem::take(&mut self.encoder.out);
        bytes.append(self.back);
        *self.back = bytes;
    }
}

/// Reads canonical CBOR, refusing what is not.
pub struct Reader<'a> {
    input: &'a [u8],
    pos: usize,
    limits: Limits,
    depth: u32,
    maps: Vec<MapState>,
    /// The limit of the one collection whose header is read next, when a lookup's field set it
    /// with [`Reader::lookup`]. It applies to that collection only, so a lookup's answer is held
    /// to [`Limits::lookup_items`] at its top and each entry to [`Limits::batch_items`] inside.
    next_collection_limit: Option<u64>,
}

#[derive(Clone, Copy)]
struct MapState {
    remaining: u64,
    seen: u128,
}

impl<'a> Reader<'a> {
    /// A reader over `input` that holds every message to `limits`.
    pub fn new(input: &'a [u8], limits: &Limits) -> Reader<'a> {
        Reader {
            input,
            pos: 0,
            limits: *limits,
            depth: 0,
            maps: Vec::new(),
            next_collection_limit: None,
        }
    }

    /// Where in the input the reader is.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Reads one value whose first array or map is held to [`Limits::lookup_items`] rather than
    /// [`Limits::batch_items`], which is a lookup's collection limit.
    ///
    /// The bytes-only validator cannot tell a lookup's array from a batch's, so a lookup's field
    /// reads through this and [`decode`] refuses a lookup with more items than the limit it
    /// answers. The limit applies to the one collection the field's value starts with, and a
    /// scalar clears it: anything inside that collection is a batch's, held to
    /// [`Limits::batch_items`].
    pub fn lookup<T: Wire>(&mut self) -> Result<T, Error> {
        let saved = self.next_collection_limit;
        self.next_collection_limit = Some(u64::from(self.limits.lookup_items));
        let value = T::decode(self);
        self.next_collection_limit = saved;
        value
    }

    fn err(&self, kind: ErrorKind) -> Error {
        Error { kind, at: self.pos }
    }

    pub(crate) fn err_at(&self, kind: ErrorKind, at: usize) -> Error {
        Error { kind, at }
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.input.get(self.pos).ok_or_else(|| self.err(ErrorKind::Truncated))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], Error> {
        let n = usize::try_from(n).map_err(|_| self.err(ErrorKind::Truncated))?;
        let end = self.pos.checked_add(n).ok_or_else(|| self.err(ErrorKind::Truncated))?;
        let slice = self.input.get(self.pos..end).ok_or_else(|| self.err(ErrorKind::Truncated))?;
        self.pos = end;
        Ok(slice)
    }

    /// The argument of a head, checking that it is in the shortest form and that lengths are
    /// definite. `None` is a head that has no argument (a simple value).
    fn argument(&mut self, major: u8) -> Result<Option<u64>, Error> {
        let at = self.pos;
        // A scalar is no collection, so a lookup's pending collection limit ends here.
        self.next_collection_limit = None;
        let ib = self.byte()?;
        if ib >> 5 != major {
            return Err(self.err_at(ErrorKind::UnexpectedType { expected: major_name(major) }, at));
        }
        let ai = ib & 0x1f;
        let value = match ai {
            0..=23 => return Ok(Some(u64::from(ai))),
            24 => {
                let v = u64::from(self.byte()?);
                if v < 24 {
                    return Err(self.err_at(ErrorKind::NonShortestHead, at));
                }
                v
            }
            25 => {
                let b = self.take(2)?;
                let v = u64::from(u16::from_be_bytes([b[0], b[1]]));
                if v <= u64::from(u8::MAX) {
                    return Err(self.err_at(ErrorKind::NonShortestHead, at));
                }
                v
            }
            26 => {
                let b = self.take(4)?;
                let v = u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                if v <= u64::from(u16::MAX) {
                    return Err(self.err_at(ErrorKind::NonShortestHead, at));
                }
                v
            }
            27 => {
                let b = self.take(8)?;
                let v = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                if v <= u64::from(u32::MAX) {
                    return Err(self.err_at(ErrorKind::NonShortestHead, at));
                }
                v
            }
            28..=30 => return Err(self.err_at(ErrorKind::ReservedAdditional(ai), at)),
            31 => return Err(self.err_at(ErrorKind::IndefiniteLength, at)),
            _ => unreachable!("the additional information is five bits"),
        };
        Ok(Some(value))
    }

    /// A length, held to `limit` for the kind of item `what` names, or to the lookup limit when a
    /// lookup's field asked for it ([`Reader::lookup`]) and this is that collection.
    ///
    /// Only an array or a map is a collection, so only those take the pending limit; a byte or
    /// text string is a scalar and clears it, as [`Reader::scalar`] does.
    fn length(&mut self, major: u8, what: &'static str, limit: u64) -> Result<u64, Error> {
        let at = self.pos;
        let limit = if major == 4 || major == 5 {
            self.next_collection_limit.take().unwrap_or(limit)
        } else {
            self.next_collection_limit = None;
            limit
        };
        let len =
            self.argument(major)?.ok_or_else(|| self.err_at(ErrorKind::IndefiniteLength, at))?;
        if len > limit {
            return Err(self.err_at(ErrorKind::OversizeItem { what, len, limit }, at));
        }
        Ok(len)
    }

    /// A scalar item's level: it sits one deeper than the containers already entered, so a
    /// scalar inside [`MAX_DEPTH`] containers is past the deepest item a message may have.
    ///
    /// The validator counts every item, containers and scalars alike, and this is the reader's
    /// half of the same rule: a container enters and leaves, a scalar is checked against the
    /// containers around it, so both refuse an item at level [`MAX_DEPTH`] + 1.
    fn scalar(&mut self) -> Result<(), Error> {
        // A scalar is no collection, so a lookup's pending collection limit ends here.
        self.next_collection_limit = None;
        if self.depth >= MAX_DEPTH {
            return Err(self.err(ErrorKind::TooDeep));
        }
        Ok(())
    }

    /// An unsigned integer, in the shortest form.
    pub fn uint(&mut self) -> Result<u64, Error> {
        self.scalar()?;
        Ok(self.argument(0)?.expect("an integer has an argument"))
    }

    /// An unsigned integer used as a map key, which a schema type compares with its field number.
    fn key(&mut self) -> Result<u64, Error> {
        let at = self.pos;
        let ib = *self.input.get(self.pos).ok_or_else(|| self.err(ErrorKind::Truncated))?;
        if ib >> 5 != 0 {
            return Err(self.err_at(ErrorKind::UnexpectedType { expected: "an integer key" }, at));
        }
        self.uint()
    }

    /// A byte string.
    pub fn bytes(&mut self) -> Result<&'a [u8], Error> {
        self.scalar()?;
        let len = self.length(2, "byte string", self.limits.blob)?;
        self.take(len)
    }
    /// A byte string of exactly `n` bytes.
    pub fn bytes_of(&mut self, n: usize) -> Result<&'a [u8], Error> {
        self.scalar()?;
        let at = self.pos;
        let len = self.length(2, "byte string", self.limits.blob)?;
        if len != n as u64 {
            return Err(self.err_at(
                ErrorKind::UnexpectedType { expected: "a byte string of its length" },
                at,
            ));
        }
        self.take(len)
    }

    /// A text string.
    pub fn text(&mut self) -> Result<&'a str, Error> {
        self.text_with_limit(u64::from(self.limits.string))
    }

    /// Escaped logical paths use the negotiated message budget, not the general string
    /// budget: escaping valid native bytes can triple their length.
    pub fn path_text(&mut self) -> Result<&'a str, Error> {
        self.text_with_limit(u64::from(self.limits.request))
    }

    /// Environment native bytes are bounded by message size, not the large blob limit.
    pub fn input_bytes(&mut self) -> Result<&'a [u8], Error> {
        self.scalar()?;
        let len = self.length(2, "input bytes", u64::from(self.limits.request))?;
        self.take(len)
    }

    fn text_with_limit(&mut self, limit: u64) -> Result<&'a str, Error> {
        self.scalar()?;
        let at = self.pos;
        let len = self.length(3, "text string", limit)?;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes).map_err(|_| self.err_at(ErrorKind::BadUtf8, at))
    }

    /// Whether the next item is `null`.
    pub fn peek_null(&self) -> bool {
        self.input.get(self.pos) == Some(&0xf6)
    }

    /// The header of an array, whose length must be exactly `n`.
    pub fn array_start(&mut self, n: usize) -> Result<(), Error> {
        let at = self.pos;
        let len = self.length(4, "array", u64::from(self.limits.batch_items))?;
        if len != n as u64 {
            return Err(
                self.err_at(ErrorKind::UnexpectedType { expected: "an array of its length" }, at)
            );
        }
        self.enter()?;
        Ok(())
    }

    /// The end of an array begun with [`Reader::array_start`].
    pub fn array_end(&mut self) -> Result<(), Error> {
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }

    /// The header of an array of any length within the collection limit.
    pub fn array(&mut self) -> Result<u64, Error> {
        let len = self.length(4, "array", u64::from(self.limits.batch_items))?;
        self.enter()?;
        Ok(len)
    }

    /// The end of an array begun with [`Reader::array`].
    pub fn array_finish(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// The header of a map, which a schema type then reads field by field.
    pub fn map_start(&mut self) -> Result<(), Error> {
        let len = self.length(5, "map", u64::from(self.limits.batch_items))?;
        self.enter()?;
        self.maps.push(MapState { remaining: len, seen: 0 });
        Ok(())
    }

    /// One map key, which must be `want`: a smaller key is a duplicate or out of order, a larger
    /// one is missing from the message, and a message with more keys than the schema has leaves
    /// one over for [`Reader::map_end`] to name.
    pub fn field(&mut self, want: u64) -> Result<(), Error> {
        let at = self.pos;
        let state = *self.maps.last().ok_or_else(|| {
            self.err(ErrorKind::UnexpectedType { expected: "a field inside a map" })
        })?;
        if state.remaining == 0 {
            return Err(self.err_at(ErrorKind::MissingKey(want), at));
        }
        let key = self.key()?;
        if key == want {
            self.mark(want);
            return Ok(());
        }
        if key > want {
            return Err(self.err_at(ErrorKind::MissingKey(want), at));
        }
        // A smaller key than the one the schema wants is one it has already read (a duplicate) or
        // one it never reads (out of order).
        if key < 128 && state.seen & (1u128 << key) != 0 {
            return Err(self.err_at(ErrorKind::DuplicateKey(key), at));
        }
        Err(self.err_at(ErrorKind::UnsortedKeys, at))
    }

    fn mark(&mut self, key: u64) {
        if let Some(state) = self.maps.last_mut() {
            state.remaining -= 1;
            if key < 128 {
                state.seen |= 1u128 << key;
            }
        }
    }

    /// The end of a map begun with [`Reader::map_start`]: every key the schema has was read, and
    /// a key the schema does not have is named.
    pub fn map_end(&mut self) -> Result<(), Error> {
        let at = self.pos;
        let state = self
            .maps
            .pop()
            .ok_or_else(|| self.err(ErrorKind::UnexpectedType { expected: "the end of a map" }))?;
        self.depth = self.depth.saturating_sub(1);
        if state.remaining > 0 {
            let key = self.key()?;
            return Err(self.err_at(ErrorKind::UnknownKey(key), at));
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err(ErrorKind::TooDeep));
        }
        Ok(())
    }

    /// The rest of the input, which must be empty once one value is read.
    fn finish(&self) -> Result<(), Error> {
        if self.pos == self.input.len() {
            Ok(())
        } else {
            Err(self.err_at(ErrorKind::TrailingBytes, self.pos))
        }
    }

    /// Walks one item, checking every rule of section 4.2 that the bytes alone decide.
    fn walk(&mut self) -> Result<(), Error> {
        self.enter()?;
        let at = self.pos;
        let ib = self.byte()?;
        // Every arm reads the head again with `argument` or `length`, which are where the shortest
        // form and the definite length are checked, so the position goes back to the head.
        self.pos = at;
        match ib >> 5 {
            0 | 1 => {
                self.argument(ib >> 5)?;
            }
            2 => {
                let len = self.length(2, "byte string", self.limits.blob)?;
                self.take(len)?;
            }
            3 => {
                let len = self.length(3, "text string", u64::from(self.limits.string))?;
                let bytes = self.take(len)?;
                std::str::from_utf8(bytes).map_err(|_| self.err_at(ErrorKind::BadUtf8, at))?;
            }
            4 => {
                let len = self.length(4, "array", u64::from(self.limits.batch_items))?;
                for _ in 0..len {
                    self.walk()?;
                }
            }
            5 => {
                let len = self.length(5, "map", u64::from(self.limits.batch_items))?;
                let mut previous: Option<(usize, usize)> = None;
                for _ in 0..len {
                    let key_at = self.pos;
                    self.walk()?;
                    let key_end = self.pos;
                    if let Some((start, end)) = previous {
                        let before = &self.input[start..end];
                        let this = &self.input[key_at..key_end];
                        match this.cmp(before) {
                            std::cmp::Ordering::Greater => {}
                            std::cmp::Ordering::Equal => {
                                return Err(self.err_at(ErrorKind::DuplicateKey(0), key_at));
                            }
                            std::cmp::Ordering::Less => {
                                return Err(self.err_at(ErrorKind::UnsortedKeys, key_at));
                            }
                        }
                    }
                    previous = Some((key_at, key_end));
                    self.walk()?;
                }
            }
            6 => return Err(self.err_at(ErrorKind::Tag, at)),
            _ => {
                // A simple value's head is the whole item, or its first byte of two: the position
                // stays past the head the check above read.
                self.pos = at + 1;
                match ib & 0x1f {
                    // 0 to 23: the head is the item.
                    0..=23 => {}
                    // RFC 8949 section 3.3: a two-byte simple value whose second byte is below 32
                    // is not well-formed, and every such form has a one-byte form (0 to 23) or a
                    // reserved number (24 to 31).
                    24 => {
                        let value = self.byte()?;
                        if value < 32 {
                            return Err(self.err_at(ErrorKind::ReservedSimpleValue(value), at));
                        }
                    }
                    25..=27 => return Err(self.err_at(ErrorKind::Float, at)),
                    28..=30 => {
                        return Err(self.err_at(ErrorKind::ReservedAdditional(ib & 0x1f), at));
                    }
                    31 => return Err(self.err_at(ErrorKind::IndefiniteLength, at)),
                    _ => unreachable!("the additional information is five bits"),
                }
            }
        }
        self.depth = self.depth.saturating_sub(1);
        Ok(())
    }
}

/// What a validator or a decoder refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The item ends before its head or its body does.
    Truncated,
    /// An additional-information value RFC 8949 reserves.
    ReservedAdditional(u8),
    /// A head that is not in the shortest form that holds its argument.
    NonShortestHead,
    /// An indefinite length, which section 4.2 forbids.
    IndefiniteLength,
    /// A two-byte simple value whose second byte is below 32, which RFC 8949 section 3.3 says is
    /// not well-formed: every such form has a one-byte form or a reserved number.
    ReservedSimpleValue(u8),
    /// A float, which the schema has none of.
    Float,
    /// A tag, which the schema has none of.
    Tag,
    /// Map keys that are not in ascending order of their encoded bytes.
    UnsortedKeys,
    /// A map key that appears twice. The key is named when the reader knows it.
    DuplicateKey(u64),
    /// A [`Set`](crate::Set)'s items that are not in ascending order of their encoded bytes.
    UnsortedSet,
    /// A [`Set`](crate::Set)'s item that appears twice.
    DuplicateItem,
    /// A key the schema type does not have.
    UnknownKey(u64),
    /// A key the schema type has that the message leaves out.
    MissingKey(u64),
    /// An item past a limit: what it is, how long it is and the limit.
    OversizeItem {
        /// What the item is, as a reader names it: "text string", "byte string", "array", "map".
        what: &'static str,
        /// The length the item gives.
        len: u64,
        /// The limit it is past.
        limit: u64,
    },
    /// A head of another type than the schema asks for: the type the schema wanted.
    UnexpectedType {
        /// What the schema asked for.
        expected: &'static str,
    },
    /// A text string that is not UTF-8.
    BadUtf8,
    /// A logical source-relative path that is not normalized or escapes its root.
    InvalidPath,
    /// Bytes after the one item a message is.
    TrailingBytes,
    /// Nesting past [`MAX_DEPTH`].
    TooDeep,
}

impl ErrorKind {
    /// The offence in the words the validator's tests use: "non-shortest head", "indefinite
    /// length", "unsorted keys", "duplicate key", "float", "tag", "unknown key", "oversize item".
    pub fn offence(&self) -> &'static str {
        match self {
            ErrorKind::Truncated => "truncated",
            ErrorKind::ReservedAdditional(_) => "reserved additional information",
            ErrorKind::NonShortestHead => "non-shortest head",
            ErrorKind::ReservedSimpleValue(_) => "reserved simple value",
            ErrorKind::IndefiniteLength => "indefinite length",
            ErrorKind::Float => "float",
            ErrorKind::Tag => "tag",
            ErrorKind::UnsortedKeys => "unsorted keys",
            ErrorKind::DuplicateKey(_) => "duplicate key",
            ErrorKind::UnsortedSet => "unsorted set",
            ErrorKind::DuplicateItem => "duplicate item",
            ErrorKind::UnknownKey(_) => "unknown key",
            ErrorKind::MissingKey(_) => "missing key",
            ErrorKind::OversizeItem { .. } => "oversize item",
            ErrorKind::UnexpectedType { .. } => "unexpected type",
            ErrorKind::BadUtf8 => "text that is not UTF-8",
            ErrorKind::InvalidPath => "invalid source-relative path",
            ErrorKind::TrailingBytes => "bytes after the item",
            ErrorKind::TooDeep => "nesting too deep",
        }
    }
}

/// A refused message, with the byte it was refused at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// What was wrong.
    pub kind: ErrorKind,
    /// The offset in the input.
    pub at: usize,
}

impl Error {
    /// The offence, in the validator's words.
    pub fn offence(&self) -> &'static str {
        self.kind.offence()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.kind.offence(), self.at)?;
        match &self.kind {
            ErrorKind::UnknownKey(key) => write!(f, ": key {key}"),
            ErrorKind::MissingKey(key) => write!(f, ": key {key}"),
            ErrorKind::DuplicateKey(key) => write!(f, ": key {key}"),
            ErrorKind::OversizeItem { what, len, limit } => {
                write!(f, ": {what} of {len} bytes or items, at most {limit}")
            }
            ErrorKind::UnexpectedType { expected } => write!(f, ": expected {expected}"),
            _ => Ok(()),
        }
    }
}

impl std::error::Error for Error {}

fn major_name(major: u8) -> &'static str {
    match major {
        0 => "an integer",
        1 => "a negative integer",
        2 => "a byte string",
        3 => "a text string",
        4 => "an array",
        5 => "a map",
        6 => "a tag",
        _ => "a simple value",
    }
}

/// Checks the bytes of one message against section 4.2, naming the first offence.
///
/// This is the check of the bytes alone: heads, lengths, order, duplicates, floats, tags, depth
/// and size. Whether a key is one the schema has is a decoder's to say; [`validate_as`] does both.
pub fn validate(bytes: &[u8], limits: &Limits) -> Result<(), Error> {
    if bytes.len() as u64 > u64::from(limits.request) {
        return Err(Error {
            kind: ErrorKind::OversizeItem {
                what: "message",
                len: bytes.len() as u64,
                limit: u64::from(limits.request),
            },
            at: 0,
        });
    }
    let mut r = Reader::new(bytes, limits);
    r.walk()?;
    r.finish()
}

/// Checks the bytes and then reads them as `T`, so a key the type does not have is refused too.
///
/// This is [`decode`]: a bounded structural pass, then each field's schema-specific budgets.
/// Logical paths use the message budget; ordinary strings keep [`Limits::string`].
pub fn validate_as<T: Wire>(bytes: &[u8], limits: &Limits) -> Result<T, Error> {
    decode(bytes, limits)
}

/// Reads one value of `T` from canonical CBOR, after the validator's pass over the same bytes.
///
/// Message size, depth and canonical form are checked before schema decoding. The structural
/// pass allows text within the message budget, because it cannot identify SourcePath.
/// The typed pass applies ordinary string and logical path budgets independently.
pub fn decode<T: Wire>(bytes: &[u8], limits: &Limits) -> Result<T, Error> {
    // The structural pass cannot know which text is a logical path. Bound it by
    // request bytes; typed fields then enforce their own budgets. Bytes-only
    // validate keeps the ordinary text-string budget.
    let structural = Limits { string: limits.request, ..*limits };
    validate(bytes, &structural)?;
    let mut r = Reader::new(bytes, limits);
    let value = T::decode(&mut r)?;
    r.finish()?;
    Ok(value)
}

/// Writes one value of `T` as canonical CBOR.
pub fn encode<T: Wire>(value: &T) -> Vec<u8> {
    Encoder::of(value)
}

impl Wire for u64 {
    fn encode(&self, e: &mut Encoder) {
        e.uint(*self);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.uint()
    }
}

macro_rules! wire_uint {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Wire for $ty {
                fn encode(&self, e: &mut Encoder) {
                    e.uint(u64::from(*self));
                }

                fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
                    let at = r.position();
                    let v = r.uint()?;
                    <$ty>::try_from(v).map_err(|_| r.err_at(
                        ErrorKind::UnexpectedType { expected: "an integer in range" },
                        at,
                    ))
                }
            }
        )*
    };
}

wire_uint!(u8, u16, u32);

impl Wire for bool {
    fn encode(&self, e: &mut Encoder) {
        e.bool(*self);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.scalar()?;
        let at = r.position();
        match r.byte()? {
            0xf4 => Ok(false),
            0xf5 => Ok(true),
            _ => Err(r.err_at(ErrorKind::UnexpectedType { expected: "a boolean" }, at)),
        }
    }
}

impl<const N: usize> Wire for [u8; N] {
    fn encode(&self, e: &mut Encoder) {
        e.bytes(self);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        let bytes = r.bytes_of(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }
}

impl Wire for String {
    fn encode(&self, e: &mut Encoder) {
        e.text(self);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        Ok(r.text()?.to_owned())
    }
}

impl<T: Wire> Wire for Option<T> {
    fn encode(&self, e: &mut Encoder) {
        match self {
            Some(v) => v.encode(e),
            None => e.null(),
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        if r.peek_null() {
            r.scalar()?;
            r.pos += 1;
            Ok(None)
        } else {
            T::decode(r).map(Some)
        }
    }
}

impl<T: Wire> Wire for Vec<T> {
    fn encode(&self, e: &mut Encoder) {
        e.array(self.len());
        for v in self {
            v.encode(e);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        let len = r.array()?;
        let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0).min(1024));
        for _ in 0..len {
            out.push(T::decode(r)?);
        }
        r.array_finish();
        Ok(out)
    }
}

impl<A: Wire, B: Wire> Wire for (A, B) {
    fn encode(&self, e: &mut Encoder) {
        e.array(2);
        self.0.encode(e);
        self.1.encode(e);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.array_start(2)?;
        let a = A::decode(r)?;
        let b = B::decode(r)?;
        r.array_end()?;
        Ok((a, b))
    }
}

impl<A: Wire, B: Wire, C: Wire> Wire for (A, B, C) {
    fn encode(&self, e: &mut Encoder) {
        e.array(3);
        self.0.encode(e);
        self.1.encode(e);
        self.2.encode(e);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        r.array_start(3)?;
        let a = A::decode(r)?;
        let b = B::decode(r)?;
        let c = C::decode(r)?;
        r.array_end()?;
        Ok((a, b, c))
    }
}

/// The keys of a map, held in ascending order of their encoded bytes and without duplicates.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Set<T> {
    items: Vec<T>,
}

impl<T: Wire> Set<T> {
    /// A set of `items`, sorted by the bytes of each item's encoding, duplicates dropped.
    pub fn new(items: Vec<T>) -> Set<T> {
        let mut keyed: Vec<(Vec<u8>, T)> =
            items.into_iter().map(|item| (Encoder::of(&item), item)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.dedup_by(|a, b| a.0 == b.0);
        Set { items: keyed.into_iter().map(|(_, item)| item).collect() }
    }

    /// The items, in their canonical order.
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// The items, in their canonical order.
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }

    /// How many items the set has.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items, in their canonical order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.items.iter()
    }

    /// The items of an array whose header is already read, in their canonical order.
    fn items_of(r: &mut Reader<'_>, len: u64) -> Result<Set<T>, Error> {
        let mut items = Vec::with_capacity(usize::try_from(len).unwrap_or(0).min(1024));
        let mut previous: Option<Vec<u8>> = None;
        for _ in 0..len {
            let at = r.position();
            let item = T::decode(r)?;
            let bytes = Encoder::of(&item);
            if let Some(before) = &previous {
                match bytes.cmp(before) {
                    std::cmp::Ordering::Greater => {}
                    std::cmp::Ordering::Equal => {
                        r.array_finish();
                        return Err(r.err_at(ErrorKind::DuplicateItem, at));
                    }
                    std::cmp::Ordering::Less => {
                        r.array_finish();
                        return Err(r.err_at(ErrorKind::UnsortedSet, at));
                    }
                }
            }
            previous = Some(bytes);
            items.push(item);
        }
        r.array_finish();
        Ok(Set { items })
    }
}

impl<T: Wire> FromIterator<T> for Set<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Set<T> {
        Set::new(iter.into_iter().collect())
    }
}

impl<T: Wire> Wire for Set<T> {
    fn encode(&self, e: &mut Encoder) {
        // A set built by `new` is already sorted; one built by hand is sorted here, so the bytes
        // are canonical either way.
        let mut keyed: Vec<(Vec<u8>, &T)> =
            self.items.iter().map(|item| (Encoder::of(item), item)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.dedup_by(|a, b| a.0 == b.0);
        e.array(keyed.len());
        for (_, item) in keyed {
            item.encode(e);
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        let len = r.array()?;
        Set::items_of(r, len)
    }
}

/// Bytes that are not a `Vec`'s array: a byte string, as a blob, a hash or a program's output is.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes {
    /// The bytes.
    pub bytes: Vec<u8>,
}

impl Bytes {
    /// These bytes.
    pub fn new(bytes: Vec<u8>) -> Bytes {
        Bytes { bytes }
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Bytes {
        Bytes { bytes }
    }
}

impl crate::vectors::Sample for Bytes {
    fn sample(g: &mut crate::vectors::Gen) -> Self {
        Bytes { bytes: g.bytes() }
    }
}

impl Wire for Bytes {
    fn encode(&self, e: &mut Encoder) {
        e.bytes(&self.bytes);
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, Error> {
        Ok(Bytes { bytes: r.bytes()?.to_vec() })
    }
}

/// A schema type's struct: a map of the given field numbers, every key present, ascending.
///
/// The keys are literals in ascending order; the decoder reads them in that order, so a missing,
/// repeated, out-of-order or unknown key is refused by name.
///
/// A `lookup [0, 1]` clause after the name holds those fields' collections to
/// [`Limits::lookup_items`] rather than [`Limits::batch_items`]: a lookup's collection is 4,096
/// items and a batch's is 65,536, and the bytes-only validator cannot tell one array from the
/// other, so a lookup's schema holds its own field to the smaller number.
#[macro_export]
macro_rules! wire_struct {
    (
        $(#[$meta:meta])*
        $name:ident { $(validate $validator:path,)? $(sample $repair:path,)? $(lookup [$($lookup:literal),* $(,)?],)? $($key:literal => $field:ident : $ty:ty),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name {
            $(
                /// The field the schema numbers.
                pub $field: $ty,
            )*
        }

        impl $crate::Wire for $name {
            fn encode(&self, e: &mut $crate::Encoder) {
                e.map($crate::wire_struct!(@count $($field),*));
                $(
                    e.uint($key);
                    $crate::Wire::encode(&self.$field, e);
                )*
            }

            fn decode(r: &mut $crate::Reader<'_>) -> Result<Self, $crate::Error> {
                // The keys whose collections are a lookup's rather than a batch's, as the
                // struct's `lookup` clause lists them. The slice is built once, so the per-field
                // repetition below only names `$key`.
                let lookup: &[u64] = &[$($($lookup),*)?];
                r.map_start()?;
                $(
                    r.field($key)?;
                    let $field = if lookup.contains(&$key) {
                        r.lookup::<$ty>()?
                    } else {
                        <$ty as $crate::Wire>::decode(r)?
                    };
                )*
                r.map_end()?;
                let value = $name { $($field),* };
                $($validator(&value)?;)?
                Ok(value)
            }
        }

        impl $crate::Named for $name {
            const NAME: &'static str = stringify!($name);
        }

        impl $crate::vectors::Sample for $name {
            fn sample(g: &mut $crate::vectors::Gen) -> Self {
                #[allow(unused_mut)]
                let mut value = $name {
                    $($field: $crate::vectors::Sample::sample(g)),*
                };
                $($repair(&mut value);)?
                value
            }
        }
    };
    (@count $($field:ident),*) => {
        <[()]>::len(&[$($crate::wire_struct!(@one $field)),*])
    };
    (@one $field:ident) => { () };
}

/// A schema type's enum: the two-element array `[discriminant, payload]`, the payload the payload
/// type's own map (or a unit variant's `null`).
///
/// The macro accumulates the enum's items in a token list, because a macro call is not an item
/// inside an `enum` body, and writes the three walks (discriminant, encode, decode) as statements.
#[macro_export]
macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $($variants:tt)* }
    ) => {
        $crate::wire_enum!(@declare $name { $($variants)* } [ $($variants)* ] {} $(#[$meta])*);
    };

    (@declare $name:ident { $($orig:tt)* } [ $variant:ident => $disc:literal ($ty:ty), $($rest:tt)* ] { $($done:tt)* } $(#[$meta:meta])*) => {
        $crate::wire_enum!(@declare $name { $($orig)* } [ $($rest)* ] {
            $($done)*
            #[allow(missing_docs)]
            $variant($ty),
        } $(#[$meta])*);
    };
    (@declare $name:ident { $($orig:tt)* } [ $variant:ident => $disc:literal, $($rest:tt)* ] { $($done:tt)* } $(#[$meta:meta])*) => {
        $crate::wire_enum!(@declare $name { $($orig)* } [ $($rest)* ] {
            $($done)*
            #[allow(missing_docs)]
            $variant,
        } $(#[$meta])*);
    };
    (@declare $name:ident { $($orig:tt)* } [ ] { $($done:tt)* } $(#[$meta:meta])*) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($done)*
        }

        impl $name {
            /// The integer of the variant's comment in docs/distributed.md.
            pub fn discriminant(&self) -> u64 {
                $crate::wire_enum!(@disc self, $name, $($orig)*);
                unreachable!("every variant has a discriminant")
            }
        }

        impl $crate::Wire for $name {
            fn encode(&self, e: &mut $crate::Encoder) {
                e.array(2);
                $crate::wire_enum!(@write self, e, $name, $($orig)*);
                unreachable!("every variant is written")
            }

            fn decode(r: &mut $crate::Reader<'_>) -> Result<Self, $crate::Error> {
                r.array_start(2)?;
                let disc = r.uint()?;
                $crate::wire_enum!(@read r, disc, $name, $($orig)*);
                let at = r.position();
                Err($crate::Error { kind: $crate::ErrorKind::UnknownKey(disc), at })
            }
        }

        impl $crate::Named for $name {
            const NAME: &'static str = stringify!($name);
        }

        impl $crate::vectors::Sample for $name {
            #[allow(unused_assignments)]
            fn sample(g: &mut $crate::vectors::Gen) -> Self {
                let mut pick = g.below($crate::wire_enum!(@count $($orig)*) as u64);
                $crate::wire_enum!(@sample g, pick, $name, $($orig)*);
                unreachable!("every variant is sampled")
            }
        }
    };

    (@sample $g:ident, $pick:ident, $name:ident, $variant:ident => $disc:literal ($ty:ty), $($rest:tt)*) => {
        if $pick == 0 {
            return $name::$variant($crate::vectors::Sample::sample($g));
        }
        $pick -= 1;
        $crate::wire_enum!(@sample $g, $pick, $name, $($rest)*);
    };
    (@sample $g:ident, $pick:ident, $name:ident, $variant:ident => $disc:literal, $($rest:tt)*) => {
        if $pick == 0 {
            return $name::$variant;
        }
        $pick -= 1;
        $crate::wire_enum!(@sample $g, $pick, $name, $($rest)*);
    };
    (@sample $g:ident, $pick:ident, $name:ident,) => {};

    (@count $variant:ident => $disc:literal ($ty:ty), $($rest:tt)*) => {
        1 + $crate::wire_enum!(@count $($rest)*)
    };
    (@count $variant:ident => $disc:literal, $($rest:tt)*) => {
        1 + $crate::wire_enum!(@count $($rest)*)
    };
    (@count) => { 0 };

    (@disc $this:ident, $name:ident, $variant:ident => $disc:literal ($ty:ty), $($rest:tt)*) => {
        if let $name::$variant(..) = $this {
            return $disc;
        }
        $crate::wire_enum!(@disc $this, $name, $($rest)*);
    };
    (@disc $this:ident, $name:ident, $variant:ident => $disc:literal, $($rest:tt)*) => {
        if matches!($this, $name::$variant) {
            return $disc;
        }
        $crate::wire_enum!(@disc $this, $name, $($rest)*);
    };
    (@disc $this:ident, $name:ident,) => {};

    (@write $this:ident, $e:ident, $name:ident, $variant:ident => $disc:literal ($ty:ty), $($rest:tt)*) => {
        if let $name::$variant(payload) = $this {
            $e.uint($disc);
            $crate::Wire::encode(payload, $e);
            return;
        }
        $crate::wire_enum!(@write $this, $e, $name, $($rest)*);
    };
    (@write $this:ident, $e:ident, $name:ident, $variant:ident => $disc:literal, $($rest:tt)*) => {
        if matches!($this, $name::$variant) {
            $e.uint($disc);
            $e.null();
            return;
        }
        $crate::wire_enum!(@write $this, $e, $name, $($rest)*);
    };
    (@write $this:ident, $e:ident, $name:ident,) => {};

    (@read $r:ident, $disc:ident, $name:ident, $variant:ident => $discriminant:literal ($ty:ty), $($rest:tt)*) => {
        if $disc == $discriminant {
            let value = $name::$variant(<$ty as $crate::Wire>::decode($r)?);
            $r.array_end()?;
            return Ok(value);
        }
        $crate::wire_enum!(@read $r, $disc, $name, $($rest)*);
    };
    (@read $r:ident, $disc:ident, $name:ident, $variant:ident => $discriminant:literal, $($rest:tt)*) => {
        if $disc == $discriminant {
            $r.null()?;
            $r.array_end()?;
            return Ok($name::$variant);
        }
        $crate::wire_enum!(@read $r, $disc, $name, $($rest)*);
    };
    (@read $r:ident, $disc:ident, $name:ident,) => {};
}

impl Reader<'_> {
    /// `null`, and nothing else.
    pub fn null(&mut self) -> Result<(), Error> {
        self.scalar()?;
        let at = self.pos;
        if self.input.get(at) == Some(&0xf6) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err_at(ErrorKind::UnexpectedType { expected: "null" }, at))
        }
    }
}
