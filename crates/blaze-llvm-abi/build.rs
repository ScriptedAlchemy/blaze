//! Link only the verified LLVM prefix and compile blaze's existing pass-manager wrapper.
//! No Rust build dependencies: native tool answers, not a registry package, own this seam.
#[cfg(feature = "native")]
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

// Share the existing pin rule, including link-manifest support for cross compilation.
#[cfg(feature = "native")]
include!("../../vendor/llvm-sys/pin.rs");

#[cfg(feature = "native")]
fn output(command: &mut Command) -> String {
    let result = command.output().unwrap_or_else(|e| panic!("{command:?}: {e}"));
    assert!(result.status.success(), "{command:?}: {}", String::from_utf8_lossy(&result.stderr));
    String::from_utf8(result.stdout).expect("native tool output is UTF-8").trim().to_owned()
}

#[cfg(feature = "native")]
fn tool(variable: &str, default: &str, host: &str, target: &str) -> OsString {
    let scope = if host == target { "HOST" } else { "TARGET" };
    for key in [
        format!("{variable}_{target}"),
        format!("{variable}_{}", target.replace('-', "_")),
        format!("{scope}_{variable}"),
        variable.to_owned(),
    ] {
        println!("cargo::rerun-if-env-changed={key}");
        if let Some(value) = env::var_os(&key).filter(|v| !v.is_empty()) {
            return value;
        }
    }
    assert!(
        host == target,
        "cross-compiling {host} to {target} needs an explicit {variable}_{target} tool"
    );
    default.into()
}

#[cfg(not(feature = "native"))]
fn main() {}

#[cfg(feature = "native")]
fn main() {
    let Some(config) = pinned_llvm_config() else { return };
    let prefix = fs::canonicalize(env::var_os(PIN_PREFIX).expect("verified prefix"))
        .expect("canonical verified prefix");
    let marker = fs::read_to_string(prefix.join(PIN_MARKER)).expect("verified marker");
    let host = env::var("HOST").expect("cargo HOST");
    let target = env::var("TARGET").expect("cargo TARGET");
    let os = env::var("CARGO_CFG_TARGET_OS").expect("cargo target OS");
    assert!(
        matches!(os.as_str(), "linux" | "macos"),
        "blaze-llvm-abi: unsupported target {target}"
    );
    if let Ok(built_target) = pin_field(&marker, "target") {
        assert_eq!(built_target, target, "LLVM prefix target must match the linked target");
    }
    let query = |args: &[&str]| match pin_answer(&marker, &prefix, false, args) {
        Some(answer) => answer.unwrap_or_else(|e| panic!("{}: {e}", prefix.display())),
        None => output(Command::new(&config).args(args)),
    };
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo manifest"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo output"));
    let wrapper = root.join("../../vendor/llvm-sys/wrappers/passes.cpp");
    let target_wrapper = root.join("target.cpp");
    for file in [
        root.join("abi.h"),
        root.join("abi-checks.cpp"),
        root.join("src/lib.rs"),
        wrapper.clone(),
        target_wrapper.clone(),
        root.join("../../vendor/llvm-sys/pin.rs"),
    ] {
        println!("cargo::rerun-if-changed={}", file.display());
    }
    println!("cargo::rerun-if-changed={}", prefix.join("include").display());
    let cxx = tool("CXX", "c++", &host, &target);
    let ar = tool("AR", "ar", &host, &target);
    let flags = query(&["--cxxflags"]);
    let flags: Vec<_> = flags.split_whitespace().collect();
    // Compile both wrappers with the owned declarations and checks for all selected signatures.
    let unit = out.join("passes.cpp");
    fs::write(
        &unit,
        format!(
            "#include {:?}\n#include {target_wrapper:?}\n#include {wrapper:?}\n",
            root.join("abi-checks.cpp"),
        ),
    )
    .expect("write wrapper translation unit");
    let object = out.join("passes.o");
    output(
        Command::new(&cxx)
            .args(&flags)
            .args(["-fPIC", "-O2", "-c"])
            .arg(&unit)
            .arg("-o")
            .arg(&object),
    );
    let archive = out.join("libblaze_llvm_passes.a");
    output(Command::new(&ar).arg("crs").arg(&archive).arg(&object));
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=blaze_llvm_passes");

    let libdir = fs::canonicalize(query(&["--libdir"])).expect("LLVM library directory");
    assert!(
        libdir.starts_with(&prefix),
        "LLVM library directory is outside verified prefix: {}",
        libdir.display()
    );
    println!("cargo::rustc-link-search=native={}", libdir.display());
    for library in query(&["--libnames", "--link-static"]).split_whitespace() {
        let name = library
            .strip_prefix("lib")
            .and_then(|s| s.strip_suffix(".a"))
            .expect("LLVM static archive name");
        assert!(
            libdir.join(library).is_file(),
            "missing pinned LLVM archive {}",
            libdir.join(library).display()
        );
        // LLVM's support archive is part of this exact list, just like its code generators.
        println!("cargo::rustc-link-lib=static={name}");
    }
    let system = query(&["--system-libs", "--link-static"]);
    for library in system.split_whitespace() {
        if let Some(name) = library.strip_prefix("-l") {
            if name != "stdc++" && name != "c++" {
                println!("cargo::rustc-link-lib=dylib={name}");
            }
        } else if let Some(dir) = library.strip_prefix("-L") {
            println!("cargo::rustc-link-search=native={dir}");
        } else if Path::new(library).is_absolute() && library.ends_with(".a") {
            let path = Path::new(library);
            assert!(path.is_file(), "missing system archive {library}");
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("lib"))
                .and_then(|s| s.strip_suffix(".a"))
                .expect("system archive name");
            println!(
                "cargo::rustc-link-search=native={}",
                path.parent().expect("archive directory").display()
            );
            println!("cargo::rustc-link-lib=static={name}");
        } else {
            panic!("unsupported pinned LLVM system library flag {library}");
        }
    }
    if os == "macos" {
        println!("cargo::rustc-link-lib=dylib=c++");
    } else {
        // Ask the same target compiler that built the wrapper; never search host ld paths.
        let runtime = PathBuf::from(output(
            Command::new(&cxx).args(&flags).arg("-print-file-name=libstdc++.a"),
        ));
        assert!(
            runtime.is_absolute() && runtime.is_file(),
            "target C++ compiler found no static runtime: {}",
            runtime.display()
        );
        println!(
            "cargo::rustc-link-search=native={}",
            runtime.parent().expect("C++ runtime directory").display()
        );
        println!("cargo::rustc-link-lib=static:-bundle=stdc++");
    }
}
