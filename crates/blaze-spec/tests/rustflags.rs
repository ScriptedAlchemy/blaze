//! The flags of rustc: each rule of the tables read in its class, every spelling, the cfgs,
//! and cargo's precedence for where the flags come from.

mod common;

use blaze_spec::flags::Class;
use blaze_spec::rustflags::{self, CODEGEN, OTHER, Rustflags};
use blaze_spec::Refusal;

use common::{Fixture, host};

fn read(flags: &[&str]) -> Result<Rustflags, Refusal> {
    rustflags::read(&flags.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>())
}

fn refused(flags: &[&str]) -> Refusal {
    read(flags).expect_err(&format!("{flags:?} is refused"))
}

#[test]
fn every_rule_of_the_tables_is_read_in_its_class() {
    let value = |name: &str| match name {
        "opt-level" => "1",
        "debug-assertions" | "overflow-checks" | "force-frame-pointers" => "yes",
        "panic" => "abort",
        "debuginfo" => "1",
        "split-debuginfo" => "packed",
        "strip" => "symbols",
        _ => "1",
    };
    for rule in CODEGEN {
        assert!(!rule.because.is_empty(), "-C {} has no reason", rule.flag);
        let given = format!("{}={}", rule.flag, value(rule.flag));
        let read = read(&["-C", &given]);
        match rule.class {
            Class::Honoured => {
                let read = read.unwrap_or_else(|e| panic!("-C {given}: {e}"));
                assert_eq!(read.flags.len(), 1, "-C {given}");
            }
            Class::Ignored => assert_eq!(read.unwrap(), Rustflags::default(), "-C {given}"),
            Class::Refused(reason) => {
                let error = read.expect_err("a refused option");
                assert_eq!(error.id, reason.entry().id, "-C {given}");
                assert!(error.what.contains(rule.flag), "{error}");
            }
        }
    }
    for rule in OTHER {
        assert!(!rule.because.is_empty(), "{} has no reason", rule.flag);
        let given = match rule.flag {
            "--cfg" => "a",
            "--cap-lints" => "warn",
            _ => "value",
        };
        let read = read(&[rule.flag, given]);
        match rule.class {
            Class::Honoured => assert_eq!(read.unwrap().flags.len(), 1, "{}", rule.flag),
            Class::Ignored => assert_eq!(read.unwrap(), Rustflags::default(), "{}", rule.flag),
            Class::Refused(reason) => {
                let error = read.expect_err("a refused flag");
                assert_eq!(error.id, reason.entry().id, "{}", rule.flag);
                assert!(error.what.contains(rule.flag), "{error}");
            }
        }
    }
}

#[test]
fn a_codegen_option_is_read_in_every_spelling_and_kept_in_one() {
    for flags in [
        &["-C", "opt-level=3"][..],
        &["-Copt-level=3"],
        &["--codegen", "opt-level=3"],
        &["--codegen=opt-level=3"],
        &["-C", "opt_level=3"],
        &["-O"],
    ] {
        let read = read(flags).unwrap();
        assert_eq!(read.flags, ["-Copt-level=3"], "{flags:?}");
        assert_eq!(read.overrides.opt_level.as_deref(), Some("3"));
    }
    assert_eq!(read(&["-g"]).unwrap().flags, ["-Cdebuginfo=2"]);
    assert_eq!(read(&["-g"]).unwrap().overrides.debug, Some(2));
    // A switch given bare is on.
    assert_eq!(read(&["-C", "debug-assertions"]).unwrap().flags, ["-Cdebug-assertions=yes"]);
    assert_eq!(read(&["-Cdebug_assertions=off"]).unwrap().flags, ["-Cdebug-assertions=no"]);
    assert_eq!(read(&["-Coverflow-checks=true"]).unwrap().overrides.overflow_checks, Some(true));
    assert_eq!(read(&["-Cforce-frame-pointers"]).unwrap().flags, ["-Cforce-frame-pointers=yes"]);
    assert_eq!(read(&["-Cforce-frame-pointers=non-leaf"]).unwrap().flags, ["-Cforce-frame-pointers=non-leaf"]);
    for (flag, level) in [
        ("none", 0),
        ("0", 0),
        ("limited", 1),
        ("line-tables-only", 1),
        ("line-directives-only", 1),
        ("1", 1),
        ("full", 2),
        ("2", 2),
    ] {
        let read = read(&[&format!("-Cdebuginfo={flag}")]).unwrap();
        assert_eq!(read.overrides.debug, Some(level), "{flag}");
    }
    let read = read(&["-Cpanic=abort", "-Cstrip=debuginfo", "-Csplit-debuginfo=unpacked"]).unwrap();
    assert_eq!(read.overrides.panic.as_deref(), Some("abort"));
    assert_eq!(read.overrides.strip.as_deref(), Some("debuginfo"));
    assert_eq!(read.overrides.split_debuginfo.as_deref(), Some("unpacked"));
}

#[test]
fn the_flags_come_back_in_their_order_and_the_last_wins() {
    let read = read(&["-Copt-level=1", "--cfg", "a", "-Copt-level=2", "--cfg=b"]).unwrap();
    assert_eq!(read.flags, ["-Copt-level=1", "--cfg=a", "-Copt-level=2", "--cfg=b"]);
    assert_eq!(read.overrides.opt_level.as_deref(), Some("2"));
    assert_eq!(read.cfgs, ["a", "b"]);
    assert_eq!(read.cfg_flags(), ["--cfg=a", "--cfg=b"]);
}

#[test]
fn a_cfg_is_a_name_or_a_name_and_a_quoted_value() {
    let cfgs = |flags: &[&str]| read(flags).map(|it| it.cfgs);
    assert_eq!(cfgs(&["--cfg", "foo"]).unwrap(), ["foo"]);
    assert_eq!(cfgs(&["--cfg=foo"]).unwrap(), ["foo"]);
    assert_eq!(cfgs(&["--cfg", "feature=\"x\""]).unwrap(), ["feature=\"x\""]);
    assert_eq!(cfgs(&["--cfg=feature = \"x y\""]).unwrap(), ["feature=\"x y\""]);
    // Two different values are two different flags.
    assert_ne!(read(&["--cfg", "a"]).unwrap().flags, read(&["--cfg", "b"]).unwrap().flags);
    assert_ne!(
        read(&["--cfg", "k=\"a\""]).unwrap().flags,
        read(&["--cfg", "k=\"b\""]).unwrap().flags
    );
    for bad in ["a=b", "1a", "a-b", "", "a=\"b", "a=\"b\"c\""] {
        assert_eq!(refused(&["--cfg", bad]).id, "FlagCfg", "{bad}");
    }
    // The cfgs rustc sets itself, which it refuses as a flag.
    for builtin in ["debug_assertions", "unix", "target_os=\"linux\"", "panic=\"abort\"", "test", "sanitizer_cfi"] {
        assert_eq!(refused(&["--cfg", builtin]).id, "FlagCfg", "{builtin}");
    }
}

#[test]
fn link_time_optimization_is_refused_unless_it_is_off() {
    for flags in [
        &["-C", "lto"][..],
        &["-Clto=thin"],
        &["-Clto=fat"],
        &["-C", "lto=yes"],
        &["-Clto=true"],
        &["-Clinker-plugin-lto"],
        &["-C", "linker-plugin-lto=on"],
    ] {
        let error = refused(flags);
        assert_eq!(error.id, "FlagLto", "{flags:?}");
        assert!(error.what.starts_with("-C "), "{error}");
    }
    for flags in [&["-Clto=off"][..], &["-Clto=no"], &["-Clto=n"], &["-Clto=false"]] {
        assert_eq!(read(flags).unwrap(), Rustflags::default(), "{flags:?}");
    }
}

#[test]
fn what_blaze_cannot_honour_is_refused_by_name_with_its_class() {
    let cases: &[(&[&str], &str, &str)] = &[
        (&["-Ctarget-cpu=native"], "FlagCpu", "-C target-cpu=native"),
        (&["-C", "target-feature=+avx2"], "FlagCpu", "-C target-feature=+avx2"),
        (&["-Clink-arg=-fuse-ld=lld"], "FlagLink", "-C link-arg=-fuse-ld=lld"),
        (&["-C", "linker=clang"], "FlagLink", "-C linker=clang"),
        (&["-C", "prefer-dynamic"], "FlagLink", "-C prefer-dynamic"),
        (&["-L", "native=/opt/lib"], "FlagLink", "-L"),
        (&["-lfoo"], "FlagLink", "-lfoo"),
        (&["--remap-path-prefix", "/a=/b"], "FlagRemap", "--remap-path-prefix"),
        (&["--remap-path-prefix=/a=/b"], "FlagRemap", "--remap-path-prefix=/a=/b"),
        (&["-Zshare-generics"], "FlagUnstable", "-Zshare-generics"),
        (&["-Z", "macro-backtrace"], "FlagUnstable", "-Z"),
        (&["-C", "opt-level=s"], "FlagOptSize", "-C opt-level=s"),
        (&["-Copt-level=z"], "FlagOptSize", "-C opt-level=z"),
        (&["-Crelocation-model=static"], "FlagCodegen", "-C relocation-model=static"),
        (&["-C", "llvm-args=-x"], "FlagCodegen", "-C llvm-args=-x"),
        (&["-C", "instrument-coverage"], "FlagCodegen", "-C instrument-coverage"),
        (&["--target", "x86_64-unknown-linux-gnu"], "FlagCargoOwns", "--target"),
        (&["--emit=asm"], "FlagCargoOwns", "--emit=asm"),
        (&["--sysroot", "/x"], "FlagCargoOwns", "--sysroot"),
        (&["--frobnicate"], "FlagUnknown", "--frobnicate"),
        (&["-v"], "FlagUnknown", "-v"),
        (&["main.rs"], "FlagInput", "main.rs"),
    ];
    for (flags, id, what) in cases {
        let error = refused(flags);
        assert_eq!((error.id, error.what.as_str()), (*id, *what), "{flags:?}");
        assert!(!error.why.is_empty());
    }
}

#[test]
fn values_rustc_rejects_are_refused() {
    for flags in [
        &["-Copt-level=9"][..],
        &["-Copt-level"],
        &["-Cdebug-assertions=maybe"],
        &["-Cpanic=immediate-abort"],
        &["-Cpanic"],
        &["-Cdebuginfo=3"],
        &["-Csplit-debuginfo=weird"],
        &["-Cstrip=all"],
        &["-Cforce-frame-pointers=sometimes"],
        &["--cap-lints", "loud"],
        &["--cap-lints"],
        &["-C"],
    ] {
        assert_eq!(refused(flags).id, "FlagValue", "{flags:?}");
    }
}

#[test]
fn lints_and_messages_change_nothing_and_are_left_out() {
    for flags in [
        &["-D", "warnings"][..],
        &["-Dwarnings"],
        &["-A", "unused", "-W", "dead_code", "-F", "x"],
        &["--deny=warnings", "--warn", "x", "--allow", "y", "--forbid=z"],
        &["--check-cfg", "cfg(foo)"],
        &["--check-cfg=cfg(foo)"],
        &["--color", "always"],
        &["--diagnostic-width=80"],
        &["-Ccodegen-units=1", "-Cincremental=/x", "-Cembed-bitcode=no", "-Csave-temps"],
    ] {
        assert_eq!(read(flags).unwrap(), Rustflags::default(), "{flags:?}");
    }
    // The lint flags that bound what a check fails on are kept.
    let read = read(&["--cap-lints", "warn", "--force-warn=unused", "-Dwarnings"]).unwrap();
    assert_eq!(read.flags, ["--cap-lints=warn", "--force-warn=unused"]);
}

/// The words cargo gives rustc in a fixture whose configuration is `config`, with `env`.
fn words(config: &str, env: &[(&str, &str)]) -> Vec<String> {
    let fixture = Fixture::with_config(config);
    let mut variables = fixture.env();
    variables.extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
    let root = fixture.root();
    let config = blaze_manifest::Config::load(&root, variables, &[]).unwrap();
    rustflags::words(&config, &host(false)).unwrap()
}

#[test]
fn flags_come_from_where_cargo_takes_them_from() {
    let config = "[build]\nrustflags = [\"--cfg\", \"from_build\"]\n\n[target.aarch64-apple-darwin]\nrustflags = [\"--cfg\", \"from_triple\"]\n\n[target.'cfg(target_os = \"macos\")']\nrustflags = [\"--cfg\", \"from_cfg\"]\n\n[target.'cfg(target_os = \"linux\")']\nrustflags = [\"--cfg\", \"from_linux\"]\n";
    // The tables of the target, the triple's first and then each of a cfg that matches; not
    // `build.rustflags`, and not a table that does not match.
    assert_eq!(words(config, &[]), ["--cfg", "from_triple", "--cfg", "from_cfg"]);
    // RUSTFLAGS wins over every table, CARGO_ENCODED_RUSTFLAGS over that.
    assert_eq!(words(config, &[("RUSTFLAGS", "--cfg  a  --cfg b")]), ["--cfg", "a", "--cfg", "b"]);
    assert_eq!(
        words(config, &[("RUSTFLAGS", "--cfg a"), ("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}c d")]),
        ["--cfg", "c d"]
    );
    // `build.rustflags` is last.
    assert_eq!(words("[build]\nrustflags = \"--cfg x\"\n", &[]), ["--cfg", "x"]);
    assert_eq!(words("[build]\nrustflags = []\n", &[("CARGO_BUILD_RUSTFLAGS", "--cfg y")]), ["--cfg", "y"]);
    assert!(words("", &[]).is_empty());
}
