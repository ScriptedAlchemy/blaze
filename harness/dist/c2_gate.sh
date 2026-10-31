#!/bin/bash
# Run under with-heavy-lock.sh and exec_guard.py with the shared target.
# Clean wire before each Cargo entry: another worktree can leave a different
# schema in the shared target even when its source mtimes appear unchanged.
set -euo pipefail
disk_gate() {
    python3 -c 'import shutil; free = shutil.disk_usage(".").free; print("disk free bytes:", free, flush=True); assert free >= 30 * 1024**3, "under 30 GiB free; stop heavy work"'
}
python3 harness/rustcsites.py
cargo +1.99.0 fmt --all -- --check
"${PYTHON313:-python3.13}" harness/imports.py
git diff --check
disk_gate
cargo +1.99.0 clean -p blaze-wire
cargo +1.99.0 test -j 2 -p blaze-spec -p blaze-wire
disk_gate
cargo +1.99.0 clean -p blaze-wire
cargo +1.99.0 check -j 2 -p ra-jit
