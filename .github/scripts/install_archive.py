"""Run the native release recipe in Ubuntu 22.04 userspace on the hosted 24.04 kernel.

The pinned stage 0 cannot start on linux-azure 6.8's broken buffer-ring check. Moving only the
host kernel preserves the release's C compiler, old glibc and build_release.py's ABI checks.
All paths are explicit in one typed JSON input. The caller prepares the pinned Blaze, LLVM
and a verified reflink volume through .github/actions/blaze; no host system libraries enter
the container, and Blaze's own build-script sandbox remains enabled.
"""

from dataclasses import dataclass
from pathlib import Path
import re
import shutil
import subprocess
import sys


@dataclass(frozen=True)
class Archive:
    workspace: str
    temporary: str
    home: str
    cargo_home: str
    blaze_home: str
    cache_home: str
    store: str
    target: str
    llvm: str
    bash_env: str
    uv: str
    name: str
    uid: int
    gid: int


IMAGE = "blaze-install-archive"
DOCKERFILE = """FROM ubuntu:22.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \\
    build-essential ca-certificates cmake curl git pkg-config xz-utils \\
    && rm -rf /var/lib/apt/lists/*
"""
RECIPE = r'''
. /etc/os-release
[ "$VERSION_ID" = 22.04 ] || { echo "::error::release userspace is $VERSION_ID, expected 22.04"; exit 1; }
uname -r
getconf GNU_LIBC_VERSION
[ "$(ulimit -l)" = unlimited ] || { echo "::error::release container lost the locked-memory limit"; exit 1; }
store_dev=$(stat -Lc %d "$BLAZE_HOME/v2/store")
for path in "$BLAZE_HOME/std" "$CARGO_TARGET_DIR" target; do
  [ "$(stat -Lc %d "$path")" = "$store_dev" ] || { echo "::error::$path and the store are on different devices inside the release container"; exit 1; }
done
uv run --frozen --no-dev python .github/scripts/build_release.py \
  '{"target":"x86_64-unknown-linux-gnu","out":"shipped","pgo":"off"}'
'''


def command(config: Archive) -> list[str]:
    """The container boundary: only named inputs, one uid and the host's verified volume."""
    if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]+", config.name):
        raise ValueError(f"invalid archive container name: {config.name!r}")
    if config.uid < 0 or config.gid < 0:
        raise ValueError("archive uid and gid must be nonnegative")
    paths = [config.workspace, config.temporary, config.home, config.cargo_home, config.blaze_home,
             config.cache_home, config.store, config.target, config.llvm, config.bash_env, config.uv]
    for value in paths:
        if not Path(value).is_absolute() or any(char in value for char in ",\n\r\0"):
            raise ValueError(f"archive paths must be absolute and contain no mount delimiters: {value!r}")
    temporary = Path(config.temporary)
    # Docker bind mounts include existing submounts: the job's Btrfs volume is below temporary.
    for value in (config.blaze_home, config.cache_home, config.store, config.target, config.bash_env):
        if not Path(value).is_relative_to(temporary):
            raise ValueError(f"archive state must be inside the job temporary bind: {value}")
    cmd = ["docker", "run", "--rm", "--init", "--name", config.name,
           "--user", f"{config.uid}:{config.gid}",
           # Docker's outer default seccomp blocks io_uring; its AppArmor blocks the nested
           # namespaces in Blaze's own sandbox. This is also linux-6-8's container policy.
           "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined",
           "--ulimit", "memlock=-1:-1"]
    mounts = [(config.workspace, False), (config.temporary, False),
              (str(Path(config.home) / ".blaze"), False),
              (config.cargo_home, False),
              (str(Path(config.cargo_home) / "bin"), True), (config.llvm, True)]
    for path, readonly in mounts:
        cmd += ["--mount", f"type=bind,src={path},dst={path}" + (",readonly" if readonly else "")]
    cmd += ["--workdir", config.workspace]
    env = {
        "HOME": config.home, "CARGO_HOME": config.cargo_home, "BASH_ENV": config.bash_env,
        "BLAZE_HOME": config.blaze_home, "XDG_CACHE_HOME": config.cache_home,
        "BLAZE_CACHE": config.store, "CARGO_TARGET_DIR": config.target,
        "BLAZE_CACHE_MAX": "off", "BLAZE_CORES": "all", "BLAZE_LOCK": "write",
        "LLVM_SYS_231_PREFIX": config.llvm,
        "PATH": f"{config.cargo_home}/bin:{config.temporary}/archive-tools:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        # Never reuse the host's workspace venv or its interpreter/symlinks.
        "UV_PROJECT_ENVIRONMENT": str(temporary / "archive-venv"),
        "UV_PYTHON_INSTALL_DIR": str(temporary / "archive-python"),
    }
    for key, value in env.items():
        cmd += ["--env", f"{key}={value}"]
    return cmd + [IMAGE, "bash", "-e", "-o", "pipefail", "-c", RECIPE]


def build(config: Archive) -> None:
    cmd = command(config)
    # Stage 0's own.rs::toolchains (e930296025) uses HOME/.blaze/toolchains even when
    # BLAZE_HOME relocates the daemon. install.sh fetches it before this helper is called.
    for directory in (Path(config.workspace), Path(config.temporary),
                      Path(config.home) / ".blaze" / "toolchains",
                      Path(config.cargo_home) / "bin", Path(config.llvm) / "bin"):
        if not directory.is_dir():
            raise ValueError(f"archive setup is missing required input directory: {directory}")
    for file in (Path(config.bash_env), Path(config.uv)):
        if not file.is_file():
            raise ValueError(f"archive setup is missing required input file: {file}")
    # A Dockerfile on stdin has no build context: do not send the mounted store to Docker.
    subprocess.run(["docker", "build", "--tag", IMAGE, "-"], input=DOCKERFILE,
                   text=True, check=True, timeout=600)
    destination = Path(config.temporary) / "archive-tools"
    destination.mkdir()
    shutil.copy2(config.uv, destination / "uv")
    try:
        subprocess.run(cmd, check=True, timeout=5 * 3600)
    finally:
        # Also end the owned container if the build command times out; --rm may have removed it.
        subprocess.run(["docker", "rm", "--force", config.name], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)


def main() -> None:
    import typed_input

    if len(sys.argv) != 2:
        raise SystemExit("usage: install_archive.py CONFIG (one JSON object)")
    config = typed_input.parse(Archive, sys.argv[1], __file__)
    build(config)


if __name__ == "__main__":
    main()
