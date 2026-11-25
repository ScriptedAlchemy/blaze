//! Build-script identity and the runtime shim used by the shared selector.
pub use blaze_manifest::bootstrap::*;
pub const SHIM: &[u8] = include_bytes!("../guest/script_process.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::sha256_hex;

    fn wanted<'a>(files: &'a str, deps: &'a str) -> Wanted<'a> {
        Wanted {
            krate: CrateId { name: "shapes", version: "0.1.0", source: Source::Path { files_sha256: files }, deps, features: &[] },
            toolchain: "nightly-test", runner: Runner::Wasm { shim: "process-shim" },
        }
    }
    fn revisions(stale_selection: bool) {
        blaze_sim::check(blaze_sim::Config::cases(64), |tape| {
            let source = format!("source-{}", tape.below(1000, "source edit"));
            let deps = format!("deps-{}", tape.below(1000, "lock edit"));
            let before = wanted("old-source", "old-deps");
            let after = wanted(&source, &deps);
            let old = format!("{before}patches runtime\n");
            let new = format!("{after}patches runtime\n");
            let (old_key, new_key) = (sha256_hex(old.as_bytes()), sha256_hex(new.as_bytes()));
            for changed in [wanted(&source, "old-deps"), wanted("old-source", &deps)] {
                assert_eq!(select(&changed, [(old_key.as_str(), old.as_str())]).unwrap(), None);
            }
            let ask = if stale_selection { &before } else { &after };
            assert_eq!(select(ask, [(old_key.as_str(), old.as_str())]).unwrap(), None, "stale module accepted");
            let mut records = [(old_key.as_str(), old.as_str()), (new_key.as_str(), new.as_str())];
            if tape.coin("listing order") { records.reverse(); }
            let found = select(ask, records).unwrap().unwrap();
            assert_eq!(found.key(), new_key);
            let proof = found.with_digest(&sha256_hex(b"module bytes")).unwrap();
            assert_eq!(Selected::from_task(&proof.encode()), Some(proof));
            assert_eq!(Selected::from_task(b"w../module"), None);
            let other = format!("{after}patches different-runtime\n");
            let other_key = sha256_hex(other.as_bytes());
            assert!(matches!(select(&after, [(new_key.as_str(), new.as_str()), (other_key.as_str(), other.as_str())]), Err(Refused::Ambiguous { .. })));
        });
    }
    #[test]
    fn source_edits_select_their_record_with_old_records_present() { revisions(false); }

    #[test]
    fn duplicate_selector_fields_never_establish_compatibility() {
        for runner in [Runner::Wasm { shim: "process-shim" }, Runner::Native { target: "test-target" }] {
            let mut wanted = wanted("source", "deps");
            wanted.runner = runner;
            for source in [Source::Path { files_sha256: "source" }, Source::Registry { checksum: "checksum" }, Source::Git { commit: "commit", path: "path" }] {
                wanted.krate.source = source;
                let valid = format!("{wanted}");
                let valid_key = sha256_hex(valid.as_bytes());
                assert!(select(&wanted, [(valid_key.as_str(), valid.as_str())]).unwrap().is_some());
                for line in ["name shapes", "version 0.1.0", "deps deps", "toolchain nightly-test", "builder bootstrap", "features ", "shim process-shim", "target test-target", "files source", "checksum checksum", "git commit path"] {
                    let malformed = format!("{valid}{line}\n{line}\n");
                    let key = sha256_hex(malformed.as_bytes());
                    assert_eq!(select(&wanted, [(key.as_str(), malformed.as_str())]).unwrap(), None, "duplicate field accepted: {line}");
                }
            }
        }
    }

    #[test]
    fn workspace_manifest_inputs_are_covered() {
        let alloc = blaze_own::testing::heap().alloc();
        assert!(standalone_manifest("[package]\nname = \"p\"\nedition = \"2021\"", alloc));
        assert!(!standalone_manifest("[package]\nname = \"p\"\nedition.workspace = true", alloc));
        assert!(!standalone_manifest("[build-dependencies]\ncompiler = { workspace = true }", alloc));
        assert!(!standalone_manifest("[target.'cfg(unix)'.build-dependencies.compiler]\nworkspace = true", alloc));
        let files = digest_hex(b"package files");
        let before = b"[workspace.package]\nedition = '2021'\n";
        let after = b"[workspace.package]\nedition = '2024'\n";
        assert_eq!(path_source(files, None), files, "explicit settings keep their package source identity");
        assert_ne!(path_source(files, Some(before)), files);
        assert_ne!(path_source(files, Some(before)), path_source(files, Some(after)), "an inherited edition edit changes the module source identity");
    }
    #[test]
    #[should_panic(expected = "stale module accepted")]
    fn source_selection_control() { revisions(true); }
}
