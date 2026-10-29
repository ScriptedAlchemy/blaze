#!/usr/bin/env python3
"""Verify owned empty-library probes against rustc and LLVM's real IR verifier.

Only the harness invokes the reference compiler. The product gets a toolchain
whose compiler, Cargo, rustdoc and rustup entries record and reject invocation.
Successful probes must emit a valid empty LLVM module with the oracle's target
and data layout. Unsupported probes remain explicit producer-build failures.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import norustc


class LLVM:
    def __init__(self, toolchain, library=None):
        if library is None:
            candidates = sorted((toolchain / "lib").glob("libLLVM*"))
            # Rust's short .so file can be a linker script, not a shared library.
            candidates = [path for path in candidates if path.stat().st_size > 4096]
            assert candidates, "the reference toolchain has no LLVM shared library"
            library = candidates[0]
        self.lib = ctypes.CDLL(str(library))
        pointer = ctypes.c_void_p
        signatures = {
            "LLVMContextCreate": (pointer, []),
            "LLVMCreateMemoryBufferWithMemoryRangeCopy": (
                pointer, [ctypes.c_char_p, ctypes.c_size_t, ctypes.c_char_p]),
            "LLVMParseIRInContext": (ctypes.c_int,
                [pointer, pointer, ctypes.POINTER(pointer), ctypes.POINTER(pointer)]),
            "LLVMVerifyModule": (ctypes.c_int,
                [pointer, ctypes.c_int, ctypes.POINTER(pointer)]),
            "LLVMGetTarget": (ctypes.c_char_p, [pointer]),
            "LLVMGetDataLayoutStr": (ctypes.c_char_p, [pointer]),
            "LLVMGetFirstFunction": (pointer, [pointer]),
            "LLVMGetFirstGlobal": (pointer, [pointer]),
            "LLVMDisposeMessage": (None, [pointer]),
            "LLVMDisposeModule": (None, [pointer]),
            "LLVMContextDispose": (None, [pointer]),
        }
        for name, (result, arguments) in signatures.items():
            function = getattr(self.lib, name)
            function.restype = result
            function.argtypes = arguments

    def inspect(self, path):
        data = path.read_bytes()
        assert data, f"empty LLVM artifact: {path}"
        context = self.lib.LLVMContextCreate()
        module = ctypes.c_void_p()
        message = ctypes.c_void_p()
        try:
            buffer = self.lib.LLVMCreateMemoryBufferWithMemoryRangeCopy(
                data, len(data), os.fsencode(path))
            failed = self.lib.LLVMParseIRInContext(
                context, buffer, ctypes.byref(module), ctypes.byref(message))
            if failed:
                detail = ctypes.string_at(message).decode(errors="replace")
                raise AssertionError(f"invalid LLVM IR at {path}: {detail}")
            failed = self.lib.LLVMVerifyModule(module, 2, ctypes.byref(message))
            if failed:
                detail = ctypes.string_at(message).decode(errors="replace")
                raise AssertionError(f"invalid LLVM module at {path}: {detail}")
            assert not self.lib.LLVMGetFirstFunction(module), f"unexpected function in {path}"
            assert not self.lib.LLVMGetFirstGlobal(module), f"unexpected global in {path}"
            return {
                "target": self.lib.LLVMGetTarget(module).decode(),
                "layout": self.lib.LLVMGetDataLayoutStr(module).decode(),
            }
        finally:
            if message:
                self.lib.LLVMDisposeMessage(message)
            if module:
                self.lib.LLVMDisposeModule(module)
            self.lib.LLVMContextDispose(context)


def blocked_toolchain(real, destination, log):
    """Preserve release/source records; every product compiler invocation fails."""
    destination.mkdir()
    (destination / "bin").mkdir()
    for entry in real.iterdir():
        if entry.name != "bin":
            (destination / entry.name).symlink_to(entry, target_is_directory=entry.is_dir())
    for tool in ("rustc", "rustdoc", "cargo", "rustup"):
        path = destination / "bin" / tool
        path.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(tool)
                        + " >> " + shlex.quote(str(log)) + "\nexit 97\n")
        path.chmod(0o755)


def run(command, env, cwd, source):
    return subprocess.run(list(map(str, command)), env=env, cwd=cwd,
                          input=source, text=True, capture_output=True, timeout=90)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blaze", required=True, type=Path)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--llvm-library", type=Path)
    args = parser.parse_args()
    blaze = args.blaze.resolve()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    real = Path(norustc.real_toolchain())
    llvm = LLVM(real, args.llvm_library)
    audit = work / "toolchain"
    audit_log = work / "product-tools.log"
    blocked_toolchain(real, audit, audit_log)
    marker = work / "unsupported"
    env = dict(os.environ)
    for name in list(env):
        if name.startswith(("RA_JIT_", "BLAZE_", "CARGO_PROFILE_", "CARGO_TARGET_")):
            env.pop(name)
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_BOOTSTRAP",
                 "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "SDKROOT",
                 "MACOSX_DEPLOYMENT_TARGET"):
        env.pop(name, None)
    product = dict(env, RUSTUP_TOOLCHAIN=str(audit),
        PATH=str(audit / "bin") + os.pathsep + env["PATH"],
        RUSTC=str(audit / "bin/rustc"), RUSTDOC=str(audit / "bin/rustdoc"),
        CARGO=str(audit / "bin/cargo"), RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
        RA_JIT_STD_FROM="off", RA_JIT_PROC_MACRO_SERVER="0",
        RA_JIT_HOME=str(work / "home"), RA_JIT_CACHE=str(work / "store"),
        BLAZE_PRODUCER_PROBE_FAILURE=str(marker))
    oracle_env = dict(env, RUSTUP_TOOLCHAIN=str(real))
    cases = []
    for edition in ("2015", "2018", "2021", "2024"):
        for name, source in (
            ("empty", ""), ("comments", "// no items\n/* nor here */\n"),
            ("no-std", "#![no_std]\n"), ("docs", "//! Empty library.\n"),
            ("shebang", "#!/usr/bin/env false\n"),
        ):
            cases.append((f"{name}-{edition}", source, "probe", ["--edition=" + edition], True))
    for name in ("_", "0probe", "type", "caf\u00e9"):
        cases.append(("name-" + name, "", name, [], True))
    for name, flags in (
        ("link-arg", ["-C", "link-arg=-fuse-ld=mold"]),
        ("link-args", ["-Clink-args=-lfirst -lsecond"]),
        ("linker", ["-Clinker=nonexistent-linker"]),
    ):
        cases.append((name, "", "probe", flags, True))
    for name, source in (
        ("no-std-args", "#![no_std(unexpected)]"),
        ("no-std-value", '#![no_std = "oops"]'),
        ("feature", "#![feature(never_type)]"),
        ("unknown-attribute", "#![not_an_attribute]"),
        ("outer-attribute", "#[no_std]"), ("semicolon", ";"),
        ("crate-name-attribute", '#![crate_name = "different"]'),
        ("cfg-attribute", "#![cfg_attr(all(), no_std)]"),
        ("nonempty", "pub fn value() -> u8 { 3 }"),
    ):
        cases.append((name, source, "probe", [], False))
    for name in ("", "a-b", "a/b", "a.b", "r#type"):
        cases.append(("bad-name-" + str(len(cases)), "", name, [], False))
    for name, flags in (
        ("unknown-edition", ["--edition=2099"]),
        ("mixed-outputs", ["--emit=llvm-ir,metadata"]),
        ("repeated-outputs", ["--emit=link"]),
        ("explicit-output", ["--emit=llvm-ir=explicit.ll"]),
        ("extra-filename", ["-Cextra-filename=-suffix"]),
        ("invalid-opt", ["-Copt-level=invalid"]),
        ("invalid-panic", ["-Cpanic=invalid"]),
        ("invalid-debug", ["-Cdebuginfo=invalid"]),
        ("invalid-cfg", ["--cfg=bad("]),
    ):
        cases.append((name, "", "probe", flags, False))
    rows = []
    try:
        for name, source, crate, flags, accepted in cases:
            root = work / name
            root.mkdir()
            outputs = [root / "owned", root / "oracle"]
            for output in outputs:
                output.mkdir()
            common = ["--crate-name", crate, "--crate-type=lib", "--emit=llvm-ir", *flags]
            marker.unlink(missing_ok=True)
            ours = run([blaze, "producer-rustc", *common, "--out-dir", outputs[0], "-"],
                       product, root, source)
            reference = run([real / "bin/rustc", *common, "--out-dir", outputs[1], "-"],
                            oracle_env, root, source)
            for kind, result in (("owned", ours), ("oracle", reference)):
                (root / (kind + ".log")).write_text(result.stdout + result.stderr)
            row = {"case": name, "owned": ours.returncode, "oracle": reference.returncode,
                   "unsupported": marker.exists()}
            rows.append(row)
            if accepted:
                assert ours.returncode == reference.returncode == 0, (row, ours.stderr, reference.stderr)
                assert not marker.exists(), (row, marker.read_text())
                made = llvm.inspect(outputs[0] / (crate + ".ll"))
                expected = llvm.inspect(outputs[1] / (crate + ".ll"))
                assert made == expected, (name, made, expected)
                row["module"] = made
            else:
                assert ours.returncode != 0, f"unproven probe succeeded: {row}"
                assert not list(outputs[0].glob("*.ll")), f"refused probe emitted LLVM IR: {row}"
                if reference.returncode == 0:
                    assert marker.exists() and marker.read_text().strip(), (
                        "unsupported operation became a negative feature result", row)
            print(f"{name}: owned={ours.returncode} oracle={reference.returncode}", flush=True)
    finally:
        calls = audit_log.read_text().splitlines() if audit_log.exists() else []
        (work / "results.json").write_text(json.dumps({"rows": rows, "tool_calls": calls}, indent=2) + "\n")
    assert not calls, calls
    print(f"{len(rows)} empty-probe cases; all positive artifacts verified by LLVM; zero product tool calls")


if __name__ == "__main__":
    main()
