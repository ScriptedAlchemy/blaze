//! Effective code-generation flags of a crate. These are compiler inputs, not synthetic
//! cfgs: changing instruction selection must invalidate compiled queries even when the
//! source and its conditional compilation are unchanged.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TargetMachine {
    pub cpu: Option<String>,
    pub opt_level: Option<String>,
    /// Ordered operations: a later feature can enable a prerequisite disabled earlier.
    pub features: Vec<(String, bool)>,
}

impl TargetMachine {
    pub fn from_flags(flags: &[String]) -> Self {
        let mut options = Self::default();
        let mut words = flags.iter().map(String::as_str);
        while let Some(word) = words.next() {
            let option = match word {
                "-C" | "--codegen" => words.next(),
                _ => word.strip_prefix("-C").or_else(|| word.strip_prefix("--codegen=")),
            };
            let Some((name, value)) = option.and_then(|it| it.split_once('=')) else { continue };
            match name.replace('_', "-").as_str() {
                "target-cpu" => options.cpu = Some(value.to_owned()),
                "opt-level" => options.opt_level = Some(value.to_owned()),
                "target-feature" => {
                    for feature in value.split(',') {
                        if let Some(name) = feature.strip_prefix('+') {
                            options.features.push((name.to_owned(), true));
                        } else if let Some(name) = feature.strip_prefix('-') {
                            options.features.push((name.to_owned(), false));
                        }
                    }
                }
                _ => {}
            }
        }
        options
    }

    pub fn llvm_features(&self) -> String {
        self.features
            .iter()
            .map(|(name, enabled)| format!("{}{name}", if *enabled { '+' } else { '-' }))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Final overrides, including forward implications when enabling and inverse ones
    /// when disabling, applied in the order the flags were written.
    pub fn feature_overrides(&self, arch: &str) -> BTreeMap<String, bool> {
        crate::target_feature::flag_changes(arch, &self.features)
    }

    pub fn effective_features(
        &self,
        arch: &str,
        initial: impl IntoIterator<Item = String>,
    ) -> std::collections::BTreeSet<String> {
        crate::target_feature::flag_features(arch, initial, &self.features)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_names_accumulate_but_settings_and_cpu_use_command_line_order() {
        let flags = [
            "-Ctarget-feature=+lse,+fp16",
            "--codegen",
            "target_cpu=generic",
            "-C",
            "target-feature=-lse,+dotprod",
            "--codegen=target-cpu=apple-m1",
        ]
        .map(str::to_owned);
        let options = TargetMachine::from_flags(&flags);
        assert_eq!(options.cpu.as_deref(), Some("apple-m1"));
        assert_eq!(options.llvm_features(), "+lse,+fp16,-lse,+dotprod");
    }

    #[test]
    fn profile_options_precede_flags_and_feature_dependencies_are_arch_specific() {
        let flags = ["-Copt-level=0", "-C", "opt_level=3", "-Ctarget-feature=+avx2,-sse2"]
            .map(str::to_owned);
        let options = TargetMachine::from_flags(&flags);
        assert_eq!(options.opt_level.as_deref(), Some("3"));
        let x86 = options.effective_features("x86_64", ["sse2".to_owned()]);
        assert!(!x86.contains("avx2") && !x86.contains("sse2"));
        let options = TargetMachine::from_flags(&["-Ctarget-feature=+fp16".to_owned()]);
        let arm = options.effective_features("aarch64", []);
        assert!(arm.contains("fp16") && arm.contains("neon"));
    }

    #[test]
    fn feature_implications_follow_command_line_order() {
        for (changes, enabled) in [
            ("-sse2,+avx2", true),
            ("+avx2,-sse2", false),
            ("-avx,+avx2", true),
            ("+avx2,-avx", false),
            ("+avx2,-sse4.2,+avx2", true),
        ] {
            let options = TargetMachine::from_flags(&[format!("-Ctarget-feature={changes}")]);
            let features = options.effective_features("x86_64", ["sse2".to_owned()]);
            assert_eq!(features.contains("avx2"), enabled, "{changes}");
            assert_eq!(features.contains("avx"), enabled, "{changes}");
        }
        let first = TargetMachine::from_flags(&["-Ctarget-feature=-avx,+avx2".to_owned()]);
        let second = TargetMachine::from_flags(&["-Ctarget-feature=+avx2,-avx".to_owned()]);
        assert_ne!(first, second, "different effective machines cannot share a cache key");
        assert_eq!(first.llvm_features(), "-avx,+avx2");
        assert_eq!(second.llvm_features(), "+avx2,-avx");
        for (changes, enabled) in [("-neon,+fp16", true), ("+fp16,-neon", false)] {
            let options = TargetMachine::from_flags(&[format!("-Ctarget-feature={changes}")]);
            let features = options.effective_features("aarch64", ["neon".to_owned()]);
            assert_eq!(features.contains("fp16"), enabled, "{changes}");
            assert_eq!(features.contains("neon"), enabled, "{changes}");
        }
    }
}
