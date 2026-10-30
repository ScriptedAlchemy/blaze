use super::*;
use std::fs;

fn write(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn bindings(roots: &[(&str, &Path)]) -> Closure {
    Closure {
        roots: roots
            .iter()
            .map(|(name, path)| Root {
                name: (*name).to_owned(),
                tree: Tree::capture(path).unwrap(),
            })
            .collect(),
    }
}

fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
#[cfg(unix)]
fn exported_plan_requires_admission_and_keeps_source_and_producer_inputs() {
    use blaze_wire::native::{LinkMode, LinkPlan};
    use ide_db::{RootDatabase, base_db::salsa::Setter as _};
    use std::{os::unix::ffi::OsStringExt as _, process::Command};
    use test_fixture::WithFixture as _;

    const CHILD: &str = "BLAZE_TEST_NATIVE_LINK_PLAN";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_inputs::replay::tests::exported_plan_requires_admission_and_keeps_source_and_producer_inputs",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("RUST_BACKTRACE", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }

    let _native = crate::process::enter_native(crate::process::Native::default());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    write(&out, "libgenerated.a", b"!<arch>\n");
    let raw_dir = out.join(if cfg!(target_os = "linux") {
        OsString::from_vec(b"native-\xff space".to_vec())
    } else {
        OsString::from("native-λ space")
    });
    write(&raw_dir, "libproduced.a", b"!<arch>\n");
    let object = write(&raw_dir, "producer.o", b"\x7fELFproducer");
    let unbound = write(dir.path(), "outside.exports", b"_generated\n");
    let (mut db, _) = RootDatabase::with_single_file(&format!(
        r#"
//- /main.rs crate:app cfg:target_vendor=apple,target_arch=aarch64 env:OUT_DIR={}
#[link(name = "generated", kind = "static")]
unsafe extern "C" {{ fn generated(); }}
fn main() {{}}
"#,
        out.display()
    ));
    ide_db::base_db::set_process_env(&mut db, Default::default());
    let krate = db.test_crate();
    let program = [hir::Crate::from(krate)];
    let target = "aarch64-apple-darwin";
    let path = dir.path().join("link-plan.cbor");
    let closure = bindings(&[("producer/out", &out)]);
    set_replay_bindings(Some(&closure.encode_bindings())).unwrap();
    let search = format!("cargo:rustc-link-search=native={}\n", out.display());
    fs::write(
        dir.path().join("output"),
        format!("{search}cargo:rustc-link-arg=-Wl,-exported_symbols_list,{}\n", unbound.display()),
    )
    .unwrap();
    let error = crate::link::write_plan(&db, &program, None, target, true, &path).unwrap_err();
    assert_eq!(error.to_string(), format!("unbound native input {}", unbound.display()));
    assert!(!path.exists(), "rejected native inputs must not publish a link plan");

    fs::write(dir.path().join("output"), search).unwrap();
    crate::link::write_plan(&db, &program, None, target, true, &path).unwrap();
    let source = LinkPlan::decode(&fs::read(&path).unwrap()).unwrap();
    let mut search_arg = OsString::from("-L");
    search_arg.push(&out);
    assert_eq!(source.target, target);
    assert_eq!(source.mode, LinkMode::Shared);
    assert_eq!(source.before, words(&["-arch", "arm64"]));
    assert_eq!(
        source.after,
        [
            vec![search_arg.clone()],
            words(&["-lgenerated", "-dynamiclib", "-Wl,-dead_strip", "-nodefaultlibs"])
        ]
        .concat()
    );
    fs::remove_file(&path).unwrap();

    let secret = "native-plan-private-environment-value";
    let native_secret = OsString::from_vec(b"private-environment-\xfe".to_vec());
    let mut link_arg = OsString::from("link-arg=");
    link_arg.push(&object);
    crate::native_inputs::set_native_units(
        [(
            0,
            crate::native_inputs::NativeUnit {
                env: [
                    ("PRIVATE_TOKEN".into(), secret.into()),
                    ("PRIVATE_NATIVE".into(), native_secret.clone()),
                ]
                .into(),
                flags: vec![
                    "-L".into(),
                    out.as_os_str().to_owned(),
                    "-L".into(),
                    raw_dir.as_os_str().to_owned(),
                    "-lstatic=produced".into(),
                    "-C".into(),
                    link_arg,
                ],
            },
        )]
        .into(),
    )
    .unwrap();
    let mut extra = krate.extra_data(&db).clone();
    extra.blaze_native_unit = Some(0);
    krate.set_extra_data(&mut db).to(extra);
    crate::link::write_plan(&db, &program, None, target, false, &path).unwrap();
    let encoded = fs::read(&path).unwrap();
    let produced = LinkPlan::decode(&encoded).unwrap();
    let mut raw_search = OsString::from("-L");
    raw_search.push(&raw_dir);
    assert_eq!(produced.target, target);
    assert_eq!(produced.mode, LinkMode::Executable);
    assert_eq!(produced.before, source.before);
    assert_eq!(
        produced.after,
        [
            vec![search_arg, raw_search],
            words(&["-lgenerated", "-lproduced"]),
            vec![object.into_os_string()],
            words(&["-Wl,-dead_strip", "-nodefaultlibs"]),
        ]
        .concat()
    );
    for value in [secret.as_bytes(), native_secret.as_encoded_bytes()] {
        assert!(!encoded.windows(value.len()).any(|bytes| bytes == value));
    }
}

#[test]
fn library_search_uses_directory_order_and_all_command_line_search_flags() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    let selected = write(&first, "libchoice.a", b"!<arch>\nfirst");
    let ignored = write(&second, "libchoice.so", b"\x7fELFsecond");
    let closure = bindings(&[("source", dir.path())]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector.arguments(words(&["-lchoice", "-Lfirst", "-L", "second"])).unwrap();
    assert!(collector.seen.contains(&selected));
    assert!(!collector.seen.contains(&ignored));
}

#[test]
fn default_search_skips_proven_missing_candidates_but_never_assumes_unbound_absence() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let populated = dir.path().join("populated");
    let selected = write(&populated, "libchoice.so", b"\x7fELFselected");
    let closure = bindings(&[("linker/libraries/0", &empty), ("linker/libraries/1", &populated)]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector.arguments(words(&["-lchoice"])).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([selected.clone()]));

    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    let error = collector.arguments(words(&["-Lunbound", "-lchoice"])).unwrap_err();
    assert!(error.to_string().contains("unbound native input"), "{error:#}");
    assert!(collector.seen.is_empty());

    let closure = bindings(&[("linker/libraries/0", &populated)]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector.search.push(dir.path().join("unbound-later"));
    collector.arguments(words(&["-lchoice"])).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([selected]));
}

#[test]
fn static_mode_is_scoped_by_push_pop_and_verbatim_names_are_not_prefixed() {
    let dir = tempfile::tempdir().unwrap();
    let shared = write(dir.path(), "libchoice.so", b"\x7fELFshared");
    let archive = write(dir.path(), "libchoice.a", b"!<arch>\nstatic");
    let exact = write(dir.path(), "odd-name.object", b"\x7fELFexact");
    let closure = bindings(&[("linker/libraries/0", dir.path())]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector
        .arguments(words(&[
            "--push-state",
            "-Bstatic",
            "-lchoice",
            "--pop-state",
            "-lchoice",
            "-l:odd-name.object",
        ]))
        .unwrap();
    assert_eq!(collector.seen, BTreeSet::from([shared, archive, exact]));
    assert!(
        Collector::new(&closure, dir.path().to_owned(), false)
            .arguments(words(&["--pop-state"]))
            .is_err()
    );
    assert!(
        Collector::new(&closure, dir.path().to_owned(), false)
            .arguments(words(&["--push-state"]))
            .is_err()
    );
}

#[test]
fn linker_script_group_input_include_and_library_references_admit_each_actual_file() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "objects/first.o", b"\x7fELFfirst");
    let b = write(dir.path(), "objects/second.o", b"\x7fELFsecond");
    let library = write(dir.path(), "lib/libextra.a", b"!<arch>\nextra");
    let nested = write(dir.path(), "lib/nested.ld", b"INPUT ( objects/second.o )\n");
    let script = write(
        dir.path(),
        "main.ld",
        b"/* ld-generated libc scripts use these forms. */\nOUTPUT_FORMAT(elf64-x86-64)\nGROUP ( objects/first.o AS_NEEDED ( -lextra ) )\nINCLUDE nested.ld\n",
    );
    let closure = bindings(&[("source", dir.path())]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector.arguments(words(&["-Llib", "-T", "main.ld"])).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([a, b, library, nested, script]));
}

#[test]
fn linker_script_inputs_search_the_script_directory_before_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let libraries = dir.path().join("libraries");
    let cwd = dir.path().join("compiler-output");
    fs::create_dir(&cwd).unwrap();
    let selected = write(&libraries, "runtime.so.1", b"\x7fELFselected");
    let script = write(&libraries, "runtime.so", b"GROUP ( runtime.so.1 )\n");
    let args = vec![script.as_os_str().to_owned()];

    let closure = bindings(&[("linker/libraries/0", &libraries)]);
    let mut collector = Collector::new(&closure, cwd.clone(), false);
    collector.arguments(args.clone()).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([script.clone(), selected.clone()]));

    let ignored = write(&cwd, "runtime.so.1", b"\x7fELFwrong-directory");
    let closure = bindings(&[("linker/libraries/0", &libraries), ("source", &cwd)]);
    let mut collector = Collector::new(&closure, cwd, false);
    collector.arguments(args).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([script, selected]));
    assert!(!collector.seen.contains(&ignored));
}

#[test]
fn selected_file_mutations_and_unbound_script_inputs_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let object = write(dir.path(), "selected.o", b"\x7fELForiginal");
    let closure = bindings(&[("source", dir.path())]);
    fs::write(&object, b"\x7fELFmutation").unwrap();
    let error = Collector::new(&closure, dir.path().to_owned(), false)
        .arguments(words(&["selected.o"]))
        .unwrap_err();
    assert!(error.to_string().contains("unbound native input"), "{error:#}");

    let script = write(dir.path(), "only-script.ld", b"INPUT ( outside.o )\n");
    let closure = bindings(&[("script", &script)]);
    let error = Collector::new(&closure, dir.path().to_owned(), false)
        .arguments(words(&["only-script.ld"]))
        .unwrap_err();
    assert!(error.to_string().contains("unbound native input"), "{error:#}");
}

#[test]
fn recursive_linker_scripts_are_not_mistaken_for_already_admitted_inputs() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.ld", b"INCLUDE b.ld\n");
    write(dir.path(), "b.ld", b"INCLUDE a.ld\n");
    let closure = bindings(&[("source", dir.path())]);
    let error = Collector::new(&closure, dir.path().to_owned(), false)
        .arguments(words(&["a.ld"]))
        .unwrap_err();
    assert!(error.to_string().contains("native input cycle"), "{error:#}");
}

fn thin_archive(member: &[u8]) -> Vec<u8> {
    let mut names = member.to_vec();
    names.extend_from_slice(b"/\n");
    if names.len() % 2 != 0 {
        names.push(b'\n');
    }
    let mut archive =
        format!("!<thin>\n{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", "//", "", "", "", "", names.len())
            .into_bytes();
    archive.extend_from_slice(&names);
    archive.extend_from_slice(b"/0              0           0     0     644     8         `\n");
    archive
}

#[test]
fn thin_archive_members_are_admitted_and_revalidated_separately_from_archive_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write(dir.path(), "libthin.a", &thin_archive(b"objects/member.o"));
    let member = write(dir.path(), "objects/member.o", b"\x7fELFmember");
    let closure = bindings(&[("source", dir.path())]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector.arguments(words(&["libthin.a"])).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([archive, member.clone()]));
    fs::write(member, b"\x7fELFedited").unwrap();
    assert!(
        Collector::new(&closure, dir.path().to_owned(), false)
            .arguments(words(&["libthin.a"]))
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn response_files_preserve_native_non_unicode_library_names_and_nested_quoting() {
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join(OsString::from_vec(b"libraw-\xff.a".to_vec()));
    fs::write(&library, b"!<arch>\nraw").unwrap();
    let outer = write(dir.path(), "outer.rsp", b"@'inner response.rsp'\n");
    let inner = write(dir.path(), "inner response.rsp", b"-L. '-l:libraw-\xff.a'\n");
    let closure = bindings(&[("source", dir.path())]);
    let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
    collector.arguments(words(&["@outer.rsp"])).unwrap();
    assert_eq!(collector.seen, BTreeSet::from([library, outer, inner]));
}

#[test]
fn driver_forwarded_response_files_follow_the_linkers_response_grammar() {
    let dir = tempfile::tempdir().unwrap();
    let library = write(dir.path(), "libchoice.a", b"!<arch>\nselected");
    let response = write(dir.path(), "link.rsp", b"-L. -lchoice\n");
    let closure = bindings(&[("source", dir.path())]);
    for args in [words(&["-Wl,@link.rsp"]), words(&["-Xlinker", "@link.rsp"])] {
        let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
        collector.arguments(args).unwrap();
        assert_eq!(collector.seen, BTreeSet::from([library.clone(), response.clone()]));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn admitted_library_order_matches_an_actual_native_link() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&OsStr]| {
        let output = Command::new("cc").args(args).current_dir(dir.path()).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    for (name, value) in [("first", 7), ("second", 19)] {
        let source = write(
            dir.path(),
            &format!("{name}/choice.c"),
            format!("int choice(void) {{ return {value}; }}\n").as_bytes(),
        );
        let library = source.parent().unwrap().join("libchoice.so");
        run(&[
            OsStr::new("-shared"),
            OsStr::new("-fPIC"),
            source.as_os_str(),
            OsStr::new("-o"),
            library.as_os_str(),
        ]);
    }
    let main = write(
        dir.path(),
        "main.c",
        b"extern int choice(void); int main(void) { return choice(); }\n",
    );
    let closure = bindings(&[("source", dir.path())]);
    for (first, second, expected) in [("first", "second", 7), ("second", "first", 19)] {
        let search = format!("-L{first}");
        let later = format!("-L{second}");
        let rpath = format!("-Wl,-rpath,{}", dir.path().join(first).display());
        let binary = dir.path().join("result");
        run(&[
            main.as_os_str(),
            OsStr::new("-lchoice"),
            OsStr::new(&search),
            OsStr::new(&later),
            OsStr::new(&rpath),
            OsStr::new("-o"),
            binary.as_os_str(),
        ]);
        assert_eq!(Command::new(&binary).status().unwrap().code(), Some(expected));
        let mut collector = Collector::new(&closure, dir.path().to_owned(), false);
        collector.arguments(words(&["-lchoice", &search, &later])).unwrap();
        assert!(collector.seen.contains(&dir.path().join(first).join("libchoice.so")));
        assert!(!collector.seen.contains(&dir.path().join(second).join("libchoice.so")));
    }
}

#[test]
fn library_search_and_explicit_files_preserve_parent_component_semantics() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("a")).unwrap();
    let selected = write(root.path(), "b/libchoice.a", b"!<arch>\nchoice");
    let closure = bindings(&[("source", root.path())]);
    for args in [words(&["-La/../b", "-lchoice"]), words(&["a/../b/libchoice.a"])] {
        let mut collector = Collector::new(&closure, root.path().to_owned(), false);
        collector.arguments(args).unwrap();
        assert!(collector.seen.contains(&root.path().join("a/../b/libchoice.a")));
    }
    assert_eq!(
        fs::read(&selected).unwrap(),
        fs::read(root.path().join("a/../b/libchoice.a")).unwrap()
    );
    assert!(
        Collector::new(&closure, root.path().to_owned(), false)
            .arguments(words(&["-Lmissing/../b", "-lchoice"]))
            .is_err()
    );
}

#[test]
#[cfg(unix)]
fn linker_paths_do_not_lexically_erase_symlink_parent_traversal() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let other = root.path().join("other");
    fs::create_dir_all(other.join("inner")).unwrap();
    let selected = write(&other, "lib/libchoice.a", b"!<arch>\nactual");
    let ignored = write(&source, "lib/libchoice.a", b"!<arch>\nlexical");
    symlink(other.join("inner"), source.join("alias")).unwrap();
    let closure = bindings(&[("source", &source)]);
    assert!(
        Collector::new(&closure, source.clone(), false)
            .arguments(words(&["-Lalias/../lib", "-lchoice"]))
            .is_err()
    );
    let closure = bindings(&[("source", &source), ("native-dependency", &other)]);
    let mut collector = Collector::new(&closure, source.clone(), false);
    collector.arguments(words(&["-Lalias/../lib", "-lchoice"])).unwrap();
    let actual = source.join("alias/../lib/libchoice.a");
    assert!(collector.seen.contains(&actual));
    assert_eq!(fs::read(&actual).unwrap(), fs::read(&selected).unwrap());
    assert_ne!(fs::read(&actual).unwrap(), fs::read(&ignored).unwrap());
}
