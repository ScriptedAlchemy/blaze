//! The checked package mount shared by macro and build-script views, independent of a runner.

use std::sync::Arc;
use blaze_expand::seen::{MemberDirError, SeenRoot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRoot {
    member: Option<Arc<str>>,
}

impl blaze_own::Share for ScriptRoot {}

impl ScriptRoot {
    pub const fn package() -> ScriptRoot { ScriptRoot { member: None } }
    pub fn member(dir: &str) -> Result<ScriptRoot, MemberDirError<'_>> {
        SeenRoot::member(dir)?;
        Ok(ScriptRoot { member: Some(Arc::from(dir)) })
    }
    pub fn seen(&self) -> SeenRoot<'_> {
        match &self.member {
            None => SeenRoot::Package,
            Some(dir) => SeenRoot::Member { dir },
        }
    }
}
