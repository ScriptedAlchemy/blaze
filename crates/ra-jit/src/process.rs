//! The variables of the process a build is for. `env!` and `option_env!` read them where cargo
//! and the build scripts set nothing, and a client of the daemon sends its own with a request:
//! the daemon's are those of whoever started it.

use ide_db::{
    RootDatabase,
    base_db::{Crate, Env, ProcessEnv, process_env_var, set_process_env},
};

/// Between two variables on the line of a request.
const BETWEEN: char = '\u{1f}';

/// The variables of this process. One that is not Unicode is one `env!` can't read either.
pub(crate) fn own() -> Env {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
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
            !krate.env(db).contains_key(key) && process_env_var(db, key) != env.get(key)
        })
        .map(|(krate, _)| krate)
        .collect();
    reading.sort_unstable();
    reading.dedup();
    set_process_env(db, env);
    reading
}

#[cfg(test)]
mod tests {
    use super::*;

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
