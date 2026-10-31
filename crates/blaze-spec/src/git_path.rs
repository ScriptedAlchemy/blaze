//! Git path output is native bytes; textual object ids and metadata use a different reader.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

/// Decode one Git line, removing only its line terminator, never filename whitespace.
pub fn decode_line(mut bytes: Vec<u8>) -> Result<PathBuf, String> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    #[cfg(windows)]
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.contains(&0) {
        return Err("Git path contains NUL".to_owned());
    }
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    };
    #[cfg(not(unix))]
    let name = OsString::from(
        String::from_utf8(bytes).map_err(|_| "Git path is not representable on this host")?,
    );
    Ok(name.into())
}

/// The materialized repository root. No UTF-8 or trimming assumption is applied to paths.
pub fn root(cwd: &Path) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "Git did not locate a repository root: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    decode_line(output.stdout)
}
