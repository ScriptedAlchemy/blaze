use std::time::Duration;

use super::*;

fn dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("blaze-build-script-fresh-{test}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
}

fn write_at(path: &Path, secs: u64) {
    std::fs::write(path, "x").unwrap();
    std::fs::File::options().write(true).open(path).unwrap().set_modified(at(secs)).unwrap();
}

fn watching(files: &[&str], env: &[(&str, Option<&str>)]) -> Fingerprint {
    let reads = Reads {
        files: files.iter().map(PathBuf::from).collect(),
        env: env.iter().map(|it| it.0.to_owned()).collect(),
    };
    let value =
        |name: &str| env.iter().find(|it| it.0 == name).and_then(|it| it.1.map(str::to_owned));
    Fingerprint {
        rustc: "rustc 1.99.0".to_owned(),
        rustflags: Vec::new(),
        deps: vec![("build-script-build".to_owned(), 1)],
        local: local(Some(&reads), Path::new("/p"), &value, &|| unreachable!()),
    }
}

#[test]
fn a_script_with_no_run_that_ended_runs() {
    let now = watching(&["a"], &[]);
    assert_eq!(stale(None, &now, Path::new("/nowhere"), &[], Path::new("/p")), Some(Stale::New));
}

#[test]
fn another_fingerprint_runs_it_again() {
    let was = watching(&["a"], &[("V", Some("1"))]);
    let changed = watching(&["a"], &[("V", Some("2"))]);
    let unset = watching(&["a"], &[("V", None)]);
    let dir = dir("other");
    let output = dir.join("output");
    write_at(&output, 10);
    write_at(&dir.join("a"), 5);
    assert_eq!(stale(Some(&was), &was, &output, &[], &dir), None);
    assert_eq!(stale(Some(&was), &changed, &output, &[], &dir), Some(Stale::Other));
    assert_eq!(stale(Some(&was), &unset, &output, &[], &dir), Some(Stale::Other));
    let mut recompiled = was.clone();
    recompiled.deps[0].1 = 2;
    assert_eq!(stale(Some(&was), &recompiled, &output, &[], &dir), Some(Stale::Other));
}

#[test]
fn a_saved_output_that_is_gone_runs_it_again() {
    let dir = dir("output");
    let fingerprint = watching(&["a"], &[]);
    write_at(&dir.join("a"), 5);
    let output = dir.join("output");
    assert_eq!(
        stale(Some(&fingerprint), &fingerprint, &output, &[], &dir),
        Some(Stale::Output(output.clone()))
    );
    // So does one that names nothing: its output is its record too.
    let package =
        Fingerprint { local: vec![Local::Package { fingerprint: "1 (a)".into() }], ..fingerprint };
    assert_eq!(stale(Some(&package), &package, &output, &[], &dir), Some(Stale::Output(output)));
}

#[test]
fn files_newer_than_the_run_or_not_there_and_dependencies_made_after_it() {
    let dir = dir("files");
    let output = dir.join("output");
    write_at(&output, 10);
    let fingerprint = watching(&["a", "sub"], &[]);
    write_at(&dir.join("a"), 10);
    assert_eq!(
        stale(Some(&fingerprint), &fingerprint, &output, &[], &dir),
        Some(Stale::Missing(dir.join("sub")))
    );
    std::fs::create_dir(dir.join("sub")).unwrap();
    write_at(&dir.join("sub/b"), 9);
    std::fs::File::options().read(true).open(dir.join("sub")).unwrap().set_modified(at(9)).unwrap();
    // As old as the run is not newer.
    assert_eq!(stale(Some(&fingerprint), &fingerprint, &output, &[], &dir), None);
    write_at(&dir.join("sub/b"), 11);
    assert_eq!(
        stale(Some(&fingerprint), &fingerprint, &output, &[], &dir),
        Some(Stale::Newer(dir.join("sub")))
    );
    write_at(&dir.join("sub/b"), 9);
    let deps = [("native".to_owned(), Some(at(12)))];
    assert_eq!(
        stale(Some(&fingerprint), &fingerprint, &output, &deps, &dir),
        Some(Stale::Dependency("native".to_owned()))
    );
    let deps = [("native".to_owned(), Some(at(10)))];
    assert_eq!(stale(Some(&fingerprint), &fingerprint, &output, &deps, &dir), None);
}

#[test]
fn a_run_that_names_nothing_keeps_what_was_read_before_it() {
    let package_dir = Path::new("/p");
    let nothing = Reads::default();
    let none = |_: &str| None;
    let before = Fingerprint {
        local: local(Some(&nothing), package_dir, &none, &|| "5.000000000s (src/main.rs)".into()),
        ..Fingerprint::default()
    };
    assert_eq!(before.local, [Local::Package { fingerprint: "5.000000000s (src/main.rs)".into() }]);
    assert_eq!(after_run(&before, &nothing, package_dir, &none), before);
    let named = Reads { files: vec![PathBuf::from("/p/build.rs")], env: vec!["V".into()] };
    let after = after_run(&before, &named, package_dir, &|_| Some("1".into()));
    assert_eq!(
        after.local,
        [
            Local::Changed { paths: vec![PathBuf::from("build.rs")] },
            Local::Env { var: "V".into(), val: Some("1".into()) }
        ]
    );
    // The next build reads what that run named: another fingerprint than the one before it,
    // so a script that says other than it did runs once more.
    assert_ne!(after.hash(), before.hash());
}

#[test]
fn the_package_fingerprint_is_the_newest_file_and_its_path() {
    let fingerprint = package_fingerprint(
        Path::new("/p"),
        (at(0) + Duration::from_nanos(5), PathBuf::from("/p/src/a.rs")),
    );
    assert_eq!(fingerprint, "1000000.000000005s (src/a.rs)");
}
