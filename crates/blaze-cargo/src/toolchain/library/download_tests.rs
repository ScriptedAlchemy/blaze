//! Real library downloads through a private TLS mirror. Each client runs in a child so its
//! certificate roots and shared HTTP client cannot affect other tests in the process.

use std::{
    io::{Read as _, Write as _},
    net::TcpListener,
    os::fd::AsRawFd as _,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use blaze_core::wait::{Clock as _, Deadline, Monotonic, Waited};
use blaze_registry::http::Refused;

const CHILD: &str = "BLAZE_TEST_LIBRARY_DOWNLOAD";
const BODY: &[u8] = b"downloaded library archive";

#[test]
fn library_download_refuses_an_https_redirect_to_http() {
    redirect("library_download_refuses_an_https_redirect_to_http", true);
}

#[test]
fn library_download_follows_an_https_redirect_to_https() {
    redirect("library_download_follows_an_https_redirect_to_https", false);
}

fn redirect(test: &str, downgrade: bool) {
    if let Ok(from) = std::env::var(CHILD) {
        let got = super::download(&from, "library.tar.xz");
        if downgrade {
            let error = got.unwrap_err();
            assert!(
                matches!(error.downcast_ref::<Refused>(), Some(Refused::Downgraded { .. })),
                "{error:#}"
            );
        } else {
            assert_eq!(got.unwrap(), BODY);
        }
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let root = work.path().join("root.pem");
    let pem = base64::engine::general_purpose::STANDARD.encode(cert.der());
    std::fs::write(
        &root,
        format!("-----BEGIN CERTIFICATE-----\n{pem}\n-----END CERTIFICATE-----\n"),
    )
    .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    let tls = Arc::new(tls);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    // A separate, unopened HTTP endpoint: refusal must happen before trying to connect there.
    let plain = TcpListener::bind("127.0.0.1:0").unwrap();
    plain.set_nonblocking(true).unwrap();
    let to = if downgrade {
        format!("http://{}/asset", plain.local_addr().unwrap())
    } else {
        format!("https://{address}/asset")
    };
    let log = work.path().join("child.log");
    let output = std::fs::File::create(&log).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("toolchain::library::download_tests::{test}"), "--nocapture"])
        .env(CHILD, format!("https://{address}/v0.0.0-dev"))
        .env("SSL_CERT_FILE", root)
        .env("SSL_CERT_DIR", work.path())
        .env("NO_PROXY", "*")
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .spawn()
        .unwrap();
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), Duration::from_secs(60));
    let mut completed = None;
    let success = [
        format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", BODY.len())
            .into_bytes(),
        BODY.to_vec(),
    ]
    .concat();
    let mut plain_requests = 0;
    for _ in 0..600 {
        if let Some(status) = child.try_wait().unwrap() {
            completed = Some(status);
            break;
        }
        if deadline.left(clock.now()).is_zero() {
            break;
        }
        let tick = Deadline::after(clock.now(), Duration::from_millis(100));
        let mut fds = [
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: plain.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        if !matches!(
            blaze_core::wait::readable(
                "library redirect test mirror",
                &mut fds,
                Some(tick),
                &clock
            )
            .unwrap(),
            Waited::Ready(_)
        ) {
            continue;
        }
        if fds[1].revents != 0 {
            let (mut stream, _) = plain.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            stream.write_all(&success).unwrap();
            plain_requests += 1;
            continue;
        }
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let session = rustls::ServerConnection::new(tls.clone()).unwrap();
        let mut stream = rustls::StreamOwned::new(session, stream);
        let mut request = Vec::new();
        for _ in 0..16 {
            let mut bytes = [0; 1024];
            let count = stream.read(&mut bytes).unwrap();
            request.extend_from_slice(&bytes[..count]);
            if count == 0 || request.windows(4).any(|it| it == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).unwrap();
        let answer = if request.starts_with("GET /asset ") {
            success.clone()
        } else {
            format!("HTTP/1.1 302 Found\r\nConnection: close\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n").into_bytes()
        };
        stream.write_all(&answer).unwrap();
        stream.flush().unwrap();
    }
    if completed.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(
        completed.is_some_and(|status| status.success()),
        "library download child: {completed:?}\n{said}"
    );
    assert!(said.contains("1 passed; 0 failed"), "{said}");
    assert_eq!(plain_requests, 0, "library download contacted the HTTP redirect target");
}
