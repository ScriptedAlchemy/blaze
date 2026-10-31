"""Reproduce the reviewed stack repair and publish only temporary staging refs.

The PR refs are deliberately not changed by this workflow. An explicit leased
update of each PR ref follows after its receipt and tree have been inspected.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path.cwd()
C2 = "4b9546e8dfde4601ab99efc47b58da62b981a7c6"
C4 = "0b833a84da5b9b4c9916ffbf4474e2405f52ae65"
CI = "6ae5e5cb512bb34afaf5a443e264d332bc1dee6b"
MERGE_TREE = "23aee0a033b3f0b23f2874625768a19bc8a3acc4"
FIX_TREE = "7d3eb42024cfbf064f120369958705b68c1773b6"
PATCH = ROOT / ".github/repair/producer-policy.patch"
TEMP = Path(os.environ["RUNNER_TEMP"])


def run(*args, cwd=ROOT, check=True):
    return subprocess.run(args, cwd=cwd, check=check, text=True)


def git(*args, cwd=ROOT):
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


expected = {"dist/c2-buildspec": C2, "dist/c4-keys": C4, "ci-redesign": CI}
for branch, sha in expected.items():
    current = git("ls-remote", "origin", "refs/heads/" + branch).split()[0]
    require(current == sha, f"{branch} advanced: expected {sha}, got {current}")
    run("git", "fetch", "--no-tags", "origin", sha)
run("git", "config", "user.name", "Zack Jackson")
run("git", "config", "user.email", "25274700+ScriptedAlchemy@users.noreply.github.com")
c2 = TEMP / "blaze-repair-c2"
ci = TEMP / "blaze-repair-ci"
run("git", "worktree", "add", "--detach", str(c2), C2)
merged = run("git", "merge", "--no-commit", "--no-ff", C4, cwd=c2, check=False)
require(merged.returncode == 1, "expected the reviewed C4 compiler-query conflict")
conflicts = git("diff", "--name-only", "--diff-filter=U", cwd=c2).splitlines()
require(conflicts == ["crates/ra-jit/src/answers.rs"], f"unexpected conflicts: {conflicts}")
answers = c2 / conflicts[0]
text = answers.read_text()
require(text.count("<<<<<<< HEAD\n") == 2, "compiler-query conflict layout changed")
while "<<<<<<< HEAD\n" in text:
    start = text.index("<<<<<<< HEAD\n")
    split = text.index("=======\n", start)
    end = text.index("\n", text.index(">>>>>>> ", split)) + 1
    text = text[:start] + text[start + len("<<<<<<< HEAD\n"):split] + text[end:]
old = "let found = toolchain_rustc(compiler, command.get_current_dir(), &var);"
require(text.count(old) == 1, "explicit compiler resolution changed")
text = text.replace(old, "let found = toolchain_rustc(compiler, command.get_current_dir(), &var).ok();")
old = "None => toolchain_rustc(command.get_program(), command.get_current_dir(), &var)\n            .map("
require(text.count(old) == 1, "Cargo compiler resolution changed")
text = text.replace(old, "None => toolchain_rustc(command.get_program(), command.get_current_dir(), &var)\n            .ok()\n            .map(")
answers.write_text(text)
run("git", "add", "crates/ra-jit/src/answers.rs", cwd=c2)
run("git", "diff", "--cached", "--check", cwd=c2)
require(git("write-tree", cwd=c2) == MERGE_TREE, "C4 merge differs from the locally reviewed tree")
run("git", "commit", "-m", "Merge current C4 producer fixes into saved BuildSpec execution", "-m", "Keep the owned Cargo metadata front door while adapting compiler resolution to the strict Result-returning toolchain API. Preserve the C4 producer cfg, snapshot and fingerprint fixes.", cwd=c2)
run("git", "apply", "--index", str(PATCH), cwd=c2)
require(git("write-tree", cwd=c2) == FIX_TREE, "producer repair differs from the locally reviewed tree")
# Format only the new implementation; other merge content must remain unchanged.
run("rustfmt", "+1.99.0", "--edition", "2024", "crates/ra-jit/src/cargo/own_script.rs", cwd=c2)
require(set(git("diff", "--name-only", cwd=c2).splitlines()) <= {"crates/ra-jit/src/cargo/own_script.rs"}, "formatting changed an unrelated file")
run("git", "add", "crates/ra-jit/src/cargo/own_script.rs", cwd=c2)
run("git", "diff", "--cached", "--check", cwd=c2)
run("cargo", "+1.99.0", "fmt", "-p", "ra-jit", "--", "--check", cwd=c2)
run(sys.executable, "-I", "-B", "harness/dist/test_saved_spec.py", cwd=c2)
run(sys.executable, "-I", "-B", "harness/norustc_test.py", cwd=c2)
run("git", "commit", "-m", "Honor saved compiler policy when launching owned producers", "-m", "Use the explicit replay environment for backend, lowering and memory policy instead of restoring ambient RA_JIT settings. Remove BLAZE aliases before child startup, retain replay admission, and preserve producer diagnostics after an early stdin rejection. Cover both backend selections and conflicting alias controls.", cwd=c2)
c2_sha = git("rev-parse", "HEAD", cwd=c2)
run("git", "worktree", "add", "--detach", str(ci), CI)
run("git", "merge", "--no-ff", "-m", "Merge repaired BuildSpec execution and current C4 into CI redesign", c2_sha, cwd=ci)
run("git", "diff", "--check", "HEAD^", "HEAD", cwd=ci)
ci_sha = git("rev-parse", "HEAD", cwd=ci)
require(not git("diff", "--name-only", c2_sha, ci_sha, "--", "crates", "vendor", cwd=ci), "CI integration unexpectedly changes compiler source")
for branch, sha in expected.items():
    current = git("ls-remote", "origin", "refs/heads/" + branch).split()[0]
    require(current == sha, f"{branch} advanced during preparation; publish nothing")
run_id = os.environ["GITHUB_RUN_ID"]
c2_ref = f"repair/staged-c2-{run_id}"
ci_ref = f"repair/staged-ci-{run_id}"
run("git", "push", "--atomic", "origin", f"{c2_sha}:refs/heads/{c2_ref}", f"{ci_sha}:refs/heads/{ci_ref}")
receipt = {"expected": expected, "c2": {"sha": c2_sha, "tree": git("rev-parse", "HEAD^{tree}", cwd=c2), "staging_ref": c2_ref}, "ci": {"sha": ci_sha, "tree": git("rev-parse", "HEAD^{tree}", cwd=ci), "staging_ref": ci_ref}, "checks": ["reviewed merge and patch tree equality", "rustfmt and cargo fmt", "saved-spec Python tests", "execution-audit Python tests", "CI compiler source equality"], "rust_tests": "separate validation job; not yet claimed passing", "pr_refs_updated": False}
(TEMP / "blaze-repair-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps(receipt, indent=2))
with open(os.environ["GITHUB_OUTPUT"], "a") as output:
    output.write(f"c2_sha={c2_sha}\nci_sha={ci_sha}\n")
