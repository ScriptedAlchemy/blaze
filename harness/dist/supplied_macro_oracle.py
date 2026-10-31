#!/usr/bin/env python3
"""Reference build for the supplied-macro restored-mtime regression test.

Only this Python harness starts Cargo. The Rust test captures producer inputs before
asking for each reference build, then exercises the product's completed/streamed loaders.
"""
import os
from pathlib import Path
import subprocess
import sys


def main():
    root = Path(sys.argv[1]).resolve()
    env = dict(os.environ)
    for name in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_INCREMENTAL",
                 "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
        env.pop(name, None)
    env["RUSTUP_TOOLCHAIN"] = "1.99.0"
    result = subprocess.run(["cargo", "build", "-p", "app", "--message-format=json"],
                            cwd=root, env=env)
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
