//! Effective code-generation flags of a crate. These are compiler inputs, not synthetic
//! cfgs: changing instruction selection must invalidate compiled queries even when the
//! source and its conditional compilation are unchanged.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TargetMachine {
    pub cpu: Option<String>,
    pub opt_level: Option<String>,
    /// Last setting wins for each feature; different feature names accumulate.
    pub features: BTreeMap<String, bool>,
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
                            options.features.insert(name.to_owned(), true);
                        } else if let Some(name) = feature.strip_prefix('-') {
                            options.features.insert(name.to_owned(), false);
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
        assert_eq!(options.llvm_features(), "+dotprod,+fp16,-lse");
    }
}
