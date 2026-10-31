#!/usr/bin/env python3
"""Run one C2 command tree under the standing-order footprint/swap watchdog.

This wraps the whole build/check/test command, including blaze daemons reparented
away from it. The heavy lock is taken by the caller, not by this helper.
"""
import os
import subprocess
import sys
import uuid

HARNESS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, HARNESS)
import tracedecay


def main():
    run_id = "c2-exec-" + str(uuid.uuid4())
    env = dict(os.environ, **tracedecay.GUARD_ENV, RUST_BACKTRACE="0", RA_JIT_RUN_ID=run_id)
    watch = tracedecay.Watchdog(HARNESS, run_id=run_id)

    def pids(rows, client_tree):
        owned = {
            pid for pid, _, _, cmd in rows
            if tracedecay.env_of(cmd).get(tracedecay.RUN_ID_ENV) == run_id
        }
        return owned | tracedecay.descendants(rows, owned)

    watch.pids = pids
    child = subprocess.Popen(sys.argv[1:], env=env)
    watch.client = lambda: child.pid
    watch.start()
    try:
        code = child.wait()
    finally:
        watch.stop.set()
        watch.join()
    result = watch.take()
    print("C2 guard:", result, flush=True)
    sys.exit(1 if "watchdog" in result else code)


if __name__ == "__main__":
    main()
