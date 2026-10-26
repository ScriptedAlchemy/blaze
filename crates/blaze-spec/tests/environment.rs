//! The requester's environment: what is refused, what is carried to a build script and what is
//! ignored.

use std::collections::BTreeMap;

use blaze_spec::environment::{self, RA_JIT_CARRIED, RA_JIT_NEUTRAL, REFUSED};

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

#[test]
fn each_refused_variable_is_named() {
    for (name, reason) in REFUSED {
        let name = *name;
        let error = environment::check(&env(&[(name, "x")])).expect_err(name);
        assert_eq!(error.what, name);
        assert_eq!(error.id, reason.entry().id);
        assert_eq!(error.why, reason.entry().why);
        assert!(environment::check(&env(&[("SOME_OTHER", "x")])).is_ok());
    }
    // The ones the task names, among them.
    let names: Vec<&str> = REFUSED.iter().map(|it| it.0).collect();
    for name in ["RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTDOC", "RUSTUP_TOOLCHAIN", "RA_JIT_OTHER_TOOLCHAIN"] {
        assert!(names.contains(&name), "{name}");
    }
}

#[test]
fn an_empty_wrapper_is_none() {
    assert!(environment::check(&env(&[("RUSTC_WRAPPER", "")])).is_ok());
    assert!(environment::check(&env(&[("RUSTC_WORKSPACE_WRAPPER", "")])).is_ok());
    assert!(environment::check(&env(&[("RUSTC", "")])).is_err());
}

#[test]
fn what_changes_nothing_of_a_build_is_ignored() {
    let ignored = env(&[
        ("CARGO_INCREMENTAL", "0"),
        ("CARGO_TARGET_DIR", "/t"),
        ("CARGO_HOME", "/h"),
        ("CARGO_TERM_COLOR", "never"),
        ("CARGO_HTTP_TIMEOUT", "3"),
        ("CARGO_NET_OFFLINE", "true"),
        ("RUST_BACKTRACE", "1"),
        ("TERM", "xterm"),
    ]);
    assert!(environment::check(&ignored).is_ok());
}

#[test]
fn unstable_cargo_settings_in_the_environment_are_refused() {
    for name in ["CARGO_UNSTABLE_BUILD_STD", "CARGO_HOST_RUNNER"] {
        let error = environment::check(&env(&[(name, "x")])).unwrap_err();
        assert_eq!((error.id, error.what.as_str()), ("EnvUnstable", name));
    }
}

#[test]
fn a_ra_jit_variable_is_carried_ignored_or_refused() {
    for name in RA_JIT_CARRIED {
        assert!(environment::check(&env(&[(*name, "x")])).is_ok(), "{name} is carried");
    }
    for (name, why) in RA_JIT_NEUTRAL {
        assert!(!why.is_empty());
        let name = if name.ends_with('_') { format!("{name}ANY") } else { (*name).to_owned() };
        assert!(environment::check(&env(&[(&name, "x")])).is_ok(), "{name} changes nothing");
    }
    for name in ["RA_JIT_REMOTE_PUSH_EVERY", "RA_JIT_FN_CACHE_DIR", "RA_JIT_MEMORY_MAX", "RA_JIT_DUMP_MIR", "RA_JIT_AHEAD_PAUSE_MS", "RA_JIT_LOAD_PAUSE_MS"] {
        assert!(environment::check(&env(&[(name, "1")])).is_ok(), "{name}");
    }
    for name in ["RA_JIT_TIER", "RA_JIT_INLINE", "RA_JIT_WHOLE_BUILD", "RA_JIT_STD_FROM", "RA_JIT_SANDBOX", "RA_JIT_UNKNOWN", "RA_JIT_"] {
        let error = environment::check(&env(&[(name, "1")])).expect_err(name);
        assert_eq!((error.id, error.what.as_str()), ("EnvRaJit", name));
    }
    // Only a name that starts like a neutral one is one.
    assert!(environment::check(&env(&[("RA_JIT_HOMES", "1")])).is_err());
    assert!(environment::check(&env(&[("RA_JIT_REMOTELY", "1")])).is_err());
}

#[test]
fn a_build_script_reads_the_listed_variables_with_the_requesters_values() {
    let triple = "aarch64-apple-darwin";
    let carried = [
        ("CC", "clang"),
        ("CXX", "clang++"),
        ("AR", "ar"),
        ("RANLIB", "ranlib"),
        ("CFLAGS", "-O2"),
        ("CXXFLAGS", "-O2"),
        ("ARFLAGS", "cr"),
        ("CPPFLAGS", "-I/x"),
        ("LDFLAGS", "-L/x"),
        ("CRATE_CC_NO_DEFAULTS", "1"),
        ("CC_ENABLE_DEBUG_OUTPUT", "1"),
        ("CC_KNOWN_WRAPPER_MAP", "a:b"),
        ("SDKROOT", "/sdk"),
        ("MACOSX_DEPLOYMENT_TARGET", "14.0"),
        ("IPHONEOS_DEPLOYMENT_TARGET", "17.0"),
        ("PKG_CONFIG", "pkg-config"),
        ("PKG_CONFIG_PATH", "/p"),
        ("PKG_CONFIG_LIBDIR", "/l"),
        ("PKG_CONFIG_SYSROOT_DIR", "/s"),
        ("PKG_CONFIG_ALLOW_CROSS", "1"),
        ("CC_aarch64_apple_darwin", "clang-a"),
        ("CC_aarch64-apple-darwin", "clang-b"),
        ("CXXFLAGS_aarch64_apple_darwin", "-g"),
        ("HOST_CC", "h"),
        ("TARGET_CC", "t"),
        ("TARGET_AR", "t"),
        ("PKG_CONFIG_PATH_aarch64_apple_darwin", "/q"),
        ("OPENSSL_NO_PKG_CONFIG", "1"),
        ("CPATH", "/c"),
        ("LIBRARY_PATH", "/lp"),
    ];
    let left_out = [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/x"),
        ("RUST_BACKTRACE", "1"),
        ("NUM_JOBS", "8"),
        ("OPENSSL_DIR", "/o"),
        ("CARGO_HOME", "/h"),
        ("CC_x86_64_unknown_linux_gnu", "gcc"),
        ("CC_", "x"),
        ("_DEPLOYMENT_TARGET", "x"),
        ("lower_DEPLOYMENT_TARGET", "x"),
        ("_NO_PKG_CONFIG", "x"),
        ("HOST_RUSTC", "x"),
    ];
    let mut all: Vec<(&str, &str)> = carried.to_vec();
    all.extend(left_out);
    let read = environment::script_env(&env(&all), triple);
    let mut want: Vec<(String, String)> =
        carried.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    want.sort();
    assert_eq!(read, want);
    for (name, _) in left_out {
        assert!(!environment::is_script_var(name, triple), "{name}");
    }
    // The other triple's forms are carried when that is the triple built for.
    assert!(environment::is_script_var("CC_x86_64_unknown_linux_gnu", "x86_64-unknown-linux-gnu"));
}
