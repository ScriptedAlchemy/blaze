//! The harness's platform-independent cores, read directly from the product source.
#![feature(allocator_api, allocator_ext)]

#[path = "../../../crates/harness/harness-browser/src/cdp_ids.rs"]
pub mod cdp_ids;
#[path = "../../../crates/harness/harness-native/src/audio/completion.rs"]
pub mod completion;
#[path = "../../../crates/harness/harness-native/src/app/browser/fileview.rs"]
pub mod fileview;
#[path = "../../../crates/harness/harness-browser/src/keys.rs"]
pub mod keys;
#[path = "../../../crates/harness/harness-browser/src/ui.rs"]
pub mod ui;

/// The renderer tests never open an app page or resolve its on-disk home. Refuse such a call
/// so this adapter cannot accidentally claim coverage of the native app's IO boundary.
mod link {
    pub fn home() -> std::path::PathBuf {
        panic!("the pure-core adapter does not open pages in the native app's home")
    }
}
