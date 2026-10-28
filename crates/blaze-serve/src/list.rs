//! `GET /v1/objects?prefix=<p>`: the objects under a prefix, as CBOR.
//!
//! Section 8 has no listing call: the server collects its own cache (C7), so no client has to
//! enumerate it. Today's `ra-jit cache gc --remote` does, on the `Dir` place, the `S3` place
//! and this one alike, so C3 has the call and C7 removes the need for it. The shape is the
//! simplest one that says what a listing says: a map of one field, an array of
//! `(name, bytes, written ms since the epoch)`, in the order of their names. It is a
//! `wire_struct` like every other message, so the validator holds it to the same limits.

blaze_wire::wire_struct! {
    /// `GET /v1/objects`'s answer: the objects under the prefix, in the order of their names.
    ObjectsAnswer {
        0 => objects: Vec<ListedObject>,
    }
}

blaze_wire::wire_struct! {
    /// One object of a listing.
    ListedObject {
        0 => name: String,
        1 => len: u64,
        2 => written_ms: u64,
    }
}

/// The answer for `objects`.
pub(crate) fn answer(objects: Vec<(String, u64, u64)>) -> ObjectsAnswer {
    ObjectsAnswer {
        objects: objects
            .into_iter()
            .map(|(name, len, written_ms)| ListedObject { name, len, written_ms })
            .collect(),
    }
}

impl ObjectsAnswer {
    /// The objects, as a client reads them.
    pub fn listed(&self) -> impl Iterator<Item = (&str, u64, u64)> {
        self.objects.iter().map(|it| (it.name.as_str(), it.len, it.written_ms))
    }
}
