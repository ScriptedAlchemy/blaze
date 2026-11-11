//! The real fetch entry point in fresh process homes, with the build checkout visible or
//! hidden by bubblewrap. Run explicitly in the Linux library-source acceptance queue.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use blaze_core::wait::{Clock as _, Deadline, Monotonic, Waited};

const CHILD: &str = "BLAZE_TEST_LIBRARY_FETCH";
const TEST: &str = "toolchain::library::fetch_tests::clean_home_preserves_checkout_precedence_and_selected_release";

#[test]
#[ignore = "Linux clean-home acceptance: uses bubblewrap and archives the pinned library"]
fn clean_home_preserves_checkout_precedence_and_selected_release() {
    if std::env::var_os(CHILD).is_some() {
        crate::release::for_tests();
        assert!(super::found().is_err(), "the home must have no installed library");
        let fetched = super::fetch().unwrap();
        assert_eq!(fetched.dir, super::own().unwrap());
        assert_eq!(super::digest_of(&fetched.dir).unwrap(), super::digest());
        assert_eq!(fetched.commit, super::built_commit());
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let checkout = super::checkout().canonicalize().unwrap();
    let mirror = work.path().join("mirror");
    let release = mirror.join("v0.0.0-dev");
    std::fs::create_dir_all(&release).unwrap();
    let archive =
        std::fs::File::create(release.join(super::archive_name(super::digest()))).unwrap();
    let xz = lzma_rust2::XzWriter::new(archive, Default::default()).unwrap();
    let mut archive = tar::Builder::new(xz);
    archive.append_dir_all("library", &checkout).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    unavailable.set_nonblocking(true).unwrap();
    let exe = std::env::current_exe().unwrap();
    for hidden in [false, true] {
        let name = if hidden { "selected-release" } else { "matching-checkout" };
        let home = work.path().join(name);
        std::fs::create_dir(&home).unwrap();
        let mut command = if hidden {
            let mut wrapped = Command::new("bwrap");
            wrapped
                .args(["--die-with-parent", "--ro-bind", "/", "/", "--tmpfs"])
                .arg(&checkout)
                .arg("--bind")
                .arg(work.path())
                .arg(work.path())
                .arg("--")
                .arg(&exe);
            wrapped
        } else {
            Command::new(&exe)
        };
        command
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("BLAZE_HOME", &home)
            .env("HOME", &home)
            .env_remove(super::VAR)
            .env_remove("BLAZE_LIBRARY_FROM");
        if hidden {
            command.env("BLAZE_RELEASES", &mirror);
        } else {
            command.env("BLAZE_RELEASES", format!("https://{}", unavailable.local_addr().unwrap()));
        }
        checked(&mut command, &home.join("fetch.log"));
        assert_eq!(
            super::digest_of(&home.join("library").join(super::digest()).join("library")).unwrap(),
            super::digest()
        );
        println!("clean home {name}: fetched and verified {}", super::digest());
    }
    assert!(
        matches!(unavailable.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "development build contacted the unavailable mirror"
    );
}

fn checked(command: &mut Command, log: &Path) {
    let output = std::fs::File::create(log).unwrap();
    let mut child = command
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .spawn()
        .unwrap();
    let clock = Monotonic::new();
    let until = Deadline::after(clock.now(), Duration::from_secs(180));
    let waited = blaze_core::wait::exited(
        "clean-home library fetch child",
        child.id() as _,
        Some(until),
        &clock,
    )
    .unwrap();
    if !matches!(waited, Waited::Ready(())) {
        child.kill().unwrap();
    }
    let status = child.wait().unwrap();
    let said = std::fs::read_to_string(log).unwrap();
    assert!(
        matches!(waited, Waited::Ready(())) && status.success(),
        "{command:?}: {status}\n{said}"
    );
    assert!(said.contains("1 passed; 0 failed"), "{said}");
}
