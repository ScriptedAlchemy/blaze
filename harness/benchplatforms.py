#!/usr/bin/env python3
"""benchplatforms.py: the platforms the README's benchmarks are measured on, and the runner of each.

    benchplatforms.py matrix [--bun]

Benchmarks use standard GitHub-hosted runners. BENCH_RUNNERS selects platform slugs
(comma-separated); no selection leaves these optional measurements disabled.
BENCH_RUNNER=true retains the legacy Linux x86-64 selection. Hosted measurements
are noisy and must not be compared directly with dedicated-machine baselines.
"""
import json, os, sys

# Short macOS work paths keep daemon sockets within the 104-byte limit.
PLATFORMS = [
    dict(slug="macos-arm64", name="macOS arm64", label="macos-15", work="/tmp/ra-jit-bench"),
    dict(slug="linux-aarch64", name="Linux aarch64", label="ubuntu-24.04-arm", work=""),
    dict(slug="linux-x86-64", name="Linux x86-64", label="ubuntu-24.04", work=""),
]
NAMES = [p["name"] for p in PLATFORMS]


def platform(result):
    """The platform of a result of bench.py, from its host: one of NAMES, or what the host says."""
    host = result.get("host") or {}
    system = {"Linux": "Linux", "Darwin": "macOS"}.get(host.get("os"), host.get("os") or "unknown OS")
    arch = host.get("arch") or ""
    if arch in ("x86_64", "AMD64"):
        arch = "x86-64"
    elif arch in ("arm64", "aarch64"):
        arch = "arm64" if system == "macOS" else "aarch64"
    return f"{system} {arch}".strip()


def slug(name):
    return name.replace(" ", "-").lower()


def registered(env):
    named = {s.strip() for s in (env.get("BENCH_RUNNERS") or "").split(",") if s.strip()}
    if env.get("BENCH_RUNNER") == "true":
        named.add("linux-x86-64")
    return [p for p in PLATFORMS if p["slug"] in named]


def main():
    argv = sys.argv[1:]
    if argv[:1] != ["matrix"]:
        raise SystemExit(__doc__)
    found = registered(os.environ)
    lines = [f"matrix={json.dumps({'include': found})}", f"any={'true' if found else 'false'}"]
    out = os.environ.get("GITHUB_OUTPUT")
    with open(out, "a") if out else sys.stdout as f:
        f.write("\n".join(lines) + "\n")
    skipped = [p["name"] for p in PLATFORMS if p not in found]
    if skipped:
        print(f"not selected, so not run (the README keeps their last numbers): {', '.join(skipped)}", file=sys.stderr)


if __name__ == "__main__":
    main()
