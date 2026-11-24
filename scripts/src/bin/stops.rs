//! stops: the places a library can stop the program only go down, per crate (AGENTS.md
//! panics-fail-one-job, rule 20; no-fallback, rule 3; verify-once, rule 74). `DOC` below is what
//! its usage error prints; it says what a stop is, what the text scan misses, and the rule.
//!
//!     blaze run -q -p scripts --bin stops -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin stops -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin stops -- '{"command": "findings"}'
//!     blaze run -q -p scripts --bin stops -- selftest
//!
//! Why (2026-11-15): every `.unwrap()`, `.expect(..)`, `panic!` and unchecked index is a place
//! where a library decides, for its caller, that the program ends. A failure the caller can meet
//! is a typed error; an invariant is a type or a proof the index takes. This holds the count
//! from the text until blaze's lints (`blaze::nopanic` effects) hold it. Test code, the scanner
//! and the list are `blaze_script::ratchet`'s.

/// What the usage error prints.
const DOC: &str = r#"stops: the places a library can stop the program only go down, per crate (AGENTS.md panics-fail-one-job, rule 20).

    blaze run -q -p scripts --bin stops -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin stops -- '{"command": "write"}'
    blaze run -q -p scripts --bin stops -- '{"command": "findings"}'
    blaze run -q -p scripts --bin stops -- selftest

    command   check     each crate's count equals its row's in the list: one over its row is
                        refused (its new sites named given `base`), one under it must be lowered
                        with `write`; a crate with no row is held to 0; with `base`, no row is
                        above the base's (CI); the ten crates with the most are printed
              write     write the list from the tree as it is, the ten crates printed
              findings  print every site, with its kind, as a `blaze_script::finding` document
    root      the checkout (".")
    base      the commit the change is against, or null

The list's row is `<crate><TAB><stops>`. A stop is, at its line, one of:
  unwrap         `.unwrap()`
  expect         `.expect(..)`
  panic, unreachable, todo, unimplemented
                 the macro: the word and `!` (not `!=`), not after `.`
  index          `a[i]`: a `[` right after an expression's end (a name that is no keyword, a
                 number, `)`, `]` or `?`) whose contents are not an integer literal, a range of
                 them (`0..4`, `..=3`, `..`) or empty; a string key (`map["k"]`) counts, since a
                 map's index panics on a missing key
Comments, strings and characters are skipped. The scan reads no types, so it misses
`.unwrap_or_else(|| panic!())` (it counts the `panic!`), `assert!` (not counted), a stop a macro
makes from tokens, and an index written with `Index::index`; and over-counts an index whose type
cannot panic (a map type of the crate's own) and a slice pattern after a name it does not know
as a keyword.

Test code is `blaze_script::ratchet`'s: files under `tests` or `benches`, `tests.rs`, `test.rs`,
`#![cfg(test)]` files, the items after `#[cfg(test)]` and `#[test]`, and modules test code
declares. A crate is the nearest directory under crates/ with a Cargo.toml.
"#;

use std::{collections::BTreeMap, path::PathBuf};

use blaze_script::{
    TempDir, fail,
    finding::{self, Finding},
    input::{self, Fields, Input, InputError},
    ratchet::{self, Hit, Policy, Run, Tests, close_of, comment_end, ident_len, is_ident, quote_end, string_end, word_end},
};

const SCRIPT: &str = "stops";
/// The list, from the root.
const LIST: &str = "tools/ratchets/stops.txt";
/// How many crates `write` and `check` print, most first.
const TOP: usize = 10;
/// The words before a `[` that start a pattern, a type or an array, not an index.
const NOT_BEFORE_INDEX: [&str; 22] = [
    "in", "let", "return", "break", "mut", "for", "where", "impl", "dyn", "as", "else", "match", "if", "while", "move", "yield",
    "unsafe", "const", "static", "ref", "box", "type",
];
const MACROS: [&str; 4] = ["panic", "unreachable", "todo", "unimplemented"];
const RULE: &str = "AGENTS.md panics-fail-one-job (rule 20) and no-fallback (rule 3): a library stops nobody's program; \
                    it returns an error naming what and where, or takes a proof that the case cannot happen (rule 74)";
const INSTEAD: &str = "return a typed error (`?`) where the caller can meet the case; where it cannot, make the \
                       invariant a type (a checked index, a non-empty collection, a proof the function takes), and \
                       `.get(i)` with the error where an index can miss";

/// What a stop is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Unwrap,
    Expect,
    Panic,
    Unreachable,
    Todo,
    Unimplemented,
    Index,
}

impl Kind {
    const ALL: [Kind; 7] = [Kind::Unwrap, Kind::Expect, Kind::Panic, Kind::Unreachable, Kind::Todo, Kind::Unimplemented, Kind::Index];

    const fn finding(self) -> finding::Kind {
        let rule = "20 panics-fail-one-job";
        let (name, label, instead) = match self {
            Kind::Unwrap => ("unwrap", "`.unwrap()`s", "`?` with a typed error, or a type that holds the value"),
            Kind::Expect => ("expect", "`.expect(..)`s", "`?` with a typed error naming what and where"),
            Kind::Panic => ("panic", "`panic!`s", "return a typed error; a broken invariant is a type, not a panic"),
            Kind::Unreachable => ("unreachable", "`unreachable!`s", "a type or a match that makes the case unwritable"),
            Kind::Todo => ("todo", "`todo!`s", "build the case, or refuse it with a typed error naming the rule"),
            Kind::Unimplemented => ("unimplemented", "`unimplemented!`s", "build the case, or refuse it with a typed error"),
            Kind::Index => ("index", "unchecked indexes", "`.get(i)` with a typed error, or an index type whose bound is proved once"),
        };
        finding::Kind { name, label, rule, instead, mechanical: false }
    }
}

/// The list's first lines.
const HEADER: &str = "# The places a library can stop the program, outside test code, by crate (its directory under\n\
                      # crates/): `.unwrap()`, `.expect(..)`, `panic!`, `unreachable!`, `todo!`, `unimplemented!` and\n\
                      # an index that is no literal, read from the text. Only shrinks; a crate with no row is held to\n\
                      # 0 (AGENTS.md panics-fail-one-job). Written by scripts/src/bin/stops.rs (`{\"command\":\n\
                      # \"write\"}`), checked in CI.\n\
                      # crate\tstops\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "findings"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// The stops of `text`, as (line, kind), in text order, test code included.
fn stops(text: &str) -> Vec<(usize, Kind)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut line = 1;
    let mut counted = 0;
    let mut at = 0;
    let mut push = |at: usize, kind: Kind, line: &mut usize, counted: &mut usize| {
        *line += text[*counted..at].matches('\n').count();
        *counted = at;
        out.push((*line, kind));
    };
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b'[' => {
                if is_index(text, at) {
                    push(at, Kind::Index, &mut line, &mut counted);
                }
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                let word = &text[at..end];
                let before = text[..at].trim_end();
                let after = text[end..].trim_start();
                let method = before.ends_with('.') && !before.ends_with("..");
                let kind = match word {
                    "unwrap" if method && after.strip_prefix('(').is_some_and(|it| it.trim_start().starts_with(')')) => Some(Kind::Unwrap),
                    "expect" if method && after.starts_with('(') => Some(Kind::Expect),
                    _ if MACROS.contains(&word) && !method && after.starts_with('!') && !after.starts_with("!=") => Some(match word {
                        "panic" => Kind::Panic,
                        "unreachable" => Kind::Unreachable,
                        "todo" => Kind::Todo,
                        _ => Kind::Unimplemented,
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    push(at, kind, &mut line, &mut counted);
                }
                end
            }
            // A number's digits and suffix (`1u8`, `0x1F`): never the start of a word.
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    out
}

/// Whether the `[` at `at` opens an index that is not a literal one.
fn is_index(text: &str, at: usize) -> bool {
    let before = text[..at].trim_end();
    let Some(&last) = before.as_bytes().last() else { return false };
    let after_expression = if is_ident(last) {
        let run = before.bytes().rev().take_while(|b| is_ident(*b)).count();
        let word = &before[before.len() - run..];
        let head = before[..before.len() - run].as_bytes().last().copied();
        // `'a [u8]` is a lifetime's; `#[`, `!` are attributes' and macros'.
        !NOT_BEFORE_INDEX.contains(&word) && head != Some(b'\'')
    } else {
        matches!(last, b')' | b']' | b'?')
    };
    if !after_expression {
        return false;
    }
    let Some(close) = close_of(text, at) else { return false };
    let inside = text[at + 1..close].trim();
    !(inside.is_empty() || inside.contains(';') || literal_or_range(inside))
}

/// Whether `inside` is an integer literal or a range of them (either end may be left out).
fn literal_or_range(inside: &str) -> bool {
    let literal = |it: &str| {
        let it = it.trim();
        it.is_empty() || (it.as_bytes()[0].is_ascii_digit() && it.bytes().all(is_ident))
    };
    match inside.split_once("..") {
        Some((from, to)) => literal(from) && literal(to.strip_prefix('=').unwrap_or(to)),
        None => !inside.is_empty() && literal(inside),
    }
}

fn named(it: &Hit<Kind>) -> String {
    format!("{}:{} ({})", it.path, it.line, it.what.finding().name)
}

/// The total, and the ten crates with the most, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Hit<Kind>>>) -> String {
    let total: usize = counts.values().map(Vec::len).sum();
    let mut most: Vec<(&String, usize)> = counts.iter().map(|(krate, sites)| (krate, sites.len())).collect();
    most.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let top: Vec<String> = most.iter().take(TOP).map(|(krate, n)| format!("{krate} {n}")).collect();
    format!("{total} stops outside tests, in {} crates (most: {})", counts.len(), top.join(", "))
}

const POLICY: Policy<'static> = Policy {
    list: LIST,
    noun: "places that can stop the program (`.unwrap()`, `.expect`, `panic!`-likes, unchecked indexes)",
    instead: INSTEAD,
    rule: RULE,
    at_zero: &[],
};

fn main_with(tests: &Tests, it: &Args) {
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let counts = ratchet::walk(SCRIPT, &root, tests, stops);
    if it.command == "findings" {
        let found = counts.iter().flat_map(|(krate, sites)| {
            sites.iter().map(|it| Finding { krate: krate.clone(), file: it.path.clone(), line: it.line, kind: it.what.finding().name, detail: None })
        });
        finding::print(SCRIPT, &Kind::ALL.map(Kind::finding), found.collect());
        return;
    }
    let summary = summary(&counts);
    let run = Run { script: SCRIPT, policy: &POLICY, header: HEADER, root: &root, write: it.command == "write", base: it.base.as_deref(), summary: &summary };
    ratchet::write_or_check(&run, &counts, named);
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// The selftest's file. Line by line:
/// - 1, `.unwrap()`: unwrap; 2, `.expect("x")`: expect; 3, `.unwrap_or(0)`, `.expect_err`: none;
/// - 4, `panic!`, `unreachable!`, `todo!`, `unimplemented!`: one each;
/// - 5, `std::panic::catch_unwind` and `panic != x`: none;
/// - 6, `v[i]`: index; `v[0]`, `v[1..]`, `v[..]`, `v[..=2]`: literal, none;
/// - 7, `m["k"]`: index; `f()[j]`: index; `a[i][0]`: one;
/// - 8, `#[derive(Debug)]`, `let [a, b] = x;`, `&'a [u8]`, `[0u8; 4]`, `vec![1]`: none;
/// - 9, `x: [u8; N]`, `for y in [1, 2]`, `Some([a])`: none;
/// - 10, in a string and a comment: none;
/// - 12, in `#[cfg(test)] mod tests`: test code.
const FIXTURE: &str = r#"fn a(x: Option<u8>) -> u8 { x.unwrap() }
fn b(x: Option<u8>) -> u8 { x.expect("x") }
fn c(x: Option<u8>, y: Result<u8, u8>) -> u8 { x.unwrap_or(0) + y.expect_err(1) }
fn d(k: u8) { match k { 0 => panic!("p"), 1 => unreachable!(), 2 => todo!(), _ => unimplemented!() } }
fn e(panic: u8, x: u8) -> bool { let _ = std::panic::catch_unwind(|| ()); panic != x }
fn f(v: &[u8], i: usize) -> u8 { v[i] + v[0] + v[1..][0] + v[..][0] + v[..=2][0] }
fn g(m: &M, a: &[[u8; 2]], i: usize, j: usize) -> u8 { m["k"] + f()[j] + a[i][0] }
#[derive(Debug)] fn h<'a>(x: [u8; 2], s: &'a [u8]) { let [a, b] = x; let _ = [0u8; 4]; let _ = vec![1]; }
fn i(x: [u8; N]) { for y in [1, 2] {} let _ = Some([1]); }
const S: &str = "x.unwrap() v[i]"; // panic!()
#[cfg(test)]
mod tests { fn t(x: Option<u8>) -> u8 { x.unwrap() } }
"#;

/// [`FIXTURE`]'s stops outside test code.
const FIXTURE_SITES: [(usize, Kind); 11] = [
    (1, Kind::Unwrap),
    (2, Kind::Expect),
    (4, Kind::Panic),
    (4, Kind::Unreachable),
    (4, Kind::Todo),
    (4, Kind::Unimplemented),
    (6, Kind::Index),
    (7, Kind::Index),
    (7, Kind::Index),
    (7, Kind::Index),
    (12, Kind::Unwrap),
];

fn selftest(tests: &Tests) {
    let all = stops(FIXTURE);
    expect(all == FIXTURE_SITES, &format!("the fixture's stops, test code in: {all:?}"));
    expect(literal_or_range("0") && literal_or_range("1..") && literal_or_range("..=3") && literal_or_range("..") && literal_or_range("0x1F"), "literals and their ranges");
    expect(!literal_or_range("i") && !literal_or_range("i..") && !literal_or_range("N") && !literal_or_range("\"k\""), "names and strings are no literal");
    // Planted: each cause removed, its effect goes.
    let planted = stops(&FIXTURE.replace("v[i] + ", ""));
    expect(!planted.iter().any(|it| it.0 == 6), &format!("line 6 without `v[i]` has none: {planted:?}"));
    let planted = stops(&FIXTURE.replace("panic != x", "panic!(x)"));
    expect(planted.iter().any(|it| *it == (5, Kind::Panic)), "`panic!(x)` in place of `panic != x` counts");

    // A tree: test code out, the rows and a rise.
    let tmp = TempDir::new("stops-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    // The root workspace; a parked crate with a stop that must not be counted.
    put("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/parked\"]\n");
    put("crates/parked/Cargo.toml", "[package]\nname = \"parked\"\n");
    put("crates/parked/src/lib.rs", "pub fn p() -> u8 { None::<u8>.unwrap() }\n");
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", FIXTURE);
    put("crates/a/tests/it.rs", "fn t() { None::<u8>.unwrap(); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(counts.get("a").is_some_and(|it| it.len() == 10) && counts.len() == 1, &format!("the tree's stops: {counts:?}"));
    let rows = ratchet::parse_rows(LIST, &ratchet::render(HEADER, &counts, &[])).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    expect(ratchet::problems(&POLICY, &counts, &rows, Some(&rows), None, named).is_empty(), "the tree as recorded is right");
    let fewer = BTreeMap::from([("a".to_owned(), 9)]);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), std::collections::BTreeSet::from([1]))]);
    let said = ratchet::problems(&POLICY, &counts, &fewer, None, Some(&added), named);
    expect(said.len() == 1 && said[0].contains("over its row's 9") && said[0].contains("new: crates/a/src/lib.rs:1 (unwrap)"), &format!("a rise: {said:?}"));
    expect(summary(&counts).contains("(most: a 10)"), "the crates with the most");
    test_module_paths(tests);
    println!("{SCRIPT} selftest: ok");
}

/// The actual module graph decides which helpers are test-only, not their filenames. Each
/// planted production edge below must restore the same helper's stops without editing it.
fn test_module_paths(tests: &Tests) {
    let tmp = TempDir::new("stops-paths-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text))
            .unwrap_or_else(|e| fail(SCRIPT, &format!("{path}: {e}")));
    };
    put("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    const ROOT: &str = "mod signals;\n";
    const DECL: &str = "#[cfg(all(test, not(blaze_check)))]\n#[path = \"signals_tests.rs\"]\npub(crate) mod tests;\n";
    const HELPER: &str = "#[path = \"fixture/deep.rs\"]\nmod deep;\nfn child_case() { panic!(\"a real helper stop\"); }\n";
    const PATH: &str = "crates/a/src/signals_tests.rs";
    const DEEP: &str = "crates/a/src/fixture/deep.rs";
    const WRONG: &str = "crates/a/src/signals/tests.rs";
    put("crates/a/src/lib.rs", ROOT);
    put("crates/a/src/signals.rs", DECL);
    put(PATH, HELPER);
    put(DEEP, "fn deep() { None::<u8>.unwrap(); }\n");
    put(WRONG, "fn unrelated() { panic!(\"not the path target\"); }\n");
    let has = |counts: &BTreeMap<String, Vec<Hit<Kind>>>, path: &str| counts.values().flatten().any(|site| site.path == path);
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, PATH) && !has(&counts, DEEP) && has(&counts, WRONG), &format!("the compound cfg and explicit path exclude only their real subtree: {counts:?}"));

    for cfg in ["", "#[cfg(any(test, feature = \"production\"))]\n", "#[cfg(all(test = \"value\", unix))]\n", "#[cfg_attr(test, cfg(test))]\n"] {
        put("crates/a/src/signals.rs", &format!("{cfg}#[path = \"signals_tests.rs\"]\npub(crate) mod tests;\n"));
        let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
        expect(has(&counts, PATH) && has(&counts, DEEP), &format!("{cfg:?} cannot prove a production helper is test-only: {counts:?}"));
    }
    put("crates/a/src/signals.rs", DECL);
    for alias in [
        "#[path = \"signals_tests.rs\"] mod shared;\n",
        "#[cfg(any(test, feature = \"production\"))]\n#[path = \"signals_tests.rs\"] mod shared;\n",
        "#[cfg_attr(feature = \"production\", path = \"signals_tests.rs\")] mod shared;\n",
        "#[cfg_attr(feature = \"production\", path = r#\"signals_tests.rs\"#)] mod shared;\n",
        "#[cfg_attr(feature = \"production\", path = \"signals_\\x74ests.rs\")] mod shared;\n",
        "mod inline { #[path = \"../signals_tests.rs\"] mod shared; }\n",
        "mod /* real module */ signals_tests;\n",
        "#[path /* trivia */ = \"signals_tests.rs\"] mod shared;\n",
        "#[path = /* trivia */ \"signals_tests.rs\"] mod shared;\n",
        "#[cfg_attr(feature = \"production\", path /* trivia */ = \"signals_tests.rs\")] mod shared;\n",
        "#[r#path = \"signals_tests.rs\"] mod shared;\n",
        "#[r#cfg_attr(feature = \"production\", r#path = \"signals_tests.rs\")] mod shared;\n",
        "# [path = \"signals_tests.rs\"] mod shared;\n",
        "# /* trivia */ [path = \"signals_tests.rs\"] mod shared;\n",
    ] {
        put("crates/a/src/lib.rs", &format!("{ROOT}{alias}"));
        let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
        expect(has(&counts, PATH) && has(&counts, DEEP), &format!("a possible production alias keeps the helper and descendants counted: {alias:?}: {counts:?}"));
    }
    put("crates/a/src/lib.rs", ROOT);
    for prefix in [
        "#[cfg_attr(feature = \"lint\", allow(dead_code))]\n",
        "#[cfg_attr(feature = \"fmt\", rustfmt::skip)]\n",
    ] {
        put("crates/a/src/signals.rs", &format!("{prefix}{DECL}"));
        let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
        expect(!has(&counts, PATH) && !has(&counts, DEEP), &format!("an unrelated cfg_attr cannot change the module path: {counts:?}"));
    }
    put("crates/a/src/signals.rs", "#[path = \"signals_tests.rs\"]\nfn unrelated() {}\n#[cfg(test)]\nmod tests;\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(has(&counts, PATH) && !has(&counts, WRONG), &format!("a prior item's path never leaks onto the next module: {counts:?}"));
    put("crates/a/src/signals.rs", "#[cfg(test)]\n#[path_hint(path = \"signals_tests.rs\")]\nmod tests;\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(has(&counts, PATH) && !has(&counts, WRONG), &format!("only the exact built-in path attribute changes a module's file: {counts:?}"));
    put("crates/a/src/signals.rs", &DECL.replace("signals_tests.rs", "fixture/../signals_tests.rs"));
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, PATH) && !has(&counts, DEEP), &format!("literal parent components are normalized: {counts:?}"));

    // A path-loaded file's ordinary child is a sibling. The bounded scanner retains both
    // possible ownership contexts, so it must not exclude an unrelated stem/leaf.rs.
    put("crates/a/src/signals.rs", DECL);
    put(PATH, "mod leaf;\nfn child_case() { panic!(\"helper\"); }\n");
    put("crates/a/src/leaf.rs", "fn sibling() { panic!(\"retained conservatively\"); }\n");
    put("crates/a/src/signals_tests/leaf.rs", "fn production() { panic!(\"not the path-loaded child\"); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, PATH) && has(&counts, "crates/a/src/leaf.rs") && has(&counts, "crates/a/src/signals_tests/leaf.rs"), &format!("uncertain ownership cannot exclude either possible child: {counts:?}"));

    put(PATH, HELPER);
    put("crates/a/src/lib.rs", &format!("{ROOT}#[cfg(test)]\n#[path = \"unsupported\\x2ers\"] mod uncertain_test;\n"));
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, PATH) && !has(&counts, DEEP), &format!("an unknown test-only path cannot invalidate a proved test subtree: {counts:?}"));
    let quoted = tests.of("const TEXT: &str = r#\"\n#[cfg(test)]\n#[path = \"live.rs\"]\nmod fake;\n\"#;\nmod live;\n");
    expect(quoted.mods.len() == 1 && !quoted.whole && !quoted.test.iter().any(|test| *test), &format!("quoted attributes are not source declarations: {quoted:?}"));
    for text in [
        "fn live() { panic!(\"count me\"); } #[cfg(test)] mod tests;\n",
        "#[cfg(test)] mod tests; fn live() { panic!(\"count me\"); }\n",
        "fn live() { panic!(\"count me\"); } #[test] fn a_test() {}\n",
        "#[test] fn a_test() {} fn live() { panic!(\"count me\"); }\n",
    ] {
        let found = tests.of(text);
        expect(stops(text).iter().any(|(line, _)| !found.test.get(line - 1).copied().unwrap_or(false)), &format!("a test never masks neighboring production code: {text:?}: {found:?}"));
    }
    let complete = tests.of("  #[test] fn a_test() { panic!(\"test only\"); } // trailing trivia\n");
    expect(complete.test.first() == Some(&true), "a complete test item with only surrounding trivia is excluded");
    for (open, close) in [("(", ")"), ("[", "]")] {
        put("crates/a/src/lib.rs", ROOT);
        put("crates/a/src/signals.rs", &format!("ignore!{open}\n#![cfg(test)]\n#[cfg(test)]\n#[path = \"signals_tests.rs\"]\nmod fake;\n{close};\n"));
        let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
        expect(has(&counts, PATH), &format!("macro input cannot prove a physical file is test-only: {open}: {counts:?}"));
    }
    put("crates/a/src/signals.rs", DECL);
    for (open, close) in [("(", ")"), ("[", "]"), ("{", "}")] {
        let invocation = format!("make!{open}#[cfg(test)] #[path=\"signals_tests.rs\"] mod alias;{close};\n");
        put("crates/a/src/lib.rs", &format!("{ROOT}{invocation}"));
        let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
        expect(has(&counts, PATH), &format!("a macro may consume an apparent test attribute and emit production: {open}: {counts:?}"));
        put("crates/a/src/lib.rs", &format!("{ROOT}#[cfg(test)]\n{invocation}"));
        let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
        expect(!has(&counts, PATH), &format!("a guard around the entire macro invocation is a real test gate: {open}: {counts:?}"));
    }
    put("crates/a/src/lib.rs", "fn library_entry() { panic!(\"Cargo's library root\"); }\n");
    put("crates/a/src/bin/tool.rs", "fn main() { panic!(\"Cargo's binary root\"); }\n");
    put("crates/a/src/main.rs", "#[cfg(test)]\n#[path = \"lib.rs\"] mod fixture;\n#[cfg(test)]\n#[path = \"bin/tool.rs\"] mod tool_fixture;\n#[cfg(test)]\n#[path = \"../examples/auto.rs\"] mod example_fixture;\n");
    put("crates/a/examples/auto.rs", "fn main() { panic!(\"Cargo's example root\"); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(has(&counts, "crates/a/src/lib.rs") && has(&counts, "crates/a/src/bin/tool.rs") && has(&counts, "crates/a/examples/auto.rs"), &format!("test aliases cannot hide implicit production entry roots: {counts:?}"));

    put("crates/a/Cargo.toml", "[package]\nname = 'a'\nbuild = 'setup/custom.rs'\n[lib]\npath = 'library/custom.rs'\n[[bin]]\nname = 'tool'\npath = 'tools/custom.rs'\n[[bin]]\nname = 'mixed'\npath = 'library/main.rs'\n[[bin]]\nname = 'named-tests'\npath = 'tests.rs'\n[[example]]\nname = 'demo'\npath = 'demos/custom.rs'\n");
    put("crates/a/src/main.rs", "#[cfg(test)]\n#[path = \"../library/custom.rs\"] mod library_fixture;\n#[cfg(test)]\n#[path = \"../tools/custom.rs\"] mod binary_fixture;\n#[cfg(test)]\n#[path = \"../setup/custom.rs\"] mod build_fixture;\n#[cfg(test)]\n#[path = \"../demos/custom.rs\"] mod demo_fixture;\n");
    for target in ["library", "tools", "setup", "demos"] {
        put(&format!("crates/a/{target}/custom.rs"), "fn entry() { panic!(\"manifest's production root\"); }\n");
    }
    put("crates/a/tests.rs", "fn main() { panic!(\"a production target can be named tests.rs\"); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(["library", "tools", "setup", "demos"].iter().all(|target| has(&counts, &format!("crates/a/{target}/custom.rs"))) && has(&counts, "crates/a/tests.rs"), &format!("manifest-declared lib, bin, build and example paths are production roots regardless of filename: {counts:?}"));
    put("crates/a/library/custom.rs", "#![cfg(test)]\nfn entry() { panic!(\"the whole source requires test\"); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, "crates/a/library/custom.rs") && has(&counts, "crates/a/tools/custom.rs"), &format!("a root's own whole-file test cfg remains authoritative: {counts:?}"));

    // Only the children are test-aliased here. Aliasing the roots too would already widen
    // their possible directories and hide a missing Cargo-root ownership fact.
    put("crates/a/src/main.rs", "#[cfg(test)]\n#[path = \"../library/child.rs\"] mod library_child;\n#[cfg(test)]\n#[path = \"../examples/child/mod.rs\"] mod example_child;\n");
    for (directory, entry, child, deep) in [("library", "custom", "child.rs", "deep.rs"), ("examples", "auto", "child/mod.rs", "child/deep.rs")] {
        put(&format!("crates/a/{directory}/{entry}.rs"), "mod child;\nfn entry() { panic!(\"production root\"); }\n");
        put(&format!("crates/a/{directory}/{child}"), "#[path = \"deep.rs\"] mod deep;\nfn child() { panic!(\"production child\"); }\n");
        put(&format!("crates/a/{directory}/{deep}"), "fn deep() { panic!(\"production descendant\"); }\n");
    }
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(["library/child.rs", "library/deep.rs", "examples/child/mod.rs", "examples/child/deep.rs"].iter().all(|file| has(&counts, &format!("crates/a/{file}"))), &format!("custom and example roots own sibling children, transitively: {counts:?}"));
    put("crates/a/library/main.rs", "mod custom;\nfn main() {}\n");
    put("crates/a/src/main.rs", "#[cfg(test)]\n#[path = \"../library/child.rs\"] mod library_child;\n#[cfg(test)]\n#[path = \"../examples/child/mod.rs\"] mod example_child;\n#[cfg(test)]\n#[path = \"../library/custom/child.rs\"] mod ordinary_child;\n");
    put("crates/a/library/custom/child.rs", "fn child() { panic!(\"the binary's ordinary module context\"); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(has(&counts, "crates/a/library/child.rs") && has(&counts, "crates/a/library/custom/child.rs"), &format!("a file can be both a Cargo root and an ordinary module: {counts:?}"));
    put("crates/a/library/custom.rs", "#![cfg(test)]\nmod child;\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, "crates/a/library/child.rs") && !has(&counts, "crates/a/library/deep.rs") && !has(&counts, "crates/a/library/custom/child.rs") && has(&counts, "crates/a/examples/child/mod.rs"), &format!("removing only the root's production use removes its subtree: {counts:?}"));

    put("crates/a/tests/nested/Cargo.toml", "[package]\nname = 'fixture'\n[lib]\npath = 'custom.rs'\n");
    put("crates/a/tests/nested/custom.rs", "fn fixture() { panic!(\"fixture manifest, not a production root\"); }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, stops);
    expect(!has(&counts, "crates/a/tests/nested/custom.rs"), &format!("fixture manifests retain the caller's test-directory policy: {counts:?}"));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tests = Tests::new(SCRIPT);
    match args.as_slice() {
        [one] if one == "selftest" => selftest(&tests),
        [one] => main_with(&tests, &input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too.
    #[test]
    fn the_selftest_holds() {
        selftest(&Tests::new(SCRIPT));
    }
}
