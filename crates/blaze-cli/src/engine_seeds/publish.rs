//! The one publication path for closed dependency heads. A batch borrows completed frozen
//! cells, checks every cell before any put, then prepares the existing dependency root before
//! publishing. The request's checking stage and a completed export use the same policy.

use core::fmt;

use blaze_drive::{Key, Tool};
use blaze_engine::Stage;
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{FrozenError, FrozenHead};
use blaze_ladder::front2::tables::seeds::MOST_CELLS;
use blaze_own::{Alloc, Vec};
use bstd::cx::Cx;

use super::{CrateAt, KEY_BYTES, NotPut, crate_keyed, dependency_key};
use crate::CliError;
use crate::engine_store::Ledger;
use crate::store::Store;

/// One successful source freeze. The full crate key comes from the original names source;
/// a crate prefix alone is not evidence of its origin.
pub(crate) struct FreshHead<'h> {
    pub(crate) cell: Want,
    pub(crate) krate: Option<CrateItemKey>,
    pub(crate) bytes: &'h [u8],
}

pub(crate) struct StageOpts<'a, 'c, I> {
    pub(crate) heads: I,
    pub(crate) crates: &'c [CrateAt],
    pub(crate) names: &'c dyn Fn(&CrateAt) -> Option<Key>,
    pub(crate) tool: Tool,
    pub(crate) alloc: Alloc<'a>,
}

/// A structural publication refusal. Malformed cells, unequal duplicates, and structurally
/// invalid roots are refused before puts. A later store or root-write failure remains fatal;
/// already accepted immutable puts are not rolled back or reported as a successful batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    Frozen { cell: Want, why: FrozenError },
    Origin { cell: Want, krate: CrateItemKey },
    Unequal { cell: Want, key: Key },
    Existing { cell: Want, key: Key },
    PastCap { what: &'static str, cap: usize },
    RootBytes { bytes: usize },
    RootExpired,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frozen { cell, why } => write!(f, "dependency head {cell:?} cannot be published: {why}"),
            Self::Origin { cell, krate } => write!(f, "dependency head {cell:?} does not belong to its source crate {krate:?}"),
            Self::Unequal { cell, key } => write!(f, "dependency head {cell:?} has unequal staged values under {key:?}"),
            Self::Existing { cell, key } => write!(f, "dependency head {cell:?} differs from the immutable value already stored under {key:?}"),
            Self::PastCap { what, cap } => write!(f, "dependency head publication: {what} exceeds its cap of {cap}"),
            Self::RootBytes { bytes } => write!(f, "the dependency heads' root has {bytes} bytes, not a whole number of keys"),
            Self::RootExpired => write!(f, "the dependency heads' root expired before publication"),
        }
    }
}

struct Put<'h> {
    cell: Want,
    key: Key,
    names_at: Key,
    bytes: &'h [u8],
    toolchain: bool,
}

/// Payloads remain in their completed maker/request; only bounded key metadata is staged.
pub(crate) struct Batch<'h, 'a> {
    puts: Vec<Put<'h>, Alloc<'a>>,
    frozen: u32,
    duplicates: u32,
    not_put: NotPut,
    alloc: Alloc<'a>,
}

impl<'h, 'a> Batch<'h, 'a> {
    pub(crate) fn stage<'c, I>(opts: StageOpts<'a, 'c, I>) -> Result<Self, Invalid>
    where
        I: IntoIterator<Item = FreshHead<'h>>,
    {
        let mut puts = Vec::new_in(opts.alloc);
        let mut frozen = 0usize;
        let mut not_put = NotPut::default();
        for one in opts.heads {
            if frozen == MOST_CELLS {
                return Err(Invalid::PastCap { what: "the completed frozen cells", cap: MOST_CELLS });
            }
            frozen += 1;
            let head = FrozenHead::read(one.bytes).map_err(|why| Invalid::Frozen { cell: one.cell, why })?;
            if head.want() != one.cell {
                return Err(Invalid::Frozen { cell: one.cell, why: FrozenError::WrongCell { found: head.want() } });
            }
            if one.cell.key().is_inner() {
                not_put.local = not_put.local.saturating_add(1);
                continue;
            }
            if let Some(krate) = one.krate {
                if !one.cell.key().is_of(krate) {
                    return Err(Invalid::Origin { cell: one.cell, krate });
                }
            }
            let Some(krate) = one.krate.and_then(|key| crate_keyed(opts.crates, key)) else {
                not_put.unlisted = not_put.unlisted.saturating_add(1);
                continue;
            };
            if !krate.closed {
                not_put.member = not_put.member.saturating_add(1);
                continue;
            }
            let Some(names_at) = (opts.names)(&krate) else {
                not_put.no_names = not_put.no_names.saturating_add(1);
                continue;
            };
            puts.push(Put {
                cell: one.cell,
                key: dependency_key(names_at, one.cell, opts.tool),
                names_at,
                bytes: one.bytes,
                toolchain: krate.toolchain,
            });
        }
        puts.sort_unstable_by_key(|one| one.key);
        for (left, right) in puts.iter().zip(puts.iter().skip(1)) {
            if left.key == right.key && (left.cell != right.cell || left.names_at != right.names_at || left.bytes != right.bytes || left.toolchain != right.toolchain) {
                return Err(Invalid::Unequal { cell: right.cell, key: right.key });
            }
        }
        let before = puts.len();
        puts.dedup_by_key(|one| one.key);
        let duplicates = count(before - puts.len());
        Ok(Batch {
            puts,
            frozen: count(frozen),
            duplicates,
            not_put,
            alloc: opts.alloc,
        })
    }

    /// The export adds these exact keys to its existing toolchain root write. Checking also
    /// queues them through `Store::keep_with_toolchain_batch` for its answer's save.
    pub(crate) fn toolchain_keys(&self) -> impl Iterator<Item = Key> + '_ {
        self.puts.iter().filter(|one| one.toolchain).map(|one| one.key)
    }

    pub(crate) fn commit(self, opts: CommitOpts<'_, '_>) -> Result<Published, CliError> {
        // Root structure and the entire union are prepared before the first head put.
        let retained = Retention::prepare(&self, &opts)?;
        let mut toolchain = Vec::new_in(self.alloc);
        toolchain.extend(self.toolchain_keys());
        // A conflicting later cell must not leave earlier cells of this batch published.
        // Store::put_parts keeps its immutable-value check as well, at the actual write.
        for one in self.puts.iter() {
            if let Some(held) = opts.store.get_local(one.key)? {
                if held.bytes.get(..KEY_BYTES) != Some(one.names_at.0.0.as_slice()) || held.bytes.get(KEY_BYTES..) != Some(one.bytes) {
                    return Err(CliError::HeadPublication(Invalid::Existing { cell: one.cell, key: one.key }));
                }
            }
        }
        for one in self.puts.iter() {
            let parts: [&[u8]; 2] = [&one.names_at.0.0, one.bytes];
            opts.ledger.put(opts.stage, one.key, &parts, 0);
            opts.store.put_parts(opts.cx, one.key, &parts)?;
        }
        opts.store.keep_with_toolchain_batch(&toolchain)?;
        let counts = Published {
            frozen: self.frozen,
            put: count(self.puts.len()),
            duplicates: self.duplicates,
            listed: retained.listed,
            retained: retained.retained,
            unlisted: retained.unlisted,
            not_put: self.not_put,
        };
        retained.apply(opts.store, opts.root)?;
        Ok(counts)
    }
}

pub(crate) struct CommitOpts<'s, 'm> {
    pub(crate) cx: &'s Cx,
    pub(crate) store: &'s mut dyn Store<'m>,
    pub(crate) root: &'s str,
    pub(crate) prior: RootState,
    pub(crate) added: &'s [Key],
    pub(crate) stage: Stage,
    pub(crate) ledger: &'s mut Ledger,
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Published {
    pub(crate) frozen: u32,
    pub(crate) put: u32,
    pub(crate) duplicates: u32,
    pub(crate) listed: u32,
    pub(crate) retained: u32,
    pub(crate) unlisted: u32,
    pub(crate) not_put: NotPut,
}

/// The root's checked shape at the start of the request. A warm empty batch renews this root
/// in O(1), with no per-record probes, key copy, sort, or hashing.
#[derive(Clone, Copy)]
pub(crate) struct RootState {
    had_root: bool,
    listed: usize,
}

impl RootState {
    pub(crate) fn read<'m>(store: &dyn Store<'m>, root: &str) -> Result<Self, CliError> {
        Self::of(store.named(root)?).map_err(CliError::HeadPublication)
    }

    fn of(bytes: Option<&[u8]>) -> Result<Self, Invalid> {
        match bytes {
            None => Ok(RootState { had_root: false, listed: 0 }),
            Some(bytes) => {
                if bytes.len() % KEY_BYTES != 0 {
                    return Err(Invalid::RootBytes { bytes: bytes.len() });
                }
                let listed = bytes.len() / KEY_BYTES;
                if listed > MOST_CELLS {
                    return Err(Invalid::PastCap { what: "the stored dependency root", cap: MOST_CELLS });
                }
                Ok(RootState { had_root: true, listed })
            }
        }
    }
}

enum RootWrite<'a> {
    None,
    Renew,
    Pin(Vec<Key, Alloc<'a>>),
}

struct Retention<'a> {
    write: RootWrite<'a>,
    listed: u32,
    retained: u32,
    unlisted: u32,
}

impl<'a> Retention<'a> {
    fn prepare(batch: &Batch<'_, 'a>, opts: &CommitOpts<'_, '_>) -> Result<Self, CliError> {
        if opts.added.len() > MOST_CELLS {
            return Err(CliError::HeadPublication(Invalid::PastCap { what: "the additional dependency root keys", cap: MOST_CELLS }));
        }
        if batch.puts.is_empty() && opts.added.is_empty() && opts.prior.had_root {
            return Ok(Retention { write: RootWrite::Renew, listed: count(opts.prior.listed), retained: count(opts.prior.listed), unlisted: 0 });
        }
        if batch.puts.is_empty() && opts.added.is_empty() && opts.prior.listed == 0 {
            return Ok(Retention { write: RootWrite::None, listed: 0, retained: 0, unlisted: 0 });
        }
        let existing = opts.store.named(opts.root)?;
        let state = RootState::of(existing).map_err(CliError::HeadPublication)?;
        if opts.prior.had_root && opts.prior.listed > 0 && !state.had_root {
            return Err(CliError::HeadPublication(Invalid::RootExpired));
        }
        // Each term is capped above, so the union's staging is bounded by three tables.
        let mut keys = Vec::with_capacity_in(state.listed + opts.added.len() + batch.puts.len(), batch.alloc);
        if let Some(bytes) = existing {
            for chunk in bytes.chunks_exact(KEY_BYTES) {
                let mut key = [0u8; KEY_BYTES];
                key.copy_from_slice(chunk);
                keys.push(Key(ContentId(key)));
            }
        }
        keys.extend_from_slice(opts.added);
        keys.sort_unstable();
        keys.dedup();
        let listed = count(keys.len());
        keys.extend(batch.puts.iter().map(|one| one.key));
        keys.sort_unstable();
        keys.dedup();
        let unlisted = count(keys.len().saturating_sub(MOST_CELLS));
        keys.truncate(MOST_CELLS);
        Ok(Retention { listed, retained: count(keys.len()), unlisted, write: RootWrite::Pin(keys) })
    }

    fn apply<'m>(self, store: &mut dyn Store<'m>, root: &str) -> Result<(), CliError> {
        match self.write {
            RootWrite::None => Ok(()),
            RootWrite::Renew => {
                if store.renew_one(root)? {
                    Ok(())
                } else {
                    Err(CliError::HeadPublication(Invalid::RootExpired))
                }
            }
            RootWrite::Pin(keys) => store.pin(root, &keys),
        }
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
