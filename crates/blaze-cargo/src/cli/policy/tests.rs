use super::*;

fn read(words: &[&str]) -> Result<(Start, Asked), String> {
    let mut words = words.iter().map(OsString::from).collect();
    let start = take(&mut words)?;
    let mut asked = super::super::args::parse(words).map_err(|it| format!("{it:?}"))?;
    apply(&mut asked, start)?;
    Ok((start, asked))
}

#[test]
fn the_versioned_policy_reaches_each_supported_command_and_cargo_alias() {
    for command in ["build", "b", "test", "t", "status", "stop"] {
        let (start, asked) = read(&[CONNECT_ONLY, command]).unwrap();
        assert_eq!(start, Start::ConnectOnly);
        if let Asked::Cargo(it) = asked {
            assert_eq!(it.daemon_start, Start::ConnectOnly);
            assert!(!it.args.iter().any(|it| it.contains("daemon-policy")));
        }
        let (start, asked) = read(&[command]).unwrap();
        assert_eq!(start, Start::IfMissing);
        assert_eq!(start.passive(), Start::Never);
        if let Asked::Cargo(it) = asked {
            assert_eq!(it.daemon_start, Start::IfMissing);
        }
    }
    let words = super::super::words(
        ["cargo-blaze", "blaze", CONNECT_ONLY, "test"].into_iter().map(OsString::from),
    );
    let mut words = words;
    assert_eq!(take(&mut words).unwrap(), Start::ConnectOnly);
    assert_eq!(words, [OsString::from("test")]);
    assert!(matches!(read(&[CONNECT_ONLY, "status", "--json"]).unwrap().1, Asked::Status(_)));
}

#[test]
fn unknown_versions_misplaced_policy_and_unsupported_commands_fail_before_dispatch() {
    for words in [
        vec!["--daemon-policy=connect-only-v2", "stop"],
        vec!["--daemon-policy", "stop"],
        vec!["stop", CONNECT_ONLY],
        vec!["status", CONNECT_ONLY],
        vec!["build", CONNECT_ONLY],
        vec![CONNECT_ONLY, CONNECT_ONLY, "stop"],
    ] {
        assert!(read(&words).is_err(), "{words:?}");
    }
    for command in [
        "check",
        "run",
        "bench",
        "doc",
        "lock",
        "serve",
        "daemon",
        "explore",
        "debug",
        "reach",
        "codegen-inputs",
        "cache",
        "fetch-url",
        "clean",
        "fmt",
        "pprof",
    ] {
        assert!(read(&[CONNECT_ONLY, command]).is_err(), "{command}");
    }
    // Test arguments stay byte-for-byte the program's; they cannot change the client policy.
    let (_, Asked::Cargo(it)) = read(&[CONNECT_ONLY, "test", "--", CONNECT_ONLY]).unwrap() else {
        panic!("not a test command");
    };
    assert_eq!(it.program, [OsString::from(CONNECT_ONLY)]);
    assert_eq!(it.daemon_start.passive(), Start::ConnectOnly);
}

#[test]
fn the_launcher_never_infers_policy_support_in_a_different_release() {
    for first in [CONNECT_ONLY, "--daemon-policy=connect-only-v2"] {
        let words = [OsString::from(first), OsString::from("stop")];
        let error = super::super::allow_release_handoff(&words, "historical").unwrap_err();
        assert!(error.contains("historical") && error.contains("unverified"), "{error}");
    }
    let misplaced = [OsString::from("status"), OsString::from(CONNECT_ONLY)];
    assert!(super::super::allow_release_handoff(&misplaced, "historical").is_err());
    let program = [OsString::from("test"), OsString::from("--"), OsString::from(CONNECT_ONLY)];
    assert!(super::super::allow_release_handoff(&program, "historical").is_ok());
    assert!(super::super::allow_release_handoff(&[OsString::from("build")], "historical").is_ok());
}
