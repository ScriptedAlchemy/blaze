//! The library stability marks of an item: `#[stable]` and `#[unstable]` as the
//! standard library's source writes them, after `cfg_attr` expansion.
//!
//! rustc's stability pass reads the same attributes of a staged-API crate
//! (`rustc_passes::stability`). An item with neither mark is reported as
//! [`Stability::Unmarked`], which callers proving acceptance must not treat as stable.

use std::ops::ControlFlow;

use base_db::SourceDatabase;
use hir_expand::attrs::AstPathExt;
use syntax::{
    NodeOrToken,
    ast::{self, TokenTreeChildren},
};

use crate::AttrDefId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    /// `#[stable(...)]`, and no `#[unstable]`.
    Stable,
    /// `#[unstable(...)]`. A `soft` mark is a lint, not a hard error, in rustc.
    Unstable { soft: bool },
    /// Neither mark, or a mark this reader does not understand.
    Unmarked,
}

impl Stability {
    pub fn query(db: &dyn SourceDatabase, owner: AttrDefId) -> Stability {
        let mut stable = false;
        let mut unstable = None;
        let mut unknown = false;
        super::collect_attrs::<()>(db, owner, |meta| {
            if let ast::Meta::TokenTreeMeta(meta) = &meta {
                let path = meta.path();
                if path.is1("stable") {
                    stable = true;
                } else if path.is1("unstable") {
                    let Some(tt) = meta.token_tree() else {
                        unknown = true;
                        return ControlFlow::Continue(());
                    };
                    let soft = TokenTreeChildren::new(&tt).any(|it| {
                        matches!(it, NodeOrToken::Token(token) if token.text() == "soft")
                    });
                    unstable = Some(soft || unstable.unwrap_or(false));
                }
            } else if let Some(path) = meta.path()
                && (path.is1("stable") || path.is1("unstable"))
            {
                unknown = true;
            }
            ControlFlow::Continue(())
        });
        match (stable, unstable, unknown) {
            (_, _, true) => Stability::Unmarked,
            (_, Some(soft), false) => Stability::Unstable { soft },
            (true, None, false) => Stability::Stable,
            (false, None, false) => Stability::Unmarked,
        }
    }
}
