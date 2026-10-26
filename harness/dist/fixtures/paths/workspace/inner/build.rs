use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(
        out.join("generated.rs"),
        "pub const TABLE: [u32; 4] = [3, 1, 4, 1];\npub fn generated() -> &'static str { \"made by the build script\" }\n",
    )
    .unwrap();
    fs::write(out.join("blob.bin"), [1u8, 2, 3, 5, 8]).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=INNER_BUILT=yes");
}
