//! The build-script guest's changes to pinned std. The producer uses these anchors and
//! replacements; the host uses the digest of this file to identify compatible guests.

/// The recipe's identity field and bytes. Both the bootstrap producer and runtime use them.
pub const IDENTITY_FIELD: &str = "wasi-std";
pub const PATCH_RECIPE: &[u8] = include_bytes!("wasi_std.rs");

/// Startup anchor and the code that enters the package's working directory.
pub const RT_ANCHOR: &str = "        sys::init(argc, argv, sigpipe)\n    };\n";

pub const RT_PATCH: &str = "    // blaze: a build script runs in its package's folder, as Cargo runs it; wasi-libc starts
    // in `/`. A script with no `CARGO_MANIFEST_DIR` is not one blaze runs.
    #[cfg(target_os = \"wasi\")]
    {
        match crate::env::var_os(\"CARGO_MANIFEST_DIR\") {
            Some(dir) if crate::env::set_current_dir(&dir).is_ok() => {}
            _ => rtabort!(\"blaze: CARGO_MANIFEST_DIR could not be entered\"),
        }
    }
";

pub const PERM_ANCHOR: &str = "    pub fn perm(&self) -> FilePermissions {\n        FilePermissions { mode: (self.stat.st_mode as mode_t) }\n    }\n";

pub const PERM_PATCH: &str = "    pub fn perm(&self) -> FilePermissions {
        // blaze: WASI preview 1 has rights per descriptor and no modes, so wasi-libc's `st_mode`
        // is the file type alone and every path would read as read-only: `autocfg::new()`
        // refuses `OUT_DIR` as \"not a writable directory\". The sandbox decides each write at
        // the call, so the mode says writable.
        #[cfg(target_os = \"wasi\")]
        return FilePermissions { mode: (self.stat.st_mode as mode_t) | 0o200 };
        #[cfg(not(target_os = \"wasi\"))]
        FilePermissions { mode: (self.stat.st_mode as mode_t) }
    }
";

pub const WASI_PATHS_ANCHOR: &str = r#"    target_os = "wasi" => {
        mod wasi;
        #[expect(dead_code)]
        mod unsupported;
        mod imp {
            pub use super::unsupported::{
                JoinPathsError, SplitPaths, SplitPathsRef, current_exe, home_dir, join_paths,
                split_paths, split_paths_ref,
            };
            pub use super::wasi::{chdir, getcwd, temp_dir};
        }
    }
"#;

pub const WASI_PATHS_PATCH: &str = r#"    target_os = "wasi" => {
        mod wasi;
        mod wasi_path_list;
        #[expect(dead_code)]
        mod unsupported;
        mod imp {
            pub use super::unsupported::{current_exe, home_dir};
            pub use super::wasi::{chdir, getcwd, temp_dir};
            pub use super::wasi_path_list::{
                JoinPathsError, SplitPaths, SplitPathsRef, join_paths, split_paths, split_paths_ref,
            };
        }
    }
"#;

// These delimit the pinned Unix implementation's purely lexical path-list operations.
pub const UNIX_PATH_LIST_START: &str = "// This can't just be `impl Iterator` because that requires `'a` to be live on";
pub const UNIX_PATH_LIST_END: &str = "#[cfg(target_os = \"aix\")]\npub fn current_exe()";
pub const WASI_PATH_LIST_HEADER: &str = "use crate::ffi::{OsStr, OsString};\n\
use crate::os::wasi::ffi::{OsStrExt, OsStringExt};\n\
use crate::path::{Path, PathBuf};\n\
use crate::{fmt, iter, slice};\n\
\n\
const PATH_SEPARATOR: u8 = b':';\n\n";
