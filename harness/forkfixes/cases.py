"""The programs of harness/forkfixes.py. `fixes` names the fork commits each stands for."""

MAIN = "src/main.rs"

CASES = [
    {
        "name": "raw-duplicate-import",
        "fixes": ["47b6a115c", "f36949f1c"],
        "files": {MAIN: "mod a { pub fn x() {} }\nmod b { pub fn x() {} }\nuse a::r#x;\nuse b::x;\nfn main() { x(); }\n"},
        "error": "E0252",
    },
    {
        "name": "unresolved-paths",
        "fixes": ["cf529a630"],
        "files": {MAIN: "mod m {}\nfn main() { let _ = m::missing; }\n"},
        "error": "E0425",
    },
    {
        "name": "unresolved-bare-value",
        "fixes": ["cf529a630"],
        "files": {MAIN: "fn main() { let _ = missing_value; }\n"},
        "error": "E0425",
    },
    {
        "name": "attribute-impl-in-function",
        "fixes": ["cdb01e00b"],
        "files": {
            "Cargo.toml": '[workspace]\nmembers = ["pm", "app"]\nresolver = "2"\n',
            "pm/Cargo.toml": '[package]\nname = "pm"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\nproc-macro = true\n',
            "pm/src/lib.rs": "use proc_macro::TokenStream;\n#[proc_macro_attribute]\npub fn input_replace(attr: TokenStream, _item: TokenStream) -> TokenStream {\n    attr\n}\n",
            "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\npm = { path = "../pm" }\n',
            "app/src/main.rs": "trait Trait { fn value(&self) -> u16; }\nfn holder() {\n    #[pm::input_replace(impl Trait for u32 { fn value(&self) -> u16 { 2 } })]\n    struct Input;\n}\nfn main() {\n    holder();\n    println!(\"{}\", 0u32.value());\n}\n",
        },
        "program": "app",
        "prints": ["2"],
    },
    {
        "name": "object-default-in-body",
        "fixes": ["64f4e9d84", "b7f288aa6"],
        "files": {MAIN: """trait Speak { fn speak(&self) -> u32; }
struct Borrowed<'a>(&'a u32);
impl Speak for Borrowed<'_> { fn speak(&self) -> u32 { *self.0 } }
fn borrowed_type_arguments(value: u32) -> u32 {
    struct Ref<'a, T: ?Sized + 'a>(&'a T);
    let inner = Borrowed(&value);
    let borrowed: Ref<'_, dyn Speak> = Ref(&inner);
    let boxed: Box<dyn Speak> = Box::new(Borrowed(&value));
    borrowed.0.speak() + boxed.speak()
}
fn main() { println!("{}", borrowed_type_arguments(21)); }
"""},
        "prints": ["42"],
    },
    {
        "name": "borrowed-object-keeps-its-lifetime",
        "fixes": ["f9992a19d", "0f3715fa6"],
        "files": {MAIN: """trait Trait { fn v(&self) -> i32; }
impl Trait for i32 { fn v(&self) -> i32 { *self } }
struct Holder<'a> { value: &'a (dyn Trait + 'a) }
fn identity(x: &dyn Trait) -> &dyn Trait { x }
fn wrap(x: &dyn Trait) -> Holder<'_> { Holder { value: x } }
fn distinct<'a, 'b>(x: &'a dyn Trait, _: &'b dyn Trait) -> &'a dyn Trait { x }
fn main() {
    let (a, b) = (1, 2);
    println!("{}", identity(&a).v() + wrap(&a).value.v() + distinct(&a, &b).v());
}
"""},
        "prints": ["3"],
    },
    {
        "name": "borrowed-objects-stay-independent",
        "fixes": ["f9992a19d"],
        "files": {MAIN: """trait Trait {}
impl Trait for i32 {}
fn wrong<'a, 'b>(_: &'a dyn Trait, y: &'b dyn Trait) -> &'a dyn Trait { y }
fn main() { let (a, b) = (1, 2); let _ = wrong(&a, &b); }
"""},
        "error": "lifetime may not live long enough",
    },
    {
        "name": "borrowed-object-is-not-static",
        "fixes": ["f9992a19d"],
        "files": {MAIN: """trait Trait {}
impl Trait for i32 {}
fn static_object(x: &dyn Trait) -> &(dyn Trait + 'static) { x }
fn main() { let a = 1; let _ = static_object(&a); }
"""},
        "error": "lifetime may not live long enough",
    },
    {
        "name": "elided-object-through-a-call",
        "fixes": ["0f3715fa6"],
        "files": {MAIN: """trait Trait {}
impl Trait for i32 {}
struct Holder<'a> { value: &'a (dyn Trait + 'a) }
fn wrap(x: &dyn Trait) -> Holder<'_> { Holder { value: x } }
fn local_call<'a>() -> Holder<'a> {
    let value = 1;
    wrap(&value)
}
fn main() { let _ = local_call().value; }
"""},
        "error": "E0515",
    },
    {
        "name": "column-counts-scalars",
        "fixes": ["b96b59521"],
        "files": {MAIN: "const A: u32 = { /* x */ column!() };\nconst B: u32 = { \t column!() };\nconst C: u32 = { /* 界 */ column!() };\nconst D: u32 = { /* é */ column!() };\nmacro_rules! nested { () => { column!() } }\nmacro_rules! outer { () => { nested!() } }\nconst E: u32 = { /* 界é */\t outer!() };\nfn main() { println!(\"{A} {B} {C} {D} {E}\"); }\n"},
        "prints": ["26 20 26 27 29"],
    },
    {
        "name": "caller-location-counts-display-width",
        "fixes": ["b7f288aa6"],
        "files": {MAIN: """use std::panic::Location;
#[track_caller]
fn here() -> &'static Location<'static> { Location::caller() }
fn main() {
    let a = /* 界 */ here();
    let b = /* x */ here();
    println!("{} {}", a.column(), b.column());
    println!("{}", a.file().ends_with("main.rs"));
}
"""},
        "prints": ["22 21", "true"],
    },
    {
        "name": "track-caller-through-function-pointers",
        "fixes": ["b7f288aa6", "2bf0ffa8f"],
        "files": {MAIN: """use std::panic::Location;
#[track_caller]
fn tracked() -> u32 { Location::caller().line() }
trait T { #[track_caller] fn m(&self) -> u32 { Location::caller().line() } }
impl T for u8 {}
fn main() {
    println!("{}", tracked());
    let p: fn() -> u32 = tracked;
    println!("{}", p());
    println!("{}", 0u8.m());
    let d: &dyn T = &0u8;
    println!("{}", d.m() > 0);
}
"""},
        "prints": ["7", "3", "10", "true"],
    },
    {
        "name": "empty-array-promoted-through-iter-mut",
        "fixes": ["bec070945"],
        "files": {MAIN: "fn f<'a>() -> std::slice::IterMut<'a, u8> { [].iter_mut() }\nfn main() { println!(\"{}\", f().count()); }\n"},
        "prints": ["0"],
    },
    {
        "name": "implied-bounds-through-fields",
        "fixes": ["bec070945"],
        "files": {MAIN: """use std::marker::PhantomData;
struct Iter<'a, K> { _m: PhantomData<(&'a K, u8)> }
struct Keys<'a, K> { inner: Iter<'a, K> }
impl<'a, K> Keys<'a, K> {
    fn get(&self) -> &Iter<'a, K> { &self.inner }
}
fn main() {
    let k = Keys::<u8> { inner: Iter { _m: PhantomData } };
    let _ = k.get();
    println!("ok");
}
"""},
        "prints": ["ok"],
    },
    {
        "name": "self-of-an-impl-for-dyn",
        "fixes": ["bec070945"],
        "files": {MAIN: """trait Tr { fn n(&self) -> u32; }
impl Tr for u32 { fn n(&self) -> u32 { *self } }
impl dyn Tr {
    fn same(this: &Self) -> &Self { this }
    fn id(&self) -> u32 { Self::same(self).n() }
}
fn main() { let b: Box<dyn Tr> = Box::new(5u32); println!("{}", b.id()); }
"""},
        "prints": ["5"],
    },
]

CASES.append(
    {
        "name": "cdylib-allocates",
        "fixes": ["f715001f4", "e26811f32"],
        "files": {
            "Cargo.toml": '[workspace]\nmembers = ["exported", "app"]\nresolver = "2"\n',
            "exported/Cargo.toml": '[package]\nname = "exported"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\ncrate-type = ["cdylib"]\n',
            "exported/src/lib.rs": '#[unsafe(no_mangle)]\npub extern "C" fn exported_len(n: usize) -> usize {\n    let v: Vec<u64> = (0..n as u64).collect();\n    let s = format!("{v:?}");\n    s.len()\n}\n',
            "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n',
            "app/src/main.rs": '''use std::ffi::{CString, c_char, c_int, c_void};
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}
fn main() {
    let dir = std::env::current_exe().unwrap();
    let dir = dir.parent().unwrap();
    let name = if cfg!(target_os = "macos") { "libexported.dylib" } else { "libexported.so" };
    let path = CString::new(dir.join(name).to_str().unwrap()).unwrap();
    let symbol = CString::new("exported_len").unwrap();
    unsafe {
        let library = dlopen(path.as_ptr(), 2);
        assert!(!library.is_null(), "dlopen");
        let f = dlsym(library, symbol.as_ptr());
        assert!(!f.is_null(), "dlsym");
        let f: extern "C" fn(usize) -> usize = std::mem::transmute(f);
        println!("{}", f(4));
    }
}
''',
        },
        "program": "app",
        "steps": [{"prints": ["12"]}, {"replay": True, "prints": ["12"]}],
    }
)
CASES.append(
    {
        "name": "a-required-feature-of-a-dependency",
        "fixes": ["c69fe89a6"],
        "files": {
            "Cargo.toml": '[workspace]\nmembers = ["dep", "hostdep", "app"]\ndefault-members = ["app"]\nresolver = "2"\n',
            "dep/Cargo.toml": '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nextra = []\n',
            "dep/src/lib.rs": '#[cfg(feature = "extra")]\npub const ON: bool = true;\n#[cfg(not(feature = "extra"))]\npub const ON: bool = false;\n',
            "hostdep/Cargo.toml": '[package]\nname = "hostdep"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nextra = []\n',
            "hostdep/src/lib.rs": '#[cfg(feature = "extra")]\npub const ON: bool = true;\n#[cfg(not(feature = "extra"))]\npub const ON: bool = false;\n',
            "app/Cargo.toml": '''[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
dep = { path = "../dep" }

[build-dependencies]
hostdep = { path = "../hostdep", features = ["extra"] }

[[bin]]
name = "app"
path = "src/main.rs"

[[bin]]
name = "needs"
path = "src/needs.rs"
required-features = ["dep/extra"]

[[bin]]
name = "host-needs"
path = "src/host.rs"
required-features = ["hostdep/extra"]
''',
            "app/build.rs": 'fn main() { println!("cargo::rustc-env=HOST_ON={}", hostdep::ON); }\n',
            "app/src/main.rs": 'fn main() { println!("{} {}", dep::ON, env!("HOST_ON")); }\n',
            "app/src/needs.rs": 'fn main() { println!("target {} {}", dep::ON, env!("HOST_ON")); }\n',
            "app/src/host.rs": 'fn main() { println!("host {} {}", dep::ON, env!("HOST_ON")); }\n',
        },
        "program": "app",
        "steps": [
            {"prints": ["false true"], "absent": ["needs"]},
            {"program": "host-needs", "prints": ["host false true"], "absent": ["needs"]},
            {"args": ["--bin", "needs"], "error": "dep/extra"},
            {"args": ["--features", "dep/extra"], "program": "needs", "prints": ["target true true"]},
            {"prints": ["false true"], "absent": ["needs"]},
        ],
    }
)

CASES.append(
    {
        "name": "a-test-sees-the-library-beside-its-bin",
        "fixes": [],
        "files": {
            "Cargo.toml": '[package]\nname = "both"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
            "src/lib.rs": "pub fn answer() -> u32 { 42 }\n",
            "src/main.rs": 'fn main() { println!("{}", both::answer()); }\n',
            "tests/it.rs": '#[test]\nfn sees_the_library() {\n    assert_eq!(both::answer(), 42);\n    assert!(!env!("CARGO_BIN_EXE_both").is_empty());\n}\n',
        },
        "test": "sees_the_library",
    }
)
CASES.append(
    {
        "name": "a-script-searches-where-its-build-dependencys-script-said",
        "fixes": ["c502dffd6"],
        "files": {
            "Cargo.toml": '[workspace]\nmembers = ["dep", "app"]\nresolver = "2"\n',
            "dep/Cargo.toml": '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n',
            "dep/build.rs": 'fn main() {\n    let out = std::env::var("OUT_DIR").unwrap();\n    println!("cargo::rustc-link-search=native={out}/found");\n}\n',
            "dep/src/lib.rs": "",
            "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n[build-dependencies]\ndep = { path = "../dep" }\n',
            "app/build.rs": '''fn main() {
    let var = if cfg!(target_os = "macos") { "DYLD_FALLBACK_LIBRARY_PATH" } else { "LD_LIBRARY_PATH" };
    let search = std::env::var(var).unwrap();
    assert!(search.contains("/found"), "{var} is {search}");
    println!("cargo::rustc-env=SEEN=yes");
    println!("cargo::rerun-if-changed=build.rs");
}
''',
            "app/src/main.rs": 'fn main() { println!("{}", env!("SEEN")); }\n',
        },
        "program": "app",
        "prints": ["yes"],
    }
)
CASES.append(
    {
        # Found building blaze itself with `debug = 0`: rustc_type_ir's `TypeFlags` and
        # `PathsToNested`, two bitflags of different widths, put two values under one store key.
        "name": "two-derived-clones-with-no-debug-info",
        "program": "two_clones",
        "fixes": [],
        "files": {
            "Cargo.toml": '[package]\nname = "two_clones"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n\n[profile.dev]\ndebug = 0\n',
            "src/main.rs": """#[derive(Clone, Copy)]
pub struct Wide(u32);
#[derive(Clone, Copy)]
pub struct Narrow(u8);
#[derive(Clone)]
pub struct Holds(Wide);
#[derive(Clone)]
pub struct AlsoHolds(Narrow);
fn main() {
    let a = Holds(Wide(7)).clone();
    let b = AlsoHolds(Narrow(5)).clone();
    println!("{} {}", (a.0).0, (b.0).0);
}
""",
        },
        "prints": ["7 5"],
    }
)
CASES.append(
    {
        # upstream's main made a polled-again future panic in the IR and not in the walk of its
        # references, which refused every program with an `async fn`.
        "name": "an-async-fn-and-polling-it-again",
        "fixes": [],
        "files": {MAIN: """use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

async fn inner(n: u32) -> u32 {
    n + 1
}

async fn outer() -> u32 {
    inner(1).await + inner(2).await
}

fn main() {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(outer());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => println!("{value}"),
        Poll::Pending => println!("pending"),
    }
    std::panic::set_hook(Box::new(|_| {}));
    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = future.as_mut().poll(&mut context);
    }));
    let said = again.err().and_then(|it| it.downcast_ref::<&str>().map(|it| it.to_string()));
    println!("{said:?}");
}
"""},
        "prints": ["5", 'Some("`async fn` resumed after completion")'],
    }
)


# Both crates compile distinct C symbols into an archive named `collision`. A
# successful unique-name control proves the C toolchain and both wrappers work
# before changing only the second archive's name; restoring it checks invalidation.
CASES.append({
    "name": "packages-own-same-named-native-archives",
    "fixes": [],
    "files": {
        "Cargo.toml": '''[package]
name = "native_archives"
version = "0.1.0"
edition = "2024"
[workspace]
members = ["package_a", "package_b"]
[dependencies]
package_a = { path = "package_a" }
package_b = { path = "package_b" }
''',
        MAIN: '''fn main() {
    let a = package_a::value();
    let b = package_b::value();
    println!("{a} {b} {}", a + b);
}
''',
        **{
            f"package_{name}/{path}": content
            for name, value in (("a", 11), ("b", 22))
            for path, content in {
                "Cargo.toml": f'''[package]
name = "package_{name}"
version = "0.1.0"
edition = "2024"
[build-dependencies]
cc = "=1.4.3"
''',
                "build.rs": f'''fn main() {{
    cc::Build::new().file("answer.c").compile("{"collision" if name == "a" else "separate"}");
}}
''',
                "answer.c": f"int package_{name}_value(void) {{ return {value}; }}\n",
                "src/lib.rs": f'''unsafe extern "C" {{ fn package_{name}_value() -> i32; }}
pub fn value() -> i32 {{ unsafe {{ package_{name}_value() }} }}
''',
            }.items()
        },
    },
    "program": "native_archives",
    "steps": [
        {"prints": ["11 22 33"]},
        {"files": {"package_b/build.rs": 'fn main() { cc::Build::new().file("answer.c").compile("collision"); }\n'},
         "prints": ["11 22 33"]},
        {"files": {"package_b/build.rs": 'fn main() { cc::Build::new().file("answer.c").compile("separate"); }\n'},
         "prints": ["11 22 33"]},
    ],
})


# Remove each claimed cause in the same daemon, then restore it. A rejection alone does not
# prove duplicate-name checking, and a macro that emits no impl must change method lookup.
for case in CASES:
    if case["name"] == "raw-duplicate-import":
        original = case["files"][MAIN]
        distinct = original.replace("use b::x;", "use b::x as other;")
        case["steps"] = [
            {"error": "E0252"},
            {"files": {MAIN: distinct}, "prints": []},
            {"files": {MAIN: original}, "error": "E0252"},
        ]
    if case["name"] == "attribute-impl-in-function":
        name = "app/src/main.rs"
        original = case["files"][name]
        without_impl = original.replace("impl Trait for u32 { fn value(&self) -> u16 { 2 } }", "")
        case["steps"] = [
            {"prints": ["2"]},
            {"files": {name: without_impl}, "error": "E0599"},
            {"files": {name: original}, "prints": ["2"]},
        ]
