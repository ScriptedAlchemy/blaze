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
        use ide_db::base_db::SourceDatabase as _;

        let mut files = Vec::new();
        for &krate in krates {
            let modules = krate.modules(db).into_iter().map(|it| it.definition_source_file_id(db));
            files.extend(modules.map(|it| it.original_file(db).file_id(db)));
            files.extend(
                hir_def::include_macro_invoc(db, krate.into())
                    .iter()
                    .map(|(_, file)| file.file_id(db)),
            );
        }
        let named = |id: &vfs::FileId| {
            let root = db.file_source_root(*id).source_root_id(db);
            let root = db.source_root(root).source_root(db);
            Some(root.path_for_file(id)?.as_path()?.to_string())
        };
        let mut paths = Vec::new();
        let included = krates.iter().flat_map(|it| db.included_by((*it).into()));
        for path in files.iter().filter_map(named).chain(included.map(|(path, _)| path.to_string()))
        {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        anyhow::ensure!(!paths.is_empty(), "no file of `{asked}` was loaded");
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
        value: Option<&str>,
        key: [u8; 32],
    ) -> bool {
        let expected = value
            .map(|value| crate::env_identity::Identity::from_key(key).digest(value.as_bytes()));
        actual.copied() == expected
    }
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
    use super::*;

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
        assert!(ReplayTrace::value_matches(decoded.env[0].1.as_ref(), Some(token), key));
        assert!(!ReplayTrace::value_matches(decoded.env[0].1.as_ref(), Some(token), [0x72; 32]));
        assert!(!ReplayTrace::value_matches(decoded.env[0].1.as_ref(), Some("changed"), key));
        assert!(ReplayTrace::value_matches(decoded.env[1].1.as_ref(), Some(""), key));
        assert!(!ReplayTrace::value_matches(decoded.env[1].1.as_ref(), None, key));
        assert!(ReplayTrace::value_matches(decoded.env[2].1.as_ref(), None, key));
        assert!(!ReplayTrace::value_matches(decoded.env[2].1.as_ref(), Some(""), key));
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
