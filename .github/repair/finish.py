"""Finish the reviewed repair without replacing concurrent loader or compiler fixes."""
from pathlib import Path
import hashlib

path = Path('.github/repair/prepare.py')
raw = path.read_bytes()
blob = hashlib.sha1(b'blob ' + str(len(raw)).encode() + b'\0' + raw).hexdigest()
assert blob == '909dc524ca6c3bac4539821f91bcc045f025dce7', ('preparation source changed', blob)
source = raw.decode()

old_merge = 'run("git", "merge", "--no-ff", "-m", "Merge current C4 producer fixes into saved BuildSpec execution", c4_sha, cwd=c2)'
new_merge = '''merged = run("git", "merge", "--no-commit", "--no-ff", c4_sha, cwd=c2, check=False)
if merged.returncode:
    conflicts = git("diff", "--name-only", "--diff-filter=U", cwd=c2).splitlines()
    require(merged.returncode == 1 and conflicts == ["crates/ra-jit/src/main.rs"], f"unexpected conflicts: {conflicts}")
    main = c2 / conflicts[0]
    text = main.read_text()
    count = 0
    while "<<<<<<< HEAD\\n" in text:
        start = text.index("<<<<<<< HEAD\\n")
        middle = text.index("=======\\n", start)
        marker = text.index(">>>>>>> ", middle)
        end = text.index("\\n", marker) + 1
        ours = text[start + len("<<<<<<< HEAD\\n"):middle]
        theirs = text[middle + len("=======\\n"):marker]
        environment = "        self.loaded.set_proc_macro_env(self.macro_env());\\n"
        require(theirs.startswith(environment), "unexpected incoming loader conflict")
        if "for (before, root, generation) in generations" in ours:
            require(theirs == environment, "unexpected generation reconciliation")
            resolved = ours + environment
        elif "generation, manifest" in ours:
            require("for (&at, (workspace, debug, config, given, own, generation))" in theirs,
                    "unexpected manifest reconciliation")
            resolved = environment + ours
        else:
            raise RuntimeError("unreviewed loader conflict")
        text = text[:start] + resolved + text[end:]
        count += 1
    require(count == 2 and "<<<<<<<" not in text and ">>>>>>>" not in text,
            "loader conflict layout changed")
    main.write_text(text)
    run("git", "add", "crates/ra-jit/src/main.rs", cwd=c2)
run("git", "diff", "--cached", "--check", cwd=c2)
run("git", "commit", "-m", "Merge current C4 while preserving C2 manifests and macro generations", cwd=c2)'''
assert source.count(old_merge) == 1
source = source.replace(old_merge, new_merge)

start = source.index('before = implementation.read_text()')
end = source.index('\n# Test the actual production command builder', start)
source = source[:start] + '''after = implementation.read_text()
require(digest(policy_function(after)) == FIXED_POLICY,
        "the already-pushed producer policy changed; inspect it")
require("crate::child::feed(child, private)" in after,
        "concurrent producer diagnostics are missing")
before = git("show", "cfb265247a1fbb15e2f00b0447023a36165103eb:crates/ra-jit/src/cargo/own_script.rs", cwd=c2) + "\\n"
require(digest(policy_function(before)) == OLD_POLICY, "baseline producer policy changed")
''' + source[end:]

# The implementation is already on PR18. Commit only formatter corrections,
# rather than inventing a duplicate implementation commit.
start = source.index('run("git", "commit", "-m", "Honor saved compiler policy')
end = source.index('\nc2_sha = ', start)
source = source[:start] + '''if git("diff", "--cached", "--name-only", cwd=c2):
    run("git", "commit", "-m", "Format the saved producer policy regression", cwd=c2)
''' + source[end:]

# Publish the tested C4 fix independently, so a later compiler test failure
# cannot lose its successful regression receipt. Actual PR refs stay untouched.
needle = 'c4_sha = git("rev-parse", "HEAD", cwd=c4)'
assert source.count(needle) == 1
source = source.replace(needle, needle + '''
c4_ref = "repair/staged-c4-" + os.environ["GITHUB_RUN_ID"]
run("git", "push", "origin", f"{c4_sha}:refs/heads/{c4_ref}")
print("Tested C4 staging commit: " + c4_sha, flush=True)
''')
exec(compile(source, str(path), 'exec'), {'__name__': '__main__', '__file__': str(path)})
