//! Channel gating of effective crate feature attributes after cfg expansion.

use crate::{Crate, db::HirDatabase};
use hir_def::unstable_features::UnstableFeatures;

impl Crate {
    /// Stable and beta reject `#![feature]`, including stabilized feature names.
    /// The caller supplies the release channel and RUSTC_BOOTSTRAP policy.
    pub fn feature_attributes_on_stable(self, db: &dyn HirDatabase) -> bool {
        UnstableFeatures::query(db, self.id).iter().next().is_some()
    }
}
