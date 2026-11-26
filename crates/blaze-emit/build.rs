//! Holds the committed template table (table/arm64.bztab, made by `examples/gen_table.rs` from
//! enc.rs; backend-next.md D99) to its pin (table/arm64.json): the BLAKE3 digest of its bytes.
//! It only checks: it makes nothing, runs nothing and reads nothing but the two files, which it
//! includes. Whether the table is what today's encoders make is the test
//! `table_gen::tests::the_committed_table_is_the_generators`. The model is blaze-stencil's pin
//! (crates/blaze-stencil/pin.rs).

use core::fmt;

const TABLE: &[u8] = include_bytes!("table/arm64.bztab");
const PIN: &str = include_str!("table/arm64.json");
const KEY: &str = "\"blake3\": \"";

/// The native release tier's implementation: the lowering (`src/llvm.rs` and its modules), the
/// object reader (`src/llvm/object.rs`), and the pass pipeline the emitter runs
/// (`vendor/llvm-sys/wrappers/passes.cpp`, which `LLVMBlazeRunPasses` builds), whose bytes are
/// hashed into `BLAZE_EMIT_NATIVE_REVISION`. A record's bytes depend on every one of these, so
/// the backend's identity (`llvm::emit::identity`) names this digest: a repair to any of them is
/// different work, and a recipe keyed on the old identity is not reused for it. Test files are
/// left out: a test changes what is proved, not what is made. Include the owned declarations
/// and linkage because both participate in the native producer.
const NATIVE: &[(&str, &str)] = &[
    ("src/llvm.rs", include_str!("src/llvm.rs")),
    ("src/llvm/abi.rs", include_str!("src/llvm/abi.rs")),
    ("src/llvm/control.rs", include_str!("src/llvm/control.rs")),
    ("src/llvm/emit.rs", include_str!("src/llvm/emit.rs")),
    ("src/llvm/folding.rs", include_str!("src/llvm/folding.rs")),
    ("src/llvm/instruction.rs", include_str!("src/llvm/instruction.rs")),
    ("src/llvm/memory.rs", include_str!("src/llvm/memory.rs")),
    ("src/llvm/object.rs", include_str!("src/llvm/object.rs")),
    ("src/llvm/tls.rs", include_str!("src/llvm/tls.rs")),
    ("src/llvm/traps.rs", include_str!("src/llvm/traps.rs")),
    ("src/pass/target.rs", include_str!("src/pass/target.rs")),
    ("../blaze-llvm-abi/abi.h", include_str!("../blaze-llvm-abi/abi.h")),
    ("../blaze-llvm-abi/build.rs", include_str!("../blaze-llvm-abi/build.rs")),
    ("../blaze-llvm-abi/src/lib.rs", include_str!("../blaze-llvm-abi/src/lib.rs")),
    ("../../vendor/llvm-sys/pin.rs", include_str!("../../vendor/llvm-sys/pin.rs")),
    ("../../vendor/llvm-sys/wrappers/passes.cpp", include_str!("../../vendor/llvm-sys/wrappers/passes.cpp")),
];

/// The BLAKE3 of the native implementation's files, each as its path then its bytes, in
/// `NATIVE`'s order: a change to any file, a rename or a reorder changes the digest.
fn native_revision() -> [u8; 32] {
    let mut hash = blaze_blake3::Hasher::new();
    for (path, bytes) in NATIVE {
        hash.update(path.as_bytes()).update(&0u32.to_le_bytes()).update(bytes.as_bytes()).update(&0u32.to_le_bytes());
    }
    hash.finalize()
}

/// Why the table was refused: each names the file.
enum PinError {
    /// The pin has no 64-hex-digit `blake3` value.
    Malformed,
    /// The table's digest is not the pinned one.
    Digest { pinned: [u8; 32], have: [u8; 32] },
}

struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A build script's `main` shows its error by `Debug`: the message is the one to read.
impl fmt::Debug for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinError::Malformed => f.write_str("crates/blaze-emit/table/arm64.json holds no `\"blake3\": \"<64 hex digits>\"`"),
            PinError::Digest { pinned, have } => write!(
                f,
                "crates/blaze-emit/table/arm64.bztab hashes to {} but table/arm64.json pins {}: make both again with `cargo run -p blaze-emit --example gen_table` from the workspace's root",
                Hex(have),
                Hex(pinned)
            ),
        }
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

fn pinned(pin: &str) -> Result<[u8; 32], PinError> {
    let at = pin.find(KEY).ok_or(PinError::Malformed)? + KEY.len();
    let hex = pin.as_bytes().get(at..at + 64).ok_or(PinError::Malformed)?;
    let mut out = [0u8; 32];
    for (k, pair) in hex.chunks_exact(2).enumerate() {
        out[k] = nibble(pair[0]).ok_or(PinError::Malformed)? << 4 | nibble(pair[1]).ok_or(PinError::Malformed)?;
    }
    Ok(out)
}

fn main() -> Result<(), PinError> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=table/arm64.bztab");
    println!("cargo::rerun-if-changed=table/arm64.json");
    for (path, _) in NATIVE {
        println!("cargo::rerun-if-changed={path}");
    }
    let pinned = pinned(PIN)?;
    let have = blaze_blake3::hash(TABLE);
    match have == pinned {
        true => Ok(()),
        false => Err(PinError::Digest { pinned, have }),
    }?;
    let revision = native_revision();
    let mut spelled = String::with_capacity(64);
    for byte in revision {
        use core::fmt::Write;
        write!(&mut spelled, "{byte:02x}").expect("a String holds 64 hex digits");
    }
    println!("cargo::rustc-env=BLAZE_EMIT_NATIVE_REVISION={spelled}");
    Ok(())
}
