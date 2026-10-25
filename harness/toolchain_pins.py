#!/usr/bin/env python3
"""toolchain_pins.py: the checksums of the Rust release blaze downloads as its own toolchain
(crates/ra-jit/src/cargo/toolchain/pins.txt), read from the release's channel manifest on
static.rust-lang.org. blaze refuses a download whose bytes hash to anything else.

    toolchain_pins.py            say whether pins.txt is what the manifest says
    toolchain_pins.py --write    write pins.txt, after harness/toolchain moves

The manifest itself is checked against the `.sha256` the Rust project publishes beside it, and
its own hash is pinned too, since blaze writes it into the toolchain as rustup does.
"""
import hashlib, os, sys, tomllib, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RELEASE = open(os.path.join(HERE, "toolchain")).read().strip()
PINS = os.path.join(ROOT, "crates/ra-jit/src/cargo/toolchain/pins.txt")
DIST = "https://static.rust-lang.org/dist"
# The hosts blaze has a build for, and Windows for `cargo install`.
HOSTS = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
         "x86_64-pc-windows-msvc"]
# rustup's `minimal` profile without rust-docs, and the standard library's source.
COMPONENTS = ["rustc", "cargo", "rust-std", "rust-src"]


def get(url):
    with urllib.request.urlopen(url) as answer:
        return answer.read()


def size(url):
    with urllib.request.urlopen(urllib.request.Request(url, method="HEAD")) as answer:
        return int(answer.headers["Content-Length"])


def pins():
    manifest = get(f"{DIST}/channel-rust-{RELEASE}.toml")
    published = get(f"{DIST}/channel-rust-{RELEASE}.toml.sha256").split()[0].decode()
    digest = hashlib.sha256(manifest).hexdigest()
    if digest != published:
        sys.exit(f"the channel manifest hashes to {digest}, and its .sha256 says {published}")
    channel = tomllib.loads(manifest.decode())
    date = channel["date"]
    lines = [
        f"# The Rust release blaze downloads as its own toolchain: written by",
        f"# harness/toolchain_pins.py --write from {DIST}/channel-rust-{RELEASE}.toml.",
        f"# release <version> <date>; manifest <sha256> <bytes>; then per host",
        f"# <host> <package> <target> <sha256 of the .tar.xz> <bytes>.",
        f"release {RELEASE} {date}",
        f"manifest {digest} {len(manifest)}",
    ]
    for host in HOSTS:
        for package in COMPONENTS:
            target = "*" if package == "rust-src" else host
            built = channel["pkg"][package]["target"][target]
            if not built.get("available"):
                sys.exit(f"{package} is not built for {target} in Rust {RELEASE}")
            url = built["xz_url"]
            want = f"{DIST}/{date}/{package}-{RELEASE}{'' if target == '*' else '-' + target}.tar.xz"
            if url != want:
                sys.exit(f"{package} for {target} is at {url}, where blaze looks for {want}")
            lines.append(f"{host} {package} {target} {built['xz_hash']} {size(url)}")
    return "\n".join(lines) + "\n"


def main():
    written = pins()
    if "--write" in sys.argv:
        with open(PINS, "w") as out:
            out.write(written)
        print(f"wrote {os.path.relpath(PINS, ROOT)} for Rust {RELEASE}")
        return
    have = open(PINS).read() if os.path.exists(PINS) else ""
    if have != written:
        sys.exit(f"{os.path.relpath(PINS, ROOT)} is not what Rust {RELEASE}'s manifest says: "
                 "toolchain_pins.py --write writes it")
    print(f"{os.path.relpath(PINS, ROOT)} is Rust {RELEASE}'s manifest")


if __name__ == "__main__":
    main()
