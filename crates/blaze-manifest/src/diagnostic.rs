//! Errors cargo shows as a snippet of the manifest with the place marked, instead of as a chain
//! of causes: a `Cargo.toml` that does not parse or does not fit the schema.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};

/// A manifest error with the span it is about. Whatever wraps it, cargo prints only this, and
/// a front end finds it with `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct ManifestDiagnostic {
    message: String,
    path: PathBuf,
    contents: String,
    span: Range<usize>,
}

impl ManifestDiagnostic {
    pub(crate) fn from_toml(error: toml::de::Error, contents: &str, path: &Path) -> anyhow::Error {
        match error.span() {
            Some(span) => ManifestDiagnostic {
                message: error.message().to_owned(),
                path: path.to_owned(),
                contents: contents.to_owned(),
                span,
            }
            .into(),
            None => error.into(),
        }
    }

    /// The diagnostic as cargo prints it, the manifest's path relative to `cwd`.
    pub fn render(&self, cwd: &Path) -> String {
        let path = pathdiff::diff_paths(&self.path, cwd).unwrap_or_else(|| self.path.clone());
        let group = Group::with_title(Level::ERROR.primary_title(&self.message)).element(
            Snippet::source(&self.contents)
                .path(path.display().to_string())
                .annotation(AnnotationKind::Primary.span(self.span.clone())),
        );
        Renderer::plain().render(&[group])
    }
}

impl fmt::Display for ManifestDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ManifestDiagnostic {}
