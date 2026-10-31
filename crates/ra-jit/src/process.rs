//! The variables of the process a build is for. `env!` and `option_env!` read them where cargo
//! and the build scripts set nothing, and a client of the daemon sends its own with a request:
//! the daemon's are those of whoever started it.

use ide_db::{
    RootDatabase,
    base_db::{
        Crate, Env, ProcessEnv, process_env_is_non_unicode, process_env_var, set_process_env,
    },
};

use std::{cell::RefCell, collections::BTreeMap, ffi::OsString, process::Command};

/// Native process variables at child-process boundaries. Source inputs use `unicode`
/// separately. A vector on the wire preserves non-Unicode keys, unlike a JSON object.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Native(Vec<(OsString, OsString)>);

impl serde::Serialize for Native {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.encoded())
    }
}

impl<'de> serde::Deserialize<'de> for Native {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::decoded(&text).map_err(serde::de::Error::custom)
    }
}

/// The canonical native snapshot schema is `(1, platform, [(key-bytes, value-bytes)])`.
/// Hex is framing for the existing line socket, not the identity/encoding of the snapshot.
type NativeWire = (u8, u8, Vec<(blaze_wire::Bytes, blaze_wire::Bytes)>);

#[cfg(unix)]
pub(crate) fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    value.as_bytes().to_vec()
}

#[cfg(unix)]
pub(crate) fn from_os_bytes(value: Vec<u8>) -> Result<OsString, &'static str> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(OsString::from_vec(value))
}

#[cfg(windows)]
pub(crate) fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt as _;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(windows)]
pub(crate) fn from_os_bytes(value: Vec<u8>) -> Result<OsString, &'static str> {
    use std::os::windows::ffi::OsStringExt as _;
    if value.len() % 2 != 0 {
        return Err("invalid native environment");
    }
    Ok(OsString::from_wide(
        &value.chunks_exact(2).map(|it| u16::from_le_bytes([it[0], it[1]])).collect::<Vec<_>>(),
    ))
}

impl std::fmt::Debug for Native {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Native")
            .field("names", &self.0.iter().map(|it| &it.0).collect::<Vec<_>>())
            .finish()
    }
}

impl Native {
    fn encoded(&self) -> String {
        let wire: NativeWire = (
            1,
            u8::from(cfg!(windows)),
            self.0
                .iter()
                .map(|(k, v)| {
                    (blaze_wire::Bytes::new(os_bytes(k)), blaze_wire::Bytes::new(os_bytes(v)))
                })
                .collect(),
        );
        blaze_wire::encode(&wire).iter().map(|it| format!("{it:02x}")).collect()
    }

    fn decoded(text: &str) -> Result<Self, &'static str> {
        let fail = "invalid native environment";
        if text.len() % 2 != 0 || text.len() > crate::socket::LONGEST_REQUEST as usize {
            return Err(fail);
        }
        let bytes = text
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let nibble = |it| match it {
                    b'0'..=b'9' => Ok(it - b'0'),
                    b'a'..=b'f' => Ok(it - b'a' + 10),
                    _ => Err(fail),
                };
                Ok(nibble(pair[0])? * 16 + nibble(pair[1])?)
            })
            .collect::<Result<Vec<_>, &'static str>>()?;
        let wire: NativeWire =
            blaze_wire::decode(&bytes, &blaze_wire::Limits::DOCUMENTED).map_err(|_| fail)?;
        if wire.0 != 1 || wire.1 != u8::from(cfg!(windows)) {
            return Err(fail);
        }
        let vars = wire
            .2
            .into_iter()
            .map(|(k, v)| {
                if k.bytes.is_empty() {
                    return Err(fail);
                }
                #[cfg(unix)]
                if k.bytes.contains(&0) || v.bytes.contains(&0) || k.bytes.contains(&b'=') {
                    return Err(fail);
                }
                #[cfg(windows)]
                if k.bytes.chunks_exact(2).chain(v.bytes.chunks_exact(2)).any(|it| it == [0, 0]) {
                    return Err(fail);
                }
                Ok((from_os_bytes(k.bytes)?, from_os_bytes(v.bytes)?))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = Self::of(vars.clone());
        if result.0 != vars {
            return Err(fail);
        }
        Ok(result)
    }

    pub(crate) fn of(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self(vars.into_iter().collect::<BTreeMap<_, _>>().into_iter().collect())
    }

    pub(crate) fn own() -> Self {
        Self::of(std::env::vars_os())
    }

    pub(crate) fn vars(&self) -> impl Iterator<Item = (OsString, OsString)> + '_ {
        self.0.iter().cloned()
    }

    pub(crate) fn unicode(&self) -> Env {
        let mut env = Env::default();
        for (key, value) in &self.0 {
            if let Some(key) = key.to_str() {
                match value.to_str() {
                    Some(value) => env.set(key, value),
                    None => env.set_non_unicode(key),
                }
            }
        }
        env
    }

    pub(crate) fn over(&self, vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self::of(self.vars().chain(vars))
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        command.env_clear().envs(self.vars());
    }
}

thread_local! {
    /// The operation running on this thread. Detached children capture it before leaving
    /// the thread, never consult mutable process-global variables.
    static NATIVE: RefCell<Option<Native>> = const { RefCell::new(None) };
}

pub(crate) fn native() -> Native {
    NATIVE.with(|it| it.borrow().clone()).unwrap_or_else(Native::own)
}

pub(crate) struct NativeScope(Option<Native>, std::marker::PhantomData<std::rc::Rc<()>>);

pub(crate) fn enter_native(env: Native) -> NativeScope {
    NativeScope(NATIVE.with(|it| it.replace(Some(env))), std::marker::PhantomData)
}

impl Drop for NativeScope {
    fn drop(&mut self) {
        NATIVE.with(|it| it.replace(self.0.take()));
    }
}

/// The native worker protocol is tagged so existing Unicode socket clients remain readable.
const NATIVE_LINE: &str = "native-env-v1:";

pub(crate) fn encode_native(env: &Native, build: u64) -> String {
    format!("{NATIVE_LINE}{}", serde_json::to_string(&(env, build)).expect("native environment"))
}

pub(crate) fn decode_native(line: &str) -> Result<(Native, u64), &'static str> {
    match line.strip_prefix(NATIVE_LINE) {
        Some(line) => serde_json::from_str(line).map_err(|_| "invalid native request environment"),
        None => {
            let (env, build) = decode_request(line);
            Ok((Native::of(env.iter().map(|(k, v)| (k.into(), v.into()))), build))
        }
    }
}

/// Between two variables on the line of a request.
const BETWEEN: char = '\u{1f}';

/// The variables of this process. One that is not Unicode is one `env!` can't read either.
pub(crate) fn own() -> Env {
    vars().collect()
}

/// The Unicode view used by source inputs, configuration and local identities.
/// Ignore non-Unicode pairs consistently rather than panicking in `env::vars`.
pub(crate) fn vars() -> impl Iterator<Item = (String, String)> {
    unicode(native().0)
}

pub(crate) fn unicode(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> impl Iterator<Item = (String, String)> {
    vars.into_iter()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
}

/// `env` on one line with no tab in it.
pub(crate) fn encode(env: &Env) -> String {
    let mut line = String::new();
    for (key, value) in Vec::from(env.clone()) {
        if !line.is_empty() {
            line.push(BETWEEN);
        }
        escape(&key, &mut line);
        line.push('=');
        escape(&value, &mut line);
    }
    line
}

/// What [`encode`] made `line` from. The name of a variable has no `=` in it.
pub(crate) fn decode(line: &str) -> Env {
    line.split(BETWEEN)
        .filter_map(|it| it.split_once('='))
        .map(|(key, value)| (unescape(key), unescape(value)))
        .collect()
}

/// What [`decode`] makes of the line of a request, without the variable that says which build
/// of the daemon's it is for, and that build: `0` when it says none.
pub(crate) fn decode_request(line: &str) -> (Env, u64) {
    let all = decode(line);
    let build = all.get(crate::cargo::daemon::BUILD).and_then(|it| it.parse().ok());
    let rest = all.iter().filter(|(name, _)| *name != crate::cargo::daemon::BUILD);
    (rest.map(|(name, value)| (name.to_owned(), value.to_owned())).collect(), build.unwrap_or(0))
}

fn escape(text: &str, line: &mut String) {
    for c in text.chars() {
        match c {
            '\\' => line.push_str("\\\\"),
            '\t' => line.push_str("\\t"),
            '\n' => line.push_str("\\n"),
            BETWEEN => line.push_str("\\s"),
            c => line.push(c),
        }
    }
}

fn unescape(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (c, c == '\\').1.then(|| chars.next()).flatten() {
            Some('t') => plain.push('\t'),
            Some('n') => plain.push('\n'),
            Some('s') => plain.push(BETWEEN),
            Some(other) => plain.push(other),
            None => plain.push(c),
        }
    }
    plain
}

/// Whether builds are for a process with other variables than `env`.
pub(crate) fn differs(db: &RootDatabase, env: &Env) -> bool {
    ProcessEnv::try_get(db).is_none_or(|it| it.vars(db) != env)
}

/// Has builds from here on be for a process with the variables `env`. The result is the crates
/// that have asked for a variable which is another now.
pub(crate) fn turn(db: &mut RootDatabase, env: Env) -> Vec<Crate> {
    let mut reading: Vec<Crate> = db
        .env_read()
        .into_iter()
        .filter(|(krate, key)| {
            !krate.env(db).contains_key(key)
                && (process_env_var(db, key) != env.get(key)
                    || process_env_is_non_unicode(db, key) != env.is_non_unicode(key))
        })
        .map(|(krate, _)| krate)
        .collect();
    reading.sort_unstable();
    reading.dedup();
    set_process_env(db, env);
    reading
}

#[cfg(all(test, unix))]
pub(crate) fn printenv_for_test() -> Command {
    let path = std::env::var_os("PATH").expect("test environment has a PATH");
    let program = std::env::split_paths(&path)
        .map(|directory| directory.join("printenv"))
        .find(|program| program.is_file())
        .expect("printenv is available in the test environment");
    Command::new(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_turns_distinguish_absent_non_unicode_and_crate_overrides() {
        use ide_db::base_db::{self, SourceDatabase as _};
        let mut db = RootDatabase::default();
        let mut graph = base_db::CrateGraphBuilder::default();
        let root = graph.add_crate_root(
            vfs::FileId::from_raw(1),
            syntax::Edition::Edition2021,
            None,
            None,
            Default::default(),
            None,
            Env::default(),
            base_db::CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            base_db::TargetKind::Lib { is_proc_macro: false },
            triomphe::Arc::new(paths::AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            triomphe::Arc::new(base_db::CrateWorkspaceData {
                target: Err("fixture".into()),
                toolchain: None,
            }),
        );
        let root = graph.set_in_db(&mut db)[&root];
        db.env_is_read(root, "MAYBE");
        set_process_env(&mut db, Env::default());
        let mut raw = Env::default();
        raw.set_non_unicode("MAYBE");
        assert_eq!(turn(&mut db, raw.clone()), [root]);
        assert!(turn(&mut db, raw.clone()).is_empty());
        assert_eq!(turn(&mut db, Env::default()), [root]);
        let mut overlaid = Env::default();
        overlaid.set("MAYBE", "crate-value");
        db.set_env(root, overlaid);
        assert!(turn(&mut db, raw).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn native_requests_preserve_bytes_absence_and_build_without_logging_values() {
        use std::os::unix::ffi::OsStringExt as _;
        let bad = OsString::from_vec(vec![0xff, b'\n', b'\t']);
        let secret = "c4-native-secret-not-a-real-token";
        let env = Native::of([
            ("TOKEN".into(), secret.into()),
            ("EMPTY".into(), OsString::new()),
            ("BYTE_VALUE".into(), bad.clone()),
            (bad.clone(), "byte-key".into()),
        ]);
        let serialized = serde_json::to_string(&env).unwrap();
        assert_eq!(serde_json::from_str::<Native>(&serialized).unwrap(), env);
        let line = encode_native(&env, 42);
        assert!(!line.contains(['\n', '\t']));
        assert_eq!(decode_native(&line).unwrap(), (env.clone(), 42));
        assert!(!format!("{env:?}").contains(secret));
        assert!(!decode_native("native-env-v1:bad-secret").unwrap_err().contains("bad-secret"));
        assert_eq!(env.unicode().get("EMPTY"), Some(String::new()));
        assert_eq!(env.unicode().get("BYTE_VALUE"), None);
        assert!(env.unicode().is_non_unicode("BYTE_VALUE"));
        let mut command = crate::process::printenv_for_test();
        command.env("DAEMON_ONLY", "must-not-leak");
        env.apply(&mut command);
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert!(!output.stdout.windows(b"DAEMON_ONLY".len()).any(|it| it == b"DAEMON_ONLY"));
        let mut want = b"BYTE_VALUE=".to_vec();
        want.extend(bad.into_vec());
        want.push(b'\n');
        assert!(output.stdout.windows(want.len()).any(|it| it == want));
        let empty = Native::default();
        assert_eq!(decode_native(&encode_native(&empty, 0)).unwrap(), (empty, 0));
        assert_eq!(decode_native("A=1").unwrap().0.unicode().get("A"), Some("1".to_owned()));
    }

    #[test]
    fn native_operation_scopes_restore_and_do_not_cross_threads() {
        let first = Native::of([("INPUT".into(), "first".into())]);
        let second = Native::of([("INPUT".into(), "second".into())]);
        let _first = enter_native(first.clone());
        {
            let _second = enter_native(second.clone());
            assert_eq!(native(), second);
        }
        assert_eq!(native(), first);
        std::thread::spawn(move || {
            let _second = enter_native(second.clone());
            assert_eq!(native(), second);
        })
        .join()
        .unwrap();
        assert_eq!(native(), first);
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_pairs_are_ignored_without_losing_unicode_values() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        let vars = unicode([
            (OsString::from("PLAIN"), OsString::from("value")),
            (OsString::from("EMPTY"), OsString::new()),
            (bad.clone(), OsString::from("bad-key")),
            (OsString::from("BAD_VALUE"), bad),
        ])
        .collect::<Vec<_>>();
        assert_eq!(vars, [("PLAIN".into(), "value".into()), ("EMPTY".into(), String::new())]);
    }

    #[test]
    fn a_line_is_read_back_as_it_was_written() {
        let env: Env = [
            ("PLAIN".to_owned(), "value".to_owned()),
            ("EMPTY".to_owned(), String::new()),
            ("ODD".to_owned(), "a\tb\nc\\d=e\u{1f}f\\t".to_owned()),
        ]
        .into_iter()
        .collect();
        let line = encode(&env);
        assert!(!line.contains(['\t', '\n']));
        assert_eq!(decode(&line), env);
    }

    /// The build a request is for is the daemon's to say, not a variable of the process.
    #[test]
    fn the_build_of_a_request_is_not_a_variable() {
        let mut env: Env = [("A".to_owned(), "1".to_owned())].into_iter().collect();
        assert_eq!(decode_request(&encode(&env)), (env.clone(), 0));
        env.set(crate::cargo::daemon::BUILD, "7");
        let (rest, build) = decode_request(&encode(&env));
        assert_eq!(build, 7);
        assert_eq!(rest, [("A".to_owned(), "1".to_owned())].into_iter().collect());
        assert_eq!(decode_request("").1, 0);
    }
}
