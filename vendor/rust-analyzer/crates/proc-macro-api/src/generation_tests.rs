//! Exercises production generation handle ownership through the protocol client.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
};

use crate::{MacroDylib, ProcMacroClient, ServerError, process::ProcessExit};

struct ExitCounter(Arc<AtomicUsize>);
impl ProcessExit for ExitCounter {
    fn exit_err(&mut self) -> Option<ServerError> { None }
}
impl Drop for ExitCounter {
    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::SeqCst); }
}

#[test]
fn failed_snapshot_copy_leaves_no_empty_file() {
    let dir = stdx::tempfile::NamedTempDir::new("generation-error-test").unwrap();
    let missing = paths::AbsPathBuf::assert_utf8(dir.path().join("missing"));
    assert!(crate::LibrarySnapshot::copy(&missing).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let unreadable = paths::AbsPathBuf::assert_utf8(dir.path().to_owned());
    assert!(crate::LibrarySnapshot::copy(&unreadable).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn thirty_generations_retire_only_after_the_last_consumer() {
    let live = Arc::new(AtomicUsize::new(0));
    let count = live.clone();
    let path = paths::AbsPathBuf::assert_utf8(
        std::env::temp_dir().join(format!("macro-generation-test-{}", std::process::id())),
    );
    std::fs::write(&path, b"generation bytes").unwrap();
    let client = ProcMacroClient::with_generation_io_channels(&path, move |_| {
        let (client, server) = UnixStream::pair()?;
        count.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            use crate::legacy_protocol::msg::{Request, Response};
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) != 0 {
                let request: Request = serde_json::from_str(&line).unwrap();
                let response = match request {
                    Request::ApiVersionCheck {} => Response::ApiVersionCheck(0),
                    Request::ListMacros { dylib_path } => {
                        assert!(std::fs::read(dylib_path).is_ok());
                        Response::ListMacros(Ok(vec![("Macro".into(), crate::ProcMacroKind::CustomDerive)]))
                    }
                    _ => panic!("unexpected request"),
                };
                writeln!(writer, "{}", serde_json::to_string(&response).unwrap()).unwrap();
                line.clear();
            }
        });
        Ok((
            Box::new(ExitCounter(count.clone())),
            Box::new(client.try_clone()?),
            Box::new(BufReader::new(client)),
        ))
    }, None, 1).unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 1);
    for _ in 0..30 {
        let handles = client.load_dylib(MacroDylib::new(path.clone())).unwrap();
        let snapshot = handles[0].dylib_path.clone();
        let held = handles[0].clone();
        assert_eq!(live.load(Ordering::SeqCst), 2);
        drop(handles);
        assert_eq!(live.load(Ordering::SeqCst), 2, "last consumer still owns its server");
        assert!(std::fs::read(snapshot.as_path()).is_ok());
        drop(held);
        assert_eq!(live.load(Ordering::SeqCst), 1, "retired generation server was released");
        assert!(!std::path::Path::new(snapshot.as_str()).exists(), "retired snapshot was deleted");
    }
    drop(client);
    assert_eq!(live.load(Ordering::SeqCst), 0);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn actual_server_process_is_killed_and_waited_when_generation_retires() {
    use std::os::unix::fs::PermissionsExt;
    let dir = stdx::tempfile::NamedTempDir::new("generation-process-test").unwrap();
    let script = dir.path().join("server");
    let pids = dir.path().join("pids");
    std::fs::write(&script, format!(
        "#!/bin/sh\necho $$ >> '{}'\nwhile IFS= read -r request; do\ncase \"$request\" in\n*ApiVersionCheck*) echo '{{\"ApiVersionCheck\":0}}';;\n*ListMacros*) echo '{{\"ListMacros\":{{\"Ok\":[[\"Macro\",\"CustomDerive\"]]}}}}';;\nesac\ndone\n",
        pids.display(),
    )).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = dir.path().join("library");
    std::fs::write(&source, b"generation").unwrap();
    let server = paths::AbsPathBuf::assert_utf8(script);
    let source = paths::AbsPathBuf::assert_utf8(source);
    let client = ProcMacroClient::spawn(
        &server, std::iter::empty::<(&str, &Option<String>)>(), None, 1,
    ).unwrap();
    let alive = |pid: &str| {
        std::process::Command::new("ps").args(["-p", pid, "-o", "pid="])
            .output().unwrap().stdout.iter().any(|byte| byte.is_ascii_digit())
    };
    for _ in 0..30 {
        let handles = client.load_dylib(MacroDylib::new(source.clone())).unwrap();
        let pid = std::fs::read_to_string(&pids).unwrap().lines().last().unwrap().to_owned();
        let snapshot = handles[0].dylib_path.clone();
        let consumer = handles[0].clone();
        drop(handles);
        assert!(alive(&pid));
        assert!(std::fs::read(snapshot.as_path()).is_ok());
        drop(consumer);
        assert!(!alive(&pid), "server must be killed and reaped synchronously");
        assert!(!std::path::Path::new(snapshot.as_str()).exists());
    }
    let baseline = std::fs::read_to_string(pids).unwrap().lines().next().unwrap().to_owned();
    assert!(alive(&baseline));
    drop(client);
    assert!(!alive(&baseline));
}
