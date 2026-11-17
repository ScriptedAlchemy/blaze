//! A build script: its run is the daemon's own, on every executor, and its output is an input of
//! the unit's keys.

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/from_script.txt"), "from the script").unwrap();
    println!("cargo::rerun-if-changed=build.rs");
}
