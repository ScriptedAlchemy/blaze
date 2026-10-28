//! Exercises production generation handle ownership through the protocol client.
#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{MacroDylib, ProcMacroClient, ServerError, process::ProcessExit};

struct ExitCounter(Arc<AtomicUsize>);

#[test]
fn poisoned_epochs_report_exit_without_waiting() {
    let path = paths::AbsPathBuf::assert_utf8(std::env::temp_dir().join("poison-test-server"));
    let client = ProcMacroClient::with_generation_io_channels(
        &path,
        |_| {
            let (client, server) = UnixStream::pair()?;
            std::thread::spawn(move || {
                let mut reader = BufReader::new(server.try_clone().unwrap());
                let mut writer = server;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) != 0 {
                    writeln!(
                        writer,
                        "{}",
                        serde_json::to_string(
                            &crate::legacy_protocol::msg::Response::ApiVersionCheck(0),
                        )
                        .unwrap()
                    )
                    .unwrap();
                    line.clear();
                }
            });
            Ok((
                Box::new(ExitCounter(Arc::new(AtomicUsize::new(1)))),
                Box::new(client.try_clone()?),
                Box::new(BufReader::new(client)),
            ))
        },
        None,
        1,
    )
    .unwrap();
    let held = client.epochs.lock().unwrap();
    assert!(client.exited().is_none(), "contended health polling stays nonblocking");
    drop(held);
    let epochs = client.epochs.clone();
    assert!(
        std::thread::spawn(move || {
            let _held = epochs.lock().unwrap();
            panic!("simulate load panic");
        })
        .join()
        .is_err()
    );
    assert!(client.exited().unwrap().message.contains("poisoned"));
}

impl ProcessExit for ExitCounter {
    fn exit_err(&mut self) -> Option<ServerError> {
        None
    }
}
impl Drop for ExitCounter {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[test]
fn failed_snapshot_copy_leaves_no_empty_file() {
    let dir = stdx::tempfile::NamedTempDir::new("generation-error-test").unwrap();
    let missing = paths::AbsPathBuf::assert_utf8(dir.path().join("missing"));
    assert!(crate::LibrarySnapshot::copy(&missing, Some(dir.path())).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let unreadable = paths::AbsPathBuf::assert_utf8(dir.path().to_owned());
    assert!(crate::LibrarySnapshot::copy(&unreadable, Some(dir.path())).is_err());
    let owned = crate::snapshot_dir::worker_dir(dir.path(), std::process::id());
    assert_eq!(std::fs::read_dir(&owned).unwrap().count(), 1, "only owner marker remains");
}

#[test]
fn thirty_generations_retire_only_after_the_last_consumer() {
    let live = Arc::new(AtomicUsize::new(0));
    let count = live.clone();
    let path = paths::AbsPathBuf::assert_utf8(
        std::env::temp_dir().join(format!("macro-generation-test-{}", std::process::id())),
    );
    std::fs::write(&path, b"generation bytes").unwrap();
    let client = ProcMacroClient::with_generation_io_channels(
        &path,
        move |_| {
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
                            let bytes = std::fs::read(dylib_path).unwrap();
                            if bytes.starts_with(b"rejected") {
                                Response::ListMacros(Err("invalid macro library".into()))
                            } else {
                                Response::ListMacros(Ok(vec![(
                                    "Macro".into(),
                                    crate::ProcMacroKind::CustomDerive,
                                )]))
                            }
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
        },
        None,
        1,
    )
    .unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 1);
    let first = client.load_dylib(MacroDylib::new(path.clone())).unwrap();
    let rejected = paths::AbsPathBuf::assert_utf8(
        std::path::Path::new(path.as_str()).with_extension("rejected"),
    );
    std::fs::write(&rejected, b"rejected library").unwrap();
    assert!(client.load_dylib(MacroDylib::new(rejected.clone())).is_err());
    std::fs::write(&rejected, b"fixed library").unwrap();
    let fixed = client.load_dylib(MacroDylib::new(rejected.clone())).unwrap();
    assert!(Arc::ptr_eq(&first[0].pool, &fixed[0].pool), "rejection never registered an image");
    assert_eq!(live.load(Ordering::SeqCst), 1);
    drop(first);
    drop(fixed);
    std::fs::remove_file(&rejected).unwrap();
    for generation in 0..30 {
        std::fs::write(&path, format!("generation {generation}")).unwrap();
        let handles = client.load_dylib(MacroDylib::new(path.clone())).unwrap();
        let snapshot = handles[0].dylib_path.clone();
        let held = handles[0].clone();
        assert_eq!(live.load(Ordering::SeqCst), 1);
        drop(handles);
        assert_eq!(live.load(Ordering::SeqCst), 1, "last consumer still owns its server");
        assert!(std::fs::read(snapshot.as_path()).is_ok());
        drop(held);
        assert_eq!(live.load(Ordering::SeqCst), 0, "retired generation server was released");
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
    let client =
        ProcMacroClient::spawn(&server, std::iter::empty::<(&str, &Option<String>)>(), None, 1)
            .unwrap();
    let alive = |pid: &str| {
        std::process::Command::new("ps")
            .args(["-p", pid, "-o", "pid="])
            .output()
            .unwrap()
            .stdout
            .iter()
            .any(|byte| byte.is_ascii_digit())
    };
    for generation in 0..30 {
        std::fs::write(&source, format!("generation {generation}")).unwrap();
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
    assert!(!alive(&baseline), "first serving pool is not retained as an idle extra");
    drop(client);
    assert!(!alive(&baseline));
}

#[test]
fn distinct_paths_share_epoch_reload_retains_consumers_and_reports_crash() {
    use std::os::unix::fs::PermissionsExt;
    let dir = stdx::tempfile::NamedTempDir::new("epoch-process-test").unwrap();
    let script = dir.path().join("server");
    let pids = dir.path().join("pids");
    std::fs::write(&script, format!(
        "#!/bin/sh\necho $$ >> '{}'\nwhile IFS= read -r request; do\ncase \"$request\" in\n*ApiVersionCheck*) echo '{{\"ApiVersionCheck\":0}}';;\n*ListMacros*) echo '{{\"ListMacros\":{{\"Ok\":[[\"Macro\",\"CustomDerive\"]]}}}}';;\nesac\ndone\n",
        pids.display(),
    )).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = paths::AbsPathBuf::assert_utf8(script);
    let client =
        ProcMacroClient::spawn(&server, std::iter::empty::<(&str, &Option<String>)>(), None, 1)
            .unwrap()
            .with_snapshot_root(Some(dir.path().to_owned()));
    let mut handles = Vec::new();
    for index in 0..20 {
        let path = paths::AbsPathBuf::assert_utf8(dir.path().join(format!("lib{index}")));
        std::fs::write(&path, format!("library {index}")).unwrap();
        handles.extend(client.load_dylib(MacroDylib::new(path)).unwrap());
    }
    assert_eq!(std::fs::read_to_string(&pids).unwrap().lines().count(), 1);
    let same = client
        .load_dylib(MacroDylib::new(handles[0].original_dylib_path.as_ref().clone()))
        .unwrap();
    assert!(Arc::ptr_eq(&same[0].pool, &handles[0].pool));
    assert_eq!(same[0].dylib_path, handles[0].dylib_path, "unchanged reload reuses native image");
    drop(same);
    // Content-addressed producer reloads use a different pathname. An explicit
    // workspace reload boundary must rotate once, not once per new dylib.
    client.begin_load_epoch();
    let moved = paths::AbsPathBuf::assert_utf8(dir.path().join("new-producer-lib"));
    std::fs::write(&moved, b"new producer").unwrap();
    let batch = client.load_dylib(MacroDylib::new(moved.clone())).unwrap();
    assert!(!Arc::ptr_eq(&batch[0].pool, &handles[0].pool));
    let again = client.load_dylib(MacroDylib::new(moved)).unwrap();
    assert!(Arc::ptr_eq(&batch[0].pool, &again[0].pool));
    drop(again);
    drop(batch);
    std::fs::write(handles[0].original_dylib_path.as_path(), b"changed bytes").unwrap();
    let next = client
        .load_dylib(MacroDylib::new(handles[0].original_dylib_path.as_ref().clone()))
        .unwrap();
    assert!(!Arc::ptr_eq(&next[0].pool, &handles[0].pool));
    assert_eq!(std::fs::read_to_string(&pids).unwrap().lines().count(), 3);
    let old_snapshot = handles[0].dylib_path.clone();
    drop(handles);
    assert!(std::fs::metadata(old_snapshot.as_path()).is_err());
    let pid = std::fs::read_to_string(&pids).unwrap().lines().last().unwrap().to_owned();
    assert!(std::process::Command::new("kill").args(["-9", &pid]).status().unwrap().success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while client.exited().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(client.exited().is_some(), "health must observe the serving epoch's crash");
    drop(next);
    drop(client);
    crate::snapshot_dir::remove_after_worker_exit(dir.path(), std::process::id()).unwrap();
    assert!(!crate::snapshot_dir::worker_dir(dir.path(), std::process::id()).exists());
}

#[test]
fn explicit_snapshot_root_rejects_foreign_marker_and_symlink() {
    use std::os::unix::fs::symlink;
    let root = stdx::tempfile::NamedTempDir::new("snapshot-owner-test").unwrap();
    let source = paths::AbsPathBuf::assert_utf8(root.path().join("library"));
    std::fs::write(&source, b"library").unwrap();
    let foreign = stdx::tempfile::NamedTempDir::new("foreign-snapshot-test").unwrap();
    symlink(foreign.path(), root.path().join("macro-snapshots")).unwrap();
    assert!(crate::LibrarySnapshot::copy(&source, Some(root.path())).is_err());
    crate::snapshot_dir::remove_after_worker_exit(root.path(), std::process::id()).unwrap();
    assert_eq!(std::fs::read_dir(foreign.path()).unwrap().count(), 0);
    std::fs::remove_file(root.path().join("macro-snapshots")).unwrap();
    let owned = crate::snapshot_dir::prepare(root.path(), std::process::id()).unwrap();
    std::fs::write(owned.join("owner"), "foreign").unwrap();
    assert!(crate::LibrarySnapshot::copy(&source, Some(root.path())).is_err());
    crate::snapshot_dir::remove_after_worker_exit(root.path(), std::process::id()).unwrap();
    assert!(owned.exists(), "foreign owner is not ours to delete");
}

/// Native fan-out measurement. Fixture compilation is performed by the external Cargo oracle.
#[test]
#[ignore = "needs pinned native fixture dylibs and matching toolchain server"]
fn native_twenty_distinct_dylibs_and_reload() {
    let root = std::path::PathBuf::from(std::env::var_os("PM_EPOCH_FIXTURES").unwrap());
    let server = paths::AbsPathBuf::assert_utf8(std::path::PathBuf::from(
        std::env::var_os("PM_EPOCH_SERVER").unwrap(),
    ));
    let snapshots = stdx::tempfile::NamedTempDir::new("native-epoch-snapshots").unwrap();
    let started = std::time::Instant::now();
    let client =
        ProcMacroClient::spawn(&server, std::iter::empty::<(&str, &Option<String>)>(), None, 1)
            .unwrap()
            .with_snapshot_root(Some(snapshots.path().to_owned()));
    let mut handles = Vec::new();
    for index in 0..20 {
        let path = paths::AbsPathBuf::assert_utf8(root.join(format!("libmacro{index}.dylib")));
        let macros = client.load_dylib(MacroDylib::new(path)).unwrap();
        assert_eq!(macros.len(), 1);
        assert_eq!(macros[0].name(), format!("macro{index}"));
        handles.extend(macros);
    }
    let cold_load_ms = started.elapsed().as_secs_f64() * 1000.0;
    let span = span::Span {
        range: span::TextRange::empty(0.into()),
        anchor: span::SpanAnchor {
            file_id: span::EditionedFileId::new(
                span::FileId::from_raw(0),
                span::Edition::Edition2024,
            ),
            ast_id: span::ROOT_ERASED_FILE_AST_ID,
        },
        ctx: span::SyntaxContext::root(span::Edition::Edition2024),
    };
    let input = tt::TopSubtree::empty(tt::DelimSpan::from_single(span));
    let expand = |mac: &crate::ProcMacro| {
        mac.expand(
            input.view(),
            None,
            vec![],
            span,
            span,
            span,
            root.to_string_lossy().into_owned(),
            None,
        )
        .unwrap()
        .unwrap()
        .to_string()
    };
    for (index, mac) in handles.iter().enumerate() {
        assert_eq!(expand(mac), index.to_string());
    }
    let children = || {
        let output = std::process::Command::new("pgrep")
            .args(["-P", &std::process::id().to_string()])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().lines().map(str::to_owned).collect::<Vec<_>>()
    };
    let measure = |label: &str| {
        let pids = children();
        println!(
            "MEASURE {label} server_count={} pids={pids:?} cold_load_ms={cold_load_ms:.3}",
            pids.len()
        );
        for pid in &pids {
            let output = std::process::Command::new("top")
                .args(["-l", "1", "-pid", pid, "-stats", "pid,command,mem"])
                .output()
                .unwrap();
            println!("FOOTPRINT {label} {}", String::from_utf8_lossy(&output.stdout));
        }
        pids
    };
    assert_eq!(measure("cold20").len(), 1);
    let first = handles[0].original_dylib_path.as_ref().clone();
    let original = std::fs::read(first.as_path()).unwrap();
    std::fs::copy(root.join("libmacro0_v2.dylib"), first.as_path()).unwrap();
    let reloaded = client.load_dylib(MacroDylib::new(first.clone())).unwrap();
    assert_ne!(reloaded[0].content_identity(), handles[0].content_identity());
    assert_eq!(expand(&reloaded[0]), "20");
    assert_eq!(expand(&handles[0]), "0", "old consumer still expands its old native image");
    assert_eq!(measure("reload_old_live").len(), 2);
    let same = client.load_dylib(MacroDylib::new(first.clone())).unwrap();
    assert_eq!(same[0].dylib_path, reloaded[0].dylib_path);
    drop(same);
    drop(handles);
    assert_eq!(measure("reload_retired").len(), 1);
    drop(reloaded);
    assert_eq!(children().len(), 0);
    drop(client);
    crate::snapshot_dir::remove_after_worker_exit(snapshots.path(), std::process::id()).unwrap();
    assert!(!crate::snapshot_dir::worker_dir(snapshots.path(), std::process::id()).exists());
    std::fs::write(first.as_path(), original).unwrap();
}
