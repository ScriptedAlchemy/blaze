//! The places of the machine a call is made on, kept out of its key.
//!
//! A macro is given the environment of the crate it expands in and the directory it runs in:
//! `CARGO_MANIFEST_DIR`, `OUT_DIR` and the workspace's root are paths of this machine's checkout
//! and target directory. Keyed as they are, the same call on another machine is another key.
//! Each of those places is written in the key as what stands for it, so that a call has the key it
//! has wherever the checkout is. What a macro reads below such a place is not in the key, here or
//! before: a macro that reads files by itself is named in `RA_JIT_PROC_MACRO_CACHE_SKIP`.
//!
//! A macro that puts a place in its output (a path in a string it makes) would hand the next
//! machine this machine's path, so its expansion is not kept ([`Places::names`]).

use std::borrow::Cow;

use base_db::Env;

/// What stands for the package's directory.
const PACKAGE: &str = "\0PACKAGE\0";
/// What stands for the target directory an `OUT_DIR` is below, up to cargo's `build`.
const TARGET: &str = "\0TARGET\0";
/// What stands for the directory the macro runs in.
const CURRENT: &str = "\0CWD\0";
/// A place shorter than this is no place of a checkout: `/` is in every path.
const LEAST: usize = 4;

/// The places of one call, each with what stands for it, the longest first.
pub(crate) struct Places(Vec<(String, &'static str)>);

impl Places {
    pub(crate) fn of(env: &Env, current_dir: &str) -> Places {
        let trimmed = |it: String| it.trim_end_matches(['/', '\\']).to_owned();
        let mut places: Vec<(String, &'static str)> = Vec::new();
        if let Some(out) = env.get("OUT_DIR").map(trimmed)
            && let Some(at) = out.replace('\\', "/").rfind("/build/")
        {
            places.push((out[..at].to_owned(), TARGET));
        }
        if let Some(manifest) = env.get("CARGO_MANIFEST_DIR").map(trimmed) {
            places.push((manifest, PACKAGE));
        }
        places.push((trimmed(current_dir.to_owned()), CURRENT));
        places.retain(|(place, _)| place.len() >= LEAST);
        // The longer first: the directory the macro runs in is often the package's, or what the
        // package is in, and the target directory is often below it.
        places.sort_by_key(|(place, _)| std::cmp::Reverse(place.len()));
        places.dedup_by(|a, b| a.0 == b.0);
        Places(places)
    }

    /// `text` with each place in it, wherever it stands as a whole path, as what stands for it.
    pub(crate) fn unplace<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut text = Cow::Borrowed(text);
        for (place, stands_for) in &self.0 {
            if !text.contains(place.as_str()) {
                continue;
            }
            let mut out = String::with_capacity(text.len());
            let mut rest = &*text;
            while let Some(at) = rest.find(place.as_str()) {
                let end = at + place.len();
                // `/ws` is not `/ws2`, whose path it starts: a place ends where a path does.
                let whole = rest[end..]
                    .chars()
                    .next()
                    .is_none_or(|it| !(it.is_alphanumeric() || matches!(it, '_' | '-' | '.')));
                out.push_str(&rest[..at]);
                out.push_str(if whole { stands_for } else { place });
                rest = &rest[end..];
            }
            out.push_str(rest);
            text = Cow::Owned(out);
        }
        text
    }

    /// Whether `bytes` have a place of this call in them: output that does belongs to this
    /// machine alone.
    pub(crate) fn names(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(place, _)| bytes.windows(place.len()).any(|it| it == place.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(manifest: &str, out: &str) -> Env {
        let mut env = Env::default();
        env.set("CARGO_MANIFEST_DIR", manifest);
        env.set("OUT_DIR", out);
        env
    }

    #[test]
    fn the_same_call_has_one_key_wherever_the_checkout_is() {
        let one = Places::of(
            &env("/home/a/ws/inner", "/home/a/ws/target/debug/build/inner-1f/out"),
            "/home/a/ws",
        );
        let other = Places::of(
            &env(
                "/Users/b/some/place/ws/inner",
                "/Users/b/some/place/ws/target/debug/build/inner-1f/out",
            ),
            "/Users/b/some/place/ws",
        );
        for (a, b) in [
            ("/home/a/ws/inner", "/Users/b/some/place/ws/inner"),
            ("/home/a/ws/inner/src", "/Users/b/some/place/ws/inner/src"),
            (
                "/home/a/ws/target/debug/build/inner-1f/out",
                "/Users/b/some/place/ws/target/debug/build/inner-1f/out",
            ),
            ("/home/a/ws", "/Users/b/some/place/ws"),
            (
                "x=/home/a/ws/inner;y=/home/a/ws",
                "x=/Users/b/some/place/ws/inner;y=/Users/b/some/place/ws",
            ),
        ] {
            assert_eq!(one.unplace(a), other.unplace(b), "{a}");
        }
        assert_eq!(one.unplace("/home/a/ws/inner/src"), "\0PACKAGE\0/src");
        assert_eq!(
            one.unplace("/home/a/ws/target/debug/build/inner-1f/out"),
            "\0TARGET\0/build/inner-1f/out"
        );
    }

    #[test]
    fn a_path_that_only_starts_with_a_place_is_not_that_place() {
        let places = Places::of(&env("/home/a/ws/inner", "/o/build/x/out"), "/home/a/ws");
        assert_eq!(places.unplace("/home/a/ws2/src"), "/home/a/ws2/src");
        assert_eq!(places.unplace("/home/a/ws.old"), "/home/a/ws.old");
        assert_eq!(places.unplace("/elsewhere/x"), "/elsewhere/x");
        // Text with no place is not copied.
        assert!(matches!(places.unplace("no path here"), Cow::Borrowed(_)));
    }

    #[test]
    fn output_with_a_place_in_it_is_told() {
        let places = Places::of(
            &env("/home/a/ws/inner", "/home/a/ws/target/debug/build/i/out"),
            "/home/a/ws",
        );
        assert!(places.names(b"const DIR: &str = \"/home/a/ws/inner/data\";"));
        assert!(places.names(b"/home/a/ws"));
        assert!(!places.names(b"const DIR: &str = \"data\";"));
        // A place of no length is every path's: it is not one.
        let none = Places::of(&Env::default(), "/");
        assert!(!none.names(b"/anything/at/all"));
    }
}
