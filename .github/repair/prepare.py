"""Stage fast-forward stack repairs; never move the actual PR refs.

Concurrent branch work is retained. Every merge must be clean, the producer
function must match its reviewed input, and actual PR updates use a separate
expected-SHA lease after the returned commits have been inspected.
"""
import ast
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import textwrap

ROOT = Path.cwd()
TEMP = Path(os.environ["RUNNER_TEMP"])
ANCHORS = {
    "dist/c2-buildspec": "cfb265247a1fbb15e2f00b0447023a36165103eb",
    "dist/c4-keys": "7a3dfa152677fba357dc767140ff213eb74f6497",
    "ci-redesign": "6ae5e5cb512bb34afaf5a443e264d332bc1dee6b",
}
OLD_POLICY = "9c6750064e06c8a902bfa687b170d0afbb6653123884caabce2eada621dade2b"
FIXED_POLICY = "3c43416c1f8e7bce012520d65ac181dd0105e88d83d678205cc2d33390949325"


def run(*args, cwd=ROOT, check=True):
    return subprocess.run(args, cwd=cwd, check=check, text=True)


def git(*args, cwd=ROOT):
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def worktree(name, sha):
    path = TEMP / name
    run("git", "worktree", "add", "--detach", str(path), sha)
    return path


def policy_function(source):
    start = source.index("fn build_command(")
    return source[start:source.index("\n/// Builds a script", start)]


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


expected = {}
for branch, anchor in ANCHORS.items():
    current = git("ls-remote", "origin", "refs/heads/" + branch).split()[0]
    run("git", "fetch", "--no-tags", "origin", current)
    run("git", "merge-base", "--is-ancestor", anchor, current)
    expected[branch] = current
print("Captured branch leases: " + json.dumps(expected), flush=True)
run("git", "config", "user.name", "Zack Jackson")
run("git", "config", "user.email", "25274700+ScriptedAlchemy@users.noreply.github.com")

# The newly added private-removed row removes the key; the following private-raw
# row used to remove it again without a default and fail before running a build.
c4 = worktree("blaze-repair-c4", expected["dist/c4-keys"])
fixture = c4 / "harness/dist/crossmachine.py"
source = fixture.read_text()
old = r'''                env.pop("PR15_PRIVATE")
                private = {} if step == "private-removed" else {"PR15_PRIVATE": "raw-\udcff-value"}
                env.update(private)'''
fixed_old = old.replace('env.pop("PR15_PRIVATE")', 'env.pop("PR15_PRIVATE", None)')
if old in source:
    scope = {"env": {"PR15_PRIVATE": "old"}}
    transition = compile(textwrap.dedent(old), "existing macro environment transition", "exec")
    scope["step"] = "private-removed"
    exec(transition, scope)
    scope["step"] = "private-raw"
    try:
        exec(transition, scope)
    except KeyError as error:
        require(error.args == ("PR15_PRIVATE",), "unexpected transition error")
        print("Reproduced: the original private-raw transition raises KeyError", flush=True)
    else:
        raise RuntimeError("original transition did not reproduce")
else:
    require(fixed_old in source, "the macro environment transition changed; inspect it")
    old = fixed_old
helper = r'''def macro_private_environment(env, step):
    """Change the witness's request environment, including repeated removals."""
    env.pop("PR15_PRIVATE", None)
    private = {} if step == "private-removed" else {"PR15_PRIVATE": "raw-\udcff-value"}
    env.update(private)
    return private


'''
require(source.count(old) == 1 and "def macro_private_environment(" not in source,
        "unexpected macro environment helper")
source = source.replace(old, "                private = macro_private_environment(env, step)")
require(source.count("def macro_content_report(") == 1, "macro witness entry changed")
source = source.replace("def macro_content_report(", helper + "def macro_content_report(")
marker = '    check("PM environment step must edit and reload",'
require(source.count(marker) == 1, "macro witness self-test changed")
tests = r'''    private_env = {"PR15_PRIVATE": "old", "KEEP": "kept"}
    check("PM private removal clears the request", macro_private_environment(private_env, "private-removed") == {} and private_env == {"KEEP": "kept"})
    check("PM raw value after removal is preserved", macro_private_environment(private_env, "private-raw") == {"PR15_PRIVATE": "raw-\udcff-value"} and private_env == {"KEEP": "kept", "PR15_PRIVATE": "raw-\udcff-value"})
    check("PM repeated removal is safe", macro_private_environment(private_env, "private-removed") == {} and macro_private_environment(private_env, "private-removed") == {} and private_env == {"KEEP": "kept"})
'''
source = source.replace(marker, tests + marker)
ast.parse(source)
fixture.write_text(source)
run(sys.executable, "-I", "-B", "harness/dist/crossmachine.py", "--self-test", cwd=c4)
run("git", "add", "harness/dist/crossmachine.py", cwd=c4)
run("git", "diff", "--cached", "--check", cwd=c4)
run("git", "commit", "-m", "Fix consecutive private environment transitions in the macro witness", "-m", "The private-removed row leaves PR15_PRIVATE absent. The following private-raw row must not pop the missing key without a default. Exercise removal, raw-byte restoration and repeated removal through the same helper the runtime witness uses.", cwd=c4)
c4_sha = git("rev-parse", "HEAD", cwd=c4)

# Current C2 already contains the concurrent strict toolchain resolution and
# child::feed changes. Never replace those with the older conflict resolution.
c2 = worktree("blaze-repair-c2", expected["dist/c2-buildspec"])
run("git", "merge", "--no-ff", "-m", "Merge current C4 producer fixes into saved BuildSpec execution", c4_sha, cwd=c2)
implementation = c2 / "crates/ra-jit/src/cargo/own_script.rs"
before = implementation.read_text()
require(digest(policy_function(before)) == OLD_POLICY,
        "producer command policy advanced; inspect instead of overwriting it")
require("crate::child::feed(child, private)" in before, "concurrent producer diagnostics missing")
patch = (ROOT / ".github/repair/producer-policy.patch").read_text()
require(patch.count("diff --git ") == 1, "unexpected patch files")
# Drop the obsolete diagnostic hunks; child::feed already implements that fix.
policy = patch[:patch.index("@@")] + patch[patch.index("@@ -320,"):]
policy_patch = TEMP / "producer-policy-only.patch"
policy_patch.write_text(policy)
run("git", "apply", "--index", str(policy_patch), cwd=c2)
after = implementation.read_text()
require(digest(policy_function(after)) == FIXED_POLICY, "producer policy differs from the reviewed function")
require("crate::child::feed(child, private)" in after, "producer diagnostics were overwritten")

# Execute the exact production command builder and its new test independently
# of the large compiler dependency graph. The full crate is tested separately.
test_start = after.index("    #[test]\n    fn saved_producer_command_keeps_the_captured_compiler_policy()")
test_end = after.index("    #[test]\n    fn a_variable_read_as_rustc_would_read_it()", test_start)
regression = after[test_start:test_end]
prefix = '''#![allow(dead_code)]
mod own_macro { pub const SERVER: &str = "RA_JIT_PROC_MACRO_SERVER"; }
mod producer {
use std::{collections::BTreeMap, ffi::{OsStr, OsString}, path::{Path, PathBuf}, process::{Command, Stdio}};
struct Made<'a> { name: &'a str, proc_macro: bool, exe: PathBuf, dep_info: PathBuf }
'''
for label, body in [("before", policy_function(before)), ("after", policy_function(after))]:
    code = TEMP / ("producer-policy-" + label + ".rs")
    binary = TEMP / ("producer-policy-" + label)
    code.write_text(prefix + body + "\n#[cfg(test)] mod tests { use super::*;\n" + regression + "\n}\n}\n")
    run("rustc", "+1.99.0", "--edition=2024", "--test", "--crate-name", "producer_policy", str(code), "-o", str(binary))
    outcome = subprocess.run([str(binary), "--nocapture"], text=True, capture_output=True, timeout=30)
    print(label + " production command builder:\n" + outcome.stdout + outcome.stderr, flush=True)
    if label == "before":
        require(outcome.returncode != 0 and "1 failed" in outcome.stdout,
                "the original command builder did not reproduce the regression")
    else:
        require(outcome.returncode == 0 and "1 passed" in outcome.stdout,
                "the repaired command builder failed its regression")
run("rustfmt", "+1.99.0", "--edition", "2024", "crates/ra-jit/src/cargo/own_script.rs", cwd=c2)
require(set(git("diff", "--name-only", cwd=c2).splitlines()) <= {"crates/ra-jit/src/cargo/own_script.rs"}, "formatting changed an unrelated file")
run("git", "add", "crates/ra-jit/src/cargo/own_script.rs", cwd=c2)
run("git", "diff", "--cached", "--check", cwd=c2)
run("cargo", "+1.99.0", "fmt", "-p", "ra-jit", "--", "--check", cwd=c2)
run(sys.executable, "-I", "-B", "harness/dist/test_saved_spec.py", cwd=c2)
run(sys.executable, "-I", "-B", "harness/norustc_test.py", cwd=c2)
run("git", "commit", "-m", "Honor saved compiler policy when launching owned producers", "-m", "Use the explicit replay environment for backend, lowering and memory policy instead of restoring ambient RA_JIT settings. Remove BLAZE aliases before child startup and retain replay admission. The extracted production command-builder regression fails before and passes after for both backends. Preserve concurrent child::feed and strict toolchain-resolution fixes.", cwd=c2)
c2_sha = git("rev-parse", "HEAD", cwd=c2)
ci = worktree("blaze-repair-ci", expected["ci-redesign"])
run("git", "merge", "--no-ff", "-m", "Merge repaired BuildSpec execution and current C4 into CI redesign", c2_sha, cwd=ci)
run("git", "diff", "--check", "HEAD^", "HEAD", cwd=ci)
ci_sha = git("rev-parse", "HEAD", cwd=ci)
require(not git("diff", "--name-only", c2_sha, ci_sha, "--", "crates", "vendor", cwd=ci), "CI integration unexpectedly changes compiler source")
run_id = os.environ["GITHUB_RUN_ID"]
heads = {"c4": (c4, c4_sha), "c2": (c2, c2_sha), "ci": (ci, ci_sha)}
staged = {name: {"sha": sha, "tree": git("rev-parse", "HEAD^{tree}", cwd=path), "staging_ref": f"repair/staged-{name}-{run_id}"} for name, (path, sha) in heads.items()}
run("git", "push", "--atomic", "origin", *[f"{data['sha']}:refs/heads/{data['staging_ref']}" for data in staged.values()])
receipt = {"expected": expected, **staged, "checks": ["all integration merges clean", "reviewed producer function SHA256 before and after", "original macro transition failure reproduced", "macro witness self-tests including repeated transitions", "extracted production Rust command-builder test fails before and passes after", "rustfmt and cargo fmt", "saved-spec Python tests", "execution-audit Python tests", "CI compiler source equality"], "rust_full_suite": "separate validation job; not yet claimed passing", "pr_refs_updated": False}
(TEMP / "blaze-repair-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps(receipt, indent=2), flush=True)
with open(os.environ["GITHUB_OUTPUT"], "a") as output:
    output.write(f"c2_sha={c2_sha}\nci_sha={ci_sha}\n")
