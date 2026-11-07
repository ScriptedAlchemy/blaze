//! Writes the golden vectors checked in under `tests/golden/`, one file per schema type.
//!
//! `blaze test -p blaze-wire` checks every type's vector against its file; this writes the files
//! again after a schema change that is meant to change them, so the diff is the review:
//!
//! ```text
//! blaze run -p blaze-wire --example write_golden
//! ```
//!
//! A type's vector is one value of it from seed 1: its canonical CBOR, and the digest of that under
//! the type's domain separation.

#[allow(clippy::print_stderr)]
fn main() -> std::io::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    std::fs::create_dir_all(&root)?;
    let mut written = 0;
    for entry in blaze_wire::vectors::every() {
        let vector = (entry.vector)(1);
        std::fs::write(root.join(format!("{}.txt", entry.name)), vector.to_golden())?;
        written += 1;
    }
    eprintln!("wrote {written} golden vectors to {}", root.display());
    Ok(())
}
