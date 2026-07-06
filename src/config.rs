//! Configuration of the set of enabled features.
//!
//! Precedence (from lowest to highest):
//!   1. defaults — all features enabled;
//!   2. config file (`--config`), if provided;
//!   3. CLI flags (`--enable` / `--disable`).
//!
//! By default no config file is required or searched for — the tool runs on
//! defaults alone.
//!
//! The file format is a minimal, TOML-compatible subset:
//! ```text
//! # comment
//! [features]
//! guards = true
//! ```
//! Section headers (`[...]`) and comments (`#`) are ignored; lines of the form
//! `key = true|false` toggle a feature, and integer lines set the numeric pass parameters:
//! `inline_max_body` (inline body-size threshold) and — in an `smt` build — the superopt
//! search limits `superopt_max_block`, `superopt_max_synth`, `superopt_timeout_ms`,
//! `superopt_max_checks`.

use std::collections::{HashMap, HashSet};

use crate::core::Category;
use crate::features;

/// Config-file/CLI key for the inline body-size threshold (a numeric parameter, not a feature
/// toggle).
const INLINE_MAX_BODY_KEY: &str = "inline_max_body";

/// Per-key feature enablement plus numeric pass parameters.
#[derive(Clone, Debug)]
pub struct FeatureConfig {
    enabled: HashMap<String, bool>,
    params: features::Params,
}

impl FeatureConfig {
    /// Defaults from the feature registry (currently — all enabled) and the default pass
    /// parameters.
    pub fn defaults() -> Self {
        let mut enabled = HashMap::new();
        for f in features::registry() {
            enabled.insert(f.key.to_string(), f.default_enabled);
        }
        FeatureConfig {
            enabled,
            params: features::Params::default(),
        }
    }

    /// Apply a config file on top of the current values.
    pub fn apply_file(&mut self, content: &str) -> Result<(), String> {
        for (lineno, raw) in content.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let (key, val) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: expected 'key = value'", lineno + 1))?;
            let key = key.trim();
            let val = val.trim();
            if let Some(slot) = self.numeric(key) {
                *slot = val.parse().map_err(|_| {
                    format!("line {}: expected an integer, got '{val}'", lineno + 1)
                })?;
                continue;
            }
            #[cfg(feature = "smt")]
            if key == "superopt_timeout_ms" {
                self.params.superopt.timeout_ms = val.parse().map_err(|_| {
                    format!("line {}: expected an integer, got '{val}'", lineno + 1)
                })?;
                continue;
            }
            if features::find(key).is_none() {
                return Err(format!("line {}: unknown feature '{key}'", lineno + 1));
            }
            let on = match val {
                "true" | "1" | "on" | "yes" => true,
                "false" | "0" | "off" | "no" => false,
                _ => return Err(format!("line {}: expected a bool, got '{val}'", lineno + 1)),
            };
            self.enabled.insert(key.to_string(), on);
        }
        Ok(())
    }

    /// The `usize` numeric-parameter slot a config/CLI key addresses, if any (the superopt
    /// timeout is `u32` and handled separately).
    fn numeric(&mut self, key: &str) -> Option<&mut usize> {
        match key {
            INLINE_MAX_BODY_KEY => Some(&mut self.params.inline_max),
            #[cfg(feature = "smt")]
            "superopt_max_block" => Some(&mut self.params.superopt.max_block),
            #[cfg(feature = "smt")]
            "superopt_max_synth" => Some(&mut self.params.superopt.max_synth),
            #[cfg(feature = "smt")]
            "superopt_max_checks" => Some(&mut self.params.superopt.max_checks),
            _ => None,
        }
    }

    /// Set the inline pass body-size threshold (instructions).
    #[inline]
    pub fn set_inline_max_body(&mut self, n: usize) {
        self.params.inline_max = n;
    }

    /// Mutable access to the superopt search limits (for the CLI overrides).
    #[cfg(feature = "smt")]
    #[inline]
    pub fn superopt(&mut self) -> &mut crate::features::superopt::Limits {
        &mut self.params.superopt
    }

    /// The resolved numeric pass parameters.
    #[inline]
    pub fn params(&self) -> features::Params {
        self.params
    }

    /// Disable a feature by key. Errors for an unknown key.
    pub fn disable(&mut self, key: &str) -> Result<(), String> {
        self.set(key, false)
    }

    /// Enable a feature by key. Errors for an unknown key.
    pub fn enable(&mut self, key: &str) -> Result<(), String> {
        self.set(key, true)
    }

    fn set(&mut self, key: &str, on: bool) -> Result<(), String> {
        if features::find(key).is_none() {
            return Err(format!("unknown feature '{key}'"));
        }
        self.enabled.insert(key.to_string(), on);
        Ok(())
    }

    /// Whether a feature is enabled.
    pub fn is_enabled(&self, key: &str) -> bool {
        self.enabled.get(key).copied().unwrap_or(false)
    }

    /// The set of categories to strip (from the enabled features).
    pub fn enabled_categories(&self) -> HashSet<Category> {
        features::registry()
            .into_iter()
            .filter(|f| self.is_enabled(f.key))
            .map(|f| f.category)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_all_on() {
        let c = FeatureConfig::defaults();
        assert!(
            c.is_enabled("inline"),
            "the inline feature must default to enabled"
        );
        assert!(
            c.is_enabled("guards"),
            "the guards feature must default to enabled"
        );
        assert!(
            c.is_enabled("shuffle"),
            "the shuffle feature must default to enabled"
        );
        assert!(
            c.is_enabled("involution"),
            "the involution feature must default to enabled"
        );
        assert!(
            c.is_enabled("recompute"),
            "the recompute feature must default to enabled"
        );
        assert!(
            c.is_enabled("foldshift"),
            "the foldshift feature must default to enabled"
        );
        assert!(
            c.is_enabled("cmpnorm"),
            "the cmpnorm feature must default to enabled"
        );
        // Seven default categories ship; an `smt`-feature build adds `superopt`.
        let expected = 7 + cfg!(feature = "smt") as usize;
        assert_eq!(
            c.enabled_categories().len(),
            expected,
            "every shipped category must be enabled by default"
        );
        assert_eq!(
            c.params().inline_max,
            features::inline::DEFAULT_MAX_BODY,
            "the inline threshold must default to the feature's default"
        );
    }

    #[test]
    fn config_file_sets_inline_threshold() {
        // The numeric inline parameter is read from the config file like a feature toggle.
        let mut c = FeatureConfig::defaults();
        c.apply_file("[features]\ninline_max_body = 35\n").unwrap();
        assert_eq!(
            c.params().inline_max,
            35,
            "the config file must set the inline body threshold"
        );
    }

    #[cfg(feature = "smt")]
    #[test]
    fn config_file_sets_superopt_limits() {
        // All four superopt search limits are read from the config file.
        let mut c = FeatureConfig::defaults();
        c.apply_file(
            "superopt_max_block = 32\nsuperopt_max_synth = 5\n\
             superopt_timeout_ms = 250\nsuperopt_max_checks = 64\n",
        )
        .unwrap();
        let s = c.params().superopt;
        assert_eq!(s.max_block, 32, "the config file must set max_block");
        assert_eq!(s.max_synth, 5, "the config file must set max_synth");
        assert_eq!(s.timeout_ms, 250, "the config file must set timeout_ms");
        assert_eq!(s.max_checks, 64, "the config file must set max_checks");
    }

    #[cfg(feature = "smt")]
    #[test]
    fn non_integer_superopt_limit_errors() {
        let mut c = FeatureConfig::defaults();
        assert!(
            c.apply_file("superopt_max_synth = huge\n").is_err(),
            "a non-integer superopt limit must be rejected"
        );
    }

    #[test]
    fn non_integer_inline_threshold_errors() {
        let mut c = FeatureConfig::defaults();
        assert!(
            c.apply_file("inline_max_body = big\n").is_err(),
            "a non-integer inline threshold must be rejected"
        );
    }

    #[test]
    fn file_overrides_defaults() {
        let mut c = FeatureConfig::defaults();
        c.apply_file("[features]\nguards = false\n").unwrap();
        assert!(
            !c.is_enabled("guards"),
            "a config file must be able to disable the feature"
        );
    }

    #[test]
    fn cli_overrides_file() {
        let mut c = FeatureConfig::defaults();
        c.apply_file("guards = false\n").unwrap();
        c.enable("guards").unwrap();
        assert!(
            c.is_enabled("guards"),
            "a CLI enable must override the config file"
        );
    }

    #[test]
    fn unknown_feature_errors() {
        let mut c = FeatureConfig::defaults();
        assert!(c.disable("nope").is_err());
        assert!(c.apply_file("nope = true\n").is_err());
    }
}
