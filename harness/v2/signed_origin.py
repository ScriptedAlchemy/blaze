"""Exercise the signed toolchain origin through the release CLI on Linux.

Pass one JSON object with `blaze` (an installed release CLI built with `publish` and
`fetch`), `shelf` (its installed toolchain directory), and `scratch` (a new evidence
directory). `alternate_blaze` optionally measures an installed developer build with
the same shelf pin and target. It measures its own cold baseline and checks that even
the trusted builder leaves its local records out of publication. `timeout` bounds each
Blaze command (seconds, default 300).
"""

import contextlib
import hashlib
import http.server
import json
import os
import re
import secrets
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
from collections.abc import Generator
from dataclasses import dataclass, field
from pathlib import Path
from typing import TypedDict

from typed_input import fail, parse


@dataclass(frozen=True)
class Input:
    blaze: str
    shelf: str
    scratch: str
    alternate_blaze: str | None = None
    control_publisher: str | None = None
    timeout: int = 300


class RangeRead(TypedDict):
    path: str
    first: int
    bytes: int


class TakenSet(TypedDict):
    entered: int
    records: int
    bytes: int
    reads: list[RangeRead]


@dataclass
class OriginTraffic:
    requests: list[str] = field(default_factory=list[str])
    reads: list[RangeRead] = field(default_factory=list[RangeRead])
    refusals: list[str] = field(default_factory=list[str])
    handshake_failed: threading.Event = field(default_factory=threading.Event)


def require(condition: bool, why: str) -> None:
    if not condition:
        raise ValueError(why)


def digest(path: Path) -> str:
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def certificate(directory: Path) -> tuple[ssl.SSLContext, Path, Path]:
    def openssl(*args: str) -> None:
        subprocess.run(["openssl", *args], cwd=directory, capture_output=True, check=True, timeout=20)

    for name in ("origin", "unrelated"):
        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                "-subj", f"/CN=Blaze signed-origin fixture {name}",
                "-addext", "basicConstraints=critical,CA:TRUE",
                "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                "-addext", "subjectKeyIdentifier=hash",
                "-keyout", f"{name}.key", "-out", f"{name}.pem")
    openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
            "-keyout", "server.key", "-out", "server.csr")
    (directory / "server.ext").write_text(
        "subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n"
        "keyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n"
        "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid\n"
    )
    openssl("x509", "-req", "-days", "1", "-in", "server.csr", "-CA", "origin.pem", "-CAkey", "origin.key",
            "-CAcreateserial", "-extfile", "server.ext", "-out", "server.pem")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(directory / "server.pem", directory / "server.key")
    return context, directory / "origin.pem", directory / "unrelated.pem"


@contextlib.contextmanager
def serve(
    directory: Path, tls: ssl.SSLContext, receipt: Path,
) -> Generator[tuple[str, OriginTraffic]]:
    objects = {"/" + path.relative_to(directory).as_posix(): path
               for path in directory.rglob("*") if path.is_file()}
    traffic = OriginTraffic()

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format: str, *args: object) -> None:
            pass

        def do_GET(self):
            traffic.requests.append(self.path)
            file = objects.get(self.path)
            if file is None:
                self.send_error(404)
                return
            asked = re.fullmatch(r"bytes=(\d+)-(\d+)", self.headers.get("Range", ""))
            if asked is None:
                self.send_error(400, "expected a closed byte range")
                return
            start, last = map(int, asked.groups())
            size = file.stat().st_size
            if start >= size or start > last:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{size}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            last = min(last, size - 1)
            count = last - start + 1
            traffic.reads.append({"path": self.path, "first": start, "bytes": count})
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{last}/{size}")
            self.send_header("Content-Length", str(count))
            self.end_headers()
            with file.open("rb") as stream:
                stream.seek(start)
                while count:
                    chunk = stream.read(min(count, 1 << 16))
                    if not chunk:
                        raise OSError(f"published object changed while serving {self.path}")
                    self.wfile.write(chunk)
                    count -= len(chunk)

    class Server(http.server.ThreadingHTTPServer):
        def get_request(self):
            sock, peer = super().get_request()
            sock.settimeout(5)
            try:
                return tls.wrap_socket(sock, server_side=True), peer
            except OSError as error:
                sock.close()
                traffic.refusals.append(str(error))
                traffic.handshake_failed.set()
                raise

    with Server(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.1})
        thread.start()
        try:
            yield f"https://127.0.0.1:{server.server_port}", traffic
        finally:
            server.shutdown()
            thread.join(timeout=6)
            receipt.write_text(json.dumps({
                "tls_refusals": traffic.refusals, "https_reads": traffic.reads, "https_requests": traffic.requests,
            }, indent=2) + "\n")
            require(not thread.is_alive(), "the disposable origin did not stop")


def fixture(scratch: Path, shelf: Path, name: str) -> tuple[Path, Path]:
    root = scratch / name
    checkout = root / "project"
    home = root / "home"
    (checkout / "src").mkdir(parents=True)
    (checkout / "Cargo.toml").write_text(
        '[package]\nname = "signed-origin-probe"\nversion = "0.1.0"\nedition = "2024"\n'
    )
    (checkout / "src/main.rs").write_text('fn main() { println!("signed origin: {}", 2 + 4); }\n')
    shutil.copytree(shelf, home / ".blaze/v2/toolchain")
    return checkout, home


def entered(stderr: str, which: str) -> int:
    line = re.search(r"(?m)^blaze: (\d+) of \d+ task\(s\) entered,", stderr)
    if line is None:
        raise ValueError(f"{which}: no --stats entered-task count")
    return int(line[1])


def run(given: Input) -> None:
    require(sys.platform == "linux", "the fixture gives Linux a disposable TLS root via SSL_CERT_FILE")
    require(type(given.timeout) is int and given.timeout > 0, "timeout must be a positive integer")
    blaze, shelf, scratch = (Path(path).resolve() for path in (given.blaze, given.shelf, given.scratch))
    alternate = Path(given.alternate_blaze).resolve() if given.alternate_blaze else None
    control_publisher = Path(given.control_publisher).resolve() if given.control_publisher else None
    binaries = {str(blaze): digest(blaze)}
    if alternate is not None:
        binaries[str(alternate)] = digest(alternate)
        require(binaries[str(alternate)] != binaries[str(blaze)], "alternate_blaze must name a different binary")
    if control_publisher is not None:
        binaries[str(control_publisher)] = digest(control_publisher)
        require(binaries[str(control_publisher)] != binaries[str(blaze)], "control publisher must name the candidate-filtered publisher")
    require(shelf.is_dir() and any(shelf.iterdir()), f"{shelf}: empty toolchain shelf")
    scratch.mkdir()
    origin = scratch / "origin"
    origin.mkdir()
    control = None
    hits: list[tuple[str, str, str, str, str]] = []

    def command(which: str, binary: Path, checkout: Path, home: Path, *args: str, ca: Path | None = None):
        env = {"HOME": str(home), "PATH": os.defpath, "LANG": "C.UTF-8", "RUST_BACKTRACE": "0",
               "BLAZE_NO_DAEMON": "1", "BLAZE_NO_LAN_PEER": "1"}
        if ca is not None:
            env["SSL_CERT_FILE"] = str(ca)
        with (scratch / f"{which}.stdout").open("w+") as stdout, (scratch / f"{which}.stderr").open("w+") as stderr:
            result = subprocess.run([binary, *args], cwd=checkout, env=env, stdout=stdout, stderr=stderr,
                                    check=False, timeout=given.timeout)
            profile = checkout / ".blaze/v2/profile.txt"
            if profile.is_file():
                shutil.copyfile(profile, scratch / f"{which}.profile.txt")
            stdout.seek(0)
            stderr.seek(0)
            return result.returncode, stdout.read(), stderr.read()

    def check(which: str, binary: Path, *args: str, ca: Path | None = None):
        checkout, home = fixture(scratch, shelf, which)
        result = command(which, binary, checkout, home, "check", "--stats", "--color", "never", *args, ca=ca)
        return checkout, home, result

    with tempfile.TemporaryDirectory(dir=scratch, prefix="private-") as private:
        directory = Path(private)
        signer = directory / "seed"
        signer.touch(mode=0o600)
        signer.write_text(secrets.token_hex(32))
        _, home, (status, _, cold_log) = check("cold", blaze)
        require(status == 0, "the publisher's cold check failed")
        cold = entered(cold_log, "cold")
        status, publication, _ = command(
            "publish", blaze, scratch, home, "store", "publish", str(origin.relative_to(scratch)),
            "--signer", str(signer.relative_to(scratch)),
        )
        made = re.search(r"published: set [0-9a-f]+ generation 1: ([1-9]\d*) records, ([1-9]\d*) objects written .* signed by ([0-9a-f]{64})", publication)
        if status != 0 or made is None:
            raise ValueError("the trusted builder did not publish a nonempty signed set")
        key = made[3]
        tls, ca, unrelated = certificate(directory)
        if control_publisher is not None:
            without_index = scratch / "without-index"
            without_index.mkdir()
            status, publication, _ = command(
                "publish-without-index", control_publisher, scratch, home, "store", "publish",
                str(without_index.relative_to(scratch)), "--signer", str(signer.relative_to(scratch)),
            )
            control_stats = re.search(r"generation 1: ([1-9]\d*) records, .* signed by ([0-9a-f]{64})", publication)
            require(status == 0 and control_stats is not None, "the index-free publisher failed")
            assert control_stats is not None
            require(control_stats[2] == key and int(control_stats[1]) < int(made[1]), "the withheld metadata changed the signer or left the set unchanged")
            with serve(without_index, tls, scratch / "without-index-https.json") as (control_url, _):
                control_args = ("--origin", control_url, "--origin-key", key)
                control_checkout, control_home, (status, answer, stderr) = check("without-index", blaze, *control_args, ca=ca)
                require(status == 0, "the index-free HTTPS check failed")
                control_tasks = entered(stderr, "withheld index")
                require(control_tasks == cold, f"the index-free check entered {control_tasks} of the original {cold} tasks")
                control_profile = (scratch / "without-index.profile.txt").read_text()
                status, stdout, _ = command("run-without-index", blaze, control_checkout, control_home, "run", *control_args, ca=ca)
                require(status == 0 and stdout == "signed origin: 6\n", "the index-free consumer did not run the expected program")
                control = {"published_records": int(control_stats[1]), "entered": control_tasks, "check_answer": answer}

        with serve(origin, tls, scratch / "https-observations.json") as (url, traffic):
            reads = traffic.reads

            def origin_args(public_key: str) -> tuple[str, ...]:
                return "--origin", url, "--origin-key", public_key

            _, _, (status, _, stderr) = check("untrusted-tls", blaze, *origin_args(key), ca=unrelated)
            # The client may close before sending a TLS alert. The paired trusted-CA run
            # below must take the same origin with the same binary and signing key.
            require(status != 0 and "the published set of --origin was not taken" in stderr
                    and traffic.handshake_failed.wait(timeout=6) and not traffic.requests,
                    f"the client did not refuse the handshake before HTTP: {traffic.refusals!r}")
            _, _, (status, _, stderr) = check("untrusted-signer", blaze, *origin_args("00" * 32), ca=ca)
            require(status != 0 and "a key this blaze was not given for the namespace" in stderr
                    and bool(reads) and all(row["path"].endswith("/prefix") for row in reads),
                    "the client did not refuse the foreign signer before reading payloads")

            def taken(which: str, binary: Path) -> tuple[Path, Path, TakenSet]:
                start = len(reads)
                checkout, home, (status, _, stderr) = check(which, binary, *origin_args(key), ca=ca)
                require(status == 0, f"{which}: HTTPS check failed")
                adopted = re.search(r"generation 1: ([1-9]\d*) records, ([1-9]\d*) bytes taken", stderr)
                if adopted is None:
                    raise ValueError(f"{which}: the command did not report taking a nonempty signed set")
                asked = reads[start:]
                require(any(row["path"].endswith("/prefix") for row in asked)
                        and any(row["path"].rsplit("/", 1)[1].startswith(("seg-", "segz-")) for row in asked),
                        f"{which}: the HTTPS origin did not supply the signed prefix and segment bytes")
                return checkout, home, {"entered": entered(stderr, which), "records": int(adopted[1]),
                                        "bytes": int(adopted[2]), "reads": asked}

            checkout, home, https = taken("https", blaze)
            profile = (scratch / "https.profile.txt").read_text()
            hits = re.findall(
                r"(?m)^signed CrateScope reused (\S+) (target|macro) subject=([0-9a-f]{64}) task=([0-9a-f]{64}) record=([0-9a-f]{64})$", profile,
            )
            require(bool(hits), "the signed HTTPS check did not name a reused scope")
            if control is not None:
                require("signed CrateScope reused " not in control_profile, "a scope was reported reused without its index")
                ran = set(re.findall(r"(?m)^\s+entered CrateScope ([0-9a-f]{12})$", control_profile))
                require(any(subject[:12] in ran for _, _, subject, _, _ in hits), "no reused scope was identified among the control's entered scopes")
                require(cold - https["entered"] == len(hits), "the count difference does not match the named signed scopes")
                require((scratch / "https.stdout").read_text() == control["check_answer"], "withholding the index changed the compiler answer")
            status, stdout, _ = command("run", blaze, checkout, home, "run", *origin_args(key), ca=ca)
            require(status == 0 and stdout == "signed origin: 6\n", "the HTTPS consumer did not run the expected program")
            comparison = None
            if alternate is not None:
                _, alt_home, (status, _, stderr) = check("alternate-cold", alternate)
                require(status == 0, "the alternate reader's cold check failed")
                alternate_cold = entered(stderr, "alternate-cold")
                private_origin = scratch / "from-developer-home"
                private_origin.mkdir()
                status, private_publication, _ = command(
                    "publish-developer-home", blaze, scratch, alt_home, "store", "publish",
                    str(private_origin.relative_to(scratch)), "--signer", str(signer.relative_to(scratch)),
                )
                excluded = re.search(r"generation 1: 0 records, \d+ objects written .*; ([1-9]\d*) left out \(local\)", private_publication)
                if status != 0 or excluded is None:
                    raise ValueError("the trusted publisher did not exclude the developer home’s local records")
                _, _, alternate_https = taken("alternate-https", alternate)
                comparison = {
                    "https": alternate_https,
                    "cold_entered": alternate_cold,
                    "reuse_observed": alternate_https["entered"] < alternate_cold,
                    "local_records_excluded": int(excluded[1]),
                }
                require(alternate_https["entered"] == alternate_cold, "the different compiler producer reused a scope")
            sets = list(origin.glob("public/*/*/prefix"))
            require(len(sets) == 1, "the publisher did not produce exactly one toolchain set")
            expected = "/" + sets[0].parent.relative_to(origin).as_posix() + "/"
            mismatched = sorted({path for path in traffic.requests if not path.startswith(expected)})
            require(not mismatched, f"the compiler queried outside the signed toolchain set: {mismatched}")

    require(all(digest(Path(path)) == sha for path, sha in binaries.items()), "a CLI changed during acceptance")
    (scratch / "result.json").write_text(json.dumps({
        "binaries_sha256": binaries, "signer": key, "published_records": int(made[1]),
        "cold_entered": cold, "same_binary_https": https, "reuse_observed": https["entered"] < cold, "alternate": comparison,
        "withheld_index": control, "reused_scopes": hits,
        "tls_refusals": traffic.refusals, "https_reads": reads, "https_requests": traffic.requests,
    }, indent=2) + "\n")
    require(https["entered"] < cold, f"the same compiler entered {https['entered']} tasks with HTTPS and {cold} cold")
    print(f"signed_origin.py: published, verified over HTTPS and ran; {cold} → {https['entered']} tasks entered; evidence {scratch}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        fail("signed_origin.py", "expected one JSON input")
    try:
        run(parse(Input, sys.argv[1], "signed_origin.py"))
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        fail("signed_origin.py", str(error))
