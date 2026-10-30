//! Version 1 listing extension. Cursors lease an immutable sorted metadata snapshot.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use hyper::{HeaderMap, Response, StatusCode, header};

use super::{Body, Server, full, unescape, written_ms};
use crate::{
    Error, ErrorCode,
    list::{ListedObject, ObjectsAnswer},
};

struct Snapshot {
    prefix: String,
    created: Instant,
    objects: Arc<Vec<ListedObject>>,
}

#[derive(Default)]
pub(super) struct Pages(Mutex<BTreeMap<String, Snapshot>>);

impl Pages {
    pub(super) fn expire(&self, age: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, snapshot| snapshot.created.elapsed() < age);
    }
}

impl Server {
    pub(super) async fn paged_objects(
        &self,
        query: &str,
        headers: &HeaderMap,
    ) -> Result<Response<Body>, Error> {
        if headers.get_all("x-blaze-listing-version").iter().count() != 1
            || headers.get("x-blaze-listing-version").and_then(|it| it.to_str().ok()) != Some("1")
        {
            return Err(Error::version("unsupported listing extension; this server speaks 1"));
        }
        let mut fields = BTreeMap::new();
        for item in query.split('&').filter(|it| !it.is_empty()) {
            let (key, value) = item
                .split_once('=')
                .ok_or_else(|| Error::malformed("listing query needs key=value"))?;
            if !matches!(key, "prefix" | "cursor" | "limit") || fields.insert(key, value).is_some()
            {
                return Err(Error::malformed("unknown or duplicate listing query field"));
            }
        }
        let prefix = unescape(fields.get("prefix").copied().unwrap_or(""));
        let count = fields
            .get("limit")
            .map(|it| it.parse::<usize>())
            .transpose()
            .map_err(|_| Error::malformed("listing limit is not a number"))?
            .unwrap_or(1024)
            .min(self.limits.batch_items as usize);
        if count == 0 {
            return Err(Error::malformed("listing limit must be positive"));
        }
        self.pages.expire(self.resources.snapshot_age);
        let (id, offset, objects) = if let Some(cursor) = fields.get("cursor") {
            let (id, offset) =
                cursor.split_once('.').ok_or_else(|| Error::malformed("invalid listing cursor"))?;
            if id.len() != 32
                || !id.bytes().all(|it| it.is_ascii_digit() || (b'a'..=b'f').contains(&it))
                || offset.is_empty()
                || !offset.bytes().all(|it| it.is_ascii_digit())
            {
                return Err(Error::malformed("invalid listing cursor"));
            }
            let offset: usize =
                offset.parse().map_err(|_| Error::malformed("invalid listing cursor offset"))?;
            let snapshots = self.pages.0.lock().unwrap_or_else(|e| e.into_inner());
            let snapshot = snapshots.get(id).ok_or_else(|| {
                Error::new(ErrorCode::Conflict, "listing cursor expired or unknown")
            })?;
            if snapshot.prefix != prefix || offset == 0 || offset >= snapshot.objects.len() {
                return Err(Error::malformed("cursor does not name a page of this prefix"));
            }
            (id.to_owned(), offset, snapshot.objects.clone())
        } else {
            // Metadata collection is on the blocking pool. Publication cannot add new rows
            // to this snapshot, and deletion after capture remains a real fetch/GC error.
            let listed_prefix = prefix.clone();
            let objects = self.disk(move |store| store.list(&listed_prefix)).await?;
            let objects = Arc::new(
                objects
                    .into_iter()
                    .map(|it| ListedObject {
                        name: it.name,
                        len: it.len,
                        written_ms: written_ms(it.written),
                    })
                    .collect::<Vec<_>>(),
            );
            let mut nonce = [0; 16];
            getrandom::fill(&mut nonce).map_err(|e| Error::internal(e.to_string()))?;
            let id: String = nonce.iter().map(|it| format!("{it:02x}")).collect();
            (id, 0, objects)
        };
        let available = count.min(objects.len() - offset);
        let encode = |count| {
            blaze_wire::encode(&ObjectsAnswer { objects: objects[offset..offset + count].to_vec() })
        };
        let mut take = available;
        let mut bytes = encode(take);
        if bytes.len() > self.limits.request as usize {
            let (mut low, mut high) = (0, take);
            while low < high {
                let middle = (low + high).div_ceil(2);
                if encode(middle).len() <= self.limits.request as usize {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            take = low;
            bytes = encode(take);
        }
        if (available != 0 && take == 0) || bytes.len() > self.limits.request as usize {
            // No page can make progress over this row. Do not retain an unreachable lease
            // until expiry or let repeated failed listings exhaust all snapshot slots.
            self.pages.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(Error::too_large(
                "one listing object cannot fit the negotiated response limit",
            ));
        }
        let mut answer = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/cbor")
            .header("x-blaze-listing-version", "1")
            .header("x-blaze-listing-total", objects.len().to_string());
        let next = offset + take;
        if next < objects.len() {
            answer = answer.header("x-blaze-listing-next", format!("{id}.{next}"));
            if offset == 0 {
                self.pages.expire(self.resources.snapshot_age);
                let mut snapshots = self.pages.0.lock().unwrap_or_else(|e| e.into_inner());
                if snapshots.len() >= self.resources.connections {
                    return Err(Error::new(ErrorCode::Busy, "listing snapshot leases are busy"));
                }
                snapshots.insert(
                    id,
                    Snapshot { prefix, created: Instant::now(), objects: objects.clone() },
                );
            }
        } else {
            self.pages.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }
        Ok(answer.body(full(bytes))?)
    }
}
