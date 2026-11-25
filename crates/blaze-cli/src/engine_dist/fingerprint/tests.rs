use super::*;
use blaze_dist_local::sys::{TempDir, unlink_path, write_file};
use blaze_sys::posix::proc::{change_mode, rename_at};
use blaze_sys::posix::types::mode_t;

fn set_mode(path: &str, mode: mode_t) {
    let mut buf = [0; 4096];
    let name = Path::in_buffer(path.as_bytes(), &mut buf).unwrap();
    let fd = openat(CWD, name, OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0).unwrap();
    change_mode(fd.fd(), mode).unwrap();
}

fn executable(path: &str, bytes: &[u8]) {
    write_file(path, bytes).unwrap();
    set_mode(path, 0o755);
}

#[test]
fn previous_fingerprint_layout_is_a_miss() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TempDir::new("blaze-export-old-fingerprint", alloc).unwrap();
    let dest = dir.join("app");
    let path = dest.as_str();
    let input = [1; 32];
    let bytes = b"old build";
    executable(path, bytes);
    let stamp = sidecar(path, alloc);
    let old = bstd::mem::format_in!(alloc, "{}", blaze_wire::json::hex(&input));
    write_file(stamp.as_str(), old.as_bytes()).unwrap();
    assert!(!matches_output_fingerprint(path, &input, alloc).unwrap(), "the old input-only layout has no output digest");
    let mut unknown = OutputFingerprint { input, len: u64::try_from(bytes.len()).unwrap(), output: ContentId::of(DOMAIN, bytes) }.encode();
    unknown[..4].copy_from_slice(b"BDF0");
    write_file(stamp.as_str(), &unknown).unwrap();
    assert!(!matches_output_fingerprint(path, &input, alloc).unwrap(), "a different magic with the current width");
    write_output_fingerprint(path, &input, bytes, alloc, write_file).unwrap();
    assert!(matches_output_fingerprint(path, &input, alloc).unwrap());
}

#[test]
fn warm_export_verifies_current_bytes_length_mode_and_inputs() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TempDir::new("blaze-export-fingerprint", alloc).unwrap();
    let dest = dir.join("app");
    let path = dest.as_str();
    let input = [1; 32];
    let bytes = [0x23; 19_001];
    let hit = || matches_output_fingerprint(path, &input, alloc).unwrap();
    executable(path, &bytes);
    assert!(!hit());
    write_output_fingerprint(path, &input, &bytes, alloc, write_file).unwrap();
    assert!(hit());
    assert!(!matches_output_fingerprint(path, &[2; 32], alloc).unwrap());
    let mut changed = bytes;
    changed[18_000] ^= 1;
    executable(path, &changed);
    assert!(!hit(), "same length and unchanged first chunk");
    executable(path, &bytes[..901]);
    assert!(!hit());
    executable(path, &bytes);
    assert!(hit());
    set_mode(path, 0o644);
    assert!(!hit());
    unlink_path(path).unwrap();
    assert!(!hit());
    let errno = Errno::Io;
    let failed = write_output_fingerprint(path, &input, &bytes, alloc, |_, _| Err(errno));
    assert_eq!(failed, Err(errno));
}

#[test]
fn interleaved_publishers_cannot_attest_another_exports_bytes() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TempDir::new("blaze-export-interleave", alloc).unwrap();
    let dest = dir.join("app");
    let path = dest.as_str();
    let a = [1; 32];
    let b = [2; 32];
    executable(path, b"first");
    executable(path, b"other");
    write_output_fingerprint(path, &b, b"other", alloc, write_file).unwrap();
    assert!(matches_output_fingerprint(path, &b, alloc).unwrap());
    write_output_fingerprint(path, &a, b"first", alloc, write_file).unwrap();
    assert!(!matches_output_fingerprint(path, &a, alloc).unwrap());
    assert!(!matches_output_fingerprint(path, &b, alloc).unwrap());

    let replacement = dir.join("replacement");
    executable(replacement.as_str(), b"third");
    let stable = inspect(path, 5, |_, _| {
        let mut from = [0; 4096];
        let mut to = [0; 4096];
        rename_at(CWD, Path::in_buffer(replacement.as_bytes(), &mut from)?, CWD, Path::in_buffer(path.as_bytes(), &mut to)?)?;
        Ok(true)
    }).unwrap();
    assert!(!stable, "the name was replaced while its open file was verified");
}

#[test]
fn fifos_at_either_path_are_cache_misses_without_a_writer() {
    let alloc = blaze_own::testing::heap().shared();
    let dir = TempDir::new("blaze-export-fifo", alloc).unwrap();
    let dest = dir.join("app");
    let path = dest.as_str();
    let input = [3; 32];
    let fifo = |path: &str| {
        let mut buf = [0; 4096];
        blaze_sys::posix::proc::make_fifo(Path::in_buffer(path.as_bytes(), &mut buf).unwrap(), 0o600).unwrap();
    };
    write_output_fingerprint(path, &input, b"", alloc, write_file).unwrap();
    fifo(path);
    assert!(!matches_output_fingerprint(path, &input, alloc).unwrap());
    let stamp = sidecar(path, alloc);
    unlink_path(stamp.as_str()).unwrap();
    fifo(stamp.as_str());
    assert!(!matches_output_fingerprint(path, &input, alloc).unwrap());
}
