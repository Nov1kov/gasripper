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
//! `key = true|false` are meaningful.

use std::collections::{HashMap, HashSet};

use crate::core::Category;
use crate::features;

/// Per-key feature enablement.
#[derive(Clone, Debug)]
pub struct FeatureConfig {
    enabled: HashMap<String, bool>,
}

impl FeatureConfig {
    /// Defaults from the feature registry (currently — all enabled).
    pub fn defaults() -> Self {
        let mut enabled = HashMap::new();
        for f in features::registry() {
            enabled.insert(f.key.to_string(), f.default_enabled);
        }
        FeatureConfig { enabled }
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
                .ok_or_else(|| format!("line {}: expected 'key = true|false'", lineno + 1))?;
            let key = key.trim();
            let val = val.trim();
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
        assert!(c.is_enabled("guards"), "the guards feature must default to enabled");
        assert!(c.is_enabled("shuffle"), "the shuffle feature must default to enabled");
        assert!(c.is_enabled("involution"), "the involution feature must default to enabled");
        assert!(c.is_enabled("recompute"), "the recompute feature must default to enabled");
        assert!(c.is_enabled("foldshift"), "the foldshift feature must default to enabled");
        assert_eq!(
            c.enabled_categories().len(),
            5,
            "every shipped category must be enabled by default"
        );
    }

    #[test]
    fn file_overrides_defaults() {
        let mut c = FeatureConfig::defaults();
        c.apply_file("[features]\nguards = false\n").unwrap();
        assert!(!c.is_enabled("guards"), "a config file must be able to disable the feature");
    }

    #[test]
    fn cli_overrides_file() {
        let mut c = FeatureConfig::defaults();
        c.apply_file("guards = false\n").unwrap();
        c.enable("guards").unwrap();
        assert!(c.is_enabled("guards"), "a CLI enable must override the config file");
    }

    #[test]
    fn unknown_feature_errors() {
        let mut c = FeatureConfig::defaults();
        assert!(c.disable("nope").is_err());
        assert!(c.apply_file("nope = true\n").is_err());
    }
}
