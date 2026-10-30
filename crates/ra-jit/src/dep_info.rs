//! rustc's dep-info files, which cargo has it write next to what it builds: rules of make,
//! `target: file file`, then a rule of no files for each file, then comments
//! `# env-dep:NAME=value` for the environment variables the crate read. There is no crate for
//! the format: cargo has its own reader, which is not a library.
//!
//! The escapes are rustc's, which are fewer than make's: a space in a file's name is written
//! `\ `, and nothing else is. `$`, `#` and a backslash anywhere else are themselves, as they
//! are to cargo: a Windows path is full of backslashes, and a directory may be called `$$`.
//! A variable's name and value have a backslash, a newline and a carriage return written as
//! `\\`, `\n` and `\r`, to stay on one line.

/// What a dep-info file says.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct DepInfo {
    /// The files of the first rule, in its order: rustc gives every target the same ones,
    /// and names the root of the crate first.
    pub(crate) files: Vec<String>,
    /// The variables read, with the value each had, `None` for one that was not set.
    pub(crate) env: Vec<(String, Option<String>)>,
}

impl DepInfo {
    /// Collect inputs through source roots rather than VFS allocation indexes, so
    /// generated doctest crates share the ordinary compiler's trace contract.
    pub(crate) fn from_crates(
        db: &crate::RootDatabase,
        krates: &[hir::Crate],
        asked: &str,
    ) -> anyhow::Result<Self> {
        let paths = source_files(db, krates, asked)?;
        let mut env: Vec<_> = db
            .env_read()
            .into_iter()
            .filter(|(krate, _)| {
                krates.iter().any(|it| ide_db::base_db::Crate::from(*it) == *krate)
            })
            .map(|(krate, key)| {
                let value =
                    krate.env(db).get(&key).or_else(|| ide_db::base_db::process_env_var(db, &key));
                (key, value)
            })
            .collect();
        env.sort_unstable();
        env.dedup();
        Ok(Self { files: paths, env })
    }

    /// Reads the text of a dep-info file. What a file cut short has of its last path is not a
    /// path.
    pub(crate) fn parse(text: &str) -> DepInfo {
        let mut info = DepInfo::default();
        let mut ruled = false;
        for line in text.lines() {
            if let Some(variable) = line.strip_prefix("# env-dep:") {
                let (name, value) = match variable.split_once('=') {
                    Some((name, value)) => (name, Some(unescaped_variable(value))),
                    None => (variable, None),
                };
                info.env.push((unescaped_variable(name), value));
                continue;
            }
            if ruled || line.starts_with('#') {
                continue;
            }
            // A target's name is not escaped: its files start after the first `: `, as cargo
            // reads them.
            let Some((_, files)) = line.split_once(": ") else { continue };
            ruled = true;
            info.files = files_of(files);
        }
        info
    }
}

fn source_files(
    db: &crate::RootDatabase,
    krates: &[hir::Crate],
    asked: &str,
) -> anyhow::Result<Vec<String>> {
    use ide_db::base_db::SourceDatabase as _;

    let mut files = Vec::new();
    for &krate in krates {
        let modules = krate.modules(db).into_iter().map(|it| it.definition_source_file_id(db));
        files.extend(modules.map(|it| it.original_file(db).file_id(db)));
        files.extend(
            hir_def::include_macro_invoc(db, krate.into()).iter().map(|(_, file)| file.file_id(db)),
        );
    }
    let named = |id: &vfs::FileId| {
        let root = db.file_source_root(*id).source_root_id(db);
        let root = db.source_root(root).source_root(db);
        Some(root.path_for_file(id)?.as_path()?.to_string())
    };
    let mut paths = Vec::new();
    let included = krates.iter().flat_map(|it| db.included_by((*it).into()));
    for path in files.iter().filter_map(named).chain(included.map(|(path, _)| path.to_string())) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    anyhow::ensure!(!paths.is_empty(), "no file of `{asked}` was loaded");
    Ok(paths)
}

pub(crate) const TRACE_KEY_FIELD: &str = "blaze_replay_trace_key";

static REPLAY_KEY: std::sync::Mutex<Option<[u8; 32]>> = std::sync::Mutex::new(None);

pub(crate) fn replay_key(project: &serde_json::Value) -> anyhow::Result<[u8; 32]> {
    serde_json::from_value(project.get(TRACE_KEY_FIELD).cloned().unwrap_or_default())
        .map_err(|_| anyhow::anyhow!("missing or invalid private replay trace key"))
}

pub(crate) fn ensure_replay_key(project: &mut serde_json::Value) -> anyhow::Result<()> {
    anyhow::ensure!(project.is_object(), "invalid private replay descriptor");
    if project.get(TRACE_KEY_FIELD).is_some() {
        replay_key(project)?;
        return Ok(());
    }
    let mut key = [0; 32];
    getrandom::fill(&mut key).map_err(|_| anyhow::anyhow!("replay trace entropy"))?;
    project[TRACE_KEY_FIELD] = serde_json::json!(key);
    Ok(())
}

pub(crate) fn install_replay_key(project: &serde_json::Value) -> anyhow::Result<()> {
    let key = project.get(TRACE_KEY_FIELD).map(|_| replay_key(project)).transpose()?;
    *crate::lock(&REPLAY_KEY) = key;
    Ok(())
}

pub(crate) fn installed_replay_key() -> Option<[u8; 32]> {
    crate::lock(&REPLAY_KEY).as_ref().copied()
}

/// Private replay traces contain keyed value identities; source-observed values stay in memory.
#[derive(Debug, PartialEq)]
pub(crate) struct ReplayTrace {
    pub(crate) files: Vec<String>,
    pub(crate) env: Vec<(String, Option<[u8; 32]>)>,
}

impl ReplayTrace {
    pub(crate) fn from_crates(
        db: &crate::RootDatabase,
        krates: &[hir::Crate],
        asked: &str,
        key: [u8; 32],
    ) -> anyhow::Result<Self> {
        let files = source_files(db, krates, asked)?;
        let ambient: std::collections::BTreeMap<_, _> = crate::process::native().vars().collect();
        let mut env = Vec::new();
        for (krate, name) in db.env_read() {
            if !krates.iter().any(|it| ide_db::base_db::Crate::from(*it) == krate) {
                continue;
            }
            let declared = krate.env(db);
            let value = if declared.is_non_unicode(&name) {
                let unit =
                    crate::native_inputs::native_unit_for_crate(db, krate)?.ok_or_else(|| {
                        anyhow::anyhow!("missing native environment for raw compiler input")
                    })?;
                let value = unit
                    .env
                    .get(std::ffi::OsStr::new(&name))
                    .filter(|value| value.to_str().is_none())
                    .ok_or_else(|| {
                        anyhow::anyhow!("invalid native environment for raw compiler input")
                    })?;
                Some(value.clone())
            } else if let Some(value) = declared.get(&name) {
                Some(value.into())
            } else {
                ambient.get(std::ffi::OsStr::new(&name)).cloned()
            };
            env.push((name, value.as_deref().map(|value| value_identity(value, key))));
        }
        env.sort_unstable();
        env.dedup();
        Ok(Self { files, env })
    }

    #[cfg(test)]
    pub(crate) fn from_inputs(inputs: &DepInfo, key: [u8; 32]) -> Self {
        let identity = crate::env_identity::Identity::from_key(key);
        Self {
            files: inputs.files.clone(),
            env: inputs
                .env
                .iter()
                .map(|(name, value)| {
                    (name.clone(), value.as_ref().map(|value| identity.digest(value.as_bytes())))
                })
                .collect(),
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        blaze_wire::encode(&(2_u32, self.files.clone(), self.env.clone()))
    }

    pub(crate) fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        // Native path projection can expand legal filenames past the general string
        // budget. This private trace remains bounded by the complete message limit.
        let limits = blaze_wire::Limits {
            string: blaze_wire::Limits::DOCUMENTED.request,
            ..blaze_wire::Limits::DOCUMENTED
        };
        let (version, files, env): (u32, Vec<String>, Vec<(String, Option<[u8; 32]>)>) =
            blaze_wire::decode(bytes, &limits)?;
        anyhow::ensure!(version == 2, "unknown replay input trace version {version}");
        Ok(Self { files, env })
    }

    pub(crate) fn value_matches(
        actual: Option<&[u8; 32]>,
        value: Option<&std::ffi::OsStr>,
        key: [u8; 32],
    ) -> bool {
        let expected = value.map(|value| value_identity(value, key));
        actual.copied() == expected
    }
}

fn value_identity(value: &std::ffi::OsStr, key: [u8; 32]) -> [u8; 32] {
    let identity = crate::env_identity::Identity::from_key(key);
    if let Some(text) = value.to_str() {
        return identity.digest(text.as_bytes());
    }
    let mut bytes = b"\xffblaze-invalid-utf8\0".to_vec();
    #[cfg(unix)]
    bytes.extend_from_slice(b"unix\0");
    #[cfg(windows)]
    bytes.extend_from_slice(b"windows-utf16le\0");
    bytes.extend(crate::process::os_bytes(value));
    identity.digest(&bytes)
}

/// The files of a rule, which are apart by a space that has no backslash before it.
fn files_of(rule: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut file = String::new();
    let mut chars = rule.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                chars.next();
                file.push(' ');
            }
            // The escape of a space the line was cut before.
            '\\' if chars.peek().is_none() => return files,
            ' ' if !file.is_empty() => files.push(std::mem::take(&mut file)),
            ' ' => {}
            c => file.push(c),
        }
    }
    if !file.is_empty() {
        files.push(file);
    }
    files
}

/// `path` as a file of a rule: as rustc writes it.
pub(crate) fn escaped(path: &str) -> String {
    path.replace(' ', "\\ ")
}

/// A variable's name or value as rustc writes it after `# env-dep:`.
pub(crate) fn escaped_variable(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r")
}

/// A variable's name or value as it was before rustc escaped it. A backslash before anything
/// else is kept.
fn unescaped_variable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let escaped = match (c, chars.peek()) {
            ('\\', Some('\\')) => '\\',
            ('\\', Some('n')) => '\n',
            ('\\', Some('r')) => '\r',
            _ => {
                out.push(c);
                continue;
            }
        };
        chars.next();
        out.push(escaped);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(unix)]
    fn replay_collects_operation_native_values_from_real_crate_reads() {
        use ide_db::base_db::{self, Env, SourceDatabase as _};
        use std::os::unix::ffi::OsStringExt as _;
        use test_fixture::WithFixture;

        let (mut db, file) =
            crate::RootDatabase::with_single_file("//- /main.rs crate:app\nfn main() {}\n");
        let file = file.file_id(&db);
        let mut files = vfs::file_set::FileSet::default();
        files.insert(file, vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8("/main.rs".into())));
        let mut change = ide_db::ChangeWithProcMacros::default();
        change.set_roots(vec![base_db::SourceRoot::new_local(files)]);
        db.apply_change(change);
        let krate = base_db::all_crates(&db)[0];
        let key = [7; 32];
        for name in ["RAW", "EMPTY", "ABSENT", "OVERLAY"] {
            db.env_is_read(krate, name);
        }
        db.set_env(krate, Env::from_iter([("OVERLAY".into(), "crate-value".into())]));
        let collect = |raw| {
            let _scope = crate::process::enter_native(crate::process::Native::of([
                ("RAW".into(), std::ffi::OsString::from_vec(vec![raw])),
                ("EMPTY".into(), "".into()),
                ("OVERLAY".into(), "operation-value".into()),
            ]));
            ReplayTrace::from_crates(&db, &[krate.into()], "app", key).unwrap()
        };
        let ff = collect(0xff);
        let fe = collect(0xfe);
        let value =
            |trace: &ReplayTrace, name: &str| trace.env.iter().find(|it| it.0 == name).unwrap().1;
        assert_ne!(value(&ff, "RAW"), value(&fe, "RAW"));
        assert_eq!(value(&ff, "ABSENT"), None);
        assert!(ReplayTrace::value_matches(value(&ff, "EMPTY").as_ref(), Some("".as_ref()), key));
        assert!(ReplayTrace::value_matches(
            value(&ff, "OVERLAY").as_ref(),
            Some("crate-value".as_ref()),
            key
        ));
        assert!(!ReplayTrace::value_matches(
            value(&ff, "OVERLAY").as_ref(),
            Some("operation-value".as_ref()),
            key
        ));
        assert!(!ReplayTrace::value_matches(
            value(&ff, "RAW").as_ref(),
            Some(std::ffi::OsString::from_vec(vec![0xff]).as_os_str()),
            [8; 32]
        ));
        let encoded = ff.encode();
        assert!(!encoded.windows(b"crate-value".len()).any(|it| it == b"crate-value"));
    }
    use super::*;

    #[test]
    #[cfg(unix)]
    fn same_source_instances_collect_distinct_raw_markers() {
        use ide_db::base_db::{self, Env, SourceDatabase as _};
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _, process::Command};

        const CHILD: &str = "BLAZE_TEST_NATIVE_RECEIPT_INSTANCES";
        if std::env::var_os(CHILD).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "dep_info::tests::same_source_instances_collect_distinct_raw_markers",
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
        let mut db = crate::RootDatabase::default();
        let file = vfs::FileId::from_raw(7);
        let mut graph = base_db::CrateGraphBuilder::default();
        let mut ids = Vec::new();
        for id in 0..2 {
            let mut env = Env::default();
            env.set_non_unicode("RAW");
            let krate = graph.add_crate_root(
                file,
                syntax::Edition::Edition2021,
                Some(base_db::CrateDisplayName::from_canonical_name("shared")),
                None,
                Default::default(),
                None,
                env,
                base_db::CrateOrigin::Local { repo: None, name: None },
                Vec::new(),
                base_db::TargetKind::Lib { is_proc_macro: false },
                triomphe::Arc::new(paths::AbsPathBuf::assert_utf8(
                    std::env::current_dir().unwrap(),
                )),
                triomphe::Arc::new(base_db::CrateWorkspaceData {
                    target: Err("fixture".into()),
                    toolchain: None,
                }),
            );
            graph.set_native_unit(krate, Some(id));
            ids.push(krate);
        }
        let mapped = graph.set_in_db(&mut db);
        assert_ne!(mapped[&ids[0]], mapped[&ids[1]]);
        let mut files = vfs::file_set::FileSet::default();
        files.insert(file, vfs::VfsPath::from(paths::AbsPathBuf::assert_utf8("/shared.rs".into())));
        let mut change = ide_db::ChangeWithProcMacros::default();
        change.set_roots(vec![base_db::SourceRoot::new_local(files)]);
        change.change_file(file, Some("pub fn value() {}".into()));
        db.apply_change(change);
        for id in &ids {
            db.env_is_read(mapped[id], "RAW");
        }
        let units: std::collections::BTreeMap<_, _> = [0xff, 0xfe]
            .into_iter()
            .enumerate()
            .map(|(id, byte)| {
                (
                    id as u32,
                    crate::native_inputs::NativeUnit {
                        env: [(
                            "RAW".into(),
                            OsString::from_vec([b"native-value-".as_slice(), &[byte]].concat()),
                        )]
                        .into(),
                        flags: Vec::new(),
                    },
                )
            })
            .collect();
        crate::native_inputs::set_native_units(units.clone()).unwrap();
        let krates: Vec<_> = ids.iter().map(|id| mapped[id].into()).collect();
        let trace = ReplayTrace::from_crates(&db, &krates, "shared", [7; 32]).unwrap();
        let mut expected = vec![
            (
                "RAW".into(),
                Some([
                    219, 147, 173, 155, 170, 214, 121, 78, 123, 253, 55, 204, 97, 28, 214, 8, 17,
                    48, 54, 203, 16, 74, 122, 109, 161, 114, 210, 32, 32, 74, 1, 79,
                ]),
            ),
            (
                "RAW".into(),
                Some([
                    197, 210, 32, 124, 230, 204, 50, 112, 113, 76, 18, 26, 100, 74, 181, 84, 71,
                    104, 99, 90, 210, 192, 83, 54, 90, 95, 158, 143, 103, 214, 27, 117,
                ]),
            ),
        ];
        expected.sort_unstable();
        assert_eq!(trace.env, expected);
        assert_eq!(trace.files, vec!["/shared.rs"]);
        let encoded = trace.encode();
        assert!(!encoded.windows(b"native-value-".len()).any(|it| it == b"native-value-"));
        let mut missing = units.clone();
        missing.remove(&1);
        crate::native_inputs::set_native_units(missing).unwrap();
        assert!(ReplayTrace::from_crates(&db, &krates, "shared", [7; 32]).is_err());
        let mut wrong = units;
        wrong.get_mut(&1).unwrap().env.insert("RAW".into(), "unicode".into());
        crate::native_inputs::set_native_units(wrong).unwrap();
        let error = ReplayTrace::from_crates(&db, &krates, "shared", [7; 32]).unwrap_err();
        assert!(!error.to_string().contains("unicode"));
    }

    #[test]
    #[cfg(unix)]
    fn native_value_identities_distinguish_presence_and_raw_bytes() {
        use std::os::unix::ffi::OsStringExt as _;
        let key = [7; 32];
        let raw_ff = std::ffi::OsString::from_vec(vec![0xff]);
        let raw_fe = std::ffi::OsString::from_vec(vec![0xfe]);
        let ff = value_identity(&raw_ff, key);
        assert_ne!(ff, value_identity(&raw_fe, key));
        assert!(ReplayTrace::value_matches(Some(&ff), Some(&raw_ff), key));
        assert!(!ReplayTrace::value_matches(Some(&ff), Some(&raw_fe), key));
        assert!(!ReplayTrace::value_matches(Some(&ff), Some(&raw_ff), [8; 32]));
        assert_eq!(
            value_identity("é".as_ref(), key),
            [
                0x73, 0x78, 0x19, 0xbb, 0xf9, 0x47, 0xb5, 0x66, 0x28, 0xbb, 0x1c, 0xd8, 0x50, 0x45,
                0x57, 0xe0, 0xe0, 0x5f, 0x8f, 0x0e, 0x1b, 0xd4, 0xc9, 0x0d, 0xaf, 0xe8, 0x49, 0x35,
                0x65, 0x6c, 0x69, 0x99,
            ]
        );
        assert_ne!(value_identity("".as_ref(), key), ff);
        assert!(!ReplayTrace::value_matches(None, Some("".as_ref()), key));
    }

    #[test]
    fn replay_preserves_inputs_that_cargo_dep_info_cannot_represent() {
        let trace = DepInfo {
            files: vec![
                "/source/line\nbreak.rs".into(),
                "/source/carriage\rreturn".into(),
                "/source/trailing\\".into(),
                format!("/source/{}", "long/".repeat(1000)),
            ],
            env: vec![("VALUE".into(), Some("line\nbreak\\\r".into())), ("ABSENT".into(), None)],
        };
        let protected = ReplayTrace::from_inputs(&trace, [7; 32]);
        let encoded = protected.encode();
        assert_eq!(ReplayTrace::decode(&encoded).unwrap(), protected);
        assert!(ReplayTrace::decode(&encoded[..encoded.len() - 1]).is_err());
        let mut newer = encoded;
        newer[1] = 3;
        assert!(ReplayTrace::decode(&newer).unwrap_err().to_string().contains("version 3"));
        assert!(ReplayTrace::decode(b"target: /source/file.rs\n").is_err());
    }

    #[test]
    fn replay_records_keyed_values_without_raw_environment_or_key_material() {
        let token = "not-a-real-replay-environment-token";
        let key = [0x71; 32];
        let inputs = DepInfo {
            files: vec!["/source/lib.rs".into()],
            env: vec![
                ("TOKEN".into(), Some(token.into())),
                ("EMPTY".into(), Some(String::new())),
                ("ABSENT".into(), None),
            ],
        };
        let trace = ReplayTrace::from_inputs(&inputs, key);
        let bytes = trace.encode();
        assert!(!bytes.windows(token.len()).any(|part| part == token.as_bytes()));
        assert!(!bytes.windows(key.len()).any(|part| part == key));
        let decoded = ReplayTrace::decode(&bytes).unwrap();
        assert_eq!(decoded, trace);
        assert!(ReplayTrace::value_matches(decoded.env[0].1.as_ref(), Some(token.as_ref()), key));
        assert!(!ReplayTrace::value_matches(
            decoded.env[0].1.as_ref(),
            Some(token.as_ref()),
            [0x72; 32]
        ));
        assert!(!ReplayTrace::value_matches(
            decoded.env[0].1.as_ref(),
            Some("changed".as_ref()),
            key
        ));
        assert!(ReplayTrace::value_matches(decoded.env[1].1.as_ref(), Some("".as_ref()), key));
        assert!(!ReplayTrace::value_matches(decoded.env[1].1.as_ref(), None, key));
        assert!(ReplayTrace::value_matches(decoded.env[2].1.as_ref(), None, key));
        assert!(!ReplayTrace::value_matches(decoded.env[2].1.as_ref(), Some("".as_ref()), key));
    }

    #[test]
    fn replay_key_is_fresh_per_description_and_invalid_transport_is_redacted() {
        let mut first = serde_json::json!({"crates": []});
        let mut second = first.clone();
        ensure_replay_key(&mut first).unwrap();
        let before = replay_key(&first).unwrap();
        ensure_replay_key(&mut first).unwrap();
        assert_eq!(replay_key(&first).unwrap(), before);
        ensure_replay_key(&mut second).unwrap();
        assert_ne!(replay_key(&second).unwrap(), before);
        let malformed = serde_json::json!({TRACE_KEY_FIELD: "not-a-real-private-trace-token"});
        let why = replay_key(&malformed).unwrap_err().to_string();
        assert_eq!(why, "missing or invalid private replay trace key");
        assert!(ensure_replay_key(&mut serde_json::Value::Null).is_err());
        let malformed = blaze_wire::encode(&(
            2_u32,
            vec![String::from("/source/lib.rs")],
            vec![(String::from("TOKEN"), Some([1_u8; 31]))],
        ));
        assert!(ReplayTrace::decode(&malformed).is_err());
    }

    #[test]
    fn spaces_are_escaped_and_nothing_else() {
        // What rustc 1.99 writes of `/w/a b $$ #c 'q'/m n.rs`, read with `RJ` set to
        // `x\y`, a newline and `z`.
        let text = "/w/a b $$ #c 'q'/out dir.d: /w/a\\ b\\ $$\\ #c\\ 'q'/m\\ n.rs /w/gen.rs\n\n\
                    /w/a b $$ #c 'q'/out dir: /w/a\\ b\\ $$\\ #c\\ 'q'/m\\ n.rs /w/gen.rs\n\n\
                    /w/a\\ b\\ $$\\ #c\\ 'q'/m\\ n.rs:\n/w/gen.rs:\n\n\
                    # env-dep:RJ=x\\\\y\\nz\n# env-dep:UNSET\n# checksum:abc\n";
        let info = DepInfo::parse(text);
        assert_eq!(info.files, ["/w/a b $$ #c 'q'/m n.rs", "/w/gen.rs"]);
        assert_eq!(
            info.env,
            [("RJ".to_owned(), Some("x\\y\nz".to_owned())), ("UNSET".to_owned(), None)]
        );
    }

    #[test]
    fn a_variable_reads_back() {
        let value = "x\\y\nz\r";
        assert_eq!(escaped_variable(value), "x\\\\y\\nz\\r");
        let info =
            DepInfo::parse(&format!("a: /b.rs\n\n# env-dep:V={}\n", escaped_variable(value)));
        assert_eq!(info.env, [("V".to_owned(), Some(value.to_owned()))]);
    }

    #[test]
    fn spaces_together_and_at_the_ends() {
        let info = DepInfo::parse("t.d: \\ lead a\\ \\ b trail\\  last\n");
        assert_eq!(info.files, [" lead", "a  b", "trail ", "last"]);
    }

    #[test]
    fn a_windows_path_keeps_its_backslashes() {
        let info = DepInfo::parse("C:\\t\\a.d: C:\\w\\my\\ app\\src\\lib.rs C:\\w\\#x\\b.rs\n");
        assert_eq!(info.files, ["C:\\w\\my app\\src\\lib.rs", "C:\\w\\#x\\b.rs"]);
    }

    #[test]
    fn what_is_written_reads_back() {
        let path = "/tmp/sp ace/two  spaces/$$/#/x\\y.rs";
        let info = DepInfo::parse(&format!("a: {} /b.rs\n", escaped(path)));
        assert_eq!(info.files, [path, "/b.rs"]);
    }

    #[test]
    fn empty_or_cut_short() {
        assert_eq!(DepInfo::parse(""), DepInfo::default());
        assert_eq!(DepInfo::parse("/t/a.d:"), DepInfo::default());
        assert_eq!(DepInfo::parse("/t/a.d: src/a.rs src/cut\\").files, ["src/a.rs"]);
    }
}
