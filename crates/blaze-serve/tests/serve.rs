//! End-to-end HTTP/1.1 integration tests over real TCP sockets in pure `#![no_std]`.
//!
//! Zero `std::`, zero `Global`, zero third-party deps: uses `blaze-sys` sockets, `blaze-own`
//! allocators, `blaze-sha256`, and `blaze-wire`.

#![no_std]
#![feature(allocator_ext)]

#[path = "../src/store/native_pack.rs"]
mod native_pack;
#[path = "../src/sys/directory.rs"]
mod directory;

use core::ffi::c_void;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use core::time::Duration;

use blaze_own::{Box, String, Vec, format_in};
use blaze_serve::{Config, Server, ShutdownHandle};
use blaze_sys::posix::io::{
    CWD, ClockId, OpenFlags, OwnedFd, PollEvents, PollFd, clock_gettime as read_sys_clock,
    nanosleep, openat, poll as poll_sys, write as write_fd_sys,
};
use blaze_sys::posix::net::{
    AF_INET, MsgFlags, SOCK_STREAM, connect as connect_sys, in_addr, recv as recv_sys,
    send as send_sys, sockaddr_in, sockaddr_storage, socket as socket_sys,
};
use blaze_sys::posix::proc::{
    AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFLNK, S_IFMT, Unlink, stat_at, unlink_at,
};
use blaze_sys::posix::thread::getpid;
use blaze_sys::posix::types::{sa_family_t, socklen_t};
use blaze_sys::{Errno, Fd, Path, Timespec};
use blaze_wire::{Digest, Limits, WireAlloc, WireError, WireString, decode};

// ---------------------------------------------------------------------------------------------
// Minimal `#![no_std]` test OS helpers (threads, temp dirs, TCP client, HTTP response reader).
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[allow(non_camel_case_types)]
type RawThread = usize;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn pthread_create(
        thread: *mut RawThread,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> i32;
    fn pthread_join(thread: RawThread, retval: *mut *mut c_void) -> i32;
}

struct Worker<T> {
    #[cfg(target_os = "linux")]
    handle: RawThread,
    #[cfg(target_os = "macos")]
    handle: blaze_sys::posix::thread::pthread_t,
    result_slot: *mut Option<T>,
    alloc: WireAlloc,
}

struct Trampoline<F, T> {
    func: F,
    result_slot: *mut Option<T>,
    alloc: WireAlloc,
}

impl<T: Send + 'static> Worker<T> {
    fn start<F: FnOnce() -> T + Send + 'static>(func: F, alloc: WireAlloc) -> Self {
        let slot_box = Box::new_in(None::<T>, alloc.clone());
        let (slot_ptr, _) = Box::into_raw_with_allocator(slot_box);
        let payload = Box::new_in(
            Trampoline { func, result_slot: slot_ptr, alloc: alloc.clone() },
            alloc.clone(),
        );
        let (payload_ptr, _) = Box::into_raw_with_allocator(payload);

        extern "C" fn entry<F: FnOnce() -> T + Send + 'static, T: Send + 'static>(
            raw: *mut c_void,
        ) -> *mut c_void {
            // SAFETY: `raw` was produced by `Box::into_raw_with_allocator` with `WireAlloc`.
            let boxed: Box<Trampoline<F, T>, WireAlloc> = unsafe {
                let ptr = raw.cast::<Trampoline<F, T>>();
                let alloc = (*ptr).alloc.clone();
                Box::from_raw_in(ptr, alloc)
            };
            let Trampoline { func, result_slot, .. } = Box::into_inner(boxed);
            let value = func();
            // SAFETY: `result_slot` remains uniquely owned until `Worker::join`.
            unsafe {
                *result_slot = Some(value);
            }
            core::ptr::null_mut()
        }

        #[cfg(target_os = "linux")]
        {
            let mut handle: RawThread = 0;
            // SAFETY: valid thread handle pointer, null default attrs, valid entry + payload.
            let rc = unsafe {
                pthread_create(&raw mut handle, core::ptr::null(), entry::<F, T>, payload_ptr.cast())
            };
            assert_eq!(rc, 0, "pthread_create failed with {rc}");
            Self { handle, result_slot: slot_ptr, alloc }
        }
        #[cfg(target_os = "macos")]
        {
            let mut handle = core::ptr::null_mut();
            // SAFETY: valid thread handle pointer, null default attrs, valid entry + payload.
            let rc = unsafe {
                blaze_sys::posix::thread::pthread_create(
                    &raw mut handle,
                    core::ptr::null(),
                    entry::<F, T>,
                    payload_ptr.cast(),
                )
            };
            assert_eq!(rc, 0, "pthread_create failed with {rc}");
            Self { handle, result_slot: slot_ptr, alloc }
        }
    }

    fn join(self) -> T {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `self.handle` is a live joinable pthread.
            let rc = unsafe { pthread_join(self.handle, core::ptr::null_mut()) };
            assert_eq!(rc, 0, "pthread_join failed with {rc}");
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: `self.handle` is a live joinable pthread.
            let rc = unsafe {
                blaze_sys::macos::cold::pthread_join(self.handle, core::ptr::null_mut())
            };
            assert_eq!(rc, 0, "pthread_join failed with {rc}");
        }
        // SAFETY: thread has joined, so `self.result_slot` is uniquely ours to reclaim.
        let slot_box = unsafe { Box::from_raw_in(self.result_slot, self.alloc) };
        match Box::into_inner(slot_box) {
            Some(val) => val,
            None => panic!("worker thread exited without producing a value"),
        }
    }
}

fn sleep_ms(ms: u64) {
    let ts = Timespec {
        sec: i64::try_from(ms / 1000).unwrap_or(0),
        nsec: i64::try_from((ms % 1000).saturating_mul(1_000_000)).unwrap_or(0),
    };
    let _ = nanosleep(ts);
}

fn now_stamp() -> u64 {
    match read_sys_clock(ClockId::Monotonic) {
        Ok(ts) => {
            let s = u64::try_from(ts.sec).unwrap_or(0);
            let n = u64::try_from(ts.nsec).unwrap_or(0);
            s.saturating_mul(1_000_000_000).saturating_add(n)
        }
        Err(_) => 0,
    }
}

fn with_cpath<T>(path: &str, f: impl FnOnce(Path<'_>) -> Result<T, Errno>) -> Result<T, Errno> {
    let mut buf = [0u8; 4096];
    let cpath = Path::in_buffer(path.as_bytes(), &mut buf)?;
    f(cpath)
}

fn mkdir_one(path: &str) -> Result<(), Errno> {
    with_cpath(path, |cpath| {
        #[cfg(target_os = "linux")]
        {
            blaze_sys::linux::mkdirat(CWD, cpath, 0o755)
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: `Path` keeps its NUL immediately after `bytes()` in the same live
            // backing buffer. `mkdir` reads it and keeps no pointer.
            let rc = unsafe { blaze_sys::macos::cold::mkdir(cpath.bytes().as_ptr().cast(), 0o755) };
            if rc < 0 { Err(blaze_sys::posix::signal::errno()) } else { Ok(()) }
        }
    })
}

fn list_names(dir: &str, alloc: WireAlloc) -> Vec<directory::DirEntry<WireAlloc>, WireAlloc> {
    directory::list_dir(dir, alloc).unwrap_or_else(|error| panic!("listing {dir}: {}", error.what()))
}

fn remove_tree(root: &str, alloc: WireAlloc) {
    let mut stack = Vec::new_in(alloc.clone());
    let mut root_s = String::with_capacity_in(root.len(), alloc.clone());
    root_s.push_str(root);
    stack.push((root_s, false));
    while let Some((dir, ready)) = stack.pop() {
        if ready {
            let _ = with_cpath(dir.as_str(), |c| unlink_at(CWD, c, Unlink::Dir));
            continue;
        }
        let entries = list_names(dir.as_str(), alloc.clone());
        stack.push((dir.clone(), true));
        for entry in entries {
            let child = format_in!(alloc.clone(), "{}/{}", dir.as_str(), entry.name.as_str());
            let is_real_dir = with_cpath(child.as_str(), |c| stat_at(CWD, c, AT_SYMLINK_NOFOLLOW))
                .map(|st| {
                    let kind = st.st_mode & S_IFMT;
                    kind == S_IFDIR && kind != S_IFLNK
                })
                .unwrap_or(false);
            if is_real_dir {
                stack.push((child, false));
            } else {
                let _ = with_cpath(child.as_str(), |c| unlink_at(CWD, c, Unlink::File));
            }
        }
    }
}

struct TempDir {
    path: WireString,
    alloc: WireAlloc,
}

impl TempDir {
    fn new(tag: &str, alloc: WireAlloc) -> Self {
        let pid = getpid();
        for attempt in 0u64..100 {
            let stamp = now_stamp().saturating_add(attempt);
            let path = format_in!(alloc.clone(), "/tmp/blaze-serve-it-{tag}-{pid}-{stamp}");
            if mkdir_one(path.as_str()).is_ok() {
                return Self { path, alloc };
            }
        }
        panic!("could not create temporary directory for {tag}");
    }

    fn path(&self) -> &str {
        self.path.as_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        remove_tree(self.path.as_str(), self.alloc.clone());
    }
}

fn write_file(path: &str, bytes: &[u8]) {
    let fd = match with_cpath(path, |c| {
        openat(
            CWD,
            c,
            OpenFlags::WRITE
                .with(OpenFlags::CREATE)
                .with(OpenFlags::TRUNCATE)
                .with(OpenFlags::CLOSE_ON_EXEC),
            0o644,
        )
    }) {
        Ok(fd) => fd,
        Err(err) => panic!("open {path} failed: {}", err.what()),
    };
    let mut rest = bytes;
    while !rest.is_empty() {
        match write_fd_sys(fd.fd(), rest) {
            Ok(0) => panic!("short write to {path}"),
            Ok(n) => rest = rest.get(n..).unwrap_or(&[]),
            Err(Errno::Intr) => continue,
            Err(err) => panic!("write {path} failed: {}", err.what()),
        }
    }
}

fn make_symlink(target: &str, link: &str) {
    let mut tbuf = [0u8; 4096];
    let mut lbuf = [0u8; 4096];
    let tc = match Path::in_buffer(target.as_bytes(), &mut tbuf) {
        Ok(p) => p,
        Err(e) => panic!("target path: {}", e.what()),
    };
    let lc = match Path::in_buffer(link.as_bytes(), &mut lbuf) {
        Ok(p) => p,
        Err(e) => panic!("link path: {}", e.what()),
    };
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = blaze_sys::linux::symlinkat(tc, CWD, lc) {
            panic!("symlinkat failed: {}", e.what());
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: each `Path` keeps its NUL immediately after `bytes()` in its live
        // backing buffer. `symlink` reads both strings and keeps neither pointer.
        let rc = unsafe {
            blaze_sys::macos::cold::symlink(tc.bytes().as_ptr().cast(), lc.bytes().as_ptr().cast())
        };
        assert_eq!(rc, 0, "symlink failed");
    }
}

/// Blocking TCP client built directly on `blaze-sys`.
struct TcpClient {
    fd: OwnedFd,
}

impl TcpClient {
    fn connect(port: u16) -> Result<Self, Errno> {
        let raw = socket_sys(AF_INET, SOCK_STREAM, 0)?;
        // SAFETY: `socket_sys` returned a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw(raw.raw()) };
        // SAFETY: `sockaddr_storage` is a plain C byte buffer; all zeroes is valid.
        let mut storage: sockaddr_storage = unsafe { core::mem::zeroed() };
        let sin = sockaddr_in {
            #[cfg(target_os = "macos")]
            sin_len: u8::try_from(size_of::<sockaddr_in>()).map_err(|_| Errno::Inval)?,
            sin_family: sa_family_t::try_from(AF_INET).map_err(|_| Errno::Inval)?,
            sin_port: port.to_be(),
            sin_addr: in_addr { s_addr: u32::from_ne_bytes([127, 0, 0, 1]) },
            sin_zero: [0; 8],
        };
        let len = socklen_t::try_from(size_of::<sockaddr_in>()).map_err(|_| Errno::Inval)?;
        // SAFETY: `sockaddr_storage` is larger and at least as aligned as `sockaddr_in`.
        unsafe {
            core::ptr::from_mut(&mut storage).cast::<sockaddr_in>().write(sin);
        }
        connect_sys(fd.fd(), &storage, len)?;
        Ok(Self { fd })
    }

    fn write_all(&mut self, mut data: &[u8]) -> Result<(), Errno> {
        while !data.is_empty() {
            match send_sys(self.fd.fd(), data, MsgFlags::NO_SIGNAL) {
                Ok(0) => return Err(Errno::Pipe),
                Ok(n) => data = data.get(n..).unwrap_or(&[]),
                Err(Errno::Intr) => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn fd(&self) -> Fd {
        self.fd.fd()
    }
}

trait ByteSource {
    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;
}

impl ByteSource for TcpClient {
    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut pfd = [PollFd::new(self.fd(), PollEvents::IN)];
        let timeout = Timespec { sec: 10, nsec: 0 };
        loop {
            let ready = poll_sys(&mut pfd, Some(timeout))?;
            if ready == 0 {
                return Err(Errno::TimedOut);
            }
            match recv_sys(self.fd(), buf, MsgFlags::NONE) {
                Ok(n) => return Ok(n),
                Err(Errno::Intr) => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

struct SliceSource<'a> {
    bytes: &'a [u8],
    pos: usize,
    reset_at_end: bool,
}

impl ByteSource for SliceSource<'_> {
    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.pos >= self.bytes.len() {
            return if self.reset_at_end { Err(Errno::ConnReset) } else { Ok(0) };
        }
        let rem = self.bytes.get(self.pos..).unwrap_or(&[]);
        let take = rem.len().min(buf.len());
        if let (Some(src), Some(dst)) = (rem.get(..take), buf.get_mut(..take)) {
            dst.copy_from_slice(src);
            self.pos = self.pos.saturating_add(take);
            Ok(take)
        } else {
            Ok(0)
        }
    }
}

struct Answer {
    status: u16,
    headers: Vec<(WireString, WireString), WireAlloc>,
    body: Vec<u8, WireAlloc>,
    alloc: WireAlloc,
}

impl Answer {
    fn read(source: &mut impl ByteSource, method: &str, alloc: WireAlloc) -> Result<Self, Errno> {
        let mut raw = Vec::new_in(alloc.clone());
        let mut scratch = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            let n = source.read_chunk(&mut scratch)?;
            if n == 0 {
                return Err(Errno::Pipe);
            }
            if let Some(chunk) = scratch.get(..n) {
                raw.extend_from_slice(chunk);
            }
        };

        let head_bytes = raw.get(..header_end).unwrap_or(&[]);
        let head_str = core::str::from_utf8(head_bytes).map_err(|_| Errno::Inval)?;
        let mut lines = head_str.split("\r\n");
        let status_line = lines.next().ok_or(Errno::Inval)?;
        let status = status_line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or(Errno::Inval)?;

        let mut headers = Vec::new_in(alloc.clone());
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let mut key = String::with_capacity_in(k.len(), alloc.clone());
                for ch in k.chars() {
                    key.push(ch.to_ascii_lowercase());
                }
                let trimmed = v.trim();
                let mut val = String::with_capacity_in(trimmed.len(), alloc.clone());
                val.push_str(trimmed);
                headers.push((key, val));
            }
        }

        let length = if method == "HEAD" || matches!(status, 204 | 304) {
            0usize
        } else {
            let cl = headers
                .iter()
                .find(|(k, _)| k.as_str() == "content-length")
                .map(|(_, v)| v.as_str())
                .ok_or(Errno::Inval)?;
            cl.parse::<usize>().map_err(|_| Errno::Inval)?
        };

        let mut body = Vec::with_capacity_in(length, alloc.clone());
        let after = raw.get(header_end.saturating_add(4)..).unwrap_or(&[]);
        let take_initial = after.len().min(length);
        if let Some(chunk) = after.get(..take_initial) {
            body.extend_from_slice(chunk);
        }
        while body.len() < length {
            let need = length.saturating_sub(body.len()).min(scratch.len());
            let dst = scratch.get_mut(..need).ok_or(Errno::Inval)?;
            let n = source.read_chunk(dst)?;
            if n == 0 {
                return Err(Errno::Pipe);
            }
            if let Some(chunk) = scratch.get(..n) {
                body.extend_from_slice(chunk);
            }
        }

        Ok(Self { status, headers, body, alloc })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.as_str() == name)
            .map(|(_, v)| v.as_str())
    }

    fn error(&self) -> WireError {
        match decode::<WireError>(self.body.as_slice(), &Limits::DOCUMENTED, self.alloc.clone()) {
            Ok(err) => err,
            Err(err) => panic!("decode WireError failed: {err:?}"),
        }
    }
}

struct Served {
    dir: TempDir,
    port: u16,
    stop: Option<ShutdownHandle>,
    worker: Option<Worker<()>>,
    alloc: WireAlloc,
}

impl Served {
    fn start(tag: &str, alloc: WireAlloc) -> Self {
        let dir = TempDir::new(tag, alloc.clone());
        let mut config = Config::at(dir.path(), 0, alloc.clone());
        config.listen = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        config.resources.idle = Duration::from_secs(10);
        let server = match Server::bind(config, alloc.clone()) {
            Ok(s) => s,
            Err(err) => panic!("Server::bind failed: {err}"),
        };
        let port = server.local_addr().port();
        let stop = match server.shutdown_handle() {
            Ok(h) => h,
            Err(err) => panic!("shutdown_handle failed: {err}"),
        };
        let worker = Worker::start(
            move || {
                if let Err(err) = server.run() {
                    panic!("server.run failed: {err}");
                }
            },
            alloc.clone(),
        );
        // Wait briefly for listener readiness (already bound, so first connect succeeds immediately).
        for _ in 0..50 {
            if TcpClient::connect(port).is_ok() {
                break;
            }
            sleep_ms(10);
        }
        Self { dir, port, stop: Some(stop), worker: Some(worker), alloc }
    }

    fn dir(&self) -> &str {
        self.dir.path()
    }

    fn ask(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
        let mut client = match TcpClient::connect(self.port) {
            Ok(c) => c,
            Err(e) => panic!("connect to {} failed: {}", self.port, e.what()),
        };
        let mut head = format_in!(
            self.alloc.clone(),
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n"
        );
        for (name, val) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(val);
            head.push_str("\r\n");
        }
        let cl = format_in!(self.alloc.clone(), "Content-Length: {}\r\n\r\n", body.len());
        head.push_str(cl.as_str());
        assert!(client.write_all(head.as_bytes()).is_ok());
        assert!(client.write_all(body).is_ok());
        match Answer::read(&mut client, method, self.alloc.clone()) {
            Ok(ans) => ans,
            Err(e) => panic!("reading HTTP answer failed: {}", e.what()),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.stop();
        }
        if let Some(worker) = self.worker.take() {
            worker.join();
        }
    }
}

fn digest(bytes: &[u8]) -> Digest {
    blaze_sha256::digest(bytes)
}

fn hex_of(d: &Digest, alloc: WireAlloc) -> WireString {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity_in(64, alloc);
    for &b in d {
        if let Some(&hi) = HEX.get(usize::from(b >> 4)) {
            out.push(char::from(hi));
        }
        if let Some(&lo) = HEX.get(usize::from(b & 0x0f)) {
            out.push(char::from(lo));
        }
    }
    out
}

fn pack_name(bytes: &[u8], alloc: WireAlloc) -> WireString {
    let h = hex_of(&digest(bytes), alloc.clone());
    format_in!(alloc, "packs/{}.pack", h.as_str())
}

fn pack(values: &[(Digest, &[u8])], alloc: WireAlloc) -> native_pack::Pack<WireAlloc> {
    let mut input = Vec::with_capacity_in(values.len(), alloc.clone());
    for (key, bytes) in values {
        let mut owned = Vec::with_capacity_in(bytes.len(), alloc.clone());
        owned.extend_from_slice(bytes);
        input.push(native_pack::Value { key: *key, kind: 0, bytes: owned });
    }
    match native_pack::Pack::of(input, alloc) {
        Ok(p) => p,
        Err(err) => panic!("Pack::of failed: {err}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Integration Tests (all original test function names preserved verbatim).
// ---------------------------------------------------------------------------------------------

#[test]
fn answer_reader_finishes_at_the_http_message_boundary() {
    let alloc = blaze_own::testing::heap().shared();
    let body = b"an error";
    let mut response = Vec::new_in(alloc.clone());
    response.extend_from_slice(b"HTTP/1.1 404 Not Found\r\nContent-Length: 8\r\n\r\n");
    response.extend_from_slice(body);
    let mut src = SliceSource { bytes: response.as_slice(), pos: 0, reset_at_end: true };
    let Ok(answer) = Answer::read(&mut src, "PUT", alloc) else {
        panic!("Answer::read must stop at Content-Length boundary");
    };
    assert_eq!(answer.status, 404);
    assert_eq!(answer.body.as_slice(), body);
}

#[test]
fn answer_reader_rejects_truncated_messages() {
    let alloc = blaze_own::testing::heap().shared();
    for response in [
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n".as_slice(),
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\nabc".as_slice(),
    ] {
        let mut src = SliceSource { bytes: response, pos: 0, reset_at_end: false };
        assert!(Answer::read(&mut src, "PUT", alloc.clone()).is_err());
    }
}

#[test]
fn answer_reader_respects_bodyless_responses() {
    let alloc = blaze_own::testing::heap().shared();
    for (method, status) in [("HEAD", 200u16), ("PUT", 204), ("GET", 304)] {
        let response =
            format_in!(alloc.clone(), "HTTP/1.1 {status} Response\r\nContent-Length: 100\r\n\r\n");
        let mut src = SliceSource { bytes: response.as_bytes(), pos: 0, reset_at_end: false };
        let Ok(answer) = Answer::read(&mut src, method, alloc.clone()) else {
            panic!("bodyless response failed");
        };
        assert!(answer.body.is_empty());
    }
}

#[test]
fn a_blob_is_sent_whole() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("blob-whole", alloc.clone());
    let bytes = pack(&[(digest(b"one"), b"value one"), (digest(b"two"), b"value two")], alloc.clone()).bytes;
    let name = pack_name(bytes.as_slice(), alloc.clone());
    let put_path = format_in!(alloc.clone(), "/v1/blob/{}", name.as_str());
    let put = served.ask("PUT", put_path.as_str(), &[], bytes.as_slice());
    assert_eq!(put.status, 204);

    let whole = served.ask("GET", put_path.as_str(), &[], &[]);
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body.as_slice(), bytes.as_slice());
    let expected_cl = format_in!(alloc.clone(), "{}", bytes.len());
    assert_eq!(whole.header("content-length"), Some(expected_cl.as_str()));

    let missing_path = format_in!(
        alloc.clone(),
        "/v1/blob/packs/{}.pack",
        "0000000000000000000000000000000000000000000000000000000000000000"
    );
    let gone = served.ask("GET", missing_path.as_str(), &[], &[]);
    assert_eq!(gone.status, 404);
    assert_eq!(gone.error().code, 5);
}

#[test]
fn a_write_of_a_wrong_hash_is_refused_and_names_nothing() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("wrong-hash", alloc.clone());
    let bytes = pack(&[(digest(b"one"), b"value one")], alloc.clone()).bytes;
    let name = pack_name(bytes.as_slice(), alloc.clone());
    let other = pack(&[(digest(b"other"), b"another value")], alloc.clone()).bytes;
    let path = format_in!(alloc.clone(), "/v1/blob/{}", name.as_str());

    let wrong = served.ask("PUT", path.as_str(), &[], other.as_slice());
    assert_eq!(wrong.status, 422);
    assert_eq!(wrong.error().code, 9);
    let expected_hex = hex_of(&digest(bytes.as_slice()), alloc.clone());
    assert!(wrong.error().message.as_str().contains(expected_hex.as_str()));
    assert_eq!(served.ask("GET", path.as_str(), &[], &[]).status, 404);

    let packs_dir = format_in!(alloc.clone(), "{}/packs", served.dir());
    let left = list_names(packs_dir.as_str(), alloc.clone());
    assert!(left.is_empty(), "temporary file left behind");

    let junk = b"not a pack at all";
    let junk_hex = hex_of(&digest(junk), alloc.clone());
    let junk_path = format_in!(alloc.clone(), "/v1/blob/packs/{}.pack", junk_hex.as_str());
    let refused = served.ask("PUT", junk_path.as_str(), &[], junk);
    assert_eq!(refused.status, 422);
    assert!(refused.error().message.as_str().contains("format"));
}

#[test]
fn an_aborted_put_removes_its_staged_pack() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("aborted-put", alloc.clone());
    let packs = format_in!(alloc.clone(), "{}/packs", served.dir());
    let mut client = TcpClient::connect(served.port).unwrap_or_else(|e| panic!("connect: {}", e.what()));
    let request = b"PUT /v1/blob/packs/0101010101010101010101010101010101010101010101010101010101010101.pack HTTP/1.1\r\nHost: localhost\r\nContent-Length: 64\r\n\r\nx";
    assert!(client.write_all(request).is_ok());
    let wait_for_stage = |present: bool| {
        for _ in 0..200 {
            let entries = list_names(packs.as_str(), alloc.clone());
            if match entries.as_slice() {
                [] => !present,
                [entry] if entry.name.as_str().ends_with(".tmp") => present,
                _ => panic!("unexpected file in {}", packs.as_str()),
            } {
                return;
            }
            sleep_ms(10);
        }
        panic!("timed out waiting for staged pack presence {present} in {}", packs.as_str());
    };
    wait_for_stage(true);
    drop(client);
    wait_for_stage(false);
}

#[test]
fn a_concurrent_write_of_one_blob_leaves_one_whole_blob() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("concurrent", alloc.clone());
    let big = [7u8; 4096];
    let bytes = pack(&[(digest(b"one"), b"a value that is not small"), (digest(b"two"), &big)], alloc.clone()).bytes;
    let name = pack_name(bytes.as_slice(), alloc.clone());
    let port = served.port;

    let mut workers = Vec::with_capacity_in(8, alloc.clone());
    for _ in 0..8 {
        let name_c = name.clone();
        let bytes_c = bytes.clone();
        let alloc_c = alloc.clone();
        workers.push(Worker::start(
            move || {
                let mut client = match TcpClient::connect(port) {
                    Ok(c) => c,
                    Err(e) => panic!("writer connect failed: {}", e.what()),
                };
                let head = format_in!(
                    alloc_c.clone(),
                    "PUT /v1/blob/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    name_c.as_str(),
                    bytes_c.len()
                );
                assert!(client.write_all(head.as_bytes()).is_ok());
                assert!(client.write_all(bytes_c.as_slice()).is_ok());
                match Answer::read(&mut client, "PUT", alloc_c) {
                    Ok(a) => a.status,
                    Err(e) => panic!("writer answer failed: {}", e.what()),
                }
            },
            alloc.clone(),
        ));
    }
    for w in workers {
        assert_eq!(w.join(), 204);
    }

    let path = format_in!(alloc.clone(), "/v1/blob/{}", name.as_str());
    let got = served.ask("GET", path.as_str(), &[], &[]);
    assert_eq!(got.status, 200);
    assert_eq!(got.body.as_slice(), bytes.as_slice());

    let packs_dir = format_in!(alloc.clone(), "{}/packs", served.dir());
    let left = list_names(packs_dir.as_str(), alloc.clone());
    let expected_file = format_in!(alloc.clone(), "{}.pack", hex_of(&digest(bytes.as_slice()), alloc.clone()).as_str());
    assert_eq!(left.len(), 1);
    assert_eq!(left.first().map(|entry| entry.name.as_str()), Some(expected_file.as_str()));
    let root = served.dir.path.clone();
    drop(served);
    let after_cleanup = with_cpath(root.as_str(), |path| stat_at(CWD, path, AT_SYMLINK_NOFOLLOW));
    assert!(matches!(after_cleanup, Err(Errno::NoEnt)), "test directory left at {}", root.as_str());
}

#[test]
fn a_list_is_written_once() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("list-once", alloc.clone());
    let unit_hex = hex_of(&digest(b"a unit"), alloc.clone());
    let path = format_in!(alloc.clone(), "/v1/list/manifests/{}", unit_hex.as_str());
    assert_eq!(served.ask("GET", path.as_str(), &[], &[]).status, 404);

    let malformed = served.ask("PUT", path.as_str(), &[], b"first\n");
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.error().code, 1);
    assert!(malformed.error().message.as_str().contains("pack list line 1"));
    assert_eq!(served.ask("GET", path.as_str(), &[], &[]).status, 404);

    let key = digest(b"an item");
    let first_pack = pack(&[(key, b"first")], alloc.clone());
    let second_pack = pack(&[(key, b"second")], alloc.clone());
    for p in [&first_pack, &second_pack] {
        let pname = pack_name(p.bytes.as_slice(), alloc.clone());
        let ppath = format_in!(alloc.clone(), "/v1/blob/{}", pname.as_str());
        let put = served.ask("PUT", ppath.as_str(), &[], p.bytes.as_slice());
        assert_eq!(put.status, 204);
    }

    let first = blaze_wire::pack_list::encode(core::slice::from_ref(&first_pack.reference), alloc.clone());
    let second = blaze_wire::pack_list::encode(core::slice::from_ref(&second_pack.reference), alloc.clone());
    assert_ne!(first.as_slice(), second.as_slice());

    let wrote = served.ask("PUT", path.as_str(), &[], first.as_slice());
    assert_eq!(wrote.status, 204);
    assert_eq!(served.ask("PUT", path.as_str(), &[], second.as_slice()).status, 412);

    let got = served.ask("GET", path.as_str(), &[], &[]);
    assert_eq!(got.status, 200);
    assert_eq!(got.body.as_slice(), first.as_slice());
}

#[test]
fn a_name_that_is_not_the_layouts_reaches_no_file() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("bad-names", alloc.clone());
    for name in [
        "../Cargo.toml",
        "packs/../../etc/passwd",
        "packs/x.pack",
        "keys/..",
        "blobs/x",
        "manifests/default",
    ] {
        let path = format_in!(alloc.clone(), "/v1/blob/{name}");
        let answer = served.ask("GET", path.as_str(), &[], &[]);
        assert_eq!(answer.status, 404, "{name} reached something");
    }
    let answer = served.ask("GET", "/v1/nonsense", &[], &[]);
    assert_eq!(answer.status, 404);
}

#[test]
fn uppercase_digest_routes_are_rejected_without_creating_aliases() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("uppercase", alloc.clone());
    let bytes = pack(&[(digest(b"canonical"), b"lowercase")], alloc.clone()).bytes;
    let d = hex_of(&digest(bytes.as_slice()), alloc.clone());
    let mut upper = String::with_capacity_in(d.len(), alloc.clone());
    for ch in d.as_str().chars() {
        upper.push(ch.to_ascii_uppercase());
    }
    assert_ne!(d.as_str(), upper.as_str());

    let bad_pack = format_in!(alloc.clone(), "packs/{}.pack", upper.as_str());
    let bad_unit = format_in!(alloc.clone(), "units/{}", d.as_str());
    let bad_keys = format_in!(alloc.clone(), "keys/{}", d.as_str());
    for name in [bad_pack.as_str(), bad_unit.as_str(), bad_keys.as_str(), d.as_str(), upper.as_str()] {
        let path = format_in!(alloc.clone(), "/v1/blob/{name}");
        assert_eq!(served.ask("PUT", path.as_str(), &[], bytes.as_slice()).status, 404, "{name}");
    }

    let good_path = format_in!(alloc.clone(), "/v1/blob/packs/{}.pack", d.as_str());
    assert_eq!(served.ask("PUT", good_path.as_str(), &[], bytes.as_slice()).status, 204);
    assert_eq!(served.ask("GET", good_path.as_str(), &[], &[]).body.as_slice(), bytes.as_slice());
}

#[test]
fn http_reads_reject_symlink_objects_and_oversize_upload_length() {
    let alloc = blaze_own::testing::heap().shared();
    let served = Served::start("symlink-oversize", alloc.clone());
    let outside = TempDir::new("outside", alloc.clone());
    let bytes = b"outside";
    let d = hex_of(&digest(bytes), alloc.clone());
    let name = format_in!(alloc.clone(), "packs/{}.pack", d.as_str());
    let secret = format_in!(alloc.clone(), "{}/secret", outside.path());
    write_file(secret.as_str(), bytes);
    let link_path = format_in!(alloc.clone(), "{}/{}", served.dir(), name.as_str());
    make_symlink(secret.as_str(), link_path.as_str());

    let get_path = format_in!(alloc.clone(), "/v1/blob/{}", name.as_str());
    assert_eq!(served.ask("GET", get_path.as_str(), &[], &[]).status, 400);

    let mut client = match TcpClient::connect(served.port) {
        Ok(c) => c,
        Err(e) => panic!("connect failed: {}", e.what()),
    };
    let req = format_in!(
        alloc.clone(),
        "PUT /v1/blob/packs/0101010101010101010101010101010101010101010101010101010101010101.pack HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        Limits::DOCUMENTED.blob.saturating_add(1)
    );
    assert!(client.write_all(req.as_bytes()).is_ok());
    let Ok(ans) = Answer::read(&mut client, "PUT", alloc.clone()) else {
        panic!("reading 413 response failed");
    };
    assert_eq!(ans.status, 413);

    let packs_dir = format_in!(alloc.clone(), "{}/packs", served.dir());
    let entries = list_names(packs_dir.as_str(), alloc);
    assert!(entries.iter().all(|entry| !entry.name.as_str().starts_with('.')));
}
