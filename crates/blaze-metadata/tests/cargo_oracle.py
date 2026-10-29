#!/usr/bin/env python3
"""Compare cold/locked/offline metadata with independent Cargo 1.99 workspaces.

Usage: cargo_oracle.py /absolute/path/to/blaze-metadata-example
Cargo/rustc/rustdoc are blocked in every product invocation. Cargo only runs as
this script's independent reference. All temporary fixtures are removed.
"""

import functools
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading


def write(root, path, text):
    path = root / path
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def app(root, deps=""):
    write(root, "Cargo.toml", "[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\n" + deps)
    write(root, "src/main.rs", "fn main() {}\n")


def dep_path(name):
    if len(name) == 1:
        return "1/" + name
    if len(name) == 2:
        return "2/" + name
    if len(name) == 3:
        return "3/" + name[0] + "/" + name
    return name[:2] + "/" + name[2:4] + "/" + name


def publish(registry, name, version, deps=(), yanked=False, rust_version=None, optional=()):
    manifest = f"[package]\nname='{name}'\nversion='{version}'\nedition='2021'\n[dependencies]\n"
    if rust_version:
        manifest = manifest.replace("[dependencies]", f"rust-version='{rust_version}'\n[dependencies]")
    manifest += "".join(f"{n}={{version='{v}',optional=true}}\n" if n in optional else f"{n}='{v}'\n" for n, v in deps)
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for path, text in [("Cargo.toml", manifest), ("src/lib.rs", "pub fn f() {}\n")]:
            data = text.encode()
            info = tarfile.TarInfo(f"{name}-{version}/{path}")
            info.size, info.mode = len(data), 0o644
            archive.addfile(info, io.BytesIO(data))
    data = buffer.getvalue()
    (registry / f"{name}-{version}.crate").write_bytes(data)
    path = registry / "index" / dep_path(name)
    path.parent.mkdir(parents=True, exist_ok=True)
    line = dict(name=name, vers=version, cksum=hashlib.sha256(data).hexdigest(), yanked=yanked, features={},
                deps=[dict(name=n, req=v, features=[], optional=n in optional, default_features=True, kind="normal") for n, v in deps])
    if rust_version:
        line["rust_version"] = rust_version
    with path.open("a") as out:
        out.write(json.dumps(line) + "\n")


def normalize(value, root, home):
    if isinstance(value, str):
        return value.replace(str(home), "$HOME").replace(str(root), "$ROOT")
    if isinstance(value, list):
        return [normalize(v, root, home) for v in value]
    if isinstance(value, dict):
        return {k: normalize(v, root, home) for k, v in value.items()}
    return value


def main():
    blaze = str(Path(sys.argv[1]).resolve())
    cargo = shutil.which("cargo")
    passed = 0
    with tempfile.TemporaryDirectory(prefix="blaze-cold-oracle-") as temporary:
        work = Path(temporary)
        blockers = work / "blockers"
        blockers.mkdir()
        invoked = work / "product-tool-invocations"
        for tool in ("cargo", "rustc", "rustdoc"):
            path = blockers / tool
            path.write_text(f'#!/bin/sh\necho {tool} >> "{invoked}"\nexit 97\n')
            path.chmod(0o755)
        base = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO_", "RUST"))}
        base["RUSTUP_TOOLCHAIN"] = "1.99.0"

        def pair(name):
            ours, theirs = work / (name + "-blaze"), work / (name + "-cargo")
            app(ours)
            app(theirs)
            return ours, theirs

        def run_pair(roots, flags=(), success=True, compare_lock=True):
            nonlocal passed
            results = []
            for product, root in zip((True, False), roots):
                home = work / (root.name + "-home")
                home.mkdir(exist_ok=True)
                env = {**base, "CARGO_HOME": str(home)}
                if product:
                    env.update(PATH=str(blockers) + ":/usr/bin:/bin", RUSTC=str(blockers / "rustc"), RUSTDOC=str(blockers / "rustdoc"))
                command = [blaze, *flags] if product else [cargo, "+1.99.0", "metadata", "--format-version=1", *flags]
                result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
                assert (result.returncode == 0) == success, (command, result.stderr)
                if success:
                    results.append(normalize(json.loads(result.stdout), root, home))
            if success:
                assert results[0] == results[1], json.dumps(results, indent=2)
            if compare_lock:
                locks = [root / "Cargo.lock" for root in roots]
                assert locks[0].exists() == locks[1].exists()
                if locks[0].exists():
                    assert locks[0].read_text() == locks[1].read_text(), [p.read_text() for p in locks]
            assert not invoked.exists(), invoked.read_text() if invoked.exists() else ""
            passed += 1
            print(f"PASS {roots[0].name} {' '.join(flags)} success={success}")

        roots = pair("path")
        for root in roots:
            app(root, "dep={path='dep'}\n")
            write(root, "dep/Cargo.toml", "[package]\nname='dep'\nversion='0.1.0'\nedition='2021'\n")
            write(root, "dep/src/lib.rs", "pub fn f() {}\n")
        run_pair(roots, ("--locked", "--offline"), success=False)
        run_pair(roots, ("--offline",))
        run_pair(roots, ("--locked", "--offline"))
        for root in roots:
            write(root, "dep/Cargo.toml", "[package]\nname='dep'\nversion='0.2.0'\nedition='2021'\n")
        run_pair(roots, ("--locked", "--offline"), success=False)
        run_pair(roots, ("--offline",))
        for root in roots:
            write(root, "Cargo.lock", "invalid [lock\n")
        run_pair(roots, ("--offline",), success=False)

        roots = pair("readonly-lock")
        for root in roots:
            app(root)
        run_pair(roots, ("--offline",))
        if os.geteuid() != 0:
            for root in roots:
                manifest = (root / "Cargo.toml").read_text().replace("version='0.1.0'", "version='0.2.0'")
                write(root, "Cargo.toml", manifest)
                (root / "Cargo.lock").chmod(0o400)
            run_pair(roots, ("--offline",), success=False)

        registry = work / "registry"
        (registry / "index").mkdir(parents=True)
        publish(registry, "leaf", "1.0.0")
        publish(registry, "leaf", "2.0.0")
        publish(registry, "middle", "1.0.0", [("leaf", "1")])
        publish(registry, "middle", "1.1.0", [("leaf", "2")])
        publish(registry, "middle", "1.2.0", yanked=True)
        roots = pair("registry")
        for root in roots:
            app(root, "middle='1'\n")
            write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry='{registry}'\n")
        run_pair(roots, ("--offline",))
        publish(registry, "middle", "1.3.0", [("leaf", "1")])
        run_pair(roots, ("--offline",))
        run_pair(roots, ("--frozen",))
        for root in roots:
            app(root, "middle='1.3'\n")
        run_pair(roots, ("--locked", "--offline"), success=False)
        run_pair(roots, ("--offline",))
        # Local registries resolve before reporting a missing archive.
        (registry / "middle-1.3.0.crate").unlink()
        roots = pair("offline")
        for root in roots:
            app(root, "middle='1'\n")
            write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry='{registry}'\n")
        run_pair(roots, ("--offline",), success=False)

        publish(registry, "optional_leaf", "1.0.0")
        publish(registry, "optional_middle", "1.0.0", [("optional_leaf", "1")], optional=("optional_leaf",))
        (registry / "optional_leaf-1.0.0.crate").unlink()
        roots = pair("optional")
        for root in roots:
            app(root, "optional_middle='1'\n")
            write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry='{registry}'\n")
        run_pair(roots, ("--offline",))
        run_pair(roots, ("--frozen",))

        publish(registry, "unified", "1.0.0")
        publish(registry, "unified", "1.1.0")
        publish(registry, "branch", "1.0.0", [("unified", "=1.0.0")])
        publish(registry, "branch", "1.1.0", [("unified", "=1.1.0")])
        roots = pair("backtrack")
        for root in roots:
            app(root, "branch='1'\nunified='=1.0.0'\n")
            write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry='{registry}'\n")
        run_pair(roots, ("--offline",))
        for root in roots:
            lock = (root / "Cargo.lock").read_text()
            lines = lock.splitlines()
            at = next(i for i, line in enumerate(lines) if line.startswith("checksum ="))
            lines[at] = 'checksum = "' + '0' * 64 + '"'
            write(root, "Cargo.lock", "\n".join(lines) + "\n")
        run_pair(roots, ("--frozen",), success=False)

        publish(registry, "compat", "1.0.0", rust_version="1.90")
        publish(registry, "compat", "1.1.0", rust_version="1.98")
        roots = pair("msrv")
        for root in roots:
            app(root, "compat='1'\n")
            manifest = (root / "Cargo.toml").read_text().replace("edition='2021'", "edition='2024'\nrust-version='1.90'")
            write(root, "Cargo.toml", manifest)
            write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry='{registry}'\n")
        run_pair(roots, ("--offline",))
        run_pair(roots, ("--frozen",))
        roots = pair("old-msrv")
        for root in roots:
            app(root)
            write(root, "Cargo.toml", (root / "Cargo.toml").read_text().replace("edition='2021'", "edition='2021'\nrust-version='1.60'"))
        run_pair(roots, ("--offline",))

        git = work / "git-source"
        write(git, "Cargo.toml", "[package]\nname='gitdep'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ngitleaf={path='leaf'}\n")
        write(git, "src/lib.rs", "pub fn f() {}\n")
        write(git, "leaf/Cargo.toml", "[package]\nname='gitleaf'\nversion='0.1.0'\nedition='2021'\n")
        write(git, "leaf/src/lib.rs", "pub fn f() {}\n")
        for args in (["init", "-q"], ["add", "."], ["-c", "user.name=Oracle", "-c", "user.email=oracle@example.invalid", "commit", "-qm", "fixture"]):
            subprocess.run(["git", *args], cwd=git, check=True)
        roots = pair("git")
        for root in roots:
            app(root, f"gitdep={{git='{git.as_uri()}'}}\n")
        run_pair(roots)
        run_pair(roots, ("--frozen",))

        roots = pair("git-patch")
        for root in roots:
            app(root, "gitdep='0.1'\n[patch.crates-io]\ngitdep={git='" + git.as_uri() + "'}\n")
            write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='local'\n[source.local]\nlocal-registry='{registry}'\n")
        run_pair(roots)
        original = (git / "Cargo.toml").read_text()
        for version, suffix in [("0.1.1", ""), ("0.1.0", "[features]\nchanged=[]\n")]:
            write(git, "Cargo.toml", original.replace("version='0.1.0'", f"version='{version}'") + suffix)
            subprocess.run(["git", "add", "."], cwd=git, check=True)
            subprocess.run(["git", "-c", "user.name=Oracle", "-c", "user.email=oracle@example.invalid", "commit", "-qm", version + suffix], cwd=git, check=True)
            run_pair(roots, ("--locked",))

        class Quiet(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *_):
                pass
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=str(registry)))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}"
            write(registry, "index/config.json", json.dumps({"dl": url + "/{crate}-{version}.crate"}))
            roots = pair("sparse")
            for root in roots:
                app(root, "middle='1'\n")
                write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='mirror'\n[source.mirror]\nregistry='sparse+{url}/index/'\n")
            # The newest version is listed but unavailable; constrain to a published one.
            for root in roots:
                app(root, "middle='=1.1.0'\n")
            run_pair(roots)
            run_pair(roots, ("--frozen",))
            publish(registry, "middle", "1.3.1", [("leaf", "1")])
            index = registry / "index" / dep_path("middle")
            updated = index.stat().st_mtime + 2
            os.utime(index, (updated, updated))
            incoming = pair("sparse-incoming")
            for root in incoming:
                app(root, "middle='=1.3.1'\n")
                write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='mirror'\n[source.mirror]\nregistry='sparse+{url}/index/'\n")
            run_pair(incoming)
            for root, seed in zip(roots, incoming):
                app(root, "middle='=1.3.1'\n")
                write(root, "Cargo.lock", (seed / "Cargo.lock").read_text())
            run_pair(roots, ("--locked",))

            alternate = registry / "alternate"
            (alternate / "index").mkdir(parents=True)
            publish(registry, "crossleaf", "1.0.0")
            publish(alternate, "crossleaf", "1.0.0")
            publish(alternate, "outer", "1.0.0", [("crossleaf", "1")])
            index = alternate / "index" / dep_path("outer")
            entry = json.loads(index.read_text())
            entry["deps"][0]["registry"] = "https://github.com/rust-lang/crates.io-index"
            index.write_text(json.dumps(entry) + "\n")
            write(alternate, "index/config.json", json.dumps({"dl": url + "/alternate/{crate}-{version}.crate"}))
            cross = pair("cross-registry")
            for root in cross:
                app(root, "outer={version='1',registry='alternate'}\n")
                write(root, ".cargo/config.toml", f"[source.crates-io]\nreplace-with='mirror'\n[source.mirror]\nregistry='sparse+{url}/index/'\n[registries.alternate]\nindex='sparse+{url}/alternate/index/'\n")
            run_pair(cross)
            run_pair(cross, ("--frozen",))

            # Remote indexes prefer an available cached archive in offline mode.
            publish(registry, "middle", "1.4.0", [("leaf", "2")])
            for root in roots:
                (root / "Cargo.lock").unlink()
                app(root, "middle='1'\n")
            run_pair(roots, ("--offline",))

        finally:
            server.shutdown()
            server.server_close()
            thread.join()
    print(f"{passed} Cargo 1.99 comparisons passed; 0 product Cargo/rustc/rustdoc invocations")


if __name__ == "__main__":
    main()
