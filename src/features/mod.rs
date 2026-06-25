//! Optimization features. Each feature is a class of bytecode "trimming".
//!
//! Currently all features rely on the shared engine [`crate::core::strip_guards`]
//! and own one [`Category`] of stripped revert guards. By default ALL features
//! are enabled; they can be disabled via the CLI (`--disable`) or a config file.
//!
//! Each feature lives in its own module, exports a [`FeatureMeta`] and a thin
//! `strip` function, and keeps its own set of tests pinning down exactly what it
//! removes (and what it does not).

use crate::core::Category;

#[cfg(test)]
pub mod e2e_harness;
pub mod guards;

/// Feature metadata for the registry, CLI, and config.
#[derive(Clone, Copy, Debug)]
pub struct FeatureMeta {
    /// Stable key (for `--disable`/config).
    pub key: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// Short description of what is stripped.
    pub description: &'static str,
    /// The revert-guard category this feature owns.
    pub category: Category,
    /// Whether enabled by default (currently — all enabled).
    pub default_enabled: bool,
}

/// The full registry of available features.
pub fn registry() -> Vec<FeatureMeta> {
    vec![guards::META]
}

/// Find a feature's metadata by key.
pub fn find(key: &str) -> Option<FeatureMeta> {
    registry().into_iter().find(|f| f.key == key)
}
