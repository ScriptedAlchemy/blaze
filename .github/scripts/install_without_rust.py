"""Exercise the just-built archive in a fresh Linux userspace with no Rust on PATH.

One typed JSON input names the matrix's distribution and the host-prepared reflink paths.
The container receives only the checkout/archive and that job's fresh state. In particular,
it receives no host Blaze installation, compiler, toolchain, LLVM or Cargo cache.
"""

from dataclasses import dataclass
from pathlib import Path
import re
import subprocess
import sys
from typing import Literal

import install_container


@dataclass(frozen=True)
class Smoke:
    workspace: str
    temporary: str
    image: str
    packages: Literal["apt", "dnf"]
    system: str
    blaze_home: str
    cache_home: str
    store: str
    target: str
    summary: str
    name: str
    uid: int
    gid: int


PACKAGES = {
    "apt": "apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates gcc libc6-dev passwd",
    "dnf": "dnf install -y -q gcc tar gzip shadow-utils && dnf clean all",
}

# These are the install workflow's existing assertions, run in one container so the derive
# edit reaches the daemon that built it. A failure at any step stops the matrix entry.
PREFLIGHT = install_container.PREFLIGHT + r'''
uname -r
getconf GNU_LIBC_VERSION
[ "$(ulimit -l)" = unlimited ] || { echo "::error::installer container lost the locked-memory limit"; exit 1; }
store_dev=$(stat -Lc %d "$BLAZE_HOME/v2/store")
for path in "$HOME" "$HOME/.cache" "$RUNNER_TEMP" "$BLAZE_HOME/std" "$CARGO_TARGET_DIR"; do
  [ "$(stat -Lc %d "$path")" = "$store_dev" ] || { echo "::error::$path and the store are on different devices inside the installer container"; exit 1; }
done
'''

INSTALL = r'''
for tool in rustup cargo rustc; do
  if command -v "$tool"; then echo "::error::$tool is on PATH"; exit 1; fi
done

began=$(date +%s.%N)
BLAZE_ARCHIVE="$PWD/blaze-commit-x86_64-unknown-linux-gnu.tar.gz" sh < site/install.sh
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$RUNNER_TEMP/hello/src"
cd "$RUNNER_TEMP/hello"
printf '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2021"\n' > Cargo.toml
printf 'fn main() {\n    println!("Hello, world!");\n}\n' > src/main.rs
blaze run > out.txt 2> err.txt || { cat err.txt; exit 1; }
ended=$(date +%s.%N)
cat err.txt out.txt
grep -Fx "Hello, world!" out.txt
if grep -e "blaze: bug:" -e "the daemon has no part" err.txt; then
  echo "::error::the build was not blaze's"; exit 1
fi
echo "$BLAZE_INSTALL_SYSTEM, from install.sh to Hello, world!: $(echo "$ended - $began" | awk '{ printf "%.1f s", $1 - $3 }')" | tee -a "$GITHUB_STEP_SUMMARY"
du -sh "$HOME/.blaze/toolchains" "$HOME/.cargo/bin" | tee -a "$GITHUB_STEP_SUMMARY"
for tool in rustup cargo rustc; do
  if command -v "$tool"; then echo "::error::$tool is on PATH after installing"; exit 1; fi
done
blaze stop
'''

DERIVE = r'''
mkdir -p "$RUNNER_TEMP/derive/src"
cd "$RUNNER_TEMP/derive"
printf '[package]\nname = "derive"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nserde = { version = "1", features = ["derive"] }\n' > Cargo.toml
printf '#[derive(serde::Serialize)]\nstruct Point {\n    x: i32,\n}\n\nfn serializable<T: serde::Serialize>(_: &T) -> &str {\n    "derived"\n}\n\nfn main() {\n    println!("{} {}", serializable(&Point { x: 1 }), 1);\n}\n' > src/main.rs
blaze run > out.txt 2> err.txt || { cat err.txt; exit 1; }
cat err.txt out.txt
grep -Fx "derived 1" out.txt
sed -i 's/serializable(&Point { x: 1 }), 1)/serializable(\&Point { x: 2 }), 2)/' src/main.rs
blaze run > out.txt 2>> err.txt || { cat err.txt; exit 1; }
cat err.txt out.txt
grep -Fx "derived 2" out.txt
if grep -e "blaze: bug:" -e "the daemon has no part" -e "did not make the proc macro" err.txt; then
  echo "::error::the proc macros were not blaze's"; exit 1
fi
blaze stop
'''
RECIPE = PREFLIGHT + INSTALL + DERIVE


def fresh_home(config: Smoke) -> Path:
    return Path(config.blaze_home).parent / "install-home"


def fresh_work(config: Smoke) -> Path:
    return fresh_home(config) / "work"


def dockerfile(config: Smoke) -> str:
    user = install_container.User(config.uid, config.gid, str(fresh_home(config)))
    # The home is prepared on the verified volume and enters only through temporary's bind.
    return (f"FROM {config.image}\nRUN {PACKAGES[config.packages]}\n"
            + user.dockerfile(create_home=False))


def command(config: Smoke) -> list[str]:
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]+", config.name):
        raise ValueError(f"invalid installer container name: {config.name!r}")
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_./:@-]*", config.image):
        raise ValueError(f"invalid installer image: {config.image!r}")
    if config.packages not in PACKAGES:
        raise ValueError("installer needs apt or dnf")
    paths = [config.workspace, config.temporary, config.blaze_home, config.cache_home,
             config.store, config.target, config.summary]
    for value in paths:
        path = Path(value)
        if not path.is_absolute() or ".." in path.parts or any(char in value for char in ",\n\r\0"):
            raise ValueError(f"installer paths must be absolute with no parent traversal or mount delimiters: {value!r}")
    temporary = Path(config.temporary)
    for value in paths[2:]:
        if not Path(value).is_relative_to(temporary):
            raise ValueError(f"installer state must be inside the job temporary bind: {value}")
    home = str(fresh_home(config))
    user = install_container.User(config.uid, config.gid, home)
    cmd = ["docker", "run", "--rm", "--init", "--name", config.name,
           "--user", user.ids,
           # The outer container must allow io_uring and Blaze's own namespace sandbox.
           "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined",
           "--ulimit", "memlock=-1:-1",
           "--mount", f"type=bind,src={config.workspace},dst={config.workspace},readonly",
           "--mount", f"type=bind,src={config.temporary},dst={config.temporary}",
           "--workdir", config.workspace]
    env = {
        "HOME": home, "CARGO_HOME": str(Path(home) / ".cargo"),
        "BLAZE_HOME": config.blaze_home, "XDG_CACHE_HOME": config.cache_home,
        "BLAZE_CACHE": config.store, "CARGO_TARGET_DIR": config.target,
        "BLAZE_CORES": "all", "BLAZE_LOCK": "write", "BLAZE_CACHE_MAX": "off",
        "RUNNER_TEMP": str(fresh_work(config)), "GITHUB_STEP_SUMMARY": config.summary,
        "BLAZE_INSTALL_SYSTEM": config.system,
        "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    }
    for key, value in env.items():
        cmd += ["--env", f"{key}={value}"]
    return cmd + [config.name + "-image", "sh", "-c", RECIPE]


def run(config: Smoke) -> None:
    cmd = command(config)
    artifact = Path(config.workspace) / "blaze-commit-x86_64-unknown-linux-gnu.tar.gz"
    if not artifact.is_file():
        raise ValueError(f"installer is missing this commit's archive: {artifact}")
    home = fresh_home(config)
    home.mkdir(mode=0o700)
    cache = home / ".cache"
    cache.mkdir(mode=0o700)
    work = fresh_work(config)
    work.mkdir(mode=0o700)
    device = Path(config.store).stat().st_dev
    for directory in (home, cache, work, Path(config.blaze_home) / "std", Path(config.target)):
        if directory.stat().st_dev != device:
            raise ValueError(f"installer state {directory} is not on the store's verified reflink device")
    subprocess.run(["docker", "build", "--tag", config.name + "-image", "-"],
                   input=dockerfile(config), text=True, check=True, timeout=600)
    try:
        subprocess.run(cmd, check=True, timeout=25 * 60)
    finally:
        subprocess.run(["docker", "rm", "--force", config.name], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)


def main() -> None:
    import typed_input

    if len(sys.argv) != 2:
        raise SystemExit("usage: install_without_rust.py CONFIG (one JSON object)")
    run(typed_input.parse(Smoke, sys.argv[1], __file__))


if __name__ == "__main__":
    main()
