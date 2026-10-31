"""Compiler probes rustc 1.99.0 accepts, which blaze must never answer with a "no".

Each is a library checked with `--emit=metadata`, so every body is type and borrow
checked, as a build script's probe is. They lean on what rust-analyzer's inference
models least directly: coercions, closures, trait objects and generic inference. A
"no" from blaze to one of them tells a build script a feature is absent when it is
not; a loud refusal is allowed, a rejection is not.
"""

ACCEPTED = [
    ("coerce-unsize-array", "pub fn f() -> usize { let x: &[u8] = &[1, 2, 3]; x.len() }\n"),
    ("coerce-unsize-box", "pub fn f() -> Box<[u8]> { Box::new([0u8; 4]) }\n"),
    ("coerce-deref-box", "pub fn f() -> usize { let x: &[u8] = &*Box::new([0u8; 4]); x.len() }\n"),
    ("coerce-deref-string", "fn takes(s: &str) -> usize { s.len() }\n"
     "pub fn f(s: String) -> usize { takes(&s) }\n"),
    ("coerce-deref-chain", "use std::rc::Rc;\n"
     "pub fn f(s: Rc<Box<String>>) -> usize { let t: &str = &s; t.len() }\n"),
    ("coerce-mut-to-shared", "fn read(x: &u32) -> u32 { *x }\n"
     "pub fn f(x: &mut u32) -> u32 { read(x) }\n"),
    ("coerce-raw-pointer", "pub fn f(x: &u32) -> *const u32 { x }\n"
     "pub fn g(x: &mut u32) -> *mut u32 { x }\n"),
    ("coerce-fn-item", "fn one() -> u32 { 1 }\n"
     "pub fn f() -> fn() -> u32 { one }\n"),
    ("coerce-closure-to-fn-pointer", "pub fn f() -> fn(u32) -> u32 { |x| x + 1 }\n"),
    ("coerce-never", "pub fn f(x: Option<u32>) -> u32 {\n"
     "    let v: u32 = match x { Some(v) => v, None => return 0 };\n"
     "    let w: u32 = if v > 1 { v } else { panic!() };\n    w\n}\n"),
    ("coerce-dyn-reference", "use std::fmt::Debug;\n"
     "pub fn f(x: &u32) -> String { let d: &dyn Debug = x; format!(\"{d:?}\") }\n"),
    ("coerce-dyn-box", "use std::fmt::Display;\n"
     "pub fn f() -> Vec<Box<dyn Display>> { vec![Box::new(1u8), Box::new(\"two\"), Box::new(3.0f64)] }\n"),
    ("coerce-match-arms-to-dyn", "use std::fmt::Debug;\n"
     "pub fn f(a: bool) -> Box<dyn Debug> { if a { Box::new(1u32) } else { Box::new(\"no\") } }\n"),
    ("coerce-str-literal-to-str", "pub fn f() -> &'static str { \"text\" }\n"
     "pub const NAME: &str = \"name\";\n"),
    ("coerce-byte-string", "pub fn f() -> &'static [u8] { b\"bytes\" }\n"),
    ("closure-captures", "pub fn f(v: &mut Vec<u32>) -> u32 {\n"
     "    let base = 3;\n    let mut push = |x: u32| v.push(x + base);\n"
     "    push(1);\n    push(2);\n    v.iter().sum()\n}\n"),
    ("closure-move-return", "pub fn adder(n: u32) -> impl Fn(u32) -> u32 { move |x| x + n }\n"
     "pub fn boxed(n: u32) -> Box<dyn FnMut() -> u32> { let mut c = n; Box::new(move || { c += 1; c }) }\n"),
    ("closure-fnonce", "pub fn run<F: FnOnce() -> String>(f: F) -> String { f() }\n"
     "pub fn f() -> String { let s = String::from(\"x\"); run(move || s) }\n"),
    ("closure-iterator-chain", "pub fn f(v: &[i64]) -> Vec<String> {\n"
     "    v.iter().filter(|x| **x > 0).map(|x| x * 2).map(|x| x.to_string()).collect()\n}\n"),
    ("closure-sort-key", "pub fn f(v: &mut Vec<(String, u32)>) { v.sort_by_key(|(_, n)| *n); "
     "v.sort_by(|a, b| a.0.cmp(&b.0)); }\n"),
    ("trait-object-dispatch", "pub trait Shape { fn area(&self) -> f64; fn name(&self) -> &str { \"shape\" } }\n"
     "pub struct Square(pub f64);\nimpl Shape for Square { fn area(&self) -> f64 { self.0 * self.0 } }\n"
     "pub fn total(shapes: &[&dyn Shape]) -> f64 { shapes.iter().map(|s| s.area()).sum() }\n"
     "pub fn first(shapes: &[Box<dyn Shape>]) -> Option<&str> { shapes.first().map(|s| s.name()) }\n"),
    ("trait-object-any", "use std::any::Any;\n"
     "pub fn f(x: &dyn Any) -> Option<u32> { x.downcast_ref::<u32>().copied() }\n"
     "pub fn g(x: Box<dyn Any>) -> Result<Box<String>, Box<dyn Any>> { x.downcast::<String>() }\n"),
    ("trait-object-error", "use std::error::Error;\n"
     "pub fn parse(s: &str) -> Result<u32, Box<dyn Error + Send + Sync>> {\n"
     "    let n: u32 = s.trim().parse()?;\n    if n == 0 { return Err(\"zero\".into()); }\n    Ok(n)\n}\n"),
    ("trait-object-dyn-fn", "pub fn apply(fs: &[&dyn Fn(u32) -> u32], x: u32) -> u32 { fs.iter().fold(x, |acc, f| f(acc)) }\n"
     "pub fn f() -> u32 { apply(&[&|x| x + 1, &|x| x * 2], 3) }\n"),
    ("generic-collect", "use std::collections::{BTreeMap, HashMap, HashSet};\n"
     "pub fn f<'a>(v: &[&'a str]) -> (Vec<usize>, HashSet<&'a str>, BTreeMap<usize, &'a str>) {\n"
     "    let lens = v.iter().map(|s| s.len()).collect();\n"
     "    let set = v.iter().copied().collect();\n"
     "    let map = v.iter().map(|s| (s.len(), *s)).collect();\n    (lens, set, map)\n}\n"
     "pub fn g(v: Vec<(String, u32)>) -> HashMap<String, u32> { v.into_iter().collect() }\n"),
    ("generic-default-into", "#[derive(Default, Debug)]\npub struct Config { pub name: String, pub n: u32 }\n"
     "pub fn f() -> Config { let mut c: Config = Default::default(); c.name = \"x\".into(); c.n = u8::MAX.into(); c }\n"
     "pub fn g(x: u16) -> u64 { u64::from(x) + u32::from(7u8) as u64 }\n"),
    ("generic-parse-and-sum", "pub fn f(s: &str) -> Option<u64> {\n"
     "    let parts: Result<Vec<u64>, _> = s.split(',').map(str::parse::<u64>).collect();\n"
     "    Some(parts.ok()?.iter().sum())\n}\n"),
    ("generic-integer-literals", "pub fn f() -> (u8, i64, usize, f32) { let a = 1; let b = 2; let c = 3; let d = 1.5;\n"
     "    let v: Vec<usize> = vec![c]; (a, b, v[0], d) }\n"),
    ("generic-entry-api", "use std::collections::HashMap;\n"
     "pub fn f(words: &[&str]) -> HashMap<String, usize> {\n"
     "    let mut m = HashMap::new();\n    for w in words { *m.entry(w.to_string()).or_insert(0) += 1; }\n    m\n}\n"),
    ("generic-option-combinators", "pub fn f(x: Option<&str>) -> Result<usize, String> {\n"
     "    x.map(|s| s.len()).filter(|n| *n > 1).ok_or_else(|| \"short\".to_owned())\n}\n"),
    ("generic-bounds-and-assoc", "use std::ops::Add;\n"
     "pub fn sum<T: Add<Output = T> + Copy + Default>(v: &[T]) -> T { v.iter().fold(T::default(), |a, b| a + *b) }\n"
     "pub fn f() -> (u32, f64) { (sum(&[1, 2, 3]), sum(&[1.0, 2.5])) }\n"
     "pub fn first<I: IntoIterator>(i: I) -> Option<I::Item> { i.into_iter().next() }\n"),
    ("generic-const", "pub fn f<const N: usize>(x: [u8; N]) -> usize { x.len() + N }\n"
     "pub fn g() -> usize { f([1, 2, 3]) + f([0; 8]) }\n"),
    ("generic-impl-trait-arg", "pub fn f(v: impl IntoIterator<Item = impl AsRef<str>>) -> usize { v.into_iter().map(|s| s.as_ref().len()).sum() }\n"
     "pub fn g() -> usize { f(vec![\"a\", \"bc\"]) + f([String::new()]) }\n"),
    ("autoderef-smart-pointers", "use std::{cell::RefCell, rc::Rc, sync::{Arc, Mutex}};\n"
     "pub fn f(x: Rc<RefCell<Vec<u32>>>) -> usize { x.borrow_mut().push(1); x.borrow().len() }\n"
     "pub fn g(x: Arc<Mutex<String>>) -> usize { x.lock().unwrap().push('a'); x.lock().map(|s| s.len()).unwrap_or(0) }\n"),
    ("operator-overloading", "use std::ops::{Add, Index};\n#[derive(Clone, Copy, PartialEq, Debug)]\npub struct V(pub i32, pub i32);\n"
     "impl Add for V { type Output = V; fn add(self, o: V) -> V { V(self.0 + o.0, self.1 + o.1) } }\n"
     "pub struct Grid(Vec<u8>);\nimpl Index<usize> for Grid { type Output = u8; fn index(&self, i: usize) -> &u8 { &self.0[i] } }\n"
     "pub fn f(a: V, g: &Grid) -> (V, u8) { (a + a + V(1, 1), g[0] + 1) }\n"),
    ("patterns-and-refs", "pub fn f(v: &[(u32, Option<&str>)]) -> u32 {\n"
     "    let mut n = 0;\n    for &(k, ref name) in v {\n        if let Some(s) = name { n += s.len() as u32; }\n"
     "        match v { [first, .., last] if first.0 == last.0 => n += k, [_] => {}, _ => n += 1 }\n    }\n    n\n}\n"),
    ("labeled-break-value", "pub fn f(v: &[u32]) -> u32 { let found = 'search: { for &x in v { if x > 2 { break 'search x; } } 0 }; found }\n"),
    ("async-and-futures", "use std::future::Future;\n"
     "pub async fn one() -> u32 { 1 }\n"
     "pub fn two() -> impl Future<Output = u32> { async { one().await + 1 } }\n"
     "pub fn boxed() -> std::pin::Pin<Box<dyn Future<Output = u32> + Send>> { Box::pin(async move { two().await }) }\n"),
    ("question-mark-conversions", "use std::{fs, io, num::ParseIntError};\n#[derive(Debug)]\npub enum E { Io(io::Error), Parse(ParseIntError) }\n"
     "impl From<io::Error> for E { fn from(e: io::Error) -> E { E::Io(e) } }\n"
     "impl From<ParseIntError> for E { fn from(e: ParseIntError) -> E { E::Parse(e) } }\n"
     "pub fn f(p: &str) -> Result<u32, E> { Ok(fs::read_to_string(p)?.trim().parse()?) }\n"),
    ("iterator-impls", "pub struct Count(u32);\nimpl Iterator for Count { type Item = u32; fn next(&mut self) -> Option<u32> { self.0 += 1; (self.0 < 5).then_some(self.0) } }\n"
     "pub fn f() -> (u32, Vec<(usize, u32)>, Option<u32>) { (Count(0).sum(), Count(0).enumerate().collect(), Count(0).max()) }\n"),
    ("string-formatting", "use std::fmt::{self, Write as _};\npub struct P(pub u8);\n"
     "impl fmt::Display for P { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, \"<{}>\", self.0) } }\n"
     "pub fn f(p: &P) -> String { let mut s = format!(\"{p} {:>4} {:?}\", 7, \"q\"); writeln!(s, \"{:#x}\", 255).unwrap(); s }\n"),
    ("slices-and-strings", "pub fn f(s: &str) -> (Vec<&str>, Option<char>, String, bool) {\n"
     "    let words: Vec<&str> = s.split_whitespace().collect();\n"
     "    (words, s.chars().rev().next(), s.to_uppercase(), s.as_bytes().starts_with(b\"a\"))\n}\n"),
    ("lifetimes-and-structs", "pub struct Parser<'a> { input: &'a str, at: usize }\n"
     "impl<'a> Parser<'a> { pub fn new(input: &'a str) -> Self { Parser { input, at: 0 } }\n"
     "    pub fn next_word(&mut self) -> Option<&'a str> { let rest = &self.input[self.at..]; let w = rest.split(' ').next()?;"
     " self.at += w.len() + 1; Some(w) } }\n"),
    ("proc-macro-span-calls", "extern crate proc_macro;\nuse proc_macro::{Span, TokenStream, TokenTree};\n"
     "pub fn f(s: Span) -> (usize, usize) { (s.start().line(), s.end().column()) }\n"
     "pub fn g(t: TokenStream) -> Vec<Span> { t.into_iter().map(|tt: TokenTree| tt.span()).collect() }\n"),
]
