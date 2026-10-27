#!/usr/bin/env python3
"""Apply the PR12 native-input repair to the exact reviewed source, never a moved head."""
from pathlib import Path
import hashlib
import sys

ROOT = Path.cwd()
MAIN = ROOT / 'crates/ra-jit/src/main.rs'
HARNESS = ROOT / 'harness/leasebarrier.py'


def blob_sha(data: bytes) -> str:
    return hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()


def replace_once(text: str, old: str, new: str) -> str:
    if text.count(old) != 1:
        raise RuntimeError(f'Expected exactly one source anchor: {old[:90]!r}')
    return text.replace(old, new, 1)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit('usage: apply.py /path/to/native_inputs.rs')
    source = MAIN.read_bytes()
    if blob_sha(source) != '08ceeaaa213c029dbdf4452361f01e7c00a97c5b':
        raise RuntimeError('main.rs changed since the reviewed PR12 head; reconcile instead of overwriting')
    harness_bytes = HARNESS.read_bytes()
    if blob_sha(harness_bytes) != 'e8b0d048c787e583f8a005120de92d216378a592':
        raise RuntimeError('leasebarrier.py changed; reconcile its new tests before applying')
    text = source.decode()
    text = replace_once(text, 'mod mirdump;\n', 'mod mirdump;\nmod native_inputs;\n')
    text = replace_once(text,
        '/// G\'s line. `None` when it is not one, or has it already.',
        '/// G\'s line, regenerated from the current native directives before every link.')
    text = replace_once(text,
        'args.exe.as_ref().filter(|_| args.patch.is_some() && args.link_cmd.is_none())',
        'args.exe.as_ref().filter(|_| args.patch.is_some())')
    anchor = '                    // One build is for one process: its variables are what `env!` reads.\n'
    text = replace_once(text, anchor, '''                    // Script directives and Rust crate identity can stay the same while a
                    // native archive changes. Check this before either no-op fast path.
                    match native_inputs::stale(&db, &programs) {
                        Ok(false) => {}
                        Ok(true) if !clients.is_empty() => {
                            later.push((request, asked, stream));
                            continue;
                        }
                        Ok(true) => {
                            builds.settle(&mut db, &mut programs)?;
                            if let Err(error) = native_inputs::invalidate(&db, &mut programs) {
                                let _ = writeln!(&stream, "failed: {error:#}");
                                continue;
                            }
                            lock(&as_built).clear();
                        }
                        Err(error) => {
                            let _ = writeln!(&stream, "failed: {error:#}");
                            continue;
                        }
                    }
''' + anchor)
    anchor = '        let args = &Args { out, patch, link_cmd, splice, ..args.clone() };\n'
    text = replace_once(text, anchor, anchor + '''        // Capture the effective output's identity, not another checkout's or alias's.
        // Recheck here too: a native input may have changed after request admission.
        let native_link = native_inputs::begin(db, args)?;
        if native_link.as_ref().is_some_and(|link| link.changed()) {
            if let Some(binary) = args.splice.as_ref().or(args.exe.as_ref()) {
                native_inputs::forget_binary(binary);
            }
            known.forget_base();
            *whole = None;
            unbuilt.stale = true;
            unbuilt.unlinked = true;
        }
''')
    anchor = '''        // What the caller has to load to be up to date: the functions that differ from the
        // first object when the edit could be a patch, and all of them when it could not.
'''
    text = replace_once(text, anchor, '''        // A failed, compile-only or deferred link never publishes a reusable native identity.
        if link_it && !defer {
            if let Some(native_link) = native_link {
                native_link.commit()?;
            }
        }
''' + anchor)
    harness = harness_bytes.decode()
    harness = replace_once(harness,
        'sc.check_programs("B", kept_b, exp_b, "b", gap_native=None if cross else ord("a"))',
        'sc.check_programs("B", kept_b, exp_b, "b")')
    harness = harness.replace('self.gaps.append(', 'self.fail(').replace('sc.gaps.append(', 'sc.fail(')
    harness = replace_once(harness, 'for gen_ in ("a", "b"):', 'for gen_ in ("a", "b", "a"):')
    harness = replace_once(harness, 'for gen_ in ("a", "c"):', 'for gen_ in ("a", "c", "a"):')
    new_case = r'''def native_only(sc, source_sha):
    """Same Rust inputs and archive path, changing only the script's native input bytes."""
    original_project = sc.project
    current = [97]

    def project(ws=None):
        original_project(ws)
        ws = ws or sc.ws
        script = os.path.join(ws, "build.rs")
        text = open(script).read()
        text = text.replace('let value = gen_.bytes().next().unwrap_or(b\'?\') as i32;',
                            'let value: i32 = fs::read_to_string("native-value.txt").unwrap().trim().parse().unwrap();')
        text = text.replace('println!("cargo:rerun-if-env-changed=GEN");',
                            'println!("cargo:rerun-if-env-changed=GEN");\n    println!("cargo:rerun-if-changed=native-value.txt");')
        with open(script, "w") as f:
            f.write(text)
        with open(os.path.join(ws, "native-value.txt"), "w") as f:
            f.write(str(current[0]))

    sc.project = project
    extra = {"GEN_ARCHIVE": "same"}
    expected = {}
    for value in (97, 98):
        current[0] = value
        expected[value] = sc.oracle("a", extra)
    current[0] = 97
    sc.project()
    for index, value in enumerate((97, 98, 97)):
        with open(os.path.join(sc.ws, "native-value.txt"), "w") as f:
            f.write(str(value))
        p = sc.client("a", GEN="a", **extra)
        sc.finish(p, f"native-{index}-{value}")
        if p.returncode != 0:
            sc.fail(f"native-only {value}: rc={p.returncode}: {p.err[-600:]}")
            return
        sc.check_programs(f"native-{index}", sc.keep(f"native-{index}"), expected[value], "a")
    archive = os.path.join(sc.run_dir(), "libgen.a")
    valid, stamp = open(archive, "rb").read(), os.stat(archive).st_mtime_ns
    with open(archive, "wb") as f:
        f.write(b"not a native archive")
    failed = sc.client("a", GEN="a", **extra)
    sc.finish(failed, "broken-native-link")
    if failed.returncode == 0:
        sc.fail("a corrupt native archive was accepted as an unchanged successful build")
    with open(archive, "wb") as f:
        f.write(valid)
    os.utime(archive, ns=(stamp, stamp))
    retry = sc.client("a", GEN="a", **extra)
    sc.finish(retry, "restored-native-link")
    if retry.returncode != 0:
        sc.fail(f"native restore after failed link: rc={retry.returncode}: {retry.err[-600:]}")
    sc.check_programs("native-restored", sc.keep("native-restored"), expected[97], "a")
    runs = sc.runs()
    noop = sc.client("a", GEN="a", **extra)
    sc.finish(noop, "native-noop")
    if noop.returncode != 0 or sc.runs() != runs:
        sc.fail("native no-op failed or reran its build script")
    sc.check_programs("native-noop", sc.keep("native-noop"), expected[97], "a")


'''
    harness = replace_once(harness, 'SCENARIOS = {\n', new_case + 'SCENARIOS = {\n    "native-only": native_only,\n')
    harness = harness.replace('What differs from cargo is recorded as a gap.', 'A mismatch with Cargo fails the scenario.')
    harness = harness.replace('gap, not a failure of the lease scenarios.', 'failure, including in the lease scenarios.')
    import ast
    ast.parse(harness)
    MAIN.write_text(text)
    (MAIN.parent / 'native_inputs.rs').write_bytes(Path(sys.argv[1]).read_bytes())
    HARNESS.write_text(harness)
    print('Applied native input identity, no-op invalidation, full-relink and strict runtime regressions.')


if __name__ == '__main__':
    main()
